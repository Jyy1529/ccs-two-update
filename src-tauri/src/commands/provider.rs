use indexmap::IndexMap;
use tauri::{Emitter, Manager, State};

use crate::app_config::AppType;
use crate::commands::copilot::CopilotAuthState;
use crate::error::AppError;
use crate::provider::{ClaudeDesktopMode, Provider};
use crate::services::proxy::ProxyTransactionSnapshot;
use crate::services::{
    EndpointLatency, ProviderService, ProviderSortUpdate, ProviderTransferPreview,
    ProviderTransferRequest, ProviderTransferResult, SpeedtestService, SwitchResult,
};
use crate::store::AppState;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

// 常量定义
const TEMPLATE_TYPE_GITHUB_COPILOT: &str = "github_copilot";
const TEMPLATE_TYPE_TOKEN_PLAN: &str = "token_plan";
const TEMPLATE_TYPE_BALANCE: &str = "balance";
const TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION: &str = "official_subscription";
const COPILOT_UNIT_PREMIUM: &str = "requests";

const CODEX_ROLE_RESTORE_SKIPPED: &str = "codex_provider_rollback_incomplete_skip_role_reconcile";

#[derive(Debug, Clone)]
struct CommandFileSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct CommandProviderSnapshot {
    id: String,
    provider: Option<Provider>,
}

#[derive(Debug, Clone)]
struct CodexProviderMutationSnapshot {
    providers: Vec<CommandProviderSnapshot>,
    replace_all_providers: bool,
    local_current_provider_id: Option<String>,
    database_current_provider_id: Option<String>,
    effective_current_provider_id: Option<String>,
    live_backup: Option<String>,
    files: Vec<CommandFileSnapshot>,
    proxy_transaction: ProxyTransactionSnapshot,
}

enum CodexProviderSnapshotScope {
    Selected(Vec<String>),
    All,
}

fn owned_app_state(state: &AppState) -> Arc<AppState> {
    state.owned_clone()
}

fn snapshot_command_file(path: PathBuf) -> Result<CommandFileSnapshot, AppError> {
    let content = if path.exists() {
        Some(fs::read(&path).map_err(|error| AppError::io(&path, error))?)
    } else {
        None
    };
    Ok(CommandFileSnapshot { path, content })
}

fn restore_command_file(snapshot: &CommandFileSnapshot) -> Result<(), AppError> {
    match snapshot.content.as_deref() {
        Some(content) => crate::config::atomic_write(&snapshot.path, content),
        None => match fs::remove_file(&snapshot.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::io(&snapshot.path, error)),
        },
    }
}

fn capture_codex_provider_mutation_blocking(
    state: &AppState,
    scope: CodexProviderSnapshotScope,
    proxy_transaction: ProxyTransactionSnapshot,
) -> Result<CodexProviderMutationSnapshot, AppError> {
    let all_providers = state.db.get_all_providers(AppType::Codex.as_str())?;
    let (provider_ids, replace_all_providers) = match scope {
        CodexProviderSnapshotScope::Selected(provider_ids) => (provider_ids, false),
        CodexProviderSnapshotScope::All => (all_providers.keys().cloned().collect(), true),
    };
    let mut providers = Vec::new();
    for provider_id in provider_ids {
        if providers
            .iter()
            .any(|snapshot: &CommandProviderSnapshot| snapshot.id == provider_id)
        {
            continue;
        }
        providers.push(CommandProviderSnapshot {
            provider: all_providers.get(&provider_id).cloned(),
            id: provider_id,
        });
    }

    let local_current_provider_id = crate::settings::get_current_provider(&AppType::Codex);
    let database_current_provider_id = state.db.get_current_provider(AppType::Codex.as_str())?;
    let effective_current_provider_id = local_current_provider_id
        .as_ref()
        .filter(|provider_id| all_providers.contains_key(provider_id.as_str()))
        .cloned()
        .or_else(|| {
            database_current_provider_id
                .as_ref()
                .filter(|provider_id| all_providers.contains_key(provider_id.as_str()))
                .cloned()
        });
    let files = [
        crate::codex_config::get_codex_config_path(),
        crate::codex_config::get_codex_auth_path(),
        crate::codex_config::get_codex_model_catalog_path(),
    ]
    .into_iter()
    .map(snapshot_command_file)
    .collect::<Result<Vec<_>, _>>()?;
    let live_backup =
        futures::executor::block_on(state.db.get_live_backup(AppType::Codex.as_str()))?
            .map(|backup| backup.original_config);

    Ok(CodexProviderMutationSnapshot {
        providers,
        replace_all_providers,
        local_current_provider_id,
        database_current_provider_id,
        effective_current_provider_id,
        live_backup,
        files,
        proxy_transaction,
    })
}

async fn capture_codex_provider_mutation(
    state: Arc<AppState>,
    scope: CodexProviderSnapshotScope,
) -> Result<Arc<CodexProviderMutationSnapshot>, String> {
    let proxy_transaction = state
        .proxy_service
        .snapshot_transaction_state()
        .await
        .map_err(|error| format!("读取 Provider 代理事务快照失败: {error}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        capture_codex_provider_mutation_blocking(state.as_ref(), scope, proxy_transaction)
    })
    .await
    .map_err(|error| format!("Provider 事务快照任务执行失败: {error}"))?
    .map(Arc::new)
    .map_err(|error| error.to_string())
}

async fn restore_proxy_transaction_state(
    state: &AppState,
    snapshot: &ProxyTransactionSnapshot,
    rollback_errors: &mut Vec<String>,
) -> bool {
    let errors = state
        .proxy_service
        .restore_transaction_state(snapshot)
        .await;
    if errors.is_empty() {
        true
    } else {
        rollback_errors.extend(
            errors
                .into_iter()
                .map(|error| format!("恢复 Provider 代理事务状态失败: {error}")),
        );
        false
    }
}

fn restore_codex_provider_slots(
    db: &crate::database::Database,
    snapshot: &CodexProviderMutationSnapshot,
) -> Vec<String> {
    let mut rollback_errors = Vec::new();
    if snapshot.replace_all_providers {
        match db.get_all_providers(AppType::Codex.as_str()) {
            Ok(current) => {
                for provider_id in current.keys().filter(|provider_id| {
                    !snapshot
                        .providers
                        .iter()
                        .any(|saved| saved.id == provider_id.as_str())
                }) {
                    if let Err(error) = db.delete_provider(AppType::Codex.as_str(), provider_id) {
                        rollback_errors.push(format!(
                            "移除事务内新增 Provider '{}' 失败: {error}",
                            provider_id
                        ));
                    }
                }
            }
            Err(error) => {
                rollback_errors.push(format!("读取当前 Codex Provider 集合失败: {error}"))
            }
        }
    }
    for provider_snapshot in &snapshot.providers {
        let result = match provider_snapshot.provider.as_ref() {
            Some(provider) => db.save_provider(AppType::Codex.as_str(), provider),
            None => db.delete_provider(AppType::Codex.as_str(), &provider_snapshot.id),
        };
        if let Err(error) = result {
            rollback_errors.push(format!(
                "恢复 Provider '{}' 失败: {error}",
                provider_snapshot.id
            ));
        }
    }
    rollback_errors
}

fn restore_codex_files_and_backup_blocking(
    state: &AppState,
    snapshot: &CodexProviderMutationSnapshot,
) -> Vec<String> {
    let mut rollback_errors = Vec::new();
    for file in &snapshot.files {
        if let Err(error) = restore_command_file(file) {
            rollback_errors.push(format!(
                "恢复 Codex 配置文件 '{}' 失败: {error}",
                file.path.display()
            ));
        }
    }
    let backup_result = match snapshot.live_backup.as_deref() {
        Some(backup) => {
            futures::executor::block_on(state.db.save_live_backup(AppType::Codex.as_str(), backup))
        }
        None => futures::executor::block_on(state.db.delete_live_backup(AppType::Codex.as_str())),
    };
    if let Err(error) = backup_result {
        rollback_errors.push(format!("恢复 Codex Live 备份失败: {error}"));
    }
    rollback_errors
}

fn restore_raw_codex_current_pointers_blocking(
    state: &AppState,
    snapshot: &CodexProviderMutationSnapshot,
) -> Vec<String> {
    let mut rollback_errors = Vec::new();
    if let Err(error) = state.db.set_current_provider(
        AppType::Codex.as_str(),
        snapshot
            .database_current_provider_id
            .as_deref()
            .unwrap_or(""),
    ) {
        rollback_errors.push(format!("恢复数据库当前 Provider 失败: {error}"));
    }
    if let Err(error) = crate::settings::set_current_provider(
        &AppType::Codex,
        snapshot.local_current_provider_id.as_deref(),
    ) {
        rollback_errors.push(format!("恢复设备当前 Provider 失败: {error}"));
    }
    rollback_errors
}

fn set_temporary_codex_current_none_blocking(state: &AppState) -> Vec<String> {
    let mut rollback_errors = Vec::new();
    if let Err(error) = state.db.set_current_provider(AppType::Codex.as_str(), "") {
        rollback_errors.push(format!("临时清空数据库当前 Provider 失败: {error}"));
    }
    if let Err(error) = crate::settings::set_current_provider(&AppType::Codex, None) {
        rollback_errors.push(format!("临时清空设备当前 Provider 失败: {error}"));
    }
    rollback_errors
}

async fn collect_blocking_rollback_phase<F>(
    phase_label: &str,
    rollback_errors: &mut Vec<String>,
    operation: F,
) -> bool
where
    F: FnOnce() -> Vec<String> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(operation).await {
        Ok(errors) if errors.is_empty() => true,
        Ok(errors) => {
            rollback_errors.extend(errors);
            false
        }
        Err(error) => {
            rollback_errors.push(format!("{phase_label}任务执行失败: {error}"));
            false
        }
    }
}

fn with_provider_rollback_errors(primary_error: String, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        primary_error
    } else {
        format!(
            "{primary_error}; Provider 回滚遇到错误: {}",
            rollback_errors.join("; ")
        )
    }
}

fn append_switch_rollback_warnings(rollback_errors: &mut Vec<String>, switch_result: SwitchResult) {
    rollback_errors.extend(
        switch_result
            .warnings
            .into_iter()
            .map(|warning| format!("回切 Provider 警告: {warning}")),
    );
}

