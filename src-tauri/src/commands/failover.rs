//! 故障转移队列命令
//!
//! 管理代理模式下的故障转移队列（基于 providers 表的 in_failover_queue 字段）

use crate::app_config::AppType;
use crate::database::FailoverQueueItem;
use crate::provider::Provider;
use crate::store::AppState;
use std::str::FromStr;
use tauri::Emitter;

fn require_failover_app(app_type: &str) -> Result<(), String> {
    let app = crate::app_config::AppType::from_str(app_type)
        .map_err(|error| format!("无效的应用类型: {error}"))?;
    if !app.supports_local_proxy() {
        return Err(format!("{} 不支持故障转移", app.as_str()));
    }
    Ok(())
}

fn require_failover_provider(
    db: &crate::database::Database,
    app_type: &str,
    provider_id: &str,
) -> Result<Provider, String> {
    let provider = db
        .get_provider_by_id(provider_id, app_type)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("供应商不存在: {provider_id}"))?;
    if !crate::proxy::provider_router::provider_supports_failover(app_type, &provider) {
        return Err("Codex Official 账号卡不支持自动故障转移".to_string());
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::{require_failover_app, require_failover_provider};
    use crate::database::Database;
    use crate::provider::{AuthBinding, AuthBindingSource, Provider, ProviderMeta};
    use serde_json::json;

    #[test]
    fn failover_rejects_apps_without_a_proxy_data_plane() {
        assert!(require_failover_app("claude").is_ok());
        assert!(require_failover_app("pi").is_err());
    }

    #[test]
    fn failover_rejects_codex_official_account_cards() {
        let db = Database::memory().expect("memory db");
        let mut official = Provider::with_id(
            "official-a".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        official.category = Some("official".to_string());
        official.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("account-a".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &official).expect("save official");

        assert!(require_failover_provider(&db, "codex", &official.id).is_err());
    }
}

/// 获取故障转移队列
#[tauri::command]
pub async fn get_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<FailoverQueueItem>, String> {
    require_failover_app(&app_type)?;
    let queue = state
        .db
        .get_failover_queue(&app_type)
        .map_err(|e| e.to_string())?;
    if app_type != "codex" {
        return Ok(queue);
    }
    let providers = state
        .db
        .get_all_providers(&app_type)
        .map_err(|e| e.to_string())?;
    Ok(queue
        .into_iter()
        .filter(|item| {
            providers.get(&item.provider_id).is_some_and(|provider| {
                crate::proxy::provider_router::provider_supports_failover(&app_type, provider)
            })
        })
        .collect())
}

/// 获取可添加到故障转移队列的供应商（不在队列中的）
#[tauri::command]
pub async fn get_available_providers_for_failover(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<Provider>, String> {
    require_failover_app(&app_type)?;
    let providers = state
        .db
        .get_available_providers_for_failover(&app_type)
        .map_err(|e| e.to_string())?;
    Ok(providers
        .into_iter()
        .filter(|provider| {
            crate::proxy::provider_router::provider_supports_failover(&app_type, provider)
        })
        .collect())
}

/// 添加供应商到故障转移队列
#[tauri::command]
pub async fn add_to_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), String> {
    require_failover_app(&app_type)?;
    require_failover_provider(&state.db, &app_type, &provider_id)?;
    state
        .db
        .add_to_failover_queue(&app_type, &provider_id)
        .map_err(|e| e.to_string())
}

/// 从故障转移队列移除供应商
#[tauri::command]
pub async fn remove_from_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), String> {
    require_failover_app(&app_type)?;
    state
        .db
        .remove_from_failover_queue(&app_type, &provider_id)
        .map_err(|e| e.to_string())
}

