//! 代理服务相关的 Tauri 命令
//!
//! 提供前端调用的 API 接口

use crate::app_config::AppType;
use crate::error::AppError;
use crate::proxy::types::*;
use crate::proxy::{CircuitBreakerConfig, CircuitBreakerStats};
use crate::store::AppState;
use std::str::FromStr;

const CODEX_AGENT_ROLE_PROXY_REQUIRED: &str = "codex_agent_role_proxy_required";

fn codex_agent_role_proxy_required_error() -> String {
    format!(
        "{CODEX_AGENT_ROLE_PROXY_REQUIRED}: 当前 Codex Provider 启用了前端子代理独立路由，请先在 Provider 高级选项中关闭角色路由。"
    )
}

fn with_proxy_rollback_errors(primary_error: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        primary_error
    } else {
        format!(
            "{primary_error}; 代理状态回滚遇到错误: {}",
            rollback_errors.join("; ")
        )
    }
}

async fn rollback_proxy_transaction(
    state: &AppState,
    snapshot: &crate::services::proxy::ProxyTransactionSnapshot,
    reconcile_roles: bool,
) -> Vec<String> {
    let mut rollback_errors = state
        .proxy_service
        .restore_transaction_state(snapshot)
        .await;
    if reconcile_roles {
        if let Err(error) =
            crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state).await
        {
            rollback_errors.push(format!(
                "恢复代理状态后重投影 Codex Agent Role 失败: {error}"
            ));
        }
    }
    rollback_errors
}

/// 启动代理服务器（仅启动服务，不接管 Live 配置）
#[tauri::command]
pub async fn start_proxy_server(
    state: tauri::State<'_, AppState>,
) -> Result<ProxyServerInfo, String> {
    start_proxy_server_inner(state.inner()).await
}

async fn start_proxy_server_inner(state: &AppState) -> Result<ProxyServerInfo, String> {
    let _transaction = state.proxy_service.lock_transaction().await;
    let snapshot = state.proxy_service.snapshot_transaction_state().await?;
    let info = match state.proxy_service.start_inner().await {
        Ok(info) => info,
        Err(error) => {
            let rollback_errors = rollback_proxy_transaction(state, &snapshot, false).await;
            return Err(with_proxy_rollback_errors(error, rollback_errors));
        }
    };
    if let Err(error) =
        crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state).await
    {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, true).await;
        return Err(with_proxy_rollback_errors(
            format!("启动代理后同步 Codex Agent Role 失败: {error}"),
            rollback_errors,
        ));
    }
    Ok(info)
}

/// 停止代理服务器（仅停止服务，不恢复/清理 Live 接管状态）
#[tauri::command]
pub async fn stop_proxy_server(state: tauri::State<'_, AppState>) -> Result<(), String> {
    stop_proxy_server_inner(state.inner()).await
}

async fn stop_proxy_server_inner(state: &AppState) -> Result<(), String> {
    let _transaction = state.proxy_service.lock_transaction().await;
    if crate::services::codex_agent_roles::current_codex_role_route_requires_proxy(state)
        .map_err(|error| error.to_string())?
    {
        return Err(codex_agent_role_proxy_required_error());
    }
    let takeover = state.proxy_service.get_takeover_status().await?;
    if takeover.claude
        || takeover.codex
        || takeover.gemini
        || takeover.grokbuild
        || takeover.opencode
        || takeover.openclaw
    {
        return Err(
            "仍有应用处于代理接管状态，请先在设置中关闭对应应用接管后再停止本地路由。".to_string(),
        );
    }

    let snapshot = state.proxy_service.snapshot_transaction_state().await?;
    match state.proxy_service.stop_inner().await {
        Ok(()) => Ok(()),
        Err(error) => {
            let rollback_errors = rollback_proxy_transaction(state, &snapshot, false).await;
            Err(with_proxy_rollback_errors(error, rollback_errors))
        }
    }
}

/// 停止代理服务器（恢复 Live 配置）
#[tauri::command]
pub async fn stop_proxy_with_restore(state: tauri::State<'_, AppState>) -> Result<(), String> {
    stop_proxy_with_restore_inner(state.inner()).await
}