async fn rollback_codex_provider_mutation(
    state: Arc<AppState>,
    snapshot: Arc<CodexProviderMutationSnapshot>,
) -> Vec<String> {
    let mut rollback_errors = Vec::new();
    let provider_state = Arc::clone(&state);
    let provider_snapshot = Arc::clone(&snapshot);
    let provider_complete =
        collect_blocking_rollback_phase("Provider 槽位回滚", &mut rollback_errors, move || {
            restore_codex_provider_slots(&provider_state.db, provider_snapshot.as_ref())
        })
        .await;
    let proxy_complete = restore_proxy_transaction_state(
        state.as_ref(),
        &snapshot.proxy_transaction,
        &mut rollback_errors,
    )
    .await;
    let live_state = Arc::clone(&state);
    let live_snapshot = Arc::clone(&snapshot);
    let live_complete =
        collect_blocking_rollback_phase("Provider Live 回滚", &mut rollback_errors, move || {
            restore_codex_files_and_backup_blocking(live_state.as_ref(), live_snapshot.as_ref())
        })
        .await;
    let base_complete = provider_complete && proxy_complete && live_complete;

    if base_complete {
        if let Err(error) =
            crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state.as_ref())
                .await
        {
            rollback_errors.push(format!("恢复 Codex Agent Role 投影失败: {error}"));
        }
    } else {
        rollback_errors.push(format!(
            "{CODEX_ROLE_RESTORE_SKIPPED}: Provider/代理/Live 回滚未完成，跳过 Codex Agent Role 投影恢复"
        ));
    }

    let pointer_state = Arc::clone(&state);
    let pointer_snapshot = Arc::clone(&snapshot);
    collect_blocking_rollback_phase(
        "raw current Provider 回滚",
        &mut rollback_errors,
        move || {
            restore_raw_codex_current_pointers_blocking(
                pointer_state.as_ref(),
                pointer_snapshot.as_ref(),
            )
        },
    )
    .await;
    rollback_errors
}

async fn rollback_codex_switch_transaction(
    state: Arc<AppState>,
    snapshot: Arc<CodexProviderMutationSnapshot>,
) -> Vec<String> {
    let mut rollback_errors = Vec::new();
    let initial_provider_state = Arc::clone(&state);
    let initial_provider_snapshot = Arc::clone(&snapshot);
    let initial_provider_restore_complete = collect_blocking_rollback_phase(
        "Provider 槽位预恢复",
        &mut rollback_errors,
        move || {
            restore_codex_provider_slots(
                &initial_provider_state.db,
                initial_provider_snapshot.as_ref(),
            )
        },
    )
    .await;

    let mut route_restore_complete = initial_provider_restore_complete;
    if initial_provider_restore_complete {
        if let Some(previous_id) = snapshot.effective_current_provider_id.clone() {
            let switch_state = Arc::clone(&state);
            match tauri::async_runtime::spawn_blocking(move || {
                switch_provider_internal(switch_state.as_ref(), AppType::Codex, &previous_id)
            })
            .await
            {
                Ok(Ok(switch_result)) => {
                    append_switch_rollback_warnings(&mut rollback_errors, switch_result);
                }
                Ok(Err(error)) => {
                    rollback_errors.push(format!("回切 Provider 失败: {error}"));
                    route_restore_complete = false;
                }
                Err(error) => {
                    rollback_errors.push(format!("回切 Provider 任务执行失败: {error}"));
                    route_restore_complete = false;
                }
            }
        } else {
            let temporary_state = Arc::clone(&state);
            route_restore_complete = collect_blocking_rollback_phase(
                "临时 current Provider 恢复",
                &mut rollback_errors,
                move || set_temporary_codex_current_none_blocking(temporary_state.as_ref()),
            )
            .await;
        }
    } else {
        rollback_errors.push("恢复 Provider 槽位未完成，跳过 Provider 回切".to_string());
    }

    let final_provider_state = Arc::clone(&state);
    let final_provider_snapshot = Arc::clone(&snapshot);
    let final_provider_complete = collect_blocking_rollback_phase(
        "Provider 槽位精确回滚",
        &mut rollback_errors,
        move || {
            restore_codex_provider_slots(&final_provider_state.db, final_provider_snapshot.as_ref())
        },
    )
    .await;
    let proxy_complete = restore_proxy_transaction_state(
        state.as_ref(),
        &snapshot.proxy_transaction,
        &mut rollback_errors,
    )
    .await;
    let final_live_state = Arc::clone(&state);
    let final_live_snapshot = Arc::clone(&snapshot);
    let final_live_complete = collect_blocking_rollback_phase(
        "Provider Live 精确回滚",
        &mut rollback_errors,
        move || {
            restore_codex_files_and_backup_blocking(
                final_live_state.as_ref(),
                final_live_snapshot.as_ref(),
            )
        },
    )
    .await;
    let final_base_complete = final_provider_complete && proxy_complete && final_live_complete;

    if route_restore_complete && final_base_complete {
        if let Err(error) =
            crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state.as_ref())
                .await
        {
            rollback_errors.push(format!("回切后恢复 Codex Agent Role 投影失败: {error}"));
        }
    } else {
        rollback_errors.push(format!(
            "{CODEX_ROLE_RESTORE_SKIPPED}: Provider 回切或代理/Live 回滚未完成，跳过 Codex Agent Role 投影恢复"
        ));
    }

    let pointer_state = Arc::clone(&state);
    let pointer_snapshot = Arc::clone(&snapshot);
    collect_blocking_rollback_phase(
        "raw current Provider 回滚",
        &mut rollback_errors,
        move || {
            restore_raw_codex_current_pointers_blocking(
                pointer_state.as_ref(),
                pointer_snapshot.as_ref(),
            )
        },
    )
    .await;
    rollback_errors
}

async fn run_provider_operation_blocking<T, F>(
    operation_label: &str,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{operation_label} 失败: {error}")),
        Err(error) => Err(format!("{operation_label} 任务执行失败: {error}")),
    }
}

async fn execute_provider_mutation_with_scope<T, F>(
    state: Arc<AppState>,
    app_type: AppType,
    snapshot_scope: CodexProviderSnapshotScope,
    operation_label: &'static str,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    if !matches!(&app_type, AppType::Codex) {
        return run_provider_operation_blocking(operation_label, operation).await;
    }

    tauri::async_runtime::spawn(execute_codex_provider_mutation_transaction(
        state,
        snapshot_scope,
        operation_label,
        operation,
    ))
    .await
    .map_err(|error| format!("{operation_label} supervisor 任务执行失败: {error}"))?
}

async fn execute_codex_provider_mutation_transaction<T, F>(
    state: Arc<AppState>,
    snapshot_scope: CodexProviderSnapshotScope,
    operation_label: &'static str,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    let _provider_lifecycle_guard = state.lock_codex_provider_lifecycle().await;
    let _proxy_transaction_guard = state.proxy_service.lock_transaction().await;
    let snapshot = capture_codex_provider_mutation(Arc::clone(&state), snapshot_scope).await?;
    let value = match run_provider_operation_blocking(operation_label, operation).await {
        Ok(value) => value,
        Err(primary_error) => {
            let rollback_errors =
                rollback_codex_provider_mutation(Arc::clone(&state), Arc::clone(&snapshot)).await;
            return Err(with_provider_rollback_errors(
                primary_error,
                rollback_errors,
            ));
        }
    };

    if let Err(error) =
        crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state.as_ref())
            .await
    {
        let rollback_errors =
            rollback_codex_provider_mutation(Arc::clone(&state), Arc::clone(&snapshot)).await;
        return Err(with_provider_rollback_errors(
            format!("{operation_label} 后同步 Codex Agent Role 失败: {error}"),
            rollback_errors,
        ));
    }

    Ok(value)
}

async fn execute_provider_mutation<T, F>(
    state: Arc<AppState>,
    app_type: AppType,
    provider_ids: Vec<String>,
    operation_label: &'static str,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    execute_provider_mutation_with_scope(
        state,
        app_type,
        CodexProviderSnapshotScope::Selected(provider_ids),
        operation_label,
        operation,
    )
    .await
}

pub(crate) async fn execute_codex_provider_mutation_all<T, F>(
    state: Arc<AppState>,
    operation_label: &'static str,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    execute_provider_mutation_with_scope(
        state,
        AppType::Codex,
        CodexProviderSnapshotScope::All,
        operation_label,
        operation,
    )
    .await
}

async fn execute_codex_switch_transaction_with_setup<F, Fut, O>(
    state: Arc<AppState>,
    id: String,
    operation_label: &'static str,
    setup: F,
    operation: O,
) -> Result<SwitchResult, String>
where
    F: FnOnce(Arc<AppState>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    O: FnOnce() -> Result<SwitchResult, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn(execute_codex_switch_transaction_inner(
        state,
        id,
        operation_label,
        setup,
        operation,
    ))
    .await
    .map_err(|error| format!("{operation_label} supervisor 任务执行失败: {error}"))?
}

