use crate::app_config::AppType;
use crate::database::ProviderTablesSnapshot;
use crate::error::AppError;
use crate::services::proxy::ProxyTransactionSnapshot;
use crate::services::ProviderService;
use crate::store::AppState;
use std::future::Future;
use std::sync::Arc;

struct CodexProviderLifecycleSnapshot {
    provider_tables: ProviderTablesSnapshot,
    local_current_provider: Option<String>,
    proxy: ProxyTransactionSnapshot,
}

#[cfg(test)]
#[derive(Clone)]
struct RollbackFileGuardPause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
fn rollback_file_guard_pause() -> &'static std::sync::Mutex<Option<RollbackFileGuardPause>> {
    static PAUSE: std::sync::OnceLock<std::sync::Mutex<Option<RollbackFileGuardPause>>> =
        std::sync::OnceLock::new();
    PAUSE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn set_rollback_file_guard_pause_for_test(
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) {
    *rollback_file_guard_pause()
        .lock()
        .expect("lock rollback file guard pause") =
        Some(RollbackFileGuardPause { entered, release });
}

#[cfg(test)]
async fn wait_at_rollback_file_guard_pause_for_test() {
    let pause = rollback_file_guard_pause()
        .lock()
        .expect("lock rollback file guard pause")
        .take();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.release.notified().await;
    }
}

pub(crate) async fn run_codex_provider_mutation<T, F>(
    state: Arc<AppState>,
    label: &'static str,
    operation: F,
) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(Arc<AppState>) -> Result<T, AppError> + Send + 'static,
{
    run_codex_provider_async_mutation(state, label, move |state| async move {
        tokio::task::spawn_blocking(move || operation(state))
            .await
            .map_err(|error| AppError::Message(format!("{label} operation task failed: {error}")))?
    })
    .await
}