async fn stop_proxy_with_restore_inner(state: &AppState) -> Result<(), String> {
    let _transaction = state.proxy_service.lock_transaction().await;
    if crate::services::codex_agent_roles::current_codex_role_route_requires_proxy(state)
        .map_err(|error| error.to_string())?
    {
        return Err(codex_agent_role_proxy_required_error());
    }
    let snapshot = state.proxy_service.snapshot_transaction_state().await?;
    if let Err(error) = state.proxy_service.stop_with_restore_inner().await {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, false).await;
        return Err(with_proxy_rollback_errors(error, rollback_errors));
    }
    if let Err(error) =
        crate::services::codex_agent_roles::disable_codex_agent_roles_under_proxy_transaction()
            .await
    {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, true).await;
        return Err(with_proxy_rollback_errors(
            format!("停止代理后禁用 Codex Agent Role 失败: {error}"),
            rollback_errors,
        ));
    }
    Ok(())
}

/// 获取各应用接管状态
#[tauri::command]
pub async fn get_proxy_takeover_status(
    state: tauri::State<'_, AppState>,
) -> Result<ProxyTakeoverStatus, String> {
    state.proxy_service.get_takeover_status().await
}

/// 为指定应用开启/关闭接管
#[tauri::command]
pub async fn set_proxy_takeover_for_app(
    state: tauri::State<'_, AppState>,
    app_type: String,
    enabled: bool,
) -> Result<(), String> {
    let _transaction = state.proxy_service.lock_transaction().await;
    set_proxy_takeover_for_app_inner(state.inner(), &app_type, enabled).await
}

async fn set_proxy_takeover_for_app_inner(
    state: &AppState,
    app_type: &str,
    enabled: bool,
) -> Result<(), String> {
    if app_type == AppType::Codex.as_str()
        && !enabled
        && crate::services::codex_agent_roles::current_codex_role_route_requires_proxy(state)
            .map_err(|error| error.to_string())?
    {
        return Err(codex_agent_role_proxy_required_error());
    }

    let snapshot = state.proxy_service.snapshot_transaction_state().await?;
    if let Err(error) = state
        .proxy_service
        .set_takeover_for_app_inner(app_type, enabled)
        .await
    {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, false).await;
        return Err(with_proxy_rollback_errors(error, rollback_errors));
    }

    if app_type == AppType::Codex.as_str() {
        if let Err(error) =
            crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state).await
        {
            let rollback_errors = rollback_proxy_transaction(state, &snapshot, true).await;
            return Err(with_proxy_rollback_errors(
                format!("修改 Codex 接管后同步 Codex Agent Role 失败: {error}"),
                rollback_errors,
            ));
        }
    }
    Ok(())
}

/// 获取代理服务器状态
#[tauri::command]
pub async fn get_proxy_status(state: tauri::State<'_, AppState>) -> Result<ProxyStatus, String> {
    state.proxy_service.get_status().await
}

/// 获取代理配置
#[tauri::command]
pub async fn get_proxy_config(state: tauri::State<'_, AppState>) -> Result<ProxyConfig, String> {
    state.proxy_service.get_config().await
}

/// 更新代理配置
#[tauri::command]
pub async fn update_proxy_config(
    state: tauri::State<'_, AppState>,
    config: ProxyConfig,
) -> Result<(), String> {
    update_proxy_config_inner(state.inner(), &config).await
}

async fn update_proxy_config_inner(state: &AppState, config: &ProxyConfig) -> Result<(), String> {
    let _transaction = state.proxy_service.lock_transaction().await;
    let snapshot = state.proxy_service.snapshot_transaction_state().await?;
    if let Err(error) = state.proxy_service.update_config_inner(config).await {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, false).await;
        return Err(with_proxy_rollback_errors(error, rollback_errors));
    }
    if let Err(error) =
        crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state).await
    {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, true).await;
        return Err(with_proxy_rollback_errors(
            format!("更新代理配置后同步 Codex Agent Role 失败: {error}"),
            rollback_errors,
        ));
    }
    Ok(())
}

// ==================== Global & Per-App Config ====================

