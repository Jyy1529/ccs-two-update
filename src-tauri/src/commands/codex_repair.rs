#[tauri::command]
pub async fn get_codex_repair_status(
) -> Result<crate::services::codex_repair::CodexRepairStatus, String> {
    tauri::async_runtime::spawn_blocking(crate::services::codex_repair::detect)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn launch_codex_repair(
) -> Result<crate::services::codex_repair::CodexRepairLaunchResult, String> {
    tauri::async_runtime::spawn_blocking(crate::services::codex_repair::launch_repair)
        .await
        .map_err(|error| error.to_string())?
}
