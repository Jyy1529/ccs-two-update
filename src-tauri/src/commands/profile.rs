//! 项目 Profile 管理命令

use serde::Serialize;
use tauri::{Emitter, Manager, State};

use crate::database::Profile;
use crate::services::profile::{ProfilePayload, ProfileScope, ProfileService};
use crate::store::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileDto {
    pub id: String,
    pub name: String,
    pub payload: ProfilePayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

impl From<Profile> for ProfileDto {
    fn from(profile: Profile) -> Self {
        // 单条 payload 损坏不应拖垮整个列表：降级为默认值并记日志
        let payload = serde_json::from_str(&profile.payload).unwrap_or_else(|e| {
            log::warn!(
                "解析 profile '{}' payload 失败，使用默认值: {e}",
                profile.id
            );
            ProfilePayload::default()
        });
        Self {
            id: profile.id,
            name: profile.name,
            payload,
            created_at: profile.created_at,
            updated_at: profile.updated_at,
        }
    }
}

/// 每个分组当前激活的项目 id（未使用项目时为 null）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentProfileIds {
    pub claude: Option<String>,
    pub claude_desktop: Option<String>,
    pub codex: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilesResponse {
    pub profiles: Vec<ProfileDto>,
    pub current_ids: CurrentProfileIds,
}

/// Profile 应用完成后的统一收尾：发事件 + 重建托盘菜单
///
/// 只对项目所属分组内的应用发 provider-switched。UI 与托盘两个入口必须
/// 共用此函数，保证事件 payload 形状一致（前端 App.tsx 的
/// provider-switched 监听依赖该形状）。
pub fn emit_profile_apply_events(
    app: &tauri::AppHandle,
    state: &AppState,
    profile_id: &str,
    scope: ProfileScope,
) {
    for app_type in scope.apps().iter() {
        let app_str = app_type.as_str();
        let (proxy_enabled, auto_failover_enabled) = state.db.get_proxy_flags_sync(app_str);
        let provider_id = crate::settings::get_effective_current_provider(&state.db, app_type)
            .ok()
            .flatten()
            .unwrap_or_default();
        let event_data = serde_json::json!({
            "appType": app_str,
            "proxyEnabled": proxy_enabled,
            "autoFailoverEnabled": auto_failover_enabled,
            "providerId": provider_id,
        });
        if let Err(e) = app.emit("provider-switched", event_data) {
            log::error!("发射 provider-switched 事件失败: {e}");
        }
    }
    if let Err(e) = app.emit(
        "profile-applied",
        serde_json::json!({ "profileId": profile_id, "scope": scope.as_str() }),
    ) {
        log::error!("发射 profile-applied 事件失败: {e}");
    }
    crate::tray::refresh_tray_menu(app);
}

#[tauri::command]
pub fn list_profiles(state: State<'_, AppState>) -> Result<ProfilesResponse, String> {
    let profiles = ProfileService::list(&state).map_err(|e| e.to_string())?;
    let current_ids = CurrentProfileIds {
        claude: state
            .db
            .get_current_profile_id(ProfileScope::Claude.as_str())
            .map_err(|e| e.to_string())?,
        claude_desktop: state
            .db
            .get_current_profile_id(ProfileScope::ClaudeDesktop.as_str())
            .map_err(|e| e.to_string())?,
        codex: state
            .db
            .get_current_profile_id(ProfileScope::Codex.as_str())
            .map_err(|e| e.to_string())?,
    };
    Ok(ProfilesResponse {
        profiles: profiles.into_iter().map(ProfileDto::from).collect(),
        current_ids,
    })
}

#[tauri::command]
pub fn create_profile(
    state: State<'_, AppState>,
    name: String,
    scope: String,
) -> Result<ProfileDto, String> {
    let scope = ProfileScope::parse(&scope).map_err(|e| e.to_string())?;
    ProfileService::create(&state, &name, scope)
        .map(ProfileDto::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_profile(
    state: State<'_, AppState>,
    id: String,
    name: Option<String>,
    resnapshot: Option<bool>,
    scope: Option<String>,
) -> Result<ProfileDto, String> {
    let scope = scope
        .map(|s| ProfileScope::parse(&s))
        .transpose()
        .map_err(|e| e.to_string())?;
    ProfileService::update(&state, &id, name, resnapshot.unwrap_or(false), scope)
        .map(ProfileDto::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_profile(state: State<'_, AppState>, id: String) -> Result<(), String> {
    ProfileService::delete(&state, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clear_current_profile(state: State<'_, AppState>, scope: String) -> Result<(), String> {
    let scope = ProfileScope::parse(&scope).map_err(|e| e.to_string())?;
    state
        .db
        .set_current_profile_id(scope.as_str(), None)
        .map_err(|e| e.to_string())
}

/// Profile 切换后的 Codex Agent Role 收尾。
///
/// 该函数由设置页和托盘入口共同调用，确保目标 Provider 的角色路由与 Codex
/// Live 接管状态同步。失败保留为 Profile warning，避免破坏既有 best-effort
/// Profile 切换语义。调用方位于同步 command 或 blocking 线程。
pub(crate) fn finalize_codex_role_profile_apply(
    state: &AppState,
    scope: ProfileScope,
    warnings: &mut Vec<String>,
    should_stop_proxy: bool,
) -> bool {
    if !scope.apps().contains(&crate::app_config::AppType::Codex) {
        return should_stop_proxy;
    }

    let role_result = (|| -> Result<(), crate::error::AppError> {
        if crate::services::codex_agent_roles::current_codex_role_route_requires_proxy(state)? {
            tauri::async_runtime::block_on(
                crate::services::codex_agent_roles::reconcile_current_codex_agent_roles(state),
            )?;
        } else {
            state
                .proxy_service
                .disable_takeover_for_app_sync(&crate::app_config::AppType::Codex)
                .map_err(crate::error::AppError::Message)?;
            tauri::async_runtime::block_on(
                crate::services::codex_agent_roles::reconcile_current_codex_agent_roles(state),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = role_result {
        warnings.push(format!(
            "[codex] reconcile Agent Role after profile switch failed: {error}"
        ));
    }

    !state.db.is_live_takeover_active_sync()
}

/// 应用项目快照（只作用于发起页所属分组内的应用）。
///
/// 注意：必须保持同步命令（跑在 Tauri 线程池）——`ProviderService::switch`
/// 内部使用 block_on 获取切换锁，放进 async 命令会在运行时线程上 panic。
#[tauri::command]
pub fn apply_profile(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
    scope: String,
) -> Result<Vec<String>, String> {
    let scope = ProfileScope::parse(&scope).map_err(|e| e.to_string())?;
    let (mut warnings, mut should_stop_proxy) =
        ProfileService::apply(&state, &id, scope).map_err(|e| e.to_string())?;

    should_stop_proxy =
        finalize_codex_role_profile_apply(&state, scope, &mut warnings, should_stop_proxy);

    if should_stop_proxy {
        // sync 命令线程没有 Tokio runtime，无法直接 await stop()；
        // 把停止服务放到 Tauri async runtime，停止后再补发事件刷新 UI。
        let app_handle = app.clone();
        let profile_id = id.clone();
        let proxy_service = state.proxy_service.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = proxy_service.stop().await {
                log::warn!("切换项目后停止代理服务失败: {e}");
            }
            if let Some(app_state) = app_handle.try_state::<AppState>() {
                emit_profile_apply_events(&app_handle, app_state.inner(), &profile_id, scope);
            }
        });
    } else {
        emit_profile_apply_events(&app, &state, &id, scope);
    }

    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::finalize_codex_role_profile_apply;
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::provider::{
        CodexAgentRoleRouting, CodexFrontendAgentRoleOverride, Provider, ProviderMeta,
    };
    use crate::services::codex_agent_roles::CodexAgentRolePaths;
    use crate::services::profile::ProfileScope;
    use crate::store::AppState;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

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

    fn enabled_role_routing() -> CodexAgentRoleRouting {
        CodexAgentRoleRouting {
            enabled: Some(true),
            frontend: Some(CodexFrontendAgentRoleOverride {
                provider_id: Some("provider-b".to_string()),
                upstream_model: Some("frontend-upstream".to_string()),
                model: None,
                reasoning_effort: None,
            }),
            ..Default::default()
        }
    }

    fn codex_provider(id: &str, role_routing: Option<CodexAgentRoleRouting>) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            id.to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "provider-key" },
                "config": format!(
                    "model_provider = \"{id}\"\n[model_providers.{id}]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\n"
                ),
            }),
            None,
        );
        provider.meta = role_routing.map(|codex_agent_role_routing| ProviderMeta {
            codex_agent_role_routing: Some(codex_agent_role_routing),
            ..Default::default()
        });
        provider
    }

    async fn test_state() -> (Arc<AppState>, Arc<Database>) {
        let db = Arc::new(Database::memory().expect("init database"));
        let mut proxy_config = db.get_proxy_config().await.expect("read proxy config");
        proxy_config.listen_port = 0;
        proxy_config.enable_logging = false;
        db.update_proxy_config(proxy_config)
            .await
            .expect("set dynamic proxy port");
        let state = Arc::new(AppState::new(Arc::clone(&db)));
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "live-key" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed Codex live files");
        (state, db)
    }

    fn set_current_codex_provider(db: &Database, provider_id: &str) {
        db.set_current_provider(AppType::Codex.as_str(), provider_id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(provider_id))
            .expect("set local current provider");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn profile_role_finalizer_disables_takeover_and_managed_roles_for_route_off_target() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let (state, db) = test_state().await;
        let provider_a = codex_provider("provider-a", Some(enabled_role_routing()));
        let provider_b = codex_provider("provider-b", None);
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("save provider A");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("save provider B");

        set_current_codex_provider(&db, &provider_a.id);
        let mut warnings = Vec::new();
        let should_stop_proxy = tokio::task::block_in_place(|| {
            finalize_codex_role_profile_apply(&state, ProfileScope::Codex, &mut warnings, false)
        });
        assert!(warnings.is_empty());
        assert!(!should_stop_proxy);

        let role_paths = CodexAgentRolePaths::default_codex_home();
        assert!(role_paths.frontend.exists());
        assert!(role_paths.backend.exists());

        set_current_codex_provider(&db, &provider_b.id);
        let should_stop_proxy = tokio::task::block_in_place(|| {
            finalize_codex_role_profile_apply(&state, ProfileScope::Codex, &mut warnings, false)
        });

        assert!(warnings.is_empty());
        assert!(should_stop_proxy);
        assert!(state.proxy_service.is_running().await);
        assert!(!role_paths.frontend.exists());
        assert!(!role_paths.backend.exists());
        assert!(role_paths.frontend_disabled.exists());
        assert!(role_paths.backend_disabled.exists());
        assert!(
            !db.get_proxy_config_for_app(AppType::Codex.as_str())
                .await
                .expect("read Codex proxy config")
                .enabled
        );
        state
            .proxy_service
            .stop()
            .await
            .expect("stop proxy after profile finalizer");
        assert!(!state.proxy_service.is_running().await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn profile_role_finalizer_enables_takeover_and_projects_target_route_owner() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let (state, db) = test_state().await;
        let provider_a = codex_provider("provider-a", None);
        let provider_b = codex_provider("provider-b", Some(enabled_role_routing()));
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("save provider A");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("save provider B");

        set_current_codex_provider(&db, &provider_a.id);
        let mut warnings = Vec::new();
        let should_stop_proxy = tokio::task::block_in_place(|| {
            finalize_codex_role_profile_apply(&state, ProfileScope::Codex, &mut warnings, false)
        });
        assert!(warnings.is_empty());
        assert!(should_stop_proxy);

        set_current_codex_provider(&db, &provider_b.id);
        let should_stop_proxy = tokio::task::block_in_place(|| {
            finalize_codex_role_profile_apply(&state, ProfileScope::Codex, &mut warnings, false)
        });

        assert!(warnings.is_empty());
        assert!(!should_stop_proxy);
        let role_paths = CodexAgentRolePaths::default_codex_home();
        let frontend = fs::read_to_string(&role_paths.frontend).expect("read frontend role");
        let backend = fs::read_to_string(&role_paths.backend).expect("read backend role");
        assert!(frontend.contains("x-cc-switch-role-owner = \"provider-b\""));
        assert!(backend.contains("# cc-switch-managed: codex-agent-role-v1"));
        assert!(
            db.get_proxy_config_for_app(AppType::Codex.as_str())
                .await
                .expect("read Codex proxy config")
                .enabled
        );
    }
}