/// 获取全局代理配置
///
/// 返回统一的全局配置字段（代理开关、监听地址、端口、日志开关）
#[tauri::command]
pub async fn get_global_proxy_config(
    state: tauri::State<'_, AppState>,
) -> Result<GlobalProxyConfig, String> {
    let db = &state.db;
    db.get_global_proxy_config()
        .await
        .map_err(|e| e.to_string())
}

/// 更新全局代理配置
///
/// 更新统一的全局配置字段，会同时更新三行（claude/codex/gemini）
#[tauri::command]
pub async fn update_global_proxy_config(
    state: tauri::State<'_, AppState>,
    config: GlobalProxyConfig,
) -> Result<(), String> {
    update_global_proxy_config_inner(state.inner(), &config).await
}

async fn update_global_proxy_config_inner(
    state: &AppState,
    config: &GlobalProxyConfig,
) -> Result<(), String> {
    let _transaction = state.proxy_service.lock_transaction().await;
    let snapshot = state.proxy_service.snapshot_transaction_state().await?;
    let mut proxy_config = state.proxy_service.get_config().await?;
    proxy_config.listen_address = config.listen_address.clone();
    proxy_config.listen_port = config.listen_port;
    proxy_config.enable_logging = config.enable_logging;

    if let Err(error) = state.proxy_service.update_config_inner(&proxy_config).await {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, false).await;
        return Err(with_proxy_rollback_errors(error, rollback_errors));
    }
    if let Err(error) = state.db.update_global_proxy_config(config.clone()).await {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, true).await;
        return Err(with_proxy_rollback_errors(
            format!("保存全局代理配置失败: {error}"),
            rollback_errors,
        ));
    }
    if let Err(error) =
        crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state).await
    {
        let rollback_errors = rollback_proxy_transaction(state, &snapshot, true).await;
        return Err(with_proxy_rollback_errors(
            format!("更新全局代理配置后同步 Codex Agent Role 失败: {error}"),
            rollback_errors,
        ));
    }
    Ok(())
}