async fn execute_codex_switch_transaction_inner<F, Fut, O>(
    state: Arc<AppState>,
    id: String,
    operation_label: &'static str,
    setup: F,
    operation: O,
) -> Result<SwitchResult, String>
where
    F: FnOnce(Arc<AppState>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
    O: FnOnce() -> Result<SwitchResult, AppError> + Send + 'static,
{
    let _provider_lifecycle_guard = state.lock_codex_provider_lifecycle().await;
    let _proxy_transaction_guard = state.proxy_service.lock_transaction().await;
    let snapshot =
        capture_codex_provider_mutation(Arc::clone(&state), CodexProviderSnapshotScope::All)
            .await?;

    if let Err(primary_error) = setup(Arc::clone(&state)).await {
        let rollback_errors =
            rollback_codex_provider_mutation(Arc::clone(&state), Arc::clone(&snapshot)).await;
        return Err(with_provider_rollback_errors(
            format!("{operation_label} 失败: {primary_error}"),
            rollback_errors,
        ));
    }

    let result = match run_provider_operation_blocking(operation_label, operation).await {
        Ok(result) => result,
        Err(primary_error) => {
            let rollback_errors =
                rollback_codex_switch_transaction(Arc::clone(&state), Arc::clone(&snapshot)).await;
            return Err(with_provider_rollback_errors(
                primary_error,
                rollback_errors,
            ));
        }
    };

    if let Err(error) =
        crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(state.as_ref())
            .await
    {
        let rollback_errors =
            rollback_codex_switch_transaction(Arc::clone(&state), Arc::clone(&snapshot)).await;
        return Err(with_provider_rollback_errors(
            format!("{operation_label} 后同步 Codex Agent Role 失败: {error}"),
            rollback_errors,
        ));
    }

    log::debug!("已在 Codex Provider 命令事务中完成切换: {id}");
    Ok(result)
}

async fn execute_switch_provider_with<F>(
    state: Arc<AppState>,
    app_type: AppType,
    id: String,
    operation: F,
) -> Result<SwitchResult, String>
where
    F: FnOnce() -> Result<SwitchResult, AppError> + Send + 'static,
{
    if !matches!(&app_type, AppType::Codex) {
        return run_provider_operation_blocking("切换 Provider", operation).await;
    }

    execute_codex_switch_transaction_with_setup(
        state,
        id,
        "切换 Provider",
        |_| async { Ok(()) },
        operation,
    )
    .await
}

pub(crate) async fn execute_codex_hot_switch(
    state: Arc<AppState>,
    provider_id: String,
) -> Result<SwitchResult, String> {
    let operation_state = Arc::clone(&state);
    let operation_id = provider_id.clone();
    execute_switch_provider_with(state, AppType::Codex, provider_id, move || {
        switch_provider_internal(operation_state.as_ref(), AppType::Codex, &operation_id)
    })
    .await
}

pub(crate) async fn enable_codex_auto_failover_to_provider(
    state: Arc<AppState>,
    provider_id: String,
    ensure_takeover: bool,
) -> Result<SwitchResult, String> {
    let operation_state = Arc::clone(&state);
    let operation_id = provider_id.clone();
    execute_codex_switch_transaction_with_setup(
        state,
        provider_id,
        "启用 Codex Auto 模式",
        move |setup_state| async move {
            if ensure_takeover {
                setup_state
                    .proxy_service
                    .set_takeover_for_app_inner(AppType::Codex.as_str(), true)
                    .await
            } else {
                let config = setup_state
                    .db
                    .get_proxy_config_for_app(AppType::Codex.as_str())
                    .await
                    .map_err(|error| error.to_string())?;
                if config.enabled {
                    Ok(())
                } else {
                    Err("需要先启用 Codex 的代理接管，再开启故障转移".to_string())
                }
            }
        },
        move || {
            let result =
                switch_provider_internal(operation_state.as_ref(), AppType::Codex, &operation_id)?;
            let mut config = futures::executor::block_on(
                operation_state
                    .db
                    .get_proxy_config_for_app(AppType::Codex.as_str()),
            )?;
            config.auto_failover_enabled = true;
            futures::executor::block_on(operation_state.db.update_proxy_config_for_app(config))?;
            Ok(result)
        },
    )
    .await
}

/// 获取所有供应商
#[tauri::command]
pub fn get_providers(
    state: State<'_, AppState>,
    app: String,
) -> Result<IndexMap<String, Provider>, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::list(state.inner(), app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_current_provider(state: State<'_, AppState>, app: String) -> Result<String, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::current(state.inner(), app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_provider_transfer_preview(
    state: State<'_, AppState>,
    source_app: String,
    source_provider_id: String,
) -> Result<ProviderTransferPreview, String> {
    let app_type = AppType::from_str(&source_app).map_err(|e| e.to_string())?;
    ProviderService::transfer_preview(state.inner(), app_type, &source_provider_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn transfer_provider_to_apps(
    state: State<'_, AppState>,
    request: ProviderTransferRequest,
) -> Result<Vec<ProviderTransferResult>, String> {
    let includes_codex = request
        .target_apps
        .iter()
        .any(|app| app.eq_ignore_ascii_case(AppType::Codex.as_str()));
    let owned_state = owned_app_state(state.inner());
    let _codex_lifecycle_guard = if includes_codex {
        Some(owned_state.lock_codex_provider_lifecycle().await)
    } else {
        None
    };
    let operation_state = Arc::clone(&owned_state);
    run_provider_operation_blocking("迁移 Provider", move || {
        ProviderService::transfer_to_apps(operation_state.as_ref(), request)
    })
    .await
}

#[tauri::command]
pub async fn add_provider(
    state: State<'_, AppState>,
    app: String,
    provider: Provider,
    #[allow(non_snake_case)] addToLive: Option<bool>,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    let provider_id = provider.id.clone();
    let add_to_live = addToLive.unwrap_or(true);
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    let operation_app_type = app_type.clone();
    execute_provider_mutation(
        owned_state,
        app_type,
        vec![provider_id],
        "保存 Provider",
        move || {
            ProviderService::add(
                operation_state.as_ref(),
                operation_app_type,
                provider,
                add_to_live,
            )
        },
    )
    .await
}

#[tauri::command]
pub async fn update_provider(
    state: State<'_, AppState>,
    app: String,
    provider: Provider,
    #[allow(non_snake_case)] originalId: Option<String>,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    let original_id = originalId
        .as_deref()
        .unwrap_or(provider.id.as_str())
        .to_string();
    let provider_id = provider.id.clone();
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    let operation_app_type = app_type.clone();
    execute_provider_mutation(
        owned_state,
        app_type,
        vec![original_id, provider_id],
        "更新 Provider",
        move || {
            ProviderService::update(
                operation_state.as_ref(),
                operation_app_type,
                originalId.as_deref(),
                provider,
            )
        },
    )
    .await
}

#[tauri::command]
pub async fn delete_provider(
    state: State<'_, AppState>,
    app: String,
    id: String,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    let operation_app_type = app_type.clone();
    let operation_id = id.clone();
    execute_provider_mutation(owned_state, app_type, vec![id], "?? Provider", move || {
        ProviderService::delete(operation_state.as_ref(), operation_app_type, &operation_id)
            .map(|_| true)
    })
    .await
}

#[tauri::command]
pub fn remove_provider_from_live_config(
    state: tauri::State<'_, AppState>,
    app: String,
    id: String,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::remove_from_live_config(state.inner(), app_type, &id)
        .map(|_| true)
        .map_err(|e| e.to_string())
}

fn switch_provider_internal(
    state: &AppState,
    app_type: AppType,
    id: &str,
) -> Result<SwitchResult, AppError> {
    ProviderService::switch(state, app_type, id)
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub fn switch_provider_test_hook(
    state: &AppState,
    app_type: AppType,
    id: &str,
) -> Result<SwitchResult, AppError> {
    switch_provider_internal(state, app_type, id)
}

#[tauri::command]
pub async fn switch_provider(
    app_handle: tauri::AppHandle,
    app: String,
    id: String,
) -> Result<SwitchResult, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    let operation_app_type = app_type.clone();
    let operation_id = id.clone();
    execute_switch_provider_with(owned_state, app_type, id, move || {
        switch_provider_internal(operation_state.as_ref(), operation_app_type, &operation_id)
    })
    .await
}

fn import_default_config_internal(state: &AppState, app_type: AppType) -> Result<bool, AppError> {
    let imported = ProviderService::import_default_config(state, app_type.clone())?;

    if imported {
        // Extract common config snippet (mirrors old startup logic in lib.rs)
        if state
            .db
            .should_auto_extract_config_snippet(app_type.as_str())?
        {
            match ProviderService::extract_common_config_snippet(state, app_type.clone()) {
                Ok(snippet) if !snippet.is_empty() && snippet != "{}" => {
                    let _ = state
                        .db
                        .set_config_snippet(app_type.as_str(), Some(snippet));
                    let _ = state
                        .db
                        .set_config_snippet_cleared(app_type.as_str(), false);
                }
                _ => {}
            }
        }

        ProviderService::migrate_legacy_common_config_usage_if_needed(state, app_type.clone())?;
    }

    Ok(imported)
}

#[cfg_attr(not(feature = "test-hooks"), doc(hidden))]
pub fn import_default_config_test_hook(
    state: &AppState,
    app_type: AppType,
) -> Result<bool, AppError> {
    import_default_config_internal(state, app_type)
}

#[tauri::command]
pub async fn import_default_config(
    state: State<'_, AppState>,
    app: String,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    if matches!(app_type, AppType::Codex) {
        execute_codex_provider_mutation_all(owned_state, "??????", move || {
            import_default_config_internal(operation_state.as_ref(), AppType::Codex)
        })
        .await
    } else {
        run_provider_operation_blocking("??????", move || {
            import_default_config_internal(operation_state.as_ref(), app_type)
        })
        .await
    }
}

#[tauri::command]
pub async fn get_claude_desktop_status(
    state: State<'_, AppState>,
) -> Result<crate::claude_desktop_config::ClaudeDesktopStatus, String> {
    let proxy_running = state.proxy_service.is_running().await;
    crate::claude_desktop_config::get_status(state.db.as_ref(), proxy_running)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_claude_desktop_default_routes(
) -> Vec<crate::claude_desktop_config::ClaudeDesktopDefaultRoute> {
    crate::claude_desktop_config::default_proxy_routes()
}

#[tauri::command]
pub fn import_claude_desktop_providers_from_claude(
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let claude_providers = state
        .db
        .get_all_providers(AppType::Claude.as_str())
        .map_err(|e| e.to_string())?;
    let existing_ids = state
        .db
        .get_provider_ids(AppType::ClaudeDesktop.as_str())
        .map_err(|e| e.to_string())?;

    let mut imported = 0usize;
    for provider in claude_providers.values() {
        if existing_ids.contains(&provider.id) {
            continue;
        }

        let mut desktop_provider = provider.clone();
        desktop_provider.in_failover_queue = false;
        let meta = desktop_provider.meta.get_or_insert_with(Default::default);

        if crate::claude_desktop_config::is_compatible_direct_provider(provider)
            && claude_provider_models_are_claude_safe(provider)
        {
            meta.claude_desktop_mode = Some(ClaudeDesktopMode::Direct);
        } else if let Some(routes) = suggested_claude_desktop_routes(provider) {
            meta.claude_desktop_mode = Some(ClaudeDesktopMode::Proxy);
            meta.claude_desktop_model_routes = routes;
        } else {
            continue;
        }

        state
            .db
            .save_provider(AppType::ClaudeDesktop.as_str(), &desktop_provider)
            .map_err(|e| e.to_string())?;
        imported += 1;
    }

    // Safety net: 用户可能手动删除过 claude-desktop-official seed。
    // 用户主动点 import 是"重新整理 ClaudeDesktop 表"的隐式信号，把官方入口补回来。
    // 失败只 warn，不影响 imported 主流程；imported 计数语义保持纯净。
    if let Err(e) = state.db.ensure_official_seed_by_id(
        crate::database::CLAUDE_DESKTOP_OFFICIAL_PROVIDER_ID,
        AppType::ClaudeDesktop,
    ) {
        log::warn!("Failed to ensure claude-desktop-official seed during import: {e}");
    }

    Ok(imported)
}

#[tauri::command]
pub fn ensure_claude_desktop_official_provider(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .db
        .ensure_official_seed_by_id(
            crate::database::CLAUDE_DESKTOP_OFFICIAL_PROVIDER_ID,
            AppType::ClaudeDesktop,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn ensure_codex_official_provider(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .db
        .ensure_official_seed_by_id(crate::database::CODEX_OFFICIAL_PROVIDER_ID, AppType::Codex)
        .map_err(|e| e.to_string())
}

fn claude_provider_models_are_claude_safe(provider: &Provider) -> bool {
    let Some(env) = provider
        .settings_config
        .get("env")
        .and_then(|value| value.as_object())
    else {
        return true;
    };

    [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    ]
    .into_iter()
    .filter_map(|key| env.get(key).and_then(|value| value.as_str()))
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .all(crate::claude_desktop_config::is_claude_safe_model_id)
}

pub(crate) fn suggested_claude_desktop_routes(
    provider: &Provider,
) -> Option<std::collections::HashMap<String, crate::provider::ClaudeDesktopModelRoute>> {
    let env = provider
        .settings_config
        .get("env")
        .and_then(|value| value.as_object())?;
    let mut routes = std::collections::HashMap::new();
    let supports_1m_default = !matches!(
        provider
            .meta
            .as_ref()
            .and_then(|meta| meta.provider_type.as_deref()),
        Some("github_copilot") | Some("codex_oauth")
    );

    fn add_route(
        routes: &mut std::collections::HashMap<String, crate::provider::ClaudeDesktopModelRoute>,
        env: &serde_json::Map<String, serde_json::Value>,
        route_key: &str,
        env_key: &str,
        supports_1m_default: bool,
    ) {
        let Some(raw_model) = env
            .get(env_key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return;
        };

        // Claude 端 env 值可能带 [1M] 后缀；Claude Desktop schema 不接受后缀，
        // 改用 supports1m 字段表达 1M 能力。在 import 边界做单向翻译。
        let marker = crate::claude_desktop_config::ONE_M_CONTEXT_MARKER.as_bytes();
        let raw_bytes = raw_model.as_bytes();
        let has_1m_marker = raw_bytes.len() >= marker.len()
            && raw_bytes[raw_bytes.len() - marker.len()..].eq_ignore_ascii_case(marker);
        let stripped_model: &str = if has_1m_marker {
            raw_model[..raw_model.len() - marker.len()].trim_end()
        } else {
            raw_model
        };
        if stripped_model.is_empty() {
            return;
        }
        let effective_supports_1m = supports_1m_default || has_1m_marker;
        let explicit_label_override = env
            .get(&format!("{env_key}_NAME"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let label_override = explicit_label_override.clone().or_else(|| {
            (!crate::claude_desktop_config::is_claude_safe_model_id(stripped_model))
                .then(|| stripped_model.to_string())
        });

        // 何时覆盖既有 label_override：原本为空 / 这次来的是 explicit _NAME /
        // 既有值只是 stripped_model 派生的占位（被 explicit 或更具体的值挤掉）。
        let should_overwrite = |existing: Option<&str>| {
            existing.is_none()
                || explicit_label_override.is_some()
                || existing == Some(stripped_model)
        };

        let merge_into = |existing: &mut crate::provider::ClaudeDesktopModelRoute| {
            let merged = existing.supports_1m.unwrap_or(false) || effective_supports_1m;
            existing.supports_1m = Some(merged);
            if should_overwrite(existing.label_override.as_deref()) {
                existing.label_override = label_override.clone();
            }
        };

        if let Some(existing) = routes
            .values_mut()
            .find(|existing| existing.model == stripped_model)
        {
            merge_into(existing);
            return;
        }

        routes
            .entry(route_key.to_string())
            .and_modify(merge_into)
            .or_insert_with(|| crate::provider::ClaudeDesktopModelRoute {
                model: stripped_model.to_string(),
                label_override,
                supports_1m: Some(effective_supports_1m),
            });
    }

    for spec in crate::claude_desktop_config::DEFAULT_PROXY_ROUTES {
        add_route(
            &mut routes,
            env,
            spec.route_id,
            spec.env_key,
            supports_1m_default,
        );
    }

    // 三个 default env_key 全空时用 ANTHROPIC_MODEL 派生兜底路由。
    if routes.is_empty() {
        let primary_route = crate::claude_desktop_config::DEFAULT_PROXY_ROUTES[0].route_id;
        add_route(
            &mut routes,
            env,
            primary_route,
            "ANTHROPIC_MODEL",
            supports_1m_default,
        );
    }

    (!routes.is_empty()).then_some(routes)
}

#[allow(non_snake_case)]
#[tauri::command]
pub async fn queryProviderUsage(
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
    copilot_state: State<'_, CopilotAuthState>,
    #[allow(non_snake_case)] providerId: String, // 使用 camelCase 匹配前端
    app: String,
) -> Result<crate::provider::UsageResult, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    // inner 可能以两种形式失败：
    //   1) 返回 Ok(UsageResult { success: false, .. }) —— 确定性失败（401、脚本
    //      报错、未知供应商等）。写进 UsageCache 并刷新托盘，让
    //      format_script_summary 的 success 守卫生效、suffix 自然消失。
    //   2) 返回 Err(String) —— 瞬时传输失败（网络/超时）及 DB/Copilot fetch 等。
    //      不写失败快照、不 emit：保留上一份托盘快照，与前端 react-query reject
    //      保留上次 data 的语义一致；否则失败快照会经 useUsageCacheBridge 盲写
    //      回 query 缓存，抹掉 reject 本该保留的旧值。
    let inner =
        query_provider_usage_inner(&state, &copilot_state, app_type.clone(), &providerId).await;
    if let Ok(snapshot) = &inner {
        let payload = serde_json::json!({
            "kind": "script",
            "appType": app_type.as_str(),
            "providerId": &providerId,
            "data": snapshot,
        });
        if let Err(e) = app_handle.emit("usage-cache-updated", payload) {
            log::error!("emit usage-cache-updated (script) 失败: {e}");
        }
        state
            .usage_cache
            .put_script(app_type, providerId, snapshot.clone());
        crate::tray::schedule_tray_refresh(&app_handle);
    }
    inner
}

/// Resolve `(base_url, api_key)` for native usage queries, delegating to the
/// per-app resolver on `Provider`. Missing provider → empty credentials.
fn resolve_native_credentials(app_type: &AppType, provider: Option<&Provider>) -> (String, String) {
    provider
        .map(|p| p.resolve_usage_credentials(app_type))
        .unwrap_or_default()
}

fn resolve_coding_plan_credentials(
    app_type: &AppType,
    provider: Option<&Provider>,
    usage_script: Option<&crate::provider::UsageScript>,
) -> (String, String) {
    let is_zenmux = usage_script
        .and_then(|s| s.coding_plan_provider.as_deref())
        .map(|provider| provider.eq_ignore_ascii_case("zenmux"))
        .unwrap_or(false);

    if !is_zenmux {
        return resolve_native_credentials(app_type, provider);
    }

    let script_base_url = usage_script
        .and_then(|s| s.base_url.as_deref())
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let script_api_key = usage_script
        .and_then(|s| s.api_key.as_deref())
        .unwrap_or("")
        .to_string();

    if !script_base_url.is_empty() && !script_api_key.is_empty() {
        return (script_base_url, script_api_key);
    }

    let native = resolve_native_credentials(app_type, provider);
    if !native.0.is_empty() && !native.1.is_empty() {
        native
    } else {
        (script_base_url, script_api_key)
    }
}

async fn query_provider_usage_inner(
    state: &AppState,
    copilot_state: &CopilotAuthState,
    app_type: AppType,
    provider_id: &str,
) -> Result<crate::provider::UsageResult, String> {
    // 从数据库读取供应商信息，检查特殊模板类型
    let providers = state
        .db
        .get_all_providers(app_type.as_str())
        .map_err(|e| format!("Failed to get providers: {e}"))?;
    let provider = providers.get(provider_id);
    let usage_script = provider
        .and_then(|p| p.meta.as_ref())
        .and_then(|m| m.usage_script.as_ref());
    let template_type = usage_script
        .and_then(|s| s.template_type.as_deref())
        .unwrap_or("");

    // ── GitHub Copilot 专用路径 ──
    if template_type == TEMPLATE_TYPE_GITHUB_COPILOT {
        let copilot_account_id = provider
            .and_then(|p| p.meta.as_ref())
            .and_then(|m| m.managed_account_id_for(TEMPLATE_TYPE_GITHUB_COPILOT));

        let auth_manager = copilot_state.0.read().await;
        let usage = match copilot_account_id.as_deref() {
            Some(account_id) => auth_manager
                .fetch_usage_for_account(account_id)
                .await
                .map_err(|e| format!("Failed to fetch Copilot usage: {e}"))?,
            None => auth_manager
                .fetch_usage()
                .await
                .map_err(|e| format!("Failed to fetch Copilot usage: {e}"))?,
        };
        let premium = &usage.quota_snapshots.premium_interactions;
        let used = premium.entitlement - premium.remaining;

        return Ok(crate::provider::UsageResult {
            success: true,
            data: Some(vec![crate::provider::UsageData {
                plan_name: Some(usage.copilot_plan),
                remaining: Some(premium.remaining as f64),
                total: Some(premium.entitlement as f64),
                used: Some(used as f64),
                unit: Some(COPILOT_UNIT_PREMIUM.to_string()),
                is_valid: Some(true),
                invalid_message: None,
                extra: Some(format!("Reset: {}", usage.quota_reset_date)),
            }]),
            error: None,
        });
    }

    // ── Coding Plan 专用路径 ──
    if template_type == TEMPLATE_TYPE_TOKEN_PLAN {
        let (base_url, api_key) =
            resolve_coding_plan_credentials(&app_type, provider, usage_script);

        // 火山方舟用账号 AK/SK 签名查询用量（存于 usage_script，与推理 api_key 分离）；
        // 其他供应商为 None，service 层沿用 api_key。
        let access_key_id = usage_script.and_then(|s| s.access_key_id.clone());
        let secret_access_key = usage_script.and_then(|s| s.secret_access_key.clone());
        // 智谱团队版：显式 provider 标识 + 组织/项目 ID（与个人版智谱 base_url 相同，
        // 靠 coding_plan_provider == "zhipu_team" 在 service 层路由）。
        let coding_plan_provider = usage_script.and_then(|s| s.coding_plan_provider.clone());
        let team_organization_id = usage_script.and_then(|s| s.team_organization_id.clone());
        let team_project_id = usage_script.and_then(|s| s.team_project_id.clone());

        let quota = crate::services::coding_plan::get_coding_plan_quota(
            &base_url,
            &api_key,
            access_key_id.as_deref(),
            secret_access_key.as_deref(),
            coding_plan_provider.as_deref(),
            team_organization_id.as_deref(),
            team_project_id.as_deref(),
        )
        .await
        .map_err(|e| format!("Failed to query coding plan: {e}"))?;

        // 将 SubscriptionQuota 转换为 UsageResult
        if !quota.success {
            return Ok(crate::provider::UsageResult {
                success: false,
                data: None,
                error: quota.error,
            });
        }

        // ZenMux 的 tier 携带 USD 额度信息，需要编码为 JSON extra
        let has_usd = quota
            .tiers
            .first()
            .map(|t| t.used_value_usd.is_some())
            .unwrap_or(false);
        let plan_label = quota
            .credential_message
            .as_deref()
            .and_then(|msg| msg.split(' ').next())
            .map(|tier| format!("ZenMux·{}", tier.to_uppercase()));
        let mut first_tier = true;

        let data: Vec<crate::provider::UsageData> = quota
            .tiers
            .iter()
            .map(|tier| {
                let total = 100.0;
                let used = tier.utilization;
                let remaining = total - used;
                let extra = if has_usd {
                    let mut extra_json = serde_json::json!({
                        "resetsAt": tier.resets_at,
                    });
                    if let Some(v) = tier.used_value_usd {
                        extra_json["usedValueUsd"] = serde_json::json!(v);
                    }
                    if let Some(v) = tier.max_value_usd {
                        extra_json["maxValueUsd"] = serde_json::json!(v);
                    }
                    if first_tier {
                        if let Some(ref label) = plan_label {
                            extra_json["planLabel"] = serde_json::json!(label);
                        }
                        first_tier = false;
                    }
                    Some(extra_json.to_string())
                } else {
                    tier.resets_at.clone()
                };
                crate::provider::UsageData {
                    plan_name: Some(tier.name.clone()),
                    remaining: Some(remaining),
                    total: Some(total),
                    used: Some(used),
                    unit: Some("%".to_string()),
                    is_valid: Some(true),
                    invalid_message: None,
                    extra,
                }
            })
            .collect();

        return Ok(crate::provider::UsageResult {
            success: true,
            data: if data.is_empty() { None } else { Some(data) },
            error: None,
        });
    }

    // ── 官方余额查询路径 ──
    if template_type == TEMPLATE_TYPE_BALANCE {
        // 按 app 区分的凭据存储格式提取 Base URL 与 API Key
        let (base_url, api_key) = resolve_native_credentials(&app_type, provider);

        return crate::services::balance::get_balance(&base_url, &api_key)
            .await
            .map_err(|e| format!("Failed to query balance: {e}"));
    }

    // ── 官方订阅额度查询路径 ──
    if template_type == TEMPLATE_TYPE_OFFICIAL_SUBSCRIPTION {
        if !usage_script.map(|s| s.enabled).unwrap_or(false) {
            return Ok(crate::provider::UsageResult {
                success: false,
                data: None,
                error: Some("Usage query is disabled".to_string()),
            });
        }

        let quota = crate::services::subscription::get_subscription_quota(app_type.as_str())
            .await
            .map_err(|e| format!("Failed to query subscription quota: {e}"))?;

        if !quota.success {
            return Ok(crate::provider::UsageResult {
                success: false,
                data: None,
                error: quota.error.or(quota.credential_message),
            });
        }

        let data: Vec<crate::provider::UsageData> = quota
            .tiers
            .iter()
            .map(|tier| crate::provider::UsageData {
                plan_name: Some(tier.name.clone()),
                remaining: Some(100.0 - tier.utilization),
                total: Some(100.0),
                used: Some(tier.utilization),
                unit: Some("%".to_string()),
                is_valid: Some(true),
                invalid_message: None,
                extra: tier.resets_at.clone(),
            })
            .collect();

        return Ok(crate::provider::UsageResult {
            success: true,
            data: if data.is_empty() { None } else { Some(data) },
            error: None,
        });
    }

    // ── 通用 JS 脚本路径 ──
    ProviderService::query_usage(state, app_type, provider_id)
        .await
        .map_err(|e| e.to_string())
}

#[allow(non_snake_case)]
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn testUsageScript(
    state: State<'_, AppState>,
    #[allow(non_snake_case)] providerId: String,
    app: String,
    #[allow(non_snake_case)] scriptCode: String,
    timeout: Option<u64>,
    #[allow(non_snake_case)] apiKey: Option<String>,
    #[allow(non_snake_case)] baseUrl: Option<String>,
    #[allow(non_snake_case)] accessToken: Option<String>,
    #[allow(non_snake_case)] userId: Option<String>,
    #[allow(non_snake_case)] templateType: Option<String>,
) -> Result<crate::provider::UsageResult, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::test_usage_script(
        state.inner(),
        app_type,
        &providerId,
        &scriptCode,
        timeout.unwrap_or(10),
        apiKey.as_deref(),
        baseUrl.as_deref(),
        accessToken.as_deref(),
        userId.as_deref(),
        templateType.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn read_live_provider_settings(app: String) -> Result<serde_json::Value, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::read_live_settings(app_type).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn test_api_endpoints(
    urls: Vec<String>,
    #[allow(non_snake_case)] timeoutSecs: Option<u64>,
) -> Result<Vec<EndpointLatency>, String> {
    SpeedtestService::test_endpoints(urls, timeoutSecs)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_custom_endpoints(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
) -> Result<Vec<crate::settings::CustomEndpoint>, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::get_custom_endpoints(state.inner(), app_type, &providerId)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_custom_endpoint(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
    url: String,
) -> Result<(), String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::add_custom_endpoint(state.inner(), app_type, &providerId, url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_custom_endpoint(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
    url: String,
) -> Result<(), String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::remove_custom_endpoint(state.inner(), app_type, &providerId, url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_endpoint_last_used(
    state: State<'_, AppState>,
    app: String,
    #[allow(non_snake_case)] providerId: String,
    url: String,
) -> Result<(), String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    ProviderService::update_endpoint_last_used(state.inner(), app_type, &providerId, url)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_providers_sort_order(
    state: State<'_, AppState>,
    app: String,
    updates: Vec<ProviderSortUpdate>,
) -> Result<bool, String> {
    let app_type = AppType::from_str(&app).map_err(|e| e.to_string())?;
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    if matches!(app_type, AppType::Codex) {
        execute_codex_provider_mutation_all(
            owned_state,
            "更新 Codex Provider 排序",
            move || {
                ProviderService::update_sort_order(
                    operation_state.as_ref(),
                    AppType::Codex,
                    updates,
                )
            },
        )
        .await
    } else {
        run_provider_operation_blocking("更新 Provider 排序", move || {
            ProviderService::update_sort_order(operation_state.as_ref(), app_type, updates)
        })
        .await
    }
}

use crate::provider::UniversalProvider;
use std::collections::HashMap;
use tauri::AppHandle;

#[derive(Clone, serde::Serialize)]
pub struct UniversalProviderSyncedEvent {
    pub action: String,
    pub id: String,
}

fn emit_universal_provider_synced(app: &AppHandle, action: &str, id: &str) {
    let _ = app.emit(
        "universal-provider-synced",
        UniversalProviderSyncedEvent {
            action: action.to_string(),
            id: id.to_string(),
        },
    );
}

#[tauri::command]
pub fn get_universal_providers(
    state: State<'_, AppState>,
) -> Result<HashMap<String, UniversalProvider>, String> {
    ProviderService::list_universal(state.inner()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_universal_provider(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<UniversalProvider>, String> {
    ProviderService::get_universal(state.inner(), &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn upsert_universal_provider(
    app: AppHandle,
    state: State<'_, AppState>,
    provider: UniversalProvider,
) -> Result<bool, String> {
    let id = provider.id.clone();
    let result =
        ProviderService::upsert_universal(state.inner(), provider).map_err(|e| e.to_string())?;

    emit_universal_provider_synced(&app, "upsert", &id);

    Ok(result)
}

#[tauri::command]
pub async fn delete_universal_provider(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    let operation_id = id.clone();
    let result =
        execute_codex_provider_mutation_all(owned_state, "删除 Universal Provider", move || {
            ProviderService::delete_universal(operation_state.as_ref(), &operation_id)
        })
        .await?;

    emit_universal_provider_synced(&app, "delete", &id);

    Ok(result)
}

#[tauri::command]
pub async fn sync_universal_provider(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let owned_state = owned_app_state(state.inner());
    let operation_state = Arc::clone(&owned_state);
    let operation_id = id.clone();
    let result =
        execute_codex_provider_mutation_all(owned_state, "同步 Universal Provider", move || {
            ProviderService::sync_universal_to_apps(operation_state.as_ref(), &operation_id)
        })
        .await?;

    emit_universal_provider_synced(&app, "sync", &id);

    Ok(result)
}

#[tauri::command]
pub fn import_opencode_providers_from_live(state: State<'_, AppState>) -> Result<usize, String> {
    crate::services::provider::import_opencode_providers_from_live(state.inner())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_opencode_live_provider_ids() -> Result<Vec<String>, String> {
    crate::opencode_config::get_providers()
        .map(|providers| providers.keys().cloned().collect())
        .map_err(|e| e.to_string())
}

// ============================================================================
// OpenClaw 专属命令 → 已迁移至 commands/openclaw.rs
// ============================================================================

#[cfg(test)]
mod codex_provider_mutation_tests {
    use super::{
        append_switch_rollback_warnings, capture_codex_provider_mutation, execute_codex_hot_switch,
        execute_codex_switch_transaction_with_setup, execute_provider_mutation,
        execute_switch_provider_with, owned_app_state, rollback_codex_provider_mutation,
        with_provider_rollback_errors, CodexProviderSnapshotScope, CODEX_ROLE_RESTORE_SKIPPED,
    };
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::error::AppError;
    use crate::provider::{
        CodexAgentRoleRouting, CodexFrontendAgentRoleOverride, Provider, ProviderMeta,
    };
    use crate::services::{ProviderService, SwitchResult};
    use crate::store::AppState;
    use serde_json::json;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    const OLD_CONFIG: &[u8] = b"model = \"old-model\"\n";
    const OLD_AUTH: &[u8] = br#"{"OPENAI_API_KEY":"old-key"}"#;
    const OLD_CATALOG: &[u8] = br#"{"models":[{"slug":"old-model"}]}"#;

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

    fn provider(id: &str, name: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            name.to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "test-key" },
                "config": "model_provider = \"test\"\n[model_providers.test]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\n"
            }),
            None,
        )
    }

    fn provider_with_catalog(id: &str, name: &str, model: &str) -> Provider {
        let mut provider = provider(id, name);
        provider.settings_config["modelCatalog"] = json!({
            "models": [{
                "model": model,
                "displayName": format!("{name} model")
            }]
        });
        provider
    }

    fn provider_with_role_routing(id: &str, name: &str, enabled: bool) -> Provider {
        let mut provider = provider_with_catalog(id, name, "gpt-5.6-sol");
        provider.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(CodexAgentRoleRouting {
                enabled: Some(enabled),
                frontend: Some(CodexFrontendAgentRoleOverride::default()),
                backend: None,
            }),
            ..ProviderMeta::default()
        });
        provider
    }

    fn write_file(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().expect("file parent")).expect("create file parent");
        fs::write(path, content).expect("write fixture file");
    }

    fn seed_live_files() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let config_path = crate::codex_config::get_codex_config_path();
        let auth_path = crate::codex_config::get_codex_auth_path();
        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        write_file(&config_path, OLD_CONFIG);
        write_file(&auth_path, OLD_AUTH);
        write_file(&catalog_path, OLD_CATALOG);
        (config_path, auth_path, catalog_path)
    }

    fn assert_live_files_restored(config_path: &Path, auth_path: &Path, catalog_path: &Path) {
        assert_eq!(fs::read(config_path).expect("restored config"), OLD_CONFIG);
        assert_eq!(fs::read(auth_path).expect("restored auth"), OLD_AUTH);
        assert_eq!(
            fs::read(catalog_path).expect("restored catalog"),
            OLD_CATALOG
        );
    }

    #[tokio::test]
    #[serial]
    async fn coordinated_codex_hot_switch_waits_for_provider_lifecycle() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider_a = provider("provider-a", "Provider A");
        let provider_b = provider("provider-b", "Provider B");
        for provider in [&provider_a, &provider_b] {
            db.save_provider(AppType::Codex.as_str(), provider)
                .expect("seed provider");
        }
        db.set_current_provider(AppType::Codex.as_str(), &provider_a.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set device current provider");
        let _ = seed_live_files();
        futures::executor::block_on(db.save_live_backup(
            AppType::Codex.as_str(),
            &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
        ))
        .expect("seed live backup");

        let lifecycle_guard = state.lock_codex_provider_lifecycle().await;
        let switch_state = Arc::clone(&state);
        let switch_task =
            tokio::spawn(
                async move { execute_codex_hot_switch(switch_state, provider_b.id).await },
            );
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some(provider_a.id.clone()),
            "the hot switch must wait behind an active Provider lifecycle transaction"
        );

        drop(lifecycle_guard);
        switch_task
            .await
            .expect("switch task joins")
            .expect("coordinated hot switch succeeds");
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some("provider-b".to_string())
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    #[serial]
    async fn add_command_rolls_back_real_provider_service_partial_failure() {
        use std::os::windows::fs::OpenOptionsExt;

        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let added = provider_with_catalog("provider-new", "Provider New", "new-model");
        let (config_path, auth_path, catalog_path) = seed_live_files();
        let operation_state = Arc::clone(&state);
        let operation_db = Arc::clone(&db);
        let operation_added = added.clone();
        let operation_config_path = config_path.clone();
        let operation_catalog_path = catalog_path.clone();

        let error = execute_provider_mutation(
            Arc::clone(&state),
            AppType::Codex,
            vec![added.id.clone()],
            "保存 Provider",
            move || {
                let config_lock = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode(0)
                    .open(&operation_config_path)
                    .expect("lock config without delete sharing");
                let result = ProviderService::add(
                    operation_state.as_ref(),
                    AppType::Codex,
                    operation_added.clone(),
                    true,
                );

                assert!(result.is_err(), "locked config must fail the live write");
                assert!(
                    operation_db
                        .get_provider_by_id(&operation_added.id, AppType::Codex.as_str())
                        .expect("read partially-saved provider")
                        .is_some(),
                    "ProviderService::add saves the DB row before the live write fails"
                );
                assert_eq!(
                    operation_db
                        .get_current_provider(AppType::Codex.as_str())
                        .expect("read partial database current"),
                    Some(operation_added.id.clone()),
                    "ProviderService::add sets DB current before the live write fails"
                );
                assert!(
                    fs::read_to_string(&operation_catalog_path)
                        .expect("read partially-written catalog")
                        .contains("new-model"),
                    "catalog projection occurs before the locked config write fails"
                );

                drop(config_lock);
                result
            },
        )
        .await
        .expect_err("the command must roll back ProviderService's partial failure");

        assert!(error.contains("保存 Provider 失败"));
        assert!(db
            .get_provider_by_id(&added.id, AppType::Codex.as_str())
            .expect("read rolled-back provider slot")
            .is_none());
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current"),
            None
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
    }

    #[tokio::test]
    #[serial]
    async fn add_command_rolls_back_real_service_mutations_and_catalog_on_primary_error() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let added = provider_with_catalog("provider-new", "Provider New", "new-model");
        let (config_path, auth_path, catalog_path) = seed_live_files();
        futures::executor::block_on(
            db.save_live_backup(AppType::Codex.as_str(), "old-live-backup"),
        )
        .expect("seed live backup");
        let operation_state = Arc::clone(&state);
        let operation_db = Arc::clone(&db);
        let operation_added = added.clone();

        let error = execute_provider_mutation(
            Arc::clone(&state),
            AppType::Codex,
            vec![added.id.clone()],
            "保存 Provider",
            move || {
                ProviderService::add(
                    operation_state.as_ref(),
                    AppType::Codex,
                    operation_added.clone(),
                    true,
                )?;
                crate::settings::set_current_provider(&AppType::Codex, Some(&operation_added.id))?;
                futures::executor::block_on(
                    operation_db.save_live_backup(AppType::Codex.as_str(), "new-live-backup"),
                )?;
                Err::<bool, _>(AppError::Message(
                    "injected service tail failure".to_string(),
                ))
            },
        )
        .await
        .expect_err("primary service error must roll back the command transaction");

        assert!(error.contains("injected service tail failure"));
        assert!(
            db.get_provider_by_id(&added.id, AppType::Codex.as_str())
                .expect("read added slot")
                .is_none(),
            "the provider row written by ProviderService::add must be removed"
        );
        assert_eq!(crate::settings::get_current_provider(&AppType::Codex), None);
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current"),
            None
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
        assert_eq!(
            futures::executor::block_on(db.get_live_backup(AppType::Codex.as_str()))
                .expect("read restored backup")
                .expect("backup restored")
                .original_config,
            "old-live-backup"
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_command_restores_raw_local_and_database_currents_independently() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let original = provider_with_catalog("provider-a", "Provider A", "old-model");
        let database_current = provider("provider-b", "Provider B");
        let transient_current = provider("provider-c", "Provider C");
        for provider in [&original, &database_current, &transient_current] {
            db.save_provider(AppType::Codex.as_str(), provider)
                .expect("seed provider");
        }
        crate::settings::set_current_provider(&AppType::Codex, Some(&original.id))
            .expect("set raw local current");
        db.set_current_provider(AppType::Codex.as_str(), &database_current.id)
            .expect("set raw database current");

        let (config_path, auth_path, catalog_path) = seed_live_files();

        let mut updated = provider_with_catalog("provider-a", "Provider A Updated", "new-model");
        updated.meta = original.meta.clone();
        let operation_state = Arc::clone(&state);
        let operation_db = Arc::clone(&db);
        let operation_updated = updated.clone();
        let operation_transient_current = transient_current.clone();
        let error = execute_provider_mutation(
            Arc::clone(&state),
            AppType::Codex,
            vec![original.id.clone()],
            "更新 Provider",
            move || {
                ProviderService::update(
                    operation_state.as_ref(),
                    AppType::Codex,
                    None,
                    operation_updated,
                )?;
                crate::settings::set_current_provider(
                    &AppType::Codex,
                    Some(&operation_transient_current.id),
                )?;
                operation_db.set_current_provider(
                    AppType::Codex.as_str(),
                    &operation_transient_current.id,
                )?;
                Err::<bool, _>(AppError::Message(
                    "injected service tail failure".to_string(),
                ))
            },
        )
        .await
        .expect_err("primary update error must roll back the command transaction");

        assert!(error.contains("injected service tail failure"));
        assert_eq!(
            db.get_provider_by_id(&original.id, AppType::Codex.as_str())
                .expect("read restored provider")
                .expect("provider restored")
                .name,
            original.name
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some(original.id.clone()),
            "raw device current must be restored without effective-current normalization"
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current"),
            Some(database_current.id.clone()),
            "raw database current must remain independent from the device current"
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
    }

    #[test]
    fn provider_transaction_error_includes_every_rollback_failure() {
        let error = with_provider_rollback_errors(
            "primary failure".to_string(),
            vec![
                "provider rollback failure".to_string(),
                "role projection rollback failure".to_string(),
            ],
        );
        assert!(error.contains("primary failure"));
        assert!(error.contains("provider rollback failure"));
        assert!(error.contains("role projection rollback failure"));
    }

    #[test]
    fn switch_rollback_warnings_are_aggregated_with_other_rollback_failures() {
        let mut rollback_errors = vec!["switch-back failure".to_string()];
        append_switch_rollback_warnings(
            &mut rollback_errors,
            SwitchResult {
                warnings: vec![
                    "backfill_failed:provider-b".to_string(),
                    "common_config_sync_failed:provider-b".to_string(),
                ],
            },
        );
        rollback_errors.push("role projection rollback failure".to_string());

        let error = with_provider_rollback_errors(
            "primary role projection failure".to_string(),
            rollback_errors,
        );
        for expected in [
            "primary role projection failure",
            "switch-back failure",
            "回切 Provider 警告: backfill_failed:provider-b",
            "回切 Provider 警告: common_config_sync_failed:provider-b",
            "role projection rollback failure",
        ] {
            assert!(error.contains(expected), "missing '{expected}' in: {error}");
        }
    }

    #[tokio::test]
    #[serial]
    async fn update_command_rolls_back_after_role_reconcile_failure() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let original = provider_with_role_routing("provider-a", "Provider A", false);
        db.save_provider(AppType::Codex.as_str(), &original)
            .expect("seed provider");
        db.set_current_provider(AppType::Codex.as_str(), &original.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&original.id))
            .expect("set device current provider");
        let (config_path, auth_path, catalog_path) = seed_live_files();
        let mut proxy_config = db.get_proxy_config().await.expect("read proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("use an ephemeral proxy port");

        let paths = crate::services::codex_agent_roles::CodexAgentRolePaths::default_codex_home();
        write_file(&paths.frontend, b"name = \"user-frontend\"\n");

        let updated = provider_with_role_routing("provider-a", "Provider A Updated", true);
        let operation_state = Arc::clone(&state);
        let error = execute_provider_mutation(
            Arc::clone(&state),
            AppType::Codex,
            vec![original.id.clone()],
            "更新 Provider",
            move || {
                ProviderService::update(operation_state.as_ref(), AppType::Codex, None, updated)
            },
        )
        .await
        .expect_err("unmanaged role file must fail post-save reconciliation");

        assert!(error.contains("更新 Provider 后同步 Codex Agent Role 失败"));
        let restored = db
            .get_provider_by_id(&original.id, AppType::Codex.as_str())
            .expect("read restored provider")
            .expect("provider restored");
        assert_eq!(restored.name, original.name);
        assert!(
            !restored
                .meta
                .as_ref()
                .and_then(|meta| meta.codex_agent_role_routing.as_ref())
                .is_some_and(CodexAgentRoleRouting::is_enabled),
            "the saved role-routing change must be rolled back"
        );
        assert_eq!(
            fs::read_to_string(&paths.frontend).expect("user role remains"),
            "name = \"user-frontend\"\n"
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
        assert_eq!(
            db.get_proxy_config()
                .await
                .expect("read restored proxy config")
                .listen_port,
            0,
            "failed role reconciliation must restore the ephemeral-port sentinel"
        );
        assert!(
            !state.proxy_service.is_running().await,
            "the transaction-started proxy must be stopped"
        );
        assert!(
            !state
                .proxy_service
                .get_takeover_status()
                .await
                .expect("read restored takeover")
                .codex,
            "Codex takeover must remain disabled"
        );
        assert!(
            !db.get_global_proxy_config()
                .await
                .expect("read restored global proxy config")
                .proxy_enabled,
            "the global proxy flag must return to its pre-transaction value"
        );
        assert!(
            db.get_live_backup(AppType::Codex.as_str())
                .await
                .expect("read restored live backup")
                .is_none(),
            "the failed takeover must not leave a Codex live backup"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    #[serial]
    async fn switch_command_rolls_back_real_provider_service_main_failure() {
        use std::os::windows::fs::OpenOptionsExt;

        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider_a = provider_with_catalog("provider-a", "Provider A", "old-model");
        let provider_b = provider_with_catalog("provider-b", "Provider B", "new-model");
        for provider in [&provider_a, &provider_b] {
            db.save_provider(AppType::Codex.as_str(), provider)
                .expect("seed provider");
        }
        db.set_current_provider(AppType::Codex.as_str(), &provider_a.id)
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set local current");
        let (config_path, auth_path, catalog_path) = seed_live_files();

        let operation_state = Arc::clone(&state);
        let operation_config_path = config_path.clone();
        let operation_catalog_path = catalog_path.clone();
        let operation_provider_b = provider_b.clone();
        let error = execute_switch_provider_with(
            Arc::clone(&state),
            AppType::Codex,
            provider_b.id.clone(),
            move || {
                let config_lock = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode(0)
                    .open(&operation_config_path)
                    .expect("lock config without delete sharing");
                let result = ProviderService::switch(
                    operation_state.as_ref(),
                    AppType::Codex,
                    &operation_provider_b.id,
                );
                assert!(result.is_err(), "locked config must fail the switch");
                assert!(
                    fs::read_to_string(&operation_catalog_path)
                        .expect("read partially-written catalog")
                        .contains("new-model"),
                    "switch writes the target catalog before config replacement fails"
                );
                drop(config_lock);
                result
            },
        )
        .await
        .expect_err("ProviderService main failure must trigger switch rollback");

        assert!(error.contains("切换 Provider 失败"));
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some(provider_a.id.clone())
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current"),
            Some(provider_a.id.clone())
        );
        assert_eq!(
            db.get_provider_by_id(&provider_a.id, AppType::Codex.as_str())
                .expect("read provider A")
                .expect("provider A restored")
                .settings_config,
            provider_a.settings_config
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
        assert!(
            futures::executor::block_on(db.get_live_backup(AppType::Codex.as_str()))
                .expect("read live backup")
                .is_none()
        );
    }

    #[tokio::test]
    #[serial]
    async fn switch_command_restores_none_currents_after_primary_error() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider_b = provider_with_catalog("provider-b", "Provider B", "new-model");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("seed target provider");
        let (config_path, auth_path, catalog_path) = seed_live_files();

        let operation_state = Arc::clone(&state);
        let operation_db = Arc::clone(&db);
        let operation_provider_b = provider_b.clone();
        let error = execute_switch_provider_with(
            Arc::clone(&state),
            AppType::Codex,
            provider_b.id.clone(),
            move || {
                ProviderService::switch(
                    operation_state.as_ref(),
                    AppType::Codex,
                    &operation_provider_b.id,
                )?;
                futures::executor::block_on(
                    operation_db.save_live_backup(AppType::Codex.as_str(), "transient-backup"),
                )?;
                Err(AppError::Message(
                    "injected switch tail failure".to_string(),
                ))
            },
        )
        .await
        .expect_err("primary switch error must restore empty current pointers");

        assert!(error.contains("injected switch tail failure"));
        assert_eq!(crate::settings::get_current_provider(&AppType::Codex), None);
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current"),
            None
        );
        assert!(
            futures::executor::block_on(db.get_live_backup(AppType::Codex.as_str()))
                .expect("read restored backup")
                .is_none()
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
    }

    #[tokio::test]
    #[serial]
    async fn switch_reconcile_failure_restores_split_raw_currents() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider_a = provider_with_role_routing("provider-a", "Provider A", false);
        let provider_b = provider("provider-b", "Provider B");
        let provider_c = provider_with_role_routing("provider-c", "Provider C", true);
        for provider in [&provider_a, &provider_b, &provider_c] {
            db.save_provider(AppType::Codex.as_str(), provider)
                .expect("seed provider");
        }
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider_a.id))
            .expect("set split local current");
        db.set_current_provider(AppType::Codex.as_str(), &provider_b.id)
            .expect("set split database current");
        let (config_path, auth_path, catalog_path) = seed_live_files();
        let paths = crate::services::codex_agent_roles::CodexAgentRolePaths::default_codex_home();
        write_file(&paths.frontend, b"name = \"user-frontend\"\n");

        let operation_state = Arc::clone(&state);
        let operation_provider_c = provider_c.clone();
        let error = execute_switch_provider_with(
            Arc::clone(&state),
            AppType::Codex,
            provider_c.id.clone(),
            move || {
                ProviderService::switch(
                    operation_state.as_ref(),
                    AppType::Codex,
                    &operation_provider_c.id,
                )
            },
        )
        .await
        .expect_err("unmanaged role must fail post-switch reconcile");

        assert!(error.contains("切换 Provider 后同步 Codex Agent Role 失败"));
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some(provider_a.id.clone())
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read restored database current"),
            Some(provider_b.id.clone())
        );
        assert_live_files_restored(&config_path, &auth_path, &catalog_path);
    }

    #[tokio::test]
    #[serial]
    async fn reconcile_rollback_failure_is_aggregated_before_raw_pointer_restore() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let original = provider_with_role_routing("provider-a", "Provider A", true);
        db.save_provider(AppType::Codex.as_str(), &original)
            .expect("seed provider");
        db.set_current_provider(AppType::Codex.as_str(), &original.id)
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&original.id))
            .expect("set local current");
        seed_live_files();
        let paths = crate::services::codex_agent_roles::CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::create_dir(&paths.frontend).expect("create conflicting role directory");

        let updated = provider_with_role_routing("provider-a", "Provider A Updated", true);
        let operation_state = Arc::clone(&state);
        let error = execute_provider_mutation(
            Arc::clone(&state),
            AppType::Codex,
            vec![original.id.clone()],
            "更新 Provider",
            move || {
                ProviderService::update(operation_state.as_ref(), AppType::Codex, None, updated)
            },
        )
        .await
        .expect_err("both initial and rollback role reconciliation must fail");

        assert!(error.contains("更新 Provider 后同步 Codex Agent Role 失败"));
        assert!(error.contains("恢复 Codex Agent Role 投影失败"));
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some(original.id.clone())
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current"),
            Some(original.id.clone())
        );
    }

    #[tokio::test]
    #[serial]
    async fn incomplete_base_rollback_skips_second_role_reconcile() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let original = provider_with_role_routing("provider-a", "Provider A", true);
        db.save_provider(AppType::Codex.as_str(), &original)
            .expect("seed provider");
        db.set_current_provider(AppType::Codex.as_str(), &original.id)
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&original.id))
            .expect("set local current");
        let (_, _, catalog_path) = seed_live_files();
        let paths = crate::services::codex_agent_roles::CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::create_dir(&paths.frontend).expect("create conflicting role directory");

        let operation_catalog_path = catalog_path.clone();
        let error = execute_provider_mutation(
            Arc::clone(&state),
            AppType::Codex,
            vec![original.id.clone()],
            "更新 Provider",
            move || {
                fs::remove_file(&operation_catalog_path)
                    .expect("remove catalog before obstruction");
                fs::create_dir(&operation_catalog_path).expect("replace catalog with directory");
                Err::<bool, _>(AppError::Message("injected base failure".to_string()))
            },
        )
        .await
        .expect_err("file rollback failure must skip role reconciliation");

        assert!(error.contains(CODEX_ROLE_RESTORE_SKIPPED));
        assert!(!error.contains("恢复 Codex Agent Role 投影失败"));
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex),
            Some(original.id.clone())
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current"),
            Some(original.id.clone())
        );
    }

    #[tokio::test]
    #[serial]
    async fn all_scope_rollback_removes_providers_created_inside_transaction() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let original = provider("provider-a", "Provider A");
        db.save_provider(AppType::Codex.as_str(), &original)
            .expect("seed original provider");

        let snapshot =
            capture_codex_provider_mutation(Arc::clone(&state), CodexProviderSnapshotScope::All)
                .await
                .expect("capture all providers");
        let added = provider("provider-b", "Provider B");
        db.save_provider(AppType::Codex.as_str(), &added)
            .expect("add provider during transaction");

        let errors = rollback_codex_provider_mutation(Arc::clone(&state), snapshot).await;
        assert!(errors.is_empty(), "rollback errors: {errors:?}");
        assert!(db
            .get_provider_by_id(&added.id, AppType::Codex.as_str())
            .expect("read added provider")
            .is_none());
        assert!(db
            .get_provider_by_id(&original.id, AppType::Codex.as_str())
            .expect("read original provider")
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn codex_mutations_are_serialized_across_snapshot_operation_and_reconcile() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let first_state = owned_app_state(state.as_ref());
        let first_active = Arc::clone(&active);
        let first_max = Arc::clone(&max_active);
        let first = execute_provider_mutation(
            first_state,
            AppType::Codex,
            Vec::new(),
            "first mutation",
            move || {
                let active_now = first_active.fetch_add(1, Ordering::SeqCst) + 1;
                first_max.fetch_max(active_now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(80));
                first_active.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, AppError>(true)
            },
        );

        let second_state = owned_app_state(state.as_ref());
        let second_active = Arc::clone(&active);
        let second_max = Arc::clone(&max_active);
        let second = execute_provider_mutation(
            second_state,
            AppType::Codex,
            Vec::new(),
            "second mutation",
            move || {
                let active_now = second_active.fetch_add(1, Ordering::SeqCst) + 1;
                second_max.fetch_max(active_now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(80));
                second_active.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, AppError>(true)
            },
        );

        let (first_result, second_result) = tokio::join!(first, second);
        assert!(first_result.is_ok(), "first mutation: {first_result:?}");
        assert!(second_result.is_ok(), "second mutation: {second_result:?}");
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn cancelled_caller_does_not_release_codex_mutation_locks_early() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db));
        let active = Arc::new(AtomicUsize::new(0));

        let mutation_state = Arc::clone(&state);
        let mutation_active = Arc::clone(&active);
        let caller = tokio::spawn(execute_provider_mutation(
            mutation_state,
            AppType::Codex,
            Vec::new(),
            "cancellation-safe mutation",
            move || {
                mutation_active.store(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(200));
                mutation_active.store(0, Ordering::SeqCst);
                Ok::<_, AppError>(true)
            },
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Codex mutation should reach its blocking operation");

        caller.abort();
        let _ = caller.await;

        assert!(
            tokio::time::timeout(
                Duration::from_millis(40),
                state.lock_codex_provider_lifecycle(),
            )
            .await
            .is_err(),
            "the supervised transaction must retain the lifecycle lock after caller cancellation"
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            while active.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the supervised mutation should finish");

        tokio::time::timeout(
            Duration::from_secs(2),
            state.lock_codex_provider_lifecycle(),
        )
        .await
        .expect("the lifecycle lock should be released after the mutation completes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn cancelled_caller_does_not_release_codex_switch_locks_early() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db));
        let active = Arc::new(AtomicUsize::new(0));

        let switch_state = Arc::clone(&state);
        let switch_active = Arc::clone(&active);
        let caller = tokio::spawn(execute_codex_switch_transaction_with_setup(
            switch_state,
            "provider-b".to_string(),
            "cancellation-safe switch",
            |_| async { Ok(()) },
            move || {
                switch_active.store(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(200));
                switch_active.store(0, Ordering::SeqCst);
                Ok::<_, AppError>(SwitchResult::default())
            },
        ));

        tokio::time::timeout(Duration::from_secs(2), async {
            while active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Codex switch should reach its blocking operation");

        caller.abort();
        let _ = caller.await;

        assert!(
            tokio::time::timeout(
                Duration::from_millis(40),
                state.lock_codex_provider_lifecycle(),
            )
            .await
            .is_err(),
            "the supervised switch must retain the lifecycle lock after caller cancellation"
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            while active.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the supervised switch should finish");

        tokio::time::timeout(
            Duration::from_secs(2),
            state.lock_codex_provider_lifecycle(),
        )
        .await
        .expect("the lifecycle lock should be released after the switch completes");
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial]
    async fn non_codex_async_mutation_uses_blocking_worker() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db));
        let runtime_thread = thread::current().id();
        let worker_thread = execute_provider_mutation(
            state,
            AppType::Claude,
            Vec::new(),
            "Claude mutation",
            || Ok::<_, AppError>(thread::current().id()),
        )
        .await
        .expect("non-Codex mutation succeeds");

        assert_ne!(runtime_thread, worker_thread);
    }

    #[tokio::test]
    #[serial]
    async fn rollback_reports_codex_agent_role_restore_failure() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let original = provider("provider-a", "Provider A");
        db.save_provider(AppType::Codex.as_str(), &original)
            .expect("seed provider");
        db.set_current_provider(AppType::Codex.as_str(), &original.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&original.id))
            .expect("set device current provider");

        let snapshot = capture_codex_provider_mutation(
            Arc::clone(&state),
            CodexProviderSnapshotScope::Selected(vec![original.id.clone()]),
        )
        .await
        .expect("capture provider mutation");
        let paths = crate::services::codex_agent_roles::CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::create_dir(&paths.frontend).expect("create conflicting role directory");

        let errors = rollback_codex_provider_mutation(Arc::clone(&state), snapshot).await;
        assert!(
            errors
                .iter()
                .any(|error| error.contains("恢复 Codex Agent Role 投影失败")),
            "unexpected rollback errors: {errors:?}"
        );
    }
}

