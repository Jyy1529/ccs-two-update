//! 故障转移切换模块
//!
//! 处理故障转移成功后的供应商切换逻辑，包括：
//! - 去重控制（避免多个请求同时触发）
//! - 托盘菜单更新
//! - 前端事件发射

use crate::database::Database;
use crate::error::AppError;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::RwLock;

struct PendingSwitchGuard {
    pending_switches: Arc<RwLock<HashSet<String>>>,
    switch_key: String,
}

impl Drop for PendingSwitchGuard {
    fn drop(&mut self) {
        let pending_switches = Arc::clone(&self.pending_switches);
        let switch_key = self.switch_key.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pending_switches.write().await.remove(&switch_key);
            });
        }
    }
}

/// 故障转移切换管理器
///
/// 负责处理故障转移成功后的供应商切换，确保 UI 能够直观反映当前使用的供应商。
#[derive(Clone)]
pub struct FailoverSwitchManager {
    /// 正在处理中的切换（key = app_type）
    pending_switches: Arc<RwLock<HashSet<String>>>,
    db: Arc<Database>,
}

impl FailoverSwitchManager {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            pending_switches: Arc::new(RwLock::new(HashSet::new())),
            db,
        }
    }

    /// 尝试执行故障转移切换
    ///
    /// 如果相同的切换已在进行中，则跳过；否则执行切换逻辑。
    ///
    /// # Returns
    /// - `Ok(true)` - 切换成功执行
    /// - `Ok(false)` - 切换已在进行中，跳过
    /// - `Err(e)` - 切换过程中发生错误
    pub async fn try_switch(
        &self,
        app_handle: Option<&tauri::AppHandle>,
        app_type: &str,
        expected_provider_id: &str,
        provider_id: &str,
        provider_name: &str,
    ) -> Result<bool, AppError> {
        let switch_key = app_type.to_string();

        // 去重检查：如果相同切换已在进行中，跳过
        {
            let mut pending = self.pending_switches.write().await;
            if pending.contains(&switch_key) {
                log::debug!("[Failover] 切换已在进行中，跳过: {app_type} -> {provider_id}");
                return Ok(false);
            }
            pending.insert(switch_key.clone());
        }

        // 独立任务持有 pending 标记和完整事务。调用方 future 被取消时，
        // 已开始的切换仍会运行到提交或回滚，不会留下半完成状态。
        let manager = self.clone();
        let app_handle = app_handle.cloned();
        let app_type = app_type.to_string();
        let expected_provider_id = expected_provider_id.to_string();
        let provider_id = provider_id.to_string();
        let provider_name = provider_name.to_string();
        tokio::spawn(async move {
            let _pending_guard = PendingSwitchGuard {
                pending_switches: Arc::clone(&manager.pending_switches),
                switch_key,
            };
            manager
                .do_switch(
                    app_handle.as_ref(),
                    &app_type,
                    &expected_provider_id,
                    &provider_id,
                    &provider_name,
                )
                .await
        })
        .await
        .map_err(|error| AppError::Message(format!("故障转移切换任务失败: {error}")))?
    }

    async fn do_switch(
        &self,
        app_handle: Option<&tauri::AppHandle>,
        app_type: &str,
        expected_provider_id: &str,
        provider_id: &str,
        provider_name: &str,
    ) -> Result<bool, AppError> {
        // 检查该应用是否已被代理接管（enabled=true）
        // 只有被接管的应用才允许执行故障转移切换
        let app_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.enabled,
            Err(e) => {
                log::warn!("[FO-002] 无法读取 {app_type} 配置: {e}，跳过切换");
                return Ok(false);
            }
        };

        if !app_enabled {
            log::debug!("[Failover] {app_type} 未启用代理，跳过切换");
            return Ok(false);
        }

        log::info!("[FO-001] 切换: {app_type} → {provider_name}");

        let mut switched = false;

        if let Some(app) = app_handle {
            if let Some(app_state) = app.try_state::<crate::store::AppState>() {
                if app_type == "codex" {
                    let _provider_lifecycle_guard = app_state.lock_codex_provider_lifecycle().await;
                    let _proxy_transaction_guard = app_state.proxy_service.lock_transaction().await;
                    if !current_provider_matches(app_state.inner(), app_type, expected_provider_id)?
                    {
                        log::info!(
                            "[Failover] 丢弃过期切换: app={app_type}, expected={expected_provider_id}, target={provider_id}"
                        );
                        return Ok(false);
                    }
                    let proxy_snapshot = app_state
                        .proxy_service
                        .snapshot_transaction_state()
                        .await
                        .map_err(AppError::Message)?;
                    let previous_local_provider =
                        crate::settings::get_current_provider(&crate::app_config::AppType::Codex);
                    let previous_database_provider = app_state
                        .db
                        .get_current_provider(crate::app_config::AppType::Codex.as_str())?;

                    switched = {
                        let _switch_guard =
                            app_state.proxy_service.lock_switch_for_app(app_type).await;
                        app_state
                            .proxy_service
                            .hot_switch_provider_inner(app_type, provider_id)
                            .await
                            .map_err(AppError::Message)?
                            .logical_target_changed
                    };

                    if !switched {
                        return Ok(false);
                    }

                    if let Err(error) = crate::services::codex_agent_roles::
                        reconcile_current_codex_agent_roles_under_proxy_transaction(
                            app_state.inner(),
                        )
                        .await
                    {
                        let mut rollback_errors = Vec::new();
                        rollback_errors.extend(
                            app_state
                                .proxy_service
                                .restore_transaction_state(&proxy_snapshot)
                                .await,
                        );
                        rollback_errors.extend(restore_raw_codex_current_pointers(
                            app_state.inner(),
                            previous_local_provider.as_deref(),
                            previous_database_provider.as_deref(),
                        ));
                        if let Err(rollback_error) = crate::services::codex_agent_roles::
                            reconcile_current_codex_agent_roles_under_proxy_transaction(
                                app_state.inner(),
                            )
                            .await
                        {
                            rollback_errors.push(format!(
                                "恢复 Codex Agent Role 失败: {rollback_error}"
                            ));
                        }
                        let suffix = if rollback_errors.is_empty() {
                            String::new()
                        } else {
                            format!("; 回滚错误: {}", rollback_errors.join("; "))
                        };
                        return Err(AppError::Message(format!(
                            "同步 Codex Agent Role 失败: {error}{suffix}"
                        )));
                    }
                } else {
                    let _switch_guard = app_state.proxy_service.lock_switch_for_app(app_type).await;
                    if !current_provider_matches(app_state.inner(), app_type, expected_provider_id)?
                    {
                        log::info!(
                            "[Failover] 丢弃过期切换: app={app_type}, expected={expected_provider_id}, target={provider_id}"
                        );
                        return Ok(false);
                    }
                    switched = app_state
                        .proxy_service
                        .hot_switch_provider_inner(app_type, provider_id)
                        .await
                        .map_err(AppError::Message)?
                        .logical_target_changed;

                    if !switched {
                        return Ok(false);
                    }
                }

                if let Ok(new_menu) = crate::tray::create_tray_menu(app, app_state.inner()) {
                    if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
                        if let Err(e) = tray.set_menu(Some(new_menu)) {
                            log::error!("[Failover] 更新托盘菜单失败: {e}");
                        }
                    }
                }
            }

            // 发射事件到前端
            let event_data = serde_json::json!({
                "appType": app_type,
                "providerId": provider_id,
                "source": "failover"  // 标识来源是故障转移
            });
            if let Err(e) = app.emit("provider-switched", event_data) {
                log::error!("[Failover] 发射事件失败: {e}");
            }
        }

        Ok(switched)
    }
}