/// 获取指定应用的代理配置
///
/// 返回应用级配置（enabled、auto_failover、超时、熔断器等）
#[tauri::command]
pub async fn get_proxy_config_for_app(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<AppProxyConfig, String> {
    let db = &state.db;
    db.get_proxy_config_for_app(&app_type)
        .await
        .map_err(|e| e.to_string())
}

/// 更新指定应用的代理配置
///
/// 更新应用级配置（enabled、auto_failover、超时、熔断器等）
#[tauri::command]
pub async fn update_proxy_config_for_app(
    state: tauri::State<'_, AppState>,
    config: AppProxyConfig,
) -> Result<(), String> {
    let app_type = config.app_type.clone();
    let circuit_config = CircuitBreakerConfig::from(&config);

    if app_type == AppType::Codex.as_str() {
        let owned_state = state.inner().owned_clone();
        let operation_state = std::sync::Arc::clone(&owned_state);
        crate::commands::execute_codex_provider_mutation_all(
            owned_state,
            "更新 Codex 代理配置",
            move || {
                futures::executor::block_on(operation_state.db.update_proxy_config_for_app(config))
            },
        )
        .await?;
    } else {
        state
            .db
            .update_proxy_config_for_app(config)
            .await
            .map_err(|e| e.to_string())?;
    }

    state
        .proxy_service
        .update_circuit_breaker_config_for_app(&app_type, circuit_config)
        .await
}

async fn get_default_cost_multiplier_internal(
    state: &AppState,
    app_type: &str,
) -> Result<String, AppError> {
    let db = &state.db;
    db.get_default_cost_multiplier(app_type).await
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn get_default_cost_multiplier_test_hook(
    state: &AppState,
    app_type: &str,
) -> Result<String, AppError> {
    get_default_cost_multiplier_internal(state, app_type).await
}

/// 获取默认成本倍率
#[tauri::command]
pub async fn get_default_cost_multiplier(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<String, String> {
    get_default_cost_multiplier_internal(&state, &app_type)
        .await
        .map_err(|e| e.to_string())
}

async fn set_default_cost_multiplier_internal(
    state: &AppState,
    app_type: &str,
    value: &str,
) -> Result<(), AppError> {
    let db = &state.db;
    db.set_default_cost_multiplier(app_type, value).await
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn set_default_cost_multiplier_test_hook(
    state: &AppState,
    app_type: &str,
    value: &str,
) -> Result<(), AppError> {
    set_default_cost_multiplier_internal(state, app_type, value).await
}

/// 设置默认成本倍率
#[tauri::command]
pub async fn set_default_cost_multiplier(
    state: tauri::State<'_, AppState>,
    app_type: String,
    value: String,
) -> Result<(), String> {
    set_default_cost_multiplier_internal(&state, &app_type, &value)
        .await
        .map_err(|e| e.to_string())
}

async fn get_pricing_model_source_internal(
    state: &AppState,
    app_type: &str,
) -> Result<String, AppError> {
    let db = &state.db;
    db.get_pricing_model_source(app_type).await
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn get_pricing_model_source_test_hook(
    state: &AppState,
    app_type: &str,
) -> Result<String, AppError> {
    get_pricing_model_source_internal(state, app_type).await
}

/// 获取计费模式来源
#[tauri::command]
pub async fn get_pricing_model_source(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<String, String> {
    get_pricing_model_source_internal(&state, &app_type)
        .await
        .map_err(|e| e.to_string())
}

async fn set_pricing_model_source_internal(
    state: &AppState,
    app_type: &str,
    value: &str,
) -> Result<(), AppError> {
    let db = &state.db;
    db.set_pricing_model_source(app_type, value).await
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub async fn set_pricing_model_source_test_hook(
    state: &AppState,
    app_type: &str,
    value: &str,
) -> Result<(), AppError> {
    set_pricing_model_source_internal(state, app_type, value).await
}

/// 设置计费模式来源
#[tauri::command]
pub async fn set_pricing_model_source(
    state: tauri::State<'_, AppState>,
    app_type: String,
    value: String,
) -> Result<(), String> {
    set_pricing_model_source_internal(&state, &app_type, &value)
        .await
        .map_err(|e| e.to_string())
}

/// 检查代理服务器是否正在运行
#[tauri::command]
pub async fn is_proxy_running(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.proxy_service.is_running().await)
}

/// 检查是否处于 Live 接管模式
#[tauri::command]
pub async fn is_live_takeover_active(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    state.proxy_service.is_takeover_active().await
}

/// 代理模式下切换供应商（热切换）
#[tauri::command]
pub async fn switch_proxy_provider(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), String> {
    // Codex's built-in official provider can use the client's native OpenAI
    // login through takeover. Other official providers remain blocked.
    let provider = state
        .db
        .get_provider_by_id(&provider_id, &app_type)
        .map_err(|e| format!("读取供应商失败: {e}"))?
        .ok_or_else(|| format!("供应商不存在: {provider_id}"))?;
    let app = crate::app_config::AppType::from_str(&app_type)
        .map_err(|e| format!("无效的应用类型: {e}"))?;
    if provider.category.as_deref() == Some("official")
        && !crate::services::provider::official_provider_supports_proxy_takeover(&app, &provider)
    {
        return Err(
            "代理接管模式下不能切换到官方供应商 (Cannot switch to official provider during proxy takeover)"
                .to_string(),
        );
    }

    if matches!(app, AppType::Codex) {
        crate::commands::execute_codex_hot_switch(state.inner().owned_clone(), provider_id)
            .await
            .map(|_| ())
    } else {
        state
            .proxy_service
            .switch_proxy_target(&app_type, &provider_id)
            .await
    }
}

// ==================== 故障转移相关命令 ====================

/// 获取供应商健康状态
#[tauri::command]
pub async fn get_provider_health(
    state: tauri::State<'_, AppState>,
    provider_id: String,
    app_type: String,
) -> Result<ProviderHealth, String> {
    let db = &state.db;
    db.get_provider_health(&provider_id, &app_type)
        .await
        .map_err(|e| e.to_string())
}

/// 重置熔断器
///
/// 重置后会检查是否应该切回队列中优先级更高的供应商：
/// 1. 检查自动故障转移是否开启
/// 2. 如果恢复的供应商在队列中优先级更高（queue_order 更小），则自动切换
#[tauri::command]
pub async fn reset_circuit_breaker(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    provider_id: String,
    app_type: String,
) -> Result<(), String> {
    // 1. 重置数据库健康状态
    let db = &state.db;
    db.update_provider_health(&provider_id, &app_type, true, None)
        .await
        .map_err(|e| e.to_string())?;

    // 2. 如果代理正在运行，重置内存中的熔断器状态
    state
        .proxy_service
        .reset_provider_circuit_breaker(&provider_id, &app_type)
        .await?;

    // 3. 检查是否应该切回优先级更高的供应商（从 proxy_config 表读取）
    // 只有当该应用已被代理接管（enabled=true）且开启了自动故障转移时才执行
    let (app_enabled, auto_failover_enabled) = match db.get_proxy_config_for_app(&app_type).await {
        Ok(config) => (config.enabled, config.auto_failover_enabled),
        Err(e) => {
            log::error!("[{app_type}] Failed to read proxy_config: {e}, defaulting to disabled");
            (false, false)
        }
    };

    if app_enabled && auto_failover_enabled && state.proxy_service.is_running().await {
        // 获取当前供应商 ID
        let current_id = db
            .get_current_provider(&app_type)
            .map_err(|e| e.to_string())?;

        if let Some(current_id) = current_id {
            // 获取故障转移队列
            let queue = db
                .get_failover_queue(&app_type)
                .map_err(|e| e.to_string())?;

            // 找到恢复的供应商和当前供应商在队列中的位置（使用 sort_index）
            let restored_order = queue
                .iter()
                .find(|item| item.provider_id == provider_id)
                .and_then(|item| item.sort_index);

            let current_order = queue
                .iter()
                .find(|item| item.provider_id == current_id)
                .and_then(|item| item.sort_index);

            // 如果恢复的供应商优先级更高（sort_index 更小），则切换
            if let (Some(restored), Some(current)) = (restored_order, current_order) {
                if restored < current {
                    log::info!(
                        "[Recovery] 供应商 {provider_id} 已恢复且优先级更高 (P{restored} vs P{current})，自动切换"
                    );

                    // 获取供应商名称用于日志和事件
                    let provider_name = db
                        .get_all_providers(&app_type)
                        .ok()
                        .and_then(|providers| providers.get(&provider_id).map(|p| p.name.clone()))
                        .unwrap_or_else(|| provider_id.clone());

                    // 创建故障转移切换管理器并执行切换
                    let switch_manager =
                        crate::proxy::failover_switch::FailoverSwitchManager::new(db.clone());
                    if let Err(e) = switch_manager
                        .try_switch(
                            Some(&app_handle),
                            &app_type,
                            &current_id,
                            &provider_id,
                            &provider_name,
                        )
                        .await
                    {
                        log::error!("[Recovery] 自动切换失败: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// 获取熔断器配置
#[tauri::command]
pub async fn get_circuit_breaker_config(
    state: tauri::State<'_, AppState>,
) -> Result<CircuitBreakerConfig, String> {
    let db = &state.db;
    db.get_circuit_breaker_config()
        .await
        .map_err(|e| e.to_string())
}

/// 更新熔断器配置
#[tauri::command]
pub async fn update_circuit_breaker_config(
    state: tauri::State<'_, AppState>,
    config: CircuitBreakerConfig,
) -> Result<(), String> {
    let db = &state.db;

    // 1. 更新数据库配置
    db.update_circuit_breaker_config(&config)
        .await
        .map_err(|e| e.to_string())?;

    // 2. 如果代理正在运行，热更新内存中的熔断器配置
    state
        .proxy_service
        .update_circuit_breaker_configs(config)
        .await?;

    Ok(())
}

/// 获取熔断器统计信息（仅当代理服务器运行时）
#[tauri::command]
pub async fn get_circuit_breaker_stats(
    state: tauri::State<'_, AppState>,
    provider_id: String,
    app_type: String,
) -> Result<Option<CircuitBreakerStats>, String> {
    // 这个功能需要访问运行中的代理服务器的内存状态
    // 目前先返回 None，后续可以通过 ProxyService 暴露接口来实现
    let _ = (state, provider_id, app_type);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{
        codex_agent_role_proxy_required_error, set_proxy_takeover_for_app_inner,
        start_proxy_server_inner, stop_proxy_server_inner, stop_proxy_with_restore_inner,
        update_global_proxy_config_inner, with_proxy_rollback_errors,
        CODEX_AGENT_ROLE_PROXY_REQUIRED,
    };
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::provider::{CodexAgentRoleRouting, Provider, ProviderMeta};
    use crate::store::AppState;
    use serde_json::json;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct TestHome {
        _dir: TempDir,
        old_home: Option<OsString>,
        old_test_home: Option<OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let old_home = std::env::var_os("HOME");
            let old_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("HOME", dir.path());
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload isolated settings");
            Self {
                _dir: dir,
                old_home,
                old_test_home,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.old_home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.old_test_home.take() {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    fn codex_provider() -> Provider {
        Provider::with_id(
            "provider-a".to_string(),
            "Provider A".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "test-key" },
                "config": "model_provider = \"test\"\nmodel = \"test-model\"\n[model_providers.test]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
            }),
            None,
        )
    }

    fn codex_provider_with_role_routing() -> Provider {
        let mut provider = codex_provider();
        provider.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(CodexAgentRoleRouting {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        provider
    }

    fn set_current_codex_provider(db: &Database, provider: &Provider) {
        db.save_provider(AppType::Codex.as_str(), provider)
            .expect("seed provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set device current provider");
    }

    #[test]
    fn proxy_required_error_exposes_stable_marker() {
        assert!(codex_agent_role_proxy_required_error()
            .starts_with(&format!("{CODEX_AGENT_ROLE_PROXY_REQUIRED}:")));
    }

    #[test]
    fn proxy_transaction_error_includes_every_rollback_failure() {
        let error = with_proxy_rollback_errors(
            "primary failure".to_string(),
            vec![
                "proxy rollback failure".to_string(),
                "role projection rollback failure".to_string(),
            ],
        );
        assert!(error.contains("primary failure"));
        assert!(error.contains("proxy rollback failure"));
        assert!(error.contains("role projection rollback failure"));
    }

    #[tokio::test]
    #[serial]
    async fn every_proxy_close_entry_rejects_an_active_codex_role_route() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let provider = codex_provider_with_role_routing();
        set_current_codex_provider(&db, &provider);

        for error in [
            stop_proxy_server_inner(&state)
                .await
                .expect_err("plain stop must be blocked"),
            stop_proxy_with_restore_inner(&state)
                .await
                .expect_err("restore stop must be blocked"),
            set_proxy_takeover_for_app_inner(&state, AppType::Codex.as_str(), false)
                .await
                .expect_err("Codex takeover disable must be blocked"),
        ] {
            assert!(
                error.starts_with(&format!("{CODEX_AGENT_ROLE_PROXY_REQUIRED}:")),
                "close entry must expose the stable role-route marker: {error}"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn start_reports_global_config_rollback_failure() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let claude_path = crate::config::get_claude_settings_path();
        fs::create_dir_all(claude_path.parent().expect("Claude config parent"))
            .expect("create Claude config parent");
        let exact_live_bytes = b"{ \"env\" : { \"UNCHANGED\" : \"yes\" } }\r\n";
        fs::write(&claude_path, exact_live_bytes).expect("seed exact Claude bytes");
        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER reject_proxy_enabled_update
                 BEFORE UPDATE OF proxy_enabled ON proxy_config
                 BEGIN
                   SELECT RAISE(ABORT, 'forced global proxy config failure');
                 END;",
            )
            .expect("install global config failure trigger");
        }

        let error = state
            .proxy_service
            .start()
            .await
            .expect_err("global proxy config update must fail");

        assert!(
            error.contains("forced global proxy config failure"),
            "{error}"
        );
        assert!(error.contains("恢复全局代理配置失败"), "{error}");
        assert!(!state.proxy_service.is_running().await);
        assert_eq!(
            fs::read(&claude_path).expect("read unchanged Claude config"),
            exact_live_bytes,
            "failed ordinary start must not rewrite unrelated Live files"
        );
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_start_commands_share_one_listener() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let mut config = db.get_proxy_config().await.expect("read proxy config");
        config.listen_address = "127.0.0.1".to_string();
        config.listen_port = 0;
        db.update_proxy_config(config)
            .await
            .expect("use an ephemeral proxy port");

        let (first, second) = tokio::join!(
            start_proxy_server_inner(&state),
            start_proxy_server_inner(&state)
        );
        let first = first.expect("first start must succeed");
        let second = second.expect("second start must reuse the listener");

        assert_eq!(first.address, second.address);
        assert_eq!(first.port, second.port);
        assert_ne!(first.port, 0);
        state
            .proxy_service
            .stop()
            .await
            .expect("stop shared listener");
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_start_and_takeover_share_one_listener() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let provider = codex_provider();
        set_current_codex_provider(&db, &provider);
        crate::codex_config::write_codex_live_atomic(
            provider.settings_config.get("auth").expect("provider auth"),
            provider
                .settings_config
                .get("config")
                .and_then(|value| value.as_str()),
        )
        .expect("seed Codex live config");
        let mut config = db.get_proxy_config().await.expect("read proxy config");
        config.listen_address = "127.0.0.1".to_string();
        config.listen_port = 0;
        db.update_proxy_config(config)
            .await
            .expect("use an ephemeral proxy port");

        let (start, takeover) = tokio::join!(
            start_proxy_server_inner(&state),
            set_proxy_takeover_for_app_inner(&state, AppType::Codex.as_str(), true)
        );
        let start = start.expect("start must succeed");
        takeover.expect("takeover must succeed");
        let status = state
            .proxy_service
            .get_status()
            .await
            .expect("read proxy status");
        let takeover = state
            .proxy_service
            .get_takeover_status()
            .await
            .expect("read takeover status");

        assert!(status.running);
        assert_eq!(status.port, start.port);
        assert!(takeover.codex);
        set_proxy_takeover_for_app_inner(&state, AppType::Codex.as_str(), false)
            .await
            .expect("disable takeover and stop listener");
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_reconcile_failure_restores_previous_takeover_state() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let provider = codex_provider_with_role_routing();
        set_current_codex_provider(&db, &provider);
        crate::codex_config::write_codex_live_atomic(
            provider.settings_config.get("auth").expect("provider auth"),
            provider
                .settings_config
                .get("config")
                .and_then(|value| value.as_str()),
        )
        .expect("seed Codex live config");

        let mut proxy_config = db.get_proxy_config().await.expect("read proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("use an ephemeral proxy port");

        let paths = crate::services::codex_agent_roles::CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::create_dir(&paths.frontend).expect("create conflicting role directory");

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            set_proxy_takeover_for_app_inner(&state, AppType::Codex.as_str(), true),
        )
        .await
        .expect("proxy transaction must not deadlock while reconciling roles")
        .expect_err("role projection conflict must fail the takeover transaction");
        let takeover = state
            .proxy_service
            .get_takeover_status()
            .await
            .expect("read restored takeover status");
        let running = state.proxy_service.is_running().await;
        if running {
            let _ = state.proxy_service.stop().await;
        }

        assert!(!takeover.codex, "Codex takeover must roll back to disabled");
        assert!(!running, "a transaction-started proxy must be stopped");
        assert!(
            error.contains("恢复代理状态后重投影 Codex Agent Role 失败"),
            "rollback projection failure must be visible: {error}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_enabled_persist_failure_restores_live_backup_and_runtime() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let provider = codex_provider();
        set_current_codex_provider(&db, &provider);
        crate::codex_config::write_codex_live_atomic(
            provider.settings_config.get("auth").expect("provider auth"),
            provider
                .settings_config
                .get("config")
                .and_then(|value| value.as_str()),
        )
        .expect("seed Codex live config");
        let original_live = crate::codex_config::read_codex_live_settings()
            .expect("read original Codex live config");
        let mut proxy_config = db.get_proxy_config().await.expect("read proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("use an ephemeral proxy port");

        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER reject_codex_takeover_enable
                 BEFORE UPDATE OF enabled ON proxy_config
                 WHEN NEW.app_type = 'codex' AND OLD.enabled = 0 AND NEW.enabled = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'forced codex enabled persist failure');
                 END;",
            )
            .expect("install enabled failure trigger");
        }

        let error = set_proxy_takeover_for_app_inner(&state, AppType::Codex.as_str(), true)
            .await
            .expect_err("enabled persistence failure must abort takeover");
        let restored_live = crate::codex_config::read_codex_live_settings()
            .expect("read restored Codex live config");
        let restored_app_config = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("read restored Codex app config");
        let restored_global = db
            .get_global_proxy_config()
            .await
            .expect("read restored global config");

        assert!(
            error.contains("forced codex enabled persist failure"),
            "{error}"
        );
        assert_eq!(restored_live, original_live);
        assert!(!restored_app_config.enabled);
        assert!(
            db.get_live_backup(AppType::Codex.as_str())
                .await
                .expect("read restored backup")
                .is_none(),
            "new takeover backup must be removed"
        );
        assert!(!state.proxy_service.is_running().await);
        assert!(!restored_global.proxy_enabled);
    }

    #[tokio::test]
    #[serial]
    async fn occupied_port_update_restores_config_and_actual_listener() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let mut initial_config = db.get_proxy_config().await.expect("read proxy config");
        initial_config.listen_address = "127.0.0.1".to_string();
        initial_config.listen_port = 0;
        db.update_proxy_config(initial_config)
            .await
            .expect("use an ephemeral proxy port");
        state
            .proxy_service
            .start()
            .await
            .expect("start original proxy");

        let original_status = state
            .proxy_service
            .get_status()
            .await
            .expect("read original status");
        let original_config = state
            .proxy_service
            .get_config()
            .await
            .expect("read resolved config");
        let occupied =
            std::net::TcpListener::bind("127.0.0.1:0").expect("reserve conflicting port");
        let occupied_port = occupied.local_addr().expect("reserved address").port();
        let mut updated = db
            .get_global_proxy_config()
            .await
            .expect("read global proxy config");
        updated.listen_port = occupied_port;

        let error = update_global_proxy_config_inner(&state, &updated)
            .await
            .expect_err("occupied port must reject update");

        let restored_config = state
            .proxy_service
            .get_config()
            .await
            .expect("read restored config");
        let restored_status = state
            .proxy_service
            .get_status()
            .await
            .expect("read restored status");
        assert!(error.contains("重启代理服务器失败"), "{error}");
        assert_eq!(
            restored_config.listen_address,
            original_config.listen_address
        );
        assert_eq!(restored_config.listen_port, original_config.listen_port);
        assert!(restored_status.running);
        assert_eq!(restored_status.address, original_status.address);
        assert_eq!(restored_status.port, original_status.port);

        drop(occupied);
        state
            .proxy_service
            .stop()
            .await
            .expect("stop restored proxy");
    }

    #[tokio::test]
    #[serial]
    async fn live_sync_failure_after_address_change_restores_old_listener() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let mut initial_config = db.get_proxy_config().await.expect("read proxy config");
        initial_config.listen_address = "127.0.0.1".to_string();
        initial_config.listen_port = 0;
        db.update_proxy_config(initial_config)
            .await
            .expect("use an ephemeral proxy port");
        state
            .proxy_service
            .start()
            .await
            .expect("start original proxy");
        let original_status = state
            .proxy_service
            .get_status()
            .await
            .expect("read original status");
        let mut claude_config = db
            .get_proxy_config_for_app(AppType::Claude.as_str())
            .await
            .expect("read Claude proxy config");
        claude_config.enabled = true;
        db.update_proxy_config_for_app(claude_config)
            .await
            .expect("mark Claude takeover enabled");
        let mut updated = db
            .get_global_proxy_config()
            .await
            .expect("read global proxy config");
        updated.listen_port = 0;

        let error = update_global_proxy_config_inner(&state, &updated)
            .await
            .expect_err("missing enabled Live file must fail strict sync");
        let restored_status = state
            .proxy_service
            .get_status()
            .await
            .expect("read restored listener status");

        assert!(error.contains("Claude 配置文件不存在"), "{error}");
        assert!(restored_status.running);
        assert_eq!(restored_status.address, original_status.address);
        assert_eq!(restored_status.port, original_status.port);
        state
            .proxy_service
            .stop()
            .await
            .expect("stop restored listener");
    }
}
