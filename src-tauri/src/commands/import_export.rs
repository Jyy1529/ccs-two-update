#![allow(non_snake_case)]

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

use crate::commands::sync_support::{
    post_sync_warning_from_result, run_post_import_sync_under_proxy_transaction,
    success_payload_with_warning, sync_current_live_and_roles_under_proxy_transaction,
    with_codex_provider_transaction,
};
use crate::database::backup::BackupEntry;
use crate::database::Database;
use crate::error::AppError;
use crate::store::AppState;

// ─── File import/export ──────────────────────────────────────

/// 导出数据库为 SQL 备份
#[tauri::command]
pub async fn export_config_to_file(
    #[allow(non_snake_case)] filePath: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let target_path = PathBuf::from(&filePath);
        db.export_sql(&target_path)?;
        Ok::<_, AppError>(json!({
            "success": true,
            "message": "SQL exported successfully",
            "filePath": filePath
        }))
    })
    .await
    .map_err(|e| format!("导出配置失败: {e}"))?
    .map_err(|e: AppError| e.to_string())
}

/// 从 SQL 备份导入数据库
#[tauri::command]
pub async fn import_config_from_file(
    #[allow(non_snake_case)] filePath: String,
    state: State<'_, AppState>,
) -> Result<Value, String> {
    let app_state = state.inner().owned_clone();
    with_codex_provider_transaction(app_state, move |app_state| async move {
        let state_for_import = Arc::clone(&app_state);
        let backup_id = tauri::async_runtime::spawn_blocking(move || {
            let path_buf = PathBuf::from(&filePath);
            state_for_import.db.import_sql(&path_buf)
        })
        .await
        .map_err(|error| format!("导入配置失败: {error}"))?
        .map_err(|error| error.to_string())?;

        let warning = post_sync_warning_from_result(Ok(
            run_post_import_sync_under_proxy_transaction(app_state).await,
        ));
        if let Some(msg) = warning.as_ref() {
            log::warn!("[Import] post-import sync warning: {msg}");
        }
        Ok(success_payload_with_warning(backup_id, warning))
    })
    .await
}

#[tauri::command]
pub async fn sync_current_providers_live(state: State<'_, AppState>) -> Result<Value, String> {
    let app_state = state.inner().owned_clone();
    with_codex_provider_transaction(app_state, move |app_state| async move {
        sync_current_live_and_roles_under_proxy_transaction(app_state)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "success": true,
            "message": "Live configuration synchronized"
        }))
    })
    .await
}

// ─── File dialogs ────────────────────────────────────────────

/// 保存文件对话框
#[tauri::command]
pub async fn save_file_dialog<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    #[allow(non_snake_case)] defaultName: String,
) -> Result<Option<String>, String> {
    let dialog = app.dialog();
    let result = dialog
        .file()
        .add_filter("SQL", &["sql"])
        .set_file_name(&defaultName)
        .blocking_save_file();

    Ok(result.map(|p| p.to_string()))
}

/// 打开文件对话框
#[tauri::command]
pub async fn open_file_dialog<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<String>, String> {
    let dialog = app.dialog();
    let result = dialog
        .file()
        .add_filter("SQL", &["sql"])
        .blocking_pick_file();

    Ok(result.map(|p| p.to_string()))
}

/// 打开 ZIP 文件选择对话框
#[tauri::command]
pub async fn open_zip_file_dialog<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<String>, String> {
    let dialog = app.dialog();
    let result = dialog
        .file()
        .add_filter("ZIP / Skill", &["zip", "skill"])
        .blocking_pick_file();

    Ok(result.map(|p| p.to_string()))
}

// ─── Database backup management ─────────────────────────────

/// Manually create a database backup
#[tauri::command]
pub async fn create_db_backup(state: State<'_, AppState>) -> Result<String, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || match db.backup_database_file()? {
        Some(path) => Ok(path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()),
        None => Err(AppError::Config(
            "Database file not found, backup skipped".to_string(),
        )),
    })
    .await
    .map_err(|e| format!("Backup failed: {e}"))?
    .map_err(|e: AppError| e.to_string())
}

/// List all database backup files
#[tauri::command]
pub fn list_db_backups() -> Result<Vec<BackupEntry>, String> {
    Database::list_backups().map_err(|e| e.to_string())
}

/// Restore database from a backup file
#[tauri::command]
pub async fn restore_db_backup(
    state: State<'_, AppState>,
    filename: String,
) -> Result<String, String> {
    let app_state = state.inner().owned_clone();
    with_codex_provider_transaction(app_state, move |app_state| async move {
        let state_for_restore = Arc::clone(&app_state);
        let safety_id = tauri::async_runtime::spawn_blocking(move || {
            state_for_restore.db.restore_from_backup(&filename)
        })
        .await
        .map_err(|error| format!("Restore failed: {error}"))?
        .map_err(|error| error.to_string())?;

        if let Err(sync_error) =
            run_post_import_sync_under_proxy_transaction(Arc::clone(&app_state)).await
        {
            let rollback_errors =
                restore_safety_backup_after_sync_failure(app_state, &safety_id).await;
            let rollback_context = if rollback_errors.is_empty() {
                format!("database restored from safety backup {safety_id}")
            } else {
                format!("database rollback issues: {}", rollback_errors.join("; "))
            };
            return Err(format!(
                "Restore post-operation synchronization failed: {sync_error}; {rollback_context}"
            ));
        }

        Ok(safety_id)
    })
    .await
}

async fn restore_safety_backup_after_sync_failure(
    state: Arc<AppState>,
    safety_id: &str,
) -> Vec<String> {
    if safety_id.is_empty() {
        return vec!["no safety backup was created".to_string()];
    }

    let filename = format!("{safety_id}.db");
    let state_for_restore = Arc::clone(&state);
    let restore_result = tauri::async_runtime::spawn_blocking(move || {
        state_for_restore.db.restore_from_backup(&filename)
    })
    .await;

    match restore_result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return vec![format!("restore safety backup: {error}")],
        Err(error) => return vec![format!("join safety-backup restore task: {error}")],
    }

    match run_post_import_sync_under_proxy_transaction(state).await {
        Ok(()) => Vec::new(),
        Err(error) => vec![format!("resynchronize restored database: {error}")],
    }
}

/// Rename a database backup file
#[tauri::command]
pub fn rename_db_backup(
    #[allow(non_snake_case)] oldFilename: String,
    #[allow(non_snake_case)] newName: String,
) -> Result<String, String> {
    Database::rename_backup(&oldFilename, &newName).map_err(|e| e.to_string())
}

/// Delete a database backup file
#[tauri::command]
pub fn delete_db_backup(filename: String) -> Result<(), String> {
    Database::delete_backup(&filename).map_err(|e| e.to_string())
}