/// 获取指定应用的自动故障转移开关状态（从 proxy_config 表读取）
#[tauri::command]
pub async fn get_auto_failover_enabled(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<bool, String> {
    require_failover_app(&app_type)?;
    state
        .db
        .get_proxy_config_for_app(&app_type)
        .await
        .map(|config| config.auto_failover_enabled)
        .map_err(|e| e.to_string())
}

/// 设置指定应用的自动故障转移开关状态（写入 proxy_config 表）
///
/// 注意：关闭故障转移时不会清除队列，队列内容会保留供下次开启时使用
#[tauri::command]
pub async fn set_auto_failover_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    app_type: String,
    enabled: bool,
) -> Result<(), String> {
    require_failover_app(&app_type)?;
    log::info!(
        "[Failover] Setting auto_failover_enabled: app_type='{app_type}', enabled={enabled}"
    );

    if app_type == AppType::Codex.as_str() {
        let p1_provider_id =
            crate::services::codex_provider_lifecycle::set_codex_auto_failover_enabled(
                state.inner().owned_clone(),
                enabled,
            )
            .await
            .map_err(|e| e.to_string())?;

        if let Some(p1_provider_id) = p1_provider_id {
            let event_data = serde_json::json!({
                "appType": app_type,
                "providerId": p1_provider_id,
                "source": "failoverEnabled"
            });
            let _ = app.emit("provider-switched", event_data);
        }

        if let Ok(new_menu) = crate::tray::create_tray_menu(&app, &state) {
            if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
                let _ = tray.set_menu(Some(new_menu));
            }
        }

        return Ok(());
    }

    // 读取当前配置
    let mut config = state
        .db
        .get_proxy_config_for_app(&app_type)
        .await
        .map_err(|e| e.to_string())?;

    if enabled && !config.enabled {
        return Err("需要先启用该应用的代理接管，再开启故障转移".to_string());
    }

    // 队列为空时把当前供应商自动加入作为 P1，避免用户陷入"必须先加队列才能开启"的死锁
    let mut auto_added_provider_id: Option<String> = None;
    let p1_provider_id = if enabled {
        let all_providers = state
            .db
            .get_all_providers(&app_type)
            .map_err(|e| e.to_string())?;
        let mut queue = state
            .db
            .get_failover_queue(&app_type)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|item| {
                all_providers
                    .get(&item.provider_id)
                    .is_some_and(|provider| {
                        crate::proxy::provider_router::provider_supports_failover(
                            &app_type, provider,
                        )
                    })
            })
            .collect::<Vec<_>>();

        if queue.is_empty() {
            let app_enum = crate::app_config::AppType::from_str(&app_type)
                .map_err(|_| format!("无效的应用类型: {app_type}"))?;

            let current_id = crate::settings::get_effective_current_provider(&state.db, &app_enum)
                .map_err(|e| e.to_string())?;

            let Some(current_id) = current_id else {
                return Err("故障转移队列为空，且未设置当前供应商，无法开启故障转移".to_string());
            };

            require_failover_provider(&state.db, &app_type, &current_id)?;

            state
                .db
                .add_to_failover_queue(&app_type, &current_id)
                .map_err(|e| e.to_string())?;
            auto_added_provider_id = Some(current_id);

            queue = state
                .db
                .get_failover_queue(&app_type)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|item| {
                    all_providers
                        .get(&item.provider_id)
                        .is_some_and(|provider| {
                            crate::proxy::provider_router::provider_supports_failover(
                                &app_type, provider,
                            )
                        })
                })
                .collect();
        }

        queue
            .first()
            .map(|item| item.provider_id.clone())
            .ok_or_else(|| "故障转移队列为空，无法开启故障转移".to_string())?
    } else {
        String::new()
    };

    // 开启前先切到 P1。只有切换成功后才写入 auto_failover_enabled=true，
    // 避免 P1 不可切换（例如 official provider）时留下“开关已开但目标未切”的脏状态。
    if enabled {
        if let Err(e) = state
            .proxy_service
            .switch_proxy_target(&app_type, &p1_provider_id)
            .await
        {
            if let Some(provider_id) = auto_added_provider_id {
                let _ = state.db.remove_from_failover_queue(&app_type, &provider_id);
            }
            return Err(e);
        }
    }

    // 更新 auto_failover_enabled 字段
    config.auto_failover_enabled = enabled;

    // 写回数据库
    state
        .db
        .update_proxy_config_for_app(config)
        .await
        .map_err(|e| e.to_string())?;

    if enabled {
        // 发射 provider-switched 事件（让前端刷新当前供应商）
        let event_data = serde_json::json!({
            "appType": app_type,
            "providerId": p1_provider_id,
            "source": "failoverEnabled"
        });
        let _ = app.emit("provider-switched", event_data);
    }

    // 刷新托盘菜单，确保状态同步
    if let Ok(new_menu) = crate::tray::create_tray_menu(&app, &state) {
        if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
            let _ = tray.set_menu(Some(new_menu));
        }
    }

    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::provider::{CodexAgentRoleRouting, Provider, ProviderMeta};
    use crate::services::codex_provider_lifecycle::set_codex_auto_failover_enabled;
    use crate::store::AppState;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct TempHome {
        dir: TempDir,
        home: Option<String>,
        userprofile: Option<String>,
        test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("create temp home");
            let home = env::var("HOME").ok();
            let userprofile = env::var("USERPROFILE").ok();
            let test_home = env::var("CC_SWITCH_TEST_HOME").ok();
            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            Self {
                dir,
                home,
                userprofile,
                test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            restore_env("HOME", self.home.as_deref());
            restore_env("USERPROFILE", self.userprofile.as_deref());
            restore_env("CC_SWITCH_TEST_HOME", self.test_home.as_deref());
            crate::settings::reload_settings().expect("reload restored settings");
        }
    }

    fn restore_env(key: &str, value: Option<&str>) {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    async fn configure_codex_proxy(db: &Arc<Database>, enabled: bool) {
        let mut proxy_config = db.get_proxy_config().await.expect("read proxy config");
        proxy_config.listen_port = 0;
        proxy_config.enable_logging = false;
        db.update_proxy_config(proxy_config)
            .await
            .expect("set test proxy port");

        let mut config = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("read Codex proxy config");
        config.enabled = enabled;
        db.update_proxy_config_for_app(config)
            .await
            .expect("set Codex proxy takeover");
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
        home.dir
            .path()
            .join(".codex")
            .join("agents")
            .join("cc-switch-frontend.toml")
    }

    #[tokio::test]
    #[serial]
    async fn enabling_codex_auto_failover_reconciles_agent_roles_after_switch() {
        let home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        configure_codex_proxy(&db, true).await;

        let provider_a = codex_provider("codex-a", false);
        let provider_b = codex_provider("codex-b", true);
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("save current Codex provider");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("save P1 Codex provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider_a.id)
            .expect("set database Codex current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set local Codex current provider");
        db.add_to_failover_queue(AppType::Codex.as_str(), &provider_b.id)
            .expect("add Codex P1");

        set_codex_auto_failover_enabled(Arc::new(AppState::new(db.clone())), true)
            .await
            .expect("enable Codex auto failover");

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
        assert!(
            db.get_proxy_config_for_app(AppType::Codex.as_str())
                .await
                .expect("read Codex proxy config")
                .auto_failover_enabled
        );
        let queue = db
            .get_failover_queue(AppType::Codex.as_str())
            .expect("read Codex failover queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].provider_id, provider_b.id);

        let frontend_role = frontend_role_path(&home);
        assert!(
            fs::exists(&frontend_role).expect("read frontend role path"),
            "Codex lifecycle must reconcile the frontend role after switching P1"
        );
        assert!(fs::read_to_string(frontend_role)
            .expect("read frontend role")
            .contains("x-cc-switch-role-owner = \"codex-b\""));
    }

    #[tokio::test]
    #[serial]
    async fn codex_inner_rejects_auto_failover_before_takeover_is_enabled() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        configure_codex_proxy(&db, false).await;

        let provider = codex_provider("codex-a", false);
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("save Codex provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider.id)
            .expect("set database Codex current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local Codex current provider");

        let error = set_codex_auto_failover_enabled(Arc::new(AppState::new(db)), true)
            .await
            .expect_err("Codex inner must reject disabled takeover");
        assert!(error.to_string().contains("需要先启用该应用的代理接管"));
    }

    #[tokio::test]
    #[serial]
    async fn codex_reconcile_failure_restores_auto_added_queue_and_flags() {
        let home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        configure_codex_proxy(&db, true).await;

        let provider = codex_provider("codex-a", true);
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("save Codex provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider.id)
            .expect("set database Codex current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local Codex current provider");

        let frontend_role = frontend_role_path(&home);
        fs::create_dir_all(frontend_role.parent().expect("frontend role parent"))
            .expect("create agent directory");
        fs::write(&frontend_role, "name = \"user-frontend\"\n").expect("create user frontend role");

        let error = set_codex_auto_failover_enabled(Arc::new(AppState::new(db.clone())), true)
            .await
            .expect_err("user-owned role file must fail reconcile");
        assert!(error.to_string().contains("由用户管理"));
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current provider")
                .as_deref(),
            Some(provider.id.as_str())
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some(provider.id.as_str())
        );
        assert!(
            !db.get_proxy_config_for_app(AppType::Codex.as_str())
                .await
                .expect("read restored Codex proxy config")
                .auto_failover_enabled
        );
        assert!(db
            .get_failover_queue(AppType::Codex.as_str())
            .expect("read restored Codex queue")
            .is_empty());
        assert_eq!(
            fs::read_to_string(frontend_role).expect("read preserved user role"),
            "name = \"user-frontend\"\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_reconcile_failure_restores_preconfigured_p1_and_current_provider() {
        let home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        configure_codex_proxy(&db, true).await;

        let provider_a = codex_provider("codex-a", false);
        let provider_b = codex_provider("codex-b", true);
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("save current Codex provider");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("save P1 Codex provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider_a.id)
            .expect("set database Codex current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set local Codex current provider");
        db.add_to_failover_queue(AppType::Codex.as_str(), &provider_b.id)
            .expect("add Codex P1");

        let frontend_role = frontend_role_path(&home);
        let user_role = b"name = \"user-frontend\"\n";
        fs::create_dir_all(frontend_role.parent().expect("frontend role parent"))
            .expect("create agent directory");
        fs::write(&frontend_role, user_role).expect("create user frontend role");

        let error = set_codex_auto_failover_enabled(Arc::new(AppState::new(db.clone())), true)
            .await
            .expect_err("user-owned role file must fail reconcile");
        assert!(error.to_string().contains("由用户管理"));
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
        assert!(
            !db.get_proxy_config_for_app(AppType::Codex.as_str())
                .await
                .expect("read restored Codex proxy config")
                .auto_failover_enabled
        );
        let queue = db
            .get_failover_queue(AppType::Codex.as_str())
            .expect("read restored Codex queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].provider_id, provider_b.id);
        assert_eq!(
            fs::read(frontend_role).expect("read preserved user role"),
            user_role
        );
    }
}
