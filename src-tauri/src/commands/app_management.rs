use crate::{app_config::AppType, app_management, store::AppState};
use std::str::FromStr;
use tauri::State;

#[tauri::command]
pub fn get_app_management_state() -> Result<app_management::AppManagementState, String> {
    app_management::get_state().map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn preview_app_management_change(app_id: String, enabled: bool, state: State<'_, AppState>) -> Result<app_management::ManagementPlan, String> {
    app_management::preview_change(state.owned_clone(), AppType::from_str(&app_id).map_err(|e| e.to_string())?, enabled).await.map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn apply_app_management_change(plan_id: String, state: State<'_, AppState>) -> Result<app_management::AppManagementState, String> {
    app_management::apply_change(state.owned_clone(), plan_id).await.map_err(|e| e.to_string())
}