/// Runs `operation` while the proxy transaction is held. The operation must use
/// only `*_inner` or `*_under_proxy_transaction` APIs that do not reacquire it.
async fn run_codex_provider_async_mutation<T, F, Fut>(
    state: Arc<AppState>,
    label: &'static str,
    operation: F,
) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(Arc<AppState>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, AppError>> + Send + 'static,
{
    let supervisor = tokio::spawn(async move {
        let _lifecycle = state.lock_codex_provider_lifecycle().await;
        let _proxy_transaction = state.proxy_service.lock_transaction().await;
        let mut snapshot = CodexProviderLifecycleSnapshot {
            provider_tables: state.db.snapshot_provider_tables(AppType::Codex.as_str())?,
            local_current_provider: crate::settings::get_current_provider(&AppType::Codex),
            proxy: state
                .proxy_service
                .snapshot_transaction_state()
                .await
                .map_err(AppError::Message)?,
        };

        let operation_result = tokio::spawn(operation(Arc::clone(&state))).await;
        let primary_error = match operation_result {
            Ok(Ok(value)) => {
                match crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(
                    state.as_ref(),
                )
                .await
                {
                    Ok(_) => return Ok(value),
                    Err(error) => error,
                }
            }
            Ok(Err(error)) => error,
            Err(error) => AppError::Message(format!("{label} operation task failed: {error}")),
        };

        let rollback_errors = rollback_codex_provider_mutation(state.as_ref(), &mut snapshot).await;
        if rollback_errors.is_empty() {
            Err(primary_error)
        } else {
            Err(AppError::Message(format!(
                "{primary_error}; Codex Provider rollback encountered: {}",
                rollback_errors.join("; ")
            )))
        }
    });

    supervisor
        .await
        .map_err(|error| AppError::Message(format!("{label} supervisor task failed: {error}")))?
}

/// Updates Codex failover state while the lifecycle runner holds the proxy transaction lock.
fn set_codex_auto_failover_enabled_under_proxy_transaction(
    state: &AppState,
    enabled: bool,
) -> Result<Option<String>, AppError> {
    let mut config =
        futures::executor::block_on(state.db.get_proxy_config_for_app(AppType::Codex.as_str()))?;

    if enabled && !config.enabled {
        return Err(AppError::Message(
            "需要先启用该应用的代理接管，再开启故障转移".to_string(),
        ));
    }

    let p1_provider_id = if enabled {
        let mut queue = state.db.get_failover_queue(AppType::Codex.as_str())?;

        if queue.is_empty() {
            let current_id =
                crate::settings::get_effective_current_provider(&state.db, &AppType::Codex)?;
            let Some(current_id) = current_id else {
                return Err(AppError::Message(
                    "故障转移队列为空，且未设置当前供应商，无法开启故障转移".to_string(),
                ));
            };

            state
                .db
                .add_to_failover_queue(AppType::Codex.as_str(), &current_id)?;
            queue = state.db.get_failover_queue(AppType::Codex.as_str())?;
        }

        let p1_provider_id = queue
            .first()
            .map(|item| item.provider_id.clone())
            .ok_or_else(|| AppError::Message("故障转移队列为空，无法开启故障转移".to_string()))?;
        ProviderService::switch_under_proxy_transaction(state, AppType::Codex, &p1_provider_id)?;
        Some(p1_provider_id)
    } else {
        None
    };

    config.auto_failover_enabled = enabled;
    futures::executor::block_on(state.db.update_proxy_config_for_app(config))?;

    Ok(p1_provider_id)
}

pub(crate) async fn set_codex_auto_failover_enabled(
    state: Arc<AppState>,
    enabled: bool,
) -> Result<Option<String>, AppError> {
    run_codex_provider_mutation(state, "set Codex auto failover", move |state| {
        set_codex_auto_failover_enabled_under_proxy_transaction(state.as_ref(), enabled)
    })
    .await
}

pub(crate) async fn enable_codex_auto_mode_from_tray(
    state: Arc<AppState>,
) -> Result<String, AppError> {
    run_codex_provider_async_mutation(
        state,
        "enable Codex Auto mode from tray",
        move |state| async move {
            state
                .proxy_service
                .set_takeover_for_app_inner(AppType::Codex.as_str(), true)
                .await
                .map_err(|error| AppError::Message(format!("执行接管失败: {error}")))?;
            let operation_state = Arc::clone(&state);
            tokio::task::spawn_blocking(move || {
                set_codex_auto_failover_enabled_under_proxy_transaction(
                    operation_state.as_ref(),
                    true,
                )
            })
            .await
            .map_err(|error| {
                AppError::Message(format!(
                    "enable Codex Auto mode from tray operation task failed: {error}"
                ))
            })??
            .ok_or_else(|| AppError::Message("故障转移队列为空，无法启用 Auto 模式".to_string()))
        },
    )
    .await
}

async fn rollback_codex_provider_mutation(
    state: &AppState,
    snapshot: &mut CodexProviderLifecycleSnapshot,
) -> Vec<String> {
    let mut errors = snapshot.proxy.capture_rollback_file_guards();
    #[cfg(test)]
    wait_at_rollback_file_guard_pause_for_test().await;
    let provider_restored = match state.db.restore_provider_tables(&snapshot.provider_tables) {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("restore Codex Provider rows failed: {error}"));
            false
        }
    };
    let local_current_restored = match crate::settings::set_current_provider(
        &AppType::Codex,
        snapshot.local_current_provider.as_deref(),
    ) {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!(
                "restore local Codex current Provider failed: {error}"
            ));
            false
        }
    };
    let proxy_errors = state
        .proxy_service
        .restore_transaction_state(&snapshot.proxy)
        .await;
    let proxy_restored = proxy_errors.is_empty();
    errors.extend(
        proxy_errors
            .into_iter()
            .map(|error| format!("restore proxy transaction failed: {error}")),
    );

    if provider_restored && local_current_restored && proxy_restored {
        if let Err(error) =
            crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(
                state,
            )
            .await
        {
            errors.push(format!("restore Codex Agent Role projection failed: {error}"));
        }
    } else {
        errors.push(
            "skipped Codex Agent Role restore because Provider/proxy rollback was incomplete"
                .to_string(),
        );
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::{
        enable_codex_auto_mode_from_tray, run_codex_provider_mutation,
        set_rollback_file_guard_pause_for_test,
    };
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::error::AppError;
    use crate::provider::{CodexAgentRoleRouting, Provider, ProviderMeta};
    use crate::settings::CustomEndpoint;
    use crate::store::AppState;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::sync::mpsc;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::time::{timeout, Duration};

    struct TempHome {
        _dir: TempDir,
        home: Option<String>,
        userprofile: Option<String>,
        test_home: Option<String>,
        #[cfg(windows)]
        local_app_data: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("create temp home");
            let home = env::var("HOME").ok();
            let userprofile = env::var("USERPROFILE").ok();
            let test_home = env::var("CC_SWITCH_TEST_HOME").ok();
            #[cfg(windows)]
            let local_app_data = env::var("LOCALAPPDATA").ok();
            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            #[cfg(windows)]
            env::set_var("LOCALAPPDATA", dir.path().join("AppData").join("Local"));
            Self {
                _dir: dir,
                home,
                userprofile,
                test_home,
                #[cfg(windows)]
                local_app_data,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            restore_env("HOME", self.home.as_deref());
            restore_env("USERPROFILE", self.userprofile.as_deref());
            restore_env("CC_SWITCH_TEST_HOME", self.test_home.as_deref());
            #[cfg(windows)]
            restore_env("LOCALAPPDATA", self.local_app_data.as_deref());
            let _ = crate::settings::reload_settings();
        }
    }

    fn restore_env(key: &str, value: Option<&str>) {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    async fn configure_dynamic_proxy(db: &Arc<Database>) {
        let mut proxy_config = db.get_proxy_config().await.expect("read proxy config");
        proxy_config.listen_port = 0;
        proxy_config.enable_logging = false;
        db.update_proxy_config(proxy_config)
            .await
            .expect("set dynamic test proxy port");
    }

    fn codex_provider(id: &str, enables_role_routing: bool) -> Provider {
        let mut provider = Provider::with_id(
            id.into(),
            id.into(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"" }),
            None,
        );
        if enables_role_routing {
            provider.meta = Some(ProviderMeta {
                codex_agent_role_routing: Some(CodexAgentRoleRouting {
                    enabled: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        provider
    }

    fn frontend_role_path(home: &TempHome) -> std::path::PathBuf {
        home._dir
            .path()
            .join(".codex")
            .join("agents")
            .join("cc-switch-frontend.toml")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn operation_error_restores_raw_codex_provider_state() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let state = Arc::new(AppState::new(db.clone()));
        let mut owner = Provider::with_id(
            "owner".into(),
            "Owner".into(),
            json!({ "auth": {}, "config": "model = \"old\"" }),
            None,
        );
        owner
            .meta
            .get_or_insert_with(Default::default)
            .custom_endpoints = HashMap::from([(
            "https://old.example/v1".into(),
            CustomEndpoint {
                url: "https://old.example/v1".into(),
                added_at: 123,
                last_used: None,
            },
        )]);
        db.save_provider(AppType::Codex.as_str(), &owner)
            .expect("save owner");
        db.set_current_provider(AppType::Codex.as_str(), &owner.id)
            .expect("set database current");
        db.add_to_failover_queue(AppType::Codex.as_str(), &owner.id)
            .expect("set failover state");
        crate::settings::set_current_provider(&AppType::Codex, Some(&owner.id))
            .expect("set local current");

        let result = run_codex_provider_mutation(Arc::clone(&state), "injected failure", |state| {
            state.db.delete_provider(AppType::Codex.as_str(), "owner")?;
            let added = Provider::with_id(
                "added".into(),
                "Added".into(),
                json!({ "auth": {}, "config": "model = \"new\"" }),
                None,
            );
            state.db.save_provider(AppType::Codex.as_str(), &added)?;
            state
                .db
                .set_current_provider(AppType::Codex.as_str(), &added.id)?;
            crate::settings::set_current_provider(&AppType::Codex, Some(&added.id))?;
            Err::<(), AppError>(AppError::Message("injected operation failure".into()))
        })
        .await;

        assert!(result.is_err());
        let providers = db
            .get_all_providers(AppType::Codex.as_str())
            .expect("read restored providers");
        assert_eq!(providers.len(), 1);
        let restored = providers.get("owner").expect("owner restored");
        assert!(restored.in_failover_queue);
        assert!(restored
            .meta
            .as_ref()
            .expect("restored meta")
            .custom_endpoints
            .contains_key("https://old.example/v1"));
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current")
                .as_deref(),
            Some("owner")
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some("owner")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn operation_panic_restores_raw_codex_provider_state() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let state = Arc::new(AppState::new(db.clone()));
        let owner = Provider::with_id(
            "owner".into(),
            "Owner".into(),
            json!({ "auth": {}, "config": "model = \"old\"" }),
            None,
        );
        db.save_provider(AppType::Codex.as_str(), &owner)
            .expect("save owner");
        db.set_current_provider(AppType::Codex.as_str(), &owner.id)
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&owner.id))
            .expect("set local current");

        let result = timeout(
            Duration::from_secs(5),
            run_codex_provider_mutation(
                Arc::clone(&state),
                "panicking mutation",
                |state| -> Result<(), AppError> {
                    state
                        .db
                        .delete_provider(AppType::Codex.as_str(), "owner")
                        .expect("delete owner before panic");
                    panic!("injected operation panic");
                },
            ),
        )
        .await
        .expect("panic rollback must finish");

        let error = result.expect_err("operation panic must be reported");
        assert!(error.to_string().contains("operation task failed"));
        assert!(db
            .get_provider_by_id("owner", AppType::Codex.as_str())
            .expect("read restored owner")
            .is_some());
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current")
                .as_deref(),
            Some("owner")
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some("owner")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn caller_abort_does_not_cancel_the_supervised_mutation() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let state = Arc::new(AppState::new(db.clone()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(0);

        let caller_state = Arc::clone(&state);
        let caller = tokio::spawn(async move {
            run_codex_provider_mutation(caller_state, "cancelled caller", move |state| {
                started_tx.send(()).expect("signal operation start");
                release_rx.recv().expect("release supervised operation");
                let provider = Provider::with_id(
                    "completed".into(),
                    "Completed".into(),
                    json!({ "auth": {}, "config": "model = \"completed\"" }),
                    None,
                );
                state.db.save_provider(AppType::Codex.as_str(), &provider)
            })
            .await
        });

        started_rx.await.expect("operation started");
        caller.abort();
        let caller_error = timeout(Duration::from_secs(5), caller)
            .await
            .expect("cancelled caller must finish")
            .expect_err("caller task must be cancelled");
        assert!(caller_error.is_cancelled());
        release_tx.send(()).expect("release operation");

        let lifecycle_guard = timeout(
            Duration::from_secs(5),
            state.lock_codex_provider_lifecycle(),
        )
        .await
        .expect("supervisor must finish after caller abort");
        drop(lifecycle_guard);
        assert!(db
            .get_provider_by_id("completed", AppType::Codex.as_str())
            .expect("read completed mutation")
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial]
    async fn concurrent_codex_mutations_are_serialized_by_the_lifecycle_lock() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let state = Arc::new(AppState::new(db));
        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
        let (first_release_tx, first_release_rx) = mpsc::sync_channel(0);
        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();

        let first_state = Arc::clone(&state);
        let first = tokio::spawn(async move {
            run_codex_provider_mutation(first_state, "first mutation", move |_| {
                first_started_tx.send(()).expect("signal first start");
                first_release_rx.recv().expect("release first mutation");
                Ok::<_, AppError>(())
            })
            .await
        });
        first_started_rx.await.expect("first mutation started");

        let second_state = Arc::clone(&state);
        let second = tokio::spawn(async move {
            run_codex_provider_mutation(second_state, "second mutation", move |_| {
                second_started_tx.send(()).expect("signal second start");
                Ok::<_, AppError>(())
            })
            .await
        });
        let mut second_started_rx = second_started_rx;
        assert!(
            timeout(Duration::from_millis(100), &mut second_started_rx)
                .await
                .is_err(),
            "second mutation entered while the first lifecycle lock was held"
        );

        first_release_tx.send(()).expect("release first mutation");
        first
            .await
            .expect("join first caller")
            .expect("first mutation succeeds");
        timeout(Duration::from_secs(5), &mut second_started_rx)
            .await
            .expect("second mutation starts after first completes")
            .expect("second start signal");
        second
            .await
            .expect("join second caller")
            .expect("second mutation succeeds");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn rollback_restores_split_database_and_local_current_providers() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let state = Arc::new(AppState::new(db.clone()));
        for id in ["database-current", "local-current", "temporary"] {
            let provider = Provider::with_id(
                id.into(),
                id.into(),
                json!({ "auth": {}, "config": format!("model = \"{id}\"") }),
                None,
            );
            db.save_provider(AppType::Codex.as_str(), &provider)
                .expect("save provider");
        }
        db.set_current_provider(AppType::Codex.as_str(), "database-current")
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some("local-current"))
            .expect("set local current");

        let result = run_codex_provider_mutation(Arc::clone(&state), "split current", |state| {
            state
                .db
                .set_current_provider(AppType::Codex.as_str(), "temporary")?;
            crate::settings::set_current_provider(&AppType::Codex, Some("temporary"))?;
            Err::<(), AppError>(AppError::Message("rollback split current".into()))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current")
                .as_deref(),
            Some("database-current")
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some("local-current")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn rollback_preserves_external_file_edits_after_capturing_transaction_output() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let state = Arc::new(AppState::new(db));
        let config_path = crate::codex_config::get_codex_config_path();
        fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("create Codex config directory");
        fs::write(&config_path, b"original").expect("write original config");
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        set_rollback_file_guard_pause_for_test(Arc::clone(&entered), Arc::clone(&release));

        let mutation_state = Arc::clone(&state);
        let mutation_path = config_path.clone();
        let mutation = tokio::spawn(async move {
            run_codex_provider_mutation(mutation_state, "external edit rollback", move |_| {
                fs::write(&mutation_path, b"transaction-output")
                    .map_err(|error| AppError::Message(error.to_string()))?;
                Err::<(), AppError>(AppError::Message("injected rollback".into()))
            })
            .await
        });

        timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("rollback captured transaction output");
        fs::write(&config_path, b"external-edit").expect("write external edit");
        release.notify_one();

        let error = timeout(Duration::from_secs(5), mutation)
            .await
            .expect("mutation completes")
            .expect("join mutation")
            .expect_err("rollback must report external edit");
        assert!(error
            .to_string()
            .contains("Codex Provider rollback encountered"));
        assert!(error.to_string().contains("事务回滚检测到外部修改"));
        assert_eq!(
            fs::read(&config_path).expect("read preserved external edit"),
            b"external-edit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn role_reconcile_failure_rolls_back_provider_and_proxy_state() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let mut proxy_config = db.get_proxy_config().await.expect("proxy config");
        proxy_config.listen_port = 0;
        proxy_config.enable_logging = false;
        db.update_proxy_config(proxy_config)
            .await
            .expect("seed proxy config");
        let state = Arc::new(AppState::new(db.clone()));
        let owner = Provider::with_id(
            "owner".into(),
            "Owner".into(),
            json!({
                "auth": { "OPENAI_API_KEY": "provider-key" },
                "config": "model_provider = \"owner\"\n[model_providers.owner]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\n"
            }),
            None,
        );
        db.save_provider(AppType::Codex.as_str(), &owner)
            .expect("save owner");
        db.set_current_provider(AppType::Codex.as_str(), &owner.id)
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&owner.id))
            .expect("set local current");
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "live-key" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed Codex live files");
        db.conn
            .lock()
            .expect("lock db")
            .execute_batch(
                "CREATE TRIGGER fail_lifecycle_codex_takeover_enable
                 BEFORE UPDATE OF enabled ON proxy_config
                 WHEN OLD.app_type = 'codex' AND OLD.enabled = 0 AND NEW.enabled = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'injected lifecycle reconcile failure');
                 END;",
            )
            .expect("install reconcile failure trigger");

        let result =
            run_codex_provider_mutation(Arc::clone(&state), "reconcile failure", move |state| {
                let mut updated = state
                    .db
                    .get_provider_by_id("owner", AppType::Codex.as_str())?
                    .expect("owner exists");
                updated.meta = Some(ProviderMeta {
                    codex_agent_role_routing: Some(CodexAgentRoleRouting {
                        enabled: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                state.db.save_provider(AppType::Codex.as_str(), &updated)
            })
            .await;

        let error = result.expect_err("reconcile failure must roll back mutation");
        let error_message = error.to_string();
        assert!(
            error_message.contains("injected lifecycle reconcile failure"),
            "unexpected reconcile error: {error_message}"
        );
        let restored = db
            .get_provider_by_id("owner", AppType::Codex.as_str())
            .expect("read restored owner")
            .expect("restored owner exists");
        assert!(restored
            .meta
            .as_ref()
            .and_then(|meta| meta.codex_agent_role_routing.as_ref())
            .is_none());
        let status = state
            .proxy_service
            .get_status()
            .await
            .expect("read proxy status");
        assert!(!status.running);
        assert!(
            !db.get_proxy_config_for_app(AppType::Codex.as_str())
                .await
                .expect("read Codex proxy config")
                .enabled
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn tray_auto_mode_starts_takeover_switches_p1_and_projects_role_owner() {
        let home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        configure_dynamic_proxy(&db).await;
        let state = Arc::new(AppState::new(Arc::clone(&db)));
        let provider_a = codex_provider("codex-a", false);
        let provider_b = codex_provider("codex-b", true);
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("save current provider");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("save P1 provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider_a.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set local current provider");
        db.add_to_failover_queue(AppType::Codex.as_str(), &provider_b.id)
            .expect("add Codex P1");
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "live-key" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed Codex live files");
        assert!(
            !state
                .proxy_service
                .get_status()
                .await
                .expect("read initial proxy status")
                .running
        );
        let initial_app_config = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("read initial Codex proxy config");
        assert!(!initial_app_config.enabled);
        assert!(!initial_app_config.auto_failover_enabled);

        let p1_provider_id = enable_codex_auto_mode_from_tray(Arc::clone(&state))
            .await
            .expect("enable Codex Auto mode");

        assert_eq!(p1_provider_id, provider_b.id);
        let status = state
            .proxy_service
            .get_status()
            .await
            .expect("read proxy status");
        assert!(status.running);
        assert!(status.active_targets.iter().any(|target| {
            target.app_type == AppType::Codex.as_str() && target.provider_id == provider_b.id
        }));
        assert!(
            db.get_global_proxy_config()
                .await
                .expect("read global proxy config")
                .proxy_enabled
        );
        let app_config = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("read Codex proxy config");
        assert!(app_config.enabled);
        assert!(app_config.auto_failover_enabled);
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current provider")
                .as_deref(),
            Some(provider_b.id.as_str())
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some(provider_b.id.as_str())
        );
        assert!(fs::read_to_string(frontend_role_path(&home))
            .expect("read projected frontend role")
            .contains("x-cc-switch-role-owner = \"codex-b\""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn tray_auto_mode_role_conflict_restores_listener_flags_live_files_queue_and_currents() {
        let home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        configure_dynamic_proxy(&db).await;
        let state = Arc::new(AppState::new(Arc::clone(&db)));
        let provider_a = codex_provider("codex-a", false);
        let provider_b = codex_provider("codex-b", true);
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("save current provider");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("save P1 provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider_a.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set local current provider");
        db.add_to_failover_queue(AppType::Codex.as_str(), &provider_b.id)
            .expect("add Codex P1");

        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "live-key" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed Codex live files");
        let auth_path = crate::codex_config::get_codex_auth_path();
        let config_path = crate::codex_config::get_codex_config_path();
        let original_auth = fs::read(&auth_path).expect("snapshot auth file");
        let original_config = fs::read(&config_path).expect("snapshot config file");
        let original_global = db
            .get_global_proxy_config()
            .await
            .expect("snapshot global proxy config");
        let original_app = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("snapshot Codex proxy config");

        let frontend_role = frontend_role_path(&home);
        let user_role = b"name = \"user-frontend\"\n";
        fs::create_dir_all(frontend_role.parent().expect("frontend role parent"))
            .expect("create agent directory");
        fs::write(&frontend_role, user_role).expect("create user-owned frontend role");

        let error = enable_codex_auto_mode_from_tray(Arc::clone(&state))
            .await
            .expect_err("user-owned role must abort tray Auto transaction");
        assert!(error.to_string().contains("由用户管理"));
        let status = state
            .proxy_service
            .get_status()
            .await
            .expect("read restored proxy status");
        assert!(!status.running);
        assert!(status.active_targets.is_empty());
        assert_eq!(
            serde_json::to_value(
                db.get_global_proxy_config()
                    .await
                    .expect("read global config")
            )
            .expect("serialize global config"),
            serde_json::to_value(original_global).expect("serialize original global config")
        );
        assert_eq!(
            serde_json::to_value(
                db.get_proxy_config_for_app(AppType::Codex.as_str())
                    .await
                    .expect("read Codex proxy config")
            )
            .expect("serialize Codex proxy config"),
            serde_json::to_value(original_app).expect("serialize original Codex proxy config")
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current provider")
                .as_deref(),
            Some(provider_a.id.as_str())
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some(provider_a.id.as_str())
        );
        let queue = db
            .get_failover_queue(AppType::Codex.as_str())
            .expect("read restored Codex queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].provider_id, provider_b.id);
        assert!(db
            .get_live_backup(AppType::Codex.as_str())
            .await
            .expect("read restored live backup")
            .is_none());
        assert_eq!(
            fs::read(auth_path).expect("read restored auth file"),
            original_auth
        );
        assert_eq!(
            fs::read(config_path).expect("read restored config file"),
            original_config
        );
        assert_eq!(
            fs::read(frontend_role).expect("read user frontend role"),
            user_role
        );
    }
}
