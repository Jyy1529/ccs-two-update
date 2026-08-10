use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::services::provider::ProviderService;
use crate::settings;
use crate::store::AppState;

pub(crate) async fn with_codex_provider_transaction<T, F, Fut>(
    state: Arc<AppState>,
    operation: F,
) -> T
where
    F: FnOnce(Arc<AppState>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _codex_lifecycle_guard = state.lock_codex_provider_lifecycle().await;
    let _proxy_transaction_guard = state.proxy_service.lock_transaction().await;
    operation(state).await
}

pub(crate) async fn sync_current_live_and_roles_under_proxy_transaction(
    state: Arc<AppState>,
) -> Result<(), AppError> {
    let state_for_live = Arc::clone(&state);
    let live_result = tauri::async_runtime::spawn_blocking(move || {
        ProviderService::sync_current_to_live(state_for_live.as_ref())
    })
    .await
    .map_err(|error| AppError::Message(format!("Live sync task failed: {error}")))?;

    let role_result = crate::services::codex_agent_roles::
        reconcile_current_codex_agent_roles_under_proxy_transaction(state.as_ref())
        .await;

    match (live_result, role_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
        (Err(live_error), Err(role_error)) => Err(AppError::Message(format!(
            "Live configuration sync failed: {live_error}; Codex Agent Role reconciliation also failed: {role_error}"
        ))),
    }
}

pub(crate) async fn run_post_import_sync_under_proxy_transaction(
    state: Arc<AppState>,
) -> Result<(), AppError> {
    sync_current_live_and_roles_under_proxy_transaction(state).await?;
    settings::reload_settings()?;
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

pub(crate) fn post_sync_warning_from_result(
    result: Result<Result<(), AppError>, String>,
) -> Option<String> {
    match result {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(post_sync_warning(err)),
        Err(err) => Some(post_sync_warning(err)),
    }
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
    use super::{attach_warning, post_sync_warning_from_result, with_codex_provider_transaction};
    use crate::database::Database;
    use crate::store::AppState;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn post_sync_warning_from_result_returns_none_on_success() {
        let warning = post_sync_warning_from_result(Ok(Ok(())));
        assert!(warning.is_none());
    }

    #[test]
    fn post_sync_warning_from_result_returns_some_on_sync_error() {
        let warning =
            post_sync_warning_from_result(Ok(Err(crate::error::AppError::Config("boom".into()))));
        assert!(warning.is_some());
    }

    #[tokio::test]
    async fn post_sync_warning_from_result_returns_some_on_join_error() {
        let handle = tokio::spawn(async move {
            panic!("forced join error");
        });
        let join_err = handle.await.expect_err("task should panic");
        let warning = post_sync_warning_from_result(Err(join_err.to_string()));
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

    #[tokio::test]
    async fn provider_transaction_uses_the_shared_lifecycle_lock() {
        let state = Arc::new(AppState::new(Arc::new(
            Database::memory().expect("in-memory database"),
        )));
        let lifecycle_guard = state.lock_codex_provider_lifecycle().await;
        let task_state = state.owned_clone();
        let operation_started = Arc::new(AtomicBool::new(false));
        let task_operation_started = Arc::clone(&operation_started);

        let task = tokio::spawn(async move {
            with_codex_provider_transaction(task_state, move |_| async move {
                task_operation_started.store(true, Ordering::SeqCst);
            })
            .await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!operation_started.load(Ordering::SeqCst));
        drop(lifecycle_guard);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("transaction should continue after releasing the lifecycle lock")
            .expect("transaction task should complete");
        assert!(operation_started.load(Ordering::SeqCst));
    }
}
