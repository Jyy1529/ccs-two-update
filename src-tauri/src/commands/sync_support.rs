use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::services::codex_agent_roles::reconcile_current_codex_agent_roles;
use crate::services::provider::ProviderService;
use crate::settings;
use crate::store::AppState;

pub(crate) async fn run_post_import_sync(state: Arc<AppState>) -> Result<(), AppError> {
    let state_for_live_sync = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        ProviderService::sync_current_to_live(state_for_live_sync.as_ref())?;
        settings::reload_settings()
    })
    .await
    .map_err(|error| AppError::Message(format!("Post-import live sync task failed: {error}")))??;

    reconcile_current_codex_agent_roles(state.as_ref()).await?;
    Ok(())
}

fn post_sync_warning<E: std::fmt::Display>(err: E) -> String {
    AppError::localized(
        "sync.post_operation_sync_failed",
        format!("后置同步状态失败: {err}"),
        format!("Post-operation synchronization failed: {err}"),
    )
    .to_string()
}

pub(crate) fn post_sync_warning_from_result(result: Result<(), AppError>) -> Option<String> {
    result.err().map(post_sync_warning)
}

pub(crate) fn attach_warning(mut value: Value, warning: Option<String>) -> Value {
    if let Some(message) = warning {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), Value::String(message));
        }
    }
    value
}

pub(crate) fn success_payload_with_warning(backup_id: String, warning: Option<String>) -> Value {
    attach_warning(
        json!({
            "success": true,
            "message": "SQL imported successfully",
            "backupId": backup_id
        }),
        warning,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        attach_warning, post_sync_warning_from_result, run_post_import_sync,
        success_payload_with_warning,
    };
    use crate::database::Database;
    use crate::provider::{CodexAgentRoleRouting, Provider, ProviderMeta};
    use crate::services::codex_agent_roles::CodexAgentRolePaths;
    use crate::store::AppState;
    use crate::AppType;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct TempHome {
        #[allow(dead_code)]
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
            crate::settings::reload_settings().expect("reload isolated settings");
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
            for (key, value) in [
                ("HOME", &self.home),
                ("USERPROFILE", &self.userprofile),
                ("CC_SWITCH_TEST_HOME", &self.test_home),
            ] {
                match value {
                    Some(value) => env::set_var(key, value),
                    None => env::remove_var(key),
                }
            }
            let _ = crate::settings::reload_settings();
        }
    }

    fn codex_provider(role_routing_enabled: bool) -> Provider {
        let mut provider = Provider::with_id(
            "codex-owner".to_string(),
            "Codex Owner".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"" }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(CodexAgentRoleRouting {
                enabled: Some(role_routing_enabled),
                ..Default::default()
            }),
            ..Default::default()
        });
        provider
    }

    fn seed_current_codex_provider(db: &Arc<Database>, role_routing_enabled: bool) -> Provider {
        let provider = codex_provider(role_routing_enabled);
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("save Codex provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider.id)
            .expect("set database current Codex provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local current Codex provider");
        provider
    }

    async fn use_dynamic_proxy_port(db: &Arc<Database>) {
        let mut config = db.get_proxy_config().await.expect("read proxy config");
        config.listen_port = 0;
        config.enable_logging = false;
        db.update_proxy_config(config)
            .await
            .expect("set dynamic proxy port");
    }

    #[test]
    fn post_sync_warning_from_result_returns_none_on_success() {
        let warning = post_sync_warning_from_result(Ok(()));
        assert!(warning.is_none());
    }

    #[test]
    fn post_sync_warning_from_result_returns_some_on_sync_error() {
        let warning =
            post_sync_warning_from_result(Err(crate::error::AppError::Config("boom".into())));
        assert!(warning.is_some());
    }

    #[tokio::test]
    async fn post_sync_warning_from_result_returns_some_on_join_error() {
        let handle = tokio::spawn(async move {
            panic!("forced join error");
        });
        let join_err = handle.await.expect_err("task should panic");
        let warning = post_sync_warning_from_result(Err(crate::error::AppError::Message(format!(
            "Post-import live sync task failed: {join_err}"
        ))));
        assert!(warning.is_some());
    }

    #[test]
    fn attach_warning_adds_warning_without_dropping_existing_fields() {
        let payload = json!({ "status": "downloaded" });
        let updated = attach_warning(payload, Some("post sync warning".to_string()));
        assert_eq!(
            updated.get("status").and_then(|v| v.as_str()),
            Some("downloaded")
        );
        assert_eq!(
            updated.get("warning").and_then(|v| v.as_str()),
            Some("post sync warning")
        );
    }

    #[test]
    fn sql_import_success_payload_keeps_its_public_shape_when_post_sync_warns() {
        let payload = success_payload_with_warning(
            "safety-backup-id".to_string(),
            Some("post sync warning".to_string()),
        );

        assert_eq!(
            payload,
            json!({
                "success": true,
                "message": "SQL imported successfully",
                "backupId": "safety-backup-id",
                "warning": "post sync warning"
            })
        );
    }

    #[tokio::test]
    #[serial]
    async fn post_import_sync_projects_enabled_role_routing_with_the_real_app_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("create database"));
        use_dynamic_proxy_port(&db).await;
        let state = Arc::new(AppState::new(Arc::clone(&db)));
        seed_current_codex_provider(&db, true);

        run_post_import_sync(Arc::clone(&state))
            .await
            .expect("post-import sync succeeds");

        let paths = CodexAgentRolePaths::default_codex_home();
        assert!(
            paths.frontend.exists(),
            "frontend managed role is projected"
        );
        assert!(paths.backend.exists(), "backend managed role is projected");
        let codex_proxy = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("read Codex proxy config");
        assert!(codex_proxy.enabled, "Codex proxy takeover is enabled");
    }

    #[tokio::test]
    #[serial]
    async fn post_import_sync_disables_managed_roles_when_routing_is_disabled() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("create database"));
        use_dynamic_proxy_port(&db).await;
        let state = Arc::new(AppState::new(Arc::clone(&db)));
        let provider = seed_current_codex_provider(&db, true);

        run_post_import_sync(Arc::clone(&state))
            .await
            .expect("initial post-import sync succeeds");
        let paths = CodexAgentRolePaths::default_codex_home();
        assert!(paths.frontend.exists(), "initial managed role exists");

        let mut disabled_provider = provider;
        disabled_provider.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(CodexAgentRoleRouting {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });
        db.save_provider(AppType::Codex.as_str(), &disabled_provider)
            .expect("disable role routing");

        run_post_import_sync(Arc::clone(&state))
            .await
            .expect("post-import sync succeeds");

        assert!(!paths.frontend.exists(), "frontend role leaves discovery");
        assert!(!paths.backend.exists(), "backend role leaves discovery");
        assert!(
            paths.frontend_disabled.exists(),
            "frontend role is retained disabled"
        );
        assert!(
            paths.backend_disabled.exists(),
            "backend role is retained disabled"
        );
    }
}
