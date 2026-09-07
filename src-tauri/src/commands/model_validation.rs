//! Manual, non-mutating-to-clients model diagnostics. No application management
//! permission is required: unmanaged applications can still validate stored providers.

use crate::{
    services::model_validation::{
        fetch_validation_models, ModelValidationService, PrepareRequest, TargetInput,
        ValidationPlan, ValidationRun,
    },
    store::AppState,
};
use tauri::State;

#[tauri::command]
pub async fn fetch_model_validation_models(
    state: State<'_, AppState>,
    target: TargetInput,
) -> Result<Vec<crate::services::model_fetch::FetchedModel>, String> {
    fetch_validation_models(&state.db, target)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn prepare_model_validation(
    state: State<'_, AppState>,
    request: PrepareRequest,
) -> Result<ValidationPlan, String> {
    ModelValidationService::prepare(&state, request).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn start_model_validation(
    state: State<'_, AppState>,
    plan_id: String,
) -> Result<ValidationRun, String> {
    ModelValidationService::start(&state, &plan_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_model_validation(
    state: State<'_, AppState>,
    run_id: String,
) -> Result<ValidationRun, String> {
    ModelValidationService::get(&state, &run_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_model_validations(
    state: State<'_, AppState>,
    app_id: Option<String>,
    provider_id: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<ValidationRun>, String> {
    ModelValidationService::list(&state, app_id.as_deref(), provider_id.as_deref(), limit)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cancel_model_validation(
    state: State<'_, AppState>,
    run_id: String,
) -> Result<bool, String> {
    ModelValidationService::cancel(&state, &run_id).map_err(|e| e.to_string())
}