#[cfg(test)]
mod import_claude_desktop_tests {
    use super::suggested_claude_desktop_routes;
    use crate::provider::{Provider, ProviderMeta};
    use serde_json::json;

    fn make_provider(env: serde_json::Value, provider_type: Option<&str>) -> Provider {
        let mut p = Provider::with_id(
            "test-claude".to_string(),
            "Test".to_string(),
            json!({ "env": env }),
            None,
        );
        if let Some(pt) = provider_type {
            p.meta = Some(ProviderMeta {
                provider_type: Some(pt.to_string()),
                ..ProviderMeta::default()
            });
        }
        p
    }

    #[test]
    fn route_strips_1m_suffix_and_sets_supports_1m() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-5-20250929[1M]",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "claude-sonnet-4-5-20250929");
        assert!(
            !r.model.to_ascii_lowercase().contains("[1m]"),
            "model must not contain [1m] suffix"
        );
        assert_eq!(r.label_override, None);
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn route_preserves_model_without_suffix() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "kimi-k2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "kimi-k2");
        assert_eq!(r.label_override.as_deref(), Some("kimi-k2"));
        // 默认 provider_type 缺省 → supports_1m_default = true
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn route_uses_claude_code_model_name_as_label_override() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "kimi-k2",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Kimi K2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "kimi-k2");
        assert_eq!(r.label_override.as_deref(), Some("Kimi K2"));
    }

    #[test]
    fn route_1m_suffix_overrides_provider_type_default() {
        // github_copilot 默认 supports_1m_default = false，但 [1M] 后缀应强制 true
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5-codex[1M]",
            }),
            Some("github_copilot"),
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "gpt-5-codex");
        assert_eq!(r.label_override.as_deref(), Some("gpt-5-codex"));
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn route_github_copilot_without_suffix_keeps_false() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5-codex",
            }),
            Some("github_copilot"),
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        let r = routes.get("claude-sonnet-5").expect("sonnet route present");
        assert_eq!(r.model, "gpt-5-codex");
        assert_eq!(r.label_override.as_deref(), Some("gpt-5-codex"));
        assert_eq!(r.supports_1m, Some(false));
    }

    #[test]
    fn same_upstream_across_three_aliases_merges_to_one_route() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "MiniMax-M2",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "MiniMax-M2",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "MiniMax-M2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 1, "three aliases → one merged route");
        let r = routes.get("claude-sonnet-5").expect("merged route present");
        assert_eq!(r.model, "MiniMax-M2");
        assert_eq!(r.label_override.as_deref(), Some("MiniMax-M2"));
    }

    #[test]
    fn same_upstream_with_partial_1m_marker_takes_or_aggregation() {
        // sonnet 带 [1M]，opus/haiku 不带 → 合并后 supports_1m == Some(true)
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "MiniMax-M2[1M]",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "MiniMax-M2",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "MiniMax-M2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 1);
        let r = routes.get("claude-sonnet-5").expect("merged route present");
        assert_eq!(r.supports_1m, Some(true));
    }

    #[test]
    fn different_upstream_models_produce_separate_routes() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "GLM-4.6",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "GLM-4-Air",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "GLM-4-Flash",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 3);
        assert_eq!(routes.get("claude-sonnet-5").unwrap().model, "GLM-4.6");
        assert_eq!(routes.get("claude-opus-4-8").unwrap().model, "GLM-4-Air");
        assert_eq!(routes.get("claude-haiku-4-5").unwrap().model, "GLM-4-Flash");
        assert_eq!(
            routes
                .get("claude-sonnet-5")
                .unwrap()
                .label_override
                .as_deref(),
            Some("GLM-4.6")
        );
    }

    #[test]
    fn anthropic_model_fallback_only_triggers_when_empty() {
        // 三个 default env_key 都不填，仅 ANTHROPIC_MODEL
        let p = make_provider(
            json!({
                "ANTHROPIC_MODEL": "kimi-k2",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert_eq!(routes.len(), 1);
        let r = routes
            .get("claude-sonnet-5")
            .expect("fallback route present");
        assert_eq!(r.model, "kimi-k2");
        assert_eq!(r.label_override.as_deref(), Some("kimi-k2"));
    }

    #[test]
    fn existing_claude_prefix_not_duplicated() {
        let p = make_provider(
            json!({
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-5-20250929",
            }),
            None,
        );
        let routes = suggested_claude_desktop_routes(&p).expect("routes built");
        assert!(routes.contains_key("claude-sonnet-5"));
        assert!(!routes.contains_key("claude-claude-sonnet-4-5-20250929"));
        assert_eq!(
            routes.get("claude-sonnet-5").expect("route").label_override,
            None
        );
    }
}

#[cfg(test)]
mod native_query_credentials_tests {
    use super::{resolve_coding_plan_credentials, resolve_native_credentials};
    use crate::app_config::AppType;
    use crate::provider::{Provider, UsageScript};
    use serde_json::json;

    fn usage_script(
        coding_plan_provider: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> UsageScript {
        UsageScript {
            enabled: true,
            language: "javascript".to_string(),
            code: String::new(),
            timeout: Some(10),
            api_key: api_key.map(str::to_string),
            base_url: base_url.map(str::to_string),
            access_token: None,
            user_id: None,
            template_type: Some("token_plan".to_string()),
            auto_query_interval: None,
            coding_plan_provider: coding_plan_provider.map(str::to_string),
            access_key_id: None,
            secret_access_key: None,
            team_organization_id: None,
            team_project_id: None,
        }
    }

    #[test]
    fn delegates_to_provider_for_codex() {
        let provider = Provider::with_id(
            "test".to_string(),
            "Test".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "sk-codex" },
                "config": "model_provider = \"deepseek\"\n\
                           [model_providers.deepseek]\n\
                           base_url = \"https://api.deepseek.com\"\n",
            }),
            None,
        );
        let (base_url, api_key) = resolve_native_credentials(&AppType::Codex, Some(&provider));
        assert_eq!(base_url, "https://api.deepseek.com");
        assert_eq!(api_key, "sk-codex");
    }

    #[test]
    fn missing_provider_yields_empty() {
        let (base_url, api_key) = resolve_native_credentials(&AppType::Codex, None);
        assert!(base_url.is_empty());
        assert!(api_key.is_empty());
    }

    #[test]
    fn zenmux_coding_plan_uses_script_credentials_first() {
        let provider = Provider::with_id(
            "test".to_string(),
            "Test".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://provider.zenmux.example/v1",
                    "ANTHROPIC_AUTH_TOKEN": "sk-provider"
                }
            }),
            None,
        );
        let script = usage_script(
            Some("zenmux"),
            Some("https://script.zenmux.example/api/usage/"),
            Some("sk-script"),
        );

        let (base_url, api_key) =
            resolve_coding_plan_credentials(&AppType::Claude, Some(&provider), Some(&script));

        assert_eq!(base_url, "https://script.zenmux.example/api/usage");
        assert_eq!(api_key, "sk-script");
    }

    #[test]
    fn zenmux_coding_plan_falls_back_to_provider_credentials() {
        let provider = Provider::with_id(
            "test".to_string(),
            "Test".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://provider.zenmux.example/v1",
                    "ANTHROPIC_AUTH_TOKEN": "sk-provider"
                }
            }),
            None,
        );
        let script = usage_script(Some("zenmux"), Some("https://script.zenmux.example"), None);

        let (base_url, api_key) =
            resolve_coding_plan_credentials(&AppType::Claude, Some(&provider), Some(&script));

        assert_eq!(base_url, "https://provider.zenmux.example/v1");
        assert_eq!(api_key, "sk-provider");
    }
}
