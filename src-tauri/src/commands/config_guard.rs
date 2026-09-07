use crate::{app_config::AppType, services::config_guard::{self, GuardPreview, GuardState}};
use std::str::FromStr;

#[tauri::command]
pub fn get_config_guard_state(app_id: String) -> Result<GuardState, String> {
    config_guard::get_state(&AppType::from_str(&app_id).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
#[tauri::command]
pub fn set_config_protection(app_id: String, file_id: String, protected_paths: Vec<String>, protect_file: bool, expected_revision: String) -> Result<GuardState, String> {
    config_guard::set_protection(&AppType::from_str(&app_id).map_err(|e| e.to_string())?, &file_id, protected_paths, protect_file, &expected_revision).map_err(|e| e.to_string())
}
#[tauri::command]
pub fn preview_config_change(app_id: String, file_id: String) -> Result<GuardPreview, String> {
    config_guard::preview(&AppType::from_str(&app_id).map_err(|e| e.to_string())?, &file_id).map_err(|e| e.to_string())
}
#[tauri::command]
pub fn preview_config_restore(app_id: String, backup_id: String) -> Result<GuardPreview, String> {
    config_guard::preview_restore(&AppType::from_str(&app_id).map_err(|e| e.to_string())?, &backup_id).map_err(|e| e.to_string())
}
#[tauri::command]
pub async fn apply_config_change(preview_id: String, resolution: String, state: tauri::State<'_, crate::store::AppState>) -> Result<GuardState, String> {
    let state = state.owned_clone();
    tokio::task::spawn_blocking(move || config_guard::apply(&preview_id, &resolution, Some(&state)).map_err(|e| e.to_string()))
        .await.map_err(|e| e.to_string())?
}