fn current_provider_matches(
    state: &crate::store::AppState,
    app_type: &str,
    expected_provider_id: &str,
) -> Result<bool, AppError> {
    let app = crate::app_config::AppType::from_str(app_type)?;
    let current = crate::settings::get_effective_current_provider(&state.db, &app)?;
    let expected = (!expected_provider_id.trim().is_empty()).then_some(expected_provider_id);
    Ok(current.as_deref() == expected)
}

fn restore_raw_codex_current_pointers(
    state: &crate::store::AppState,
    local_provider_id: Option<&str>,
    database_provider_id: Option<&str>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = state.db.set_current_provider(
        crate::app_config::AppType::Codex.as_str(),
        database_provider_id.unwrap_or(""),
    ) {
        errors.push(format!("恢复数据库当前 Provider 失败: {error}"));
    }
    if let Err(error) =
        crate::settings::set_current_provider(&crate::app_config::AppType::Codex, local_provider_id)
    {
        errors.push(format!("恢复设备当前 Provider 失败: {error}"));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::{current_provider_matches, restore_raw_codex_current_pointers};
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::provider::Provider;
    use crate::store::AppState;
    use serde_json::json;
    use serial_test::serial;
    use std::ffi::OsString;
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

    fn provider(id: &str) -> Provider {
        Provider::with_id(id.to_string(), id.to_string(), json!({}), None)
    }

    #[test]
    #[serial]
    fn expected_current_compare_and_swap_rejects_a_stale_failover() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        for id in ["provider-a", "provider-c"] {
            db.save_provider(AppType::Codex.as_str(), &provider(id))
                .expect("save provider");
        }
        db.set_current_provider(AppType::Codex.as_str(), "provider-a")
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some("provider-a"))
            .expect("set local current");
        assert!(current_provider_matches(&state, "codex", "provider-a")
            .expect("compare matching current"));

        crate::settings::set_current_provider(&AppType::Codex, Some("provider-c"))
            .expect("simulate newer manual switch");
        assert!(!current_provider_matches(&state, "codex", "provider-a")
            .expect("compare stale current"));
    }

    #[test]
    #[serial]
    fn failover_rollback_restores_empty_and_split_raw_currents() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        for id in ["provider-b", "provider-local", "provider-database"] {
            db.save_provider(AppType::Codex.as_str(), &provider(id))
                .expect("save provider");
        }
        db.set_current_provider(AppType::Codex.as_str(), "provider-b")
            .expect("set database current");
        crate::settings::set_current_provider(&AppType::Codex, Some("provider-b"))
            .expect("set local current");

        assert!(restore_raw_codex_current_pointers(&state, None, None).is_empty());
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current"),
            None
        );
        assert_eq!(crate::settings::get_current_provider(&AppType::Codex), None);

        assert!(restore_raw_codex_current_pointers(
            &state,
            Some("provider-local"),
            Some("provider-database"),
        )
        .is_empty());
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read split database current")
                .as_deref(),
            Some("provider-database")
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some("provider-local")
        );
    }
}
