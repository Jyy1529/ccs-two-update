//! Tauri commands for provider folders and key-pool settings.

use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider_groups::{
    BalanceQueryByCredentialsRequest, BalanceQueryResult, BalanceQueryTemplate, KeyPoolStrategy,
    ProviderGroup, ProviderGroupStatus,
};
use crate::services::provider_groups::ProviderGroupService;
use crate::store::AppState;
use std::str::FromStr;
use tauri::State;

fn canonical_provider_group_app(raw: &str) -> Result<AppType, AppError> {
    AppType::from_str(raw)
}

#[tauri::command]
pub fn list_provider_groups(
    state: State<'_, AppState>,
    app: String,
) -> Result<Vec<ProviderGroup>, String> {
    let app = canonical_provider_group_app(&app).map_err(|error| error.to_string())?;
    ProviderGroupService::list_groups(&state.db, app.as_str()).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn create_provider_group(
    state: State<'_, AppState>,
    group: ProviderGroup,
) -> Result<ProviderGroup, String> {
    ProviderGroupService::create_group(&state.db, group).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn update_provider_group(
    state: State<'_, AppState>,
    group: ProviderGroup,
) -> Result<ProviderGroup, String> {
    ProviderGroupService::update_group(&state.db, group.clone())
        .map_err(|error| error.to_string())?;
    if !group.key_pool_enabled {
        state
            .proxy_service
            .clear_key_pool_runtime(&group.app_type, &group.id)
            .await;
    }
    state
        .db
        .get_provider_group(&group.id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Provider group disappeared after update".to_string())
}

#[tauri::command]
pub async fn delete_provider_group(
    state: State<'_, AppState>,
    group_id: String,
) -> Result<(), String> {
    let group = state
        .db
        .get_provider_group(&group_id)
        .map_err(|error| error.to_string())?;
    ProviderGroupService::delete_group(&state.db, &group_id).map_err(|error| error.to_string())?;
    if let Some(group) = group {
        state
            .proxy_service
            .clear_key_pool_runtime(&group.app_type, &group.id)
            .await;
    }
    Ok(())
}

#[tauri::command]
pub fn move_provider_to_group(
    state: State<'_, AppState>,
    app: String,
    provider_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    ProviderGroupService::move_provider(&state.db, &app, &provider_id, group_id.as_deref())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn set_group_key_pool_policy(
    state: State<'_, AppState>,
    group_id: String,
    enabled: bool,
    strategy: KeyPoolStrategy,
    max_retries: u32,
    cooldown_ms: u64,
) -> Result<ProviderGroup, String> {
    let group = ProviderGroupService::update_policy(
        &state.db,
        &group_id,
        enabled,
        strategy,
        max_retries,
        cooldown_ms,
    )
    .map_err(|error| error.to_string())?;
    state
        .proxy_service
        .clear_key_pool_runtime(&group.app_type, &group.id)
        .await;
    Ok(group)
}

#[tauri::command]
pub fn set_provider_key_pool_enabled(
    state: State<'_, AppState>,
    app: String,
    provider_id: String,
    enabled: bool,
) -> Result<(), String> {
    ProviderGroupService::set_provider_key_pool_enabled(&state.db, &app, &provider_id, enabled)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn reorder_provider_groups(
    state: State<'_, AppState>,
    app: String,
    group_ids: Vec<String>,
) -> Result<(), String> {
    let app = canonical_provider_group_app(&app).map_err(|error| error.to_string())?;
    state
        .db
        .reorder_provider_groups(app.as_str(), &group_ids)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn reorder_provider_group_members(
    state: State<'_, AppState>,
    group_id: String,
    provider_ids: Vec<String>,
) -> Result<(), String> {
    state
        .db
        .reorder_provider_group_members(&group_id, &provider_ids)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_group_key_pool_status(
    state: State<'_, AppState>,
    group_id: String,
) -> Result<ProviderGroupStatus, String> {
    let mut status = ProviderGroupService::group_status(&state.db, &group_id)
        .map_err(|error| error.to_string())?;
    state.proxy_service.fill_key_pool_status(&mut status).await;
    Ok(status)
}

#[tauri::command]
pub fn set_provider_auto_grouping(
    state: State<'_, AppState>,
    app: String,
    enabled: bool,
) -> Result<Vec<ProviderGroup>, String> {
    ProviderGroupService::set_auto_grouping(&state.db, &app, enabled)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_provider_auto_grouping(state: State<'_, AppState>, app: String) -> Result<bool, String> {
    ProviderGroupService::auto_grouping_enabled(&state.db, &app).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn set_provider_balance_template(
    state: State<'_, AppState>,
    app: String,
    provider_id: String,
    template_id: Option<String>,
) -> Result<(), String> {
    let app = canonical_provider_group_app(&app).map_err(|error| error.to_string())?;
    state
        .db
        .set_provider_balance_template(app.as_str(), &provider_id, template_id.as_deref())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn list_balance_query_templates(
    state: State<'_, AppState>,
) -> Result<Vec<BalanceQueryTemplate>, String> {
    state
        .db
        .list_balance_query_templates()
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn save_balance_query_template(
    state: State<'_, AppState>,
    template: BalanceQueryTemplate,
) -> Result<(), String> {
    state
        .db
        .save_balance_query_template(&template)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn delete_balance_query_template(
    state: State<'_, AppState>,
    template_id: String,
) -> Result<bool, String> {
    state
        .db
        .delete_balance_query_template(&template_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn query_balance_by_credentials(
    request: BalanceQueryByCredentialsRequest,
) -> Result<crate::provider::UsageResult, String> {
    canonical_provider_group_app(&request.app_type).map_err(|error| error.to_string())?;
    crate::services::balance_query::query_balance_template(
        &request.template,
        &request.base_url,
        &request.api_key,
    )
    .await
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn query_provider_balance(
    state: State<'_, AppState>,
    provider_id: String,
    app: String,
) -> Result<BalanceQueryResult, String> {
    let app = canonical_provider_group_app(&app).map_err(|error| error.to_string())?;
    crate::services::provider_balance::query_provider(&state.db, &app, &provider_id)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn query_group_balances(
    state: State<'_, AppState>,
    group_id: String,
) -> Result<Vec<BalanceQueryResult>, String> {
    crate::services::provider_balance::query_group(&state.db, &group_id)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::canonical_provider_group_app;

    #[test]
    fn command_contract_rejects_unknown_app_before_database_access() {
        assert!(canonical_provider_group_app("not-an-app").is_err());
    }
}
