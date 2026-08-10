//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct ProviderRouteAttempt {
    pub provider: Provider,
    pub outbound_model_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderRoutePlan {
    pub attempts: Vec<ProviderRouteAttempt>,
    pub use_failover_timeouts: bool,
    pub sync_logical_target: bool,
    pub bypass_single_provider_circuit_breaker: bool,
}

impl ProviderRoutePlan {
    pub fn standard(providers: Vec<Provider>, use_failover_timeouts: bool) -> Self {
        Self {
            attempts: providers
                .into_iter()
                .map(|provider| ProviderRouteAttempt {
                    provider,
                    outbound_model_override: None,
                })
                .collect(),
            use_failover_timeouts,
            sync_logical_target: true,
            bypass_single_provider_circuit_breaker: true,
        }
    }

    pub fn providers(&self) -> Vec<Provider> {
        self.attempts
            .iter()
            .map(|attempt| attempt.provider.clone())
            .collect()
    }
}

/// Releases an acquired HalfOpen probe slot if an in-flight request future is dropped.
pub struct ProviderRequestPermit {
    allowed: bool,
    breaker: Option<Arc<CircuitBreaker>>,
}

impl ProviderRequestPermit {
    pub fn allowed(&self) -> bool {
        self.allowed
    }

    pub fn used_half_open_permit(&self) -> bool {
        self.breaker.is_some()
    }

    pub fn into_used_half_open_permit(mut self) -> bool {
        self.breaker.take().is_some()
    }
}

impl Drop for ProviderRequestPermit {
    fn drop(&mut self) {
        if let Some(breaker) = self.breaker.take() {
            breaker.release_half_open_permit();
        }
    }
}

/// 供应商路由器
pub struct ProviderRouter {
    /// 数据库连接
    db: Arc<Database>,
    /// 熔断器管理器 - key 格式: "app_type:provider_id"
    circuit_breakers: Arc<RwLock<HashMap<String, Arc<CircuitBreaker>>>>,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 选择可用的供应商（支持故障转移）
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：仅使用故障转移队列，按队列顺序依次尝试（P1 → P2 → ...）
    pub async fn select_providers(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled {
            // 故障转移开启：仅按队列顺序依次尝试（P1 → P2 → ...）
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致
            let ordered_ids: Vec<String> = self
                .db
                .get_failover_queue(app_type)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();

            total_providers = ordered_ids.len();

            for provider_id in ordered_ids {
                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };

                let circuit_key = format!("{app_type}:{}", provider.id);
                let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

                if breaker.is_available().await {
                    result.push(provider);
                } else {
                    circuit_open_count += 1;
                }
            }
        } else {
            // 故障转移关闭：仅使用当前供应商，跳过熔断器检查
            let current_id = AppType::from_str(app_type)
                .ok()
                .and_then(|app_enum| {
                    crate::settings::get_effective_current_provider(&self.db, &app_enum)
                        .ok()
                        .flatten()
                })
                .or_else(|| self.db.get_current_provider(app_type).ok().flatten());

            if let Some(current_id) = current_id {
                if let Some(current) = self.db.get_provider_by_id(&current_id, app_type)? {
                    total_providers = 1;
                    result.push(current);
                }
            }
        }

        if result.is_empty() {
            if total_providers > 0 && circuit_open_count == total_providers {
                log::warn!("[{app_type}] [FO-004] 所有供应商均已熔断");
                return Err(AppError::AllProvidersCircuitOpen);
            } else {
                log::warn!("[{app_type}] [FO-005] 未配置供应商");
                return Err(AppError::NoProvidersConfigured);
            }
        }

        Ok(result)
    }

    pub async fn select_codex_frontend_route_plan(
        &self,
        owner_provider_id: &str,
        request_model: &str,
        route_token: &str,
    ) -> Result<Option<ProviderRoutePlan>, AppError> {
        const APP_TYPE: &str = "codex";

        let owner = self
            .db
            .get_provider_by_id(owner_provider_id, APP_TYPE)?
            .ok_or_else(|| {
                AppError::InvalidInput(format!(
                    "Codex role route owner is unavailable: {owner_provider_id}"
                ))
            })?;

        let routing = owner
            .meta
            .as_ref()
            .and_then(|meta| meta.codex_agent_role_routing.as_ref())
            .filter(|routing| routing.is_enabled())
            .ok_or_else(|| {
                AppError::InvalidInput(format!(
                    "Codex role routing is unavailable or disabled for owner: {owner_provider_id}"
                ))
            })?;
        if !crate::services::codex_agent_roles::verify_codex_role_route_token(
            owner_provider_id,
            crate::services::codex_agent_roles::FRONTEND_ROLE_ROUTE_VALUE,
            routing,
            route_token,
        ) {
            return Err(AppError::InvalidInput(
                "Invalid or stale Codex role route token".to_string(),
            ));
        }
        let frontend = routing.frontend.as_ref();
        let requested_target_id = frontend
            .and_then(|frontend| frontend.provider_id.as_deref())
            .map(str::trim)
            .filter(|provider_id| !provider_id.is_empty());
        let explicit_upstream_model = frontend
            .and_then(|frontend| frontend.upstream_model.as_deref())
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(ToString::to_string);
        let owner_id = owner.id.clone();

        let target = match requested_target_id {
            Some(provider_id) if provider_id != owner.id => {
                match self.db.get_provider_by_id(provider_id, APP_TYPE)? {
                    Some(provider) => Some(provider),
                    None => {
                        log::warn!(
                            "[CodexRoleRoute] frontend provider {} referenced by owner {} is unavailable; using owner provider",
                            provider_id,
                            owner.id
                        );
                        None
                    }
                }
            }
            _ => Some(owner.clone()),
        };

        let mut attempts = Vec::with_capacity(2);
        if let Some(target) = target {
            if target.id == owner.id {
                attempts.push(ProviderRouteAttempt {
                    outbound_model_override: explicit_upstream_model
                        .or_else(|| provider_default_or_request(&owner, request_model)),
                    provider: owner,
                });
            } else {
                attempts.push(ProviderRouteAttempt {
                    outbound_model_override: explicit_upstream_model
                        .or_else(|| provider_default_or_request(&target, request_model)),
                    provider: target,
                });
                attempts.push(ProviderRouteAttempt {
                    outbound_model_override: provider_default_or_request(&owner, request_model),
                    provider: owner,
                });
            }
        } else {
            attempts.push(ProviderRouteAttempt {
                outbound_model_override: provider_default_or_request(&owner, request_model),
                provider: owner,
            });
        }

        let chain = attempts
            .iter()
            .map(|attempt| {
                format!(
                    "{}:{}",
                    attempt.provider.id,
                    attempt
                        .outbound_model_override
                        .as_deref()
                        .unwrap_or(request_model)
                )
            })
            .collect::<Vec<_>>()
            .join(" -> ");
        log::info!(
            "[CodexRoleRoute] role=frontend owner={} capability_model={} chain={}",
            owner_id,
            request_model,
            chain
        );

        Ok(Some(ProviderRoutePlan {
            attempts,
            use_failover_timeouts: true,
            sync_logical_target: false,
            bypass_single_provider_circuit_breaker: false,
        }))
    }

    /// 请求执行前获取熔断器“放行许可”
    ///
    /// - Closed：直接放行
    /// - Open：超时到达后切到 HalfOpen 并放行一次探测
    /// - HalfOpen：按限流规则放行探测
    ///
    pub async fn allow_provider_request(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> ProviderRequestPermit {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        let result = breaker.allow_request().await;
        ProviderRequestPermit {
            allowed: result.allowed,
            breaker: result.used_half_open_permit.then_some(breaker),
        }
    }

    /// 记录供应商请求结果
    pub async fn record_result(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        // 1. 按应用独立获取熔断器配置
        let failure_threshold = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => app_config.circuit_failure_threshold,
            Err(_) => 5, // 默认值
        };

        // 2. 更新熔断器状态
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

        if success {
            breaker.record_success(used_half_open_permit).await;
        } else {
            breaker.record_failure(used_half_open_permit).await;
        }

        // 3. 更新数据库健康状态（使用配置的阈值）
        self.db
            .update_provider_health_with_threshold(
                provider_id,
                app_type,
                success,
                error_msg.clone(),
                failure_threshold,
            )
            .await?;

        Ok(())
    }

    /// 重置熔断器（手动恢复）
    pub async fn reset_circuit_breaker(&self, circuit_key: &str) {
        let breakers = self.circuit_breakers.read().await;
        if let Some(breaker) = breakers.get(circuit_key) {
            breaker.reset().await;
        }
    }

    /// 重置指定供应商的熔断器
    pub async fn reset_provider_breaker(&self, provider_id: &str, app_type: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.reset_circuit_breaker(&circuit_key).await;
    }

    /// 更新所有熔断器的配置（热更新）
    pub async fn update_all_configs(&self, config: CircuitBreakerConfig) {
        let breakers = self.circuit_breakers.read().await;
        for breaker in breakers.values() {
            breaker.update_config(config.clone()).await;
        }
    }

    /// 更新指定应用已创建熔断器的配置（热更新）
    pub async fn update_app_configs(&self, app_type: &str, config: CircuitBreakerConfig) {
        let prefix = format!("{app_type}:");
        let breakers = self.circuit_breakers.read().await;
        for (key, breaker) in breakers.iter() {
            if key.starts_with(&prefix) {
                breaker.update_config(config.clone()).await;
            }
        }
    }

    /// 获取熔断器状态
    #[allow(dead_code)]
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::circuit_breaker::CircuitBreakerStats> {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breakers = self.circuit_breakers.read().await;

        if let Some(breaker) = breakers.get(&circuit_key) {
            Some(breaker.get_stats().await)
        } else {
            None
        }
    }

    /// 获取或创建熔断器
    async fn get_or_create_circuit_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // 先尝试读锁获取
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(breaker) = breakers.get(key) {
                return breaker.clone();
            }
        }

        // 如果不存在，获取写锁创建
        let mut breakers = self.circuit_breakers.write().await;

        // 双重检查，防止竞争条件
        if let Some(breaker) = breakers.get(key) {
            return breaker.clone();
        }

        // 从 key 中提取 app_type (格式: "app_type:provider_id")
        let app_type = key.split(':').next().unwrap_or("claude");

        // 按应用独立读取熔断器配置
        let config = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => crate::proxy::circuit_breaker::CircuitBreakerConfig {
                failure_threshold: app_config.circuit_failure_threshold,
                success_threshold: app_config.circuit_success_threshold,
                timeout_seconds: app_config.circuit_timeout_seconds as u64,
                error_rate_threshold: app_config.circuit_error_rate_threshold,
                min_requests: app_config.circuit_min_requests,
            },
            Err(_) => crate::proxy::circuit_breaker::CircuitBreakerConfig::default(),
        };

        let breaker = Arc::new(CircuitBreaker::new(config));
        breakers.insert(key.to_string(), breaker.clone());

        breaker
    }
}

fn provider_default_or_request(provider: &Provider, request_model: &str) -> Option<String> {
    crate::proxy::providers::codex_provider_upstream_model(provider).or_else(|| {
        let request_model = request_model.trim();
        (!request_model.is_empty()).then(|| request_model.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::{CodexAgentRoleRouting, CodexFrontendAgentRoleOverride, ProviderMeta};
    use crate::services::codex_agent_roles::{
        create_codex_role_route_token, FRONTEND_ROLE_ROUTE_VALUE,
    };
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");

            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
                None => env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    async fn setup_half_open_router() -> (TempHome, Arc<ProviderRouter>) {
        let home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider).unwrap();

        let router = Arc::new(ProviderRouter::new(db));
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        (home, router)
    }

    fn codex_role_owner(
        id: &str,
        frontend_provider_id: Option<&str>,
        upstream_model: Option<&str>,
    ) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            format!("Provider {id}"),
            json!({ "config": format!("model = \"{id}-default\"\n") }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(CodexAgentRoleRouting {
                enabled: Some(true),
                frontend: Some(CodexFrontendAgentRoleOverride {
                    provider_id: frontend_provider_id.map(ToString::to_string),
                    upstream_model: upstream_model.map(ToString::to_string),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        provider
    }

    fn role_route_token(owner: &Provider) -> String {
        let routing = owner
            .meta
            .as_ref()
            .and_then(|meta| meta.codex_agent_role_routing.as_ref())
            .expect("Codex role routing");
        create_codex_role_route_token(&owner.id, FRONTEND_ROLE_ROUTE_VALUE, routing)
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_is_fixed_to_b_then_owner_a() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let owner = codex_role_owner("a", Some("b"), Some("frontend-custom"));
        let provider_b = Provider::with_id(
            "b".to_string(),
            "Provider B".to_string(),
            json!({ "model": "b-default" }),
            None,
        );
        let provider_c = Provider::with_id(
            "c".to_string(),
            "Provider C".to_string(),
            json!({ "model": "c-default" }),
            None,
        );
        db.save_provider("codex", &owner).unwrap();
        db.save_provider("codex", &provider_b).unwrap();
        db.save_provider("codex", &provider_c).unwrap();
        db.set_current_provider("codex", "c").unwrap();
        db.add_to_failover_queue("codex", "c").unwrap();

        let router = ProviderRouter::new(db);
        let token = role_route_token(&owner);
        let plan = router
            .select_codex_frontend_route_plan("a", "client-capability-model", &token)
            .await
            .unwrap()
            .expect("enabled role route plan");

        assert_eq!(
            plan.attempts
                .iter()
                .map(|attempt| attempt.provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        assert_eq!(
            plan.attempts[0].outbound_model_override.as_deref(),
            Some("frontend-custom")
        );
        assert_eq!(
            plan.attempts[1].outbound_model_override.as_deref(),
            Some("a-default")
        );
        assert!(plan.use_failover_timeouts);
        assert!(!plan.sync_logical_target);
        assert!(!plan.bypass_single_provider_circuit_breaker);
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_deduplicates_owner_and_recovers_missing_target() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let owner = codex_role_owner("a", Some("missing"), Some("frontend-custom"));
        db.save_provider("codex", &owner).unwrap();

        let router = ProviderRouter::new(db);
        let token = role_route_token(&owner);
        let missing_plan = router
            .select_codex_frontend_route_plan("a", "client-model", &token)
            .await
            .unwrap()
            .expect("enabled role route plan");
        assert_eq!(missing_plan.attempts.len(), 1);
        assert_eq!(missing_plan.attempts[0].provider.id, "a");
        assert_eq!(
            missing_plan.attempts[0].outbound_model_override.as_deref(),
            Some("a-default")
        );

        let same_owner = codex_role_owner("a", Some("a"), Some("owner-custom"));
        router.db.save_provider("codex", &same_owner).unwrap();
        let token = role_route_token(&same_owner);
        let deduplicated = router
            .select_codex_frontend_route_plan("a", "client-model", &token)
            .await
            .unwrap()
            .expect("same-owner route plan");
        assert_eq!(deduplicated.attempts.len(), 1);
        assert_eq!(deduplicated.attempts[0].provider.id, "a");
        assert_eq!(
            deduplicated.attempts[0].outbound_model_override.as_deref(),
            Some("owner-custom")
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_rejects_disabled_owner_routing() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let mut owner = codex_role_owner("a", Some("b"), Some("frontend-custom"));
        owner
            .meta
            .as_mut()
            .and_then(|meta| meta.codex_agent_role_routing.as_mut())
            .expect("role routing")
            .enabled = Some(false);
        db.save_provider("codex", &owner).unwrap();

        let router = ProviderRouter::new(db);
        let token = role_route_token(&owner);
        assert!(matches!(
            router
                .select_codex_frontend_route_plan("a", "client-model", &token)
                .await,
            Err(AppError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn missing_role_owner_is_rejected() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let current = Provider::with_id(
            "current".to_string(),
            "Current".to_string(),
            json!({ "model": "current-default" }),
            None,
        );
        db.save_provider("codex", &current).unwrap();
        db.set_current_provider("codex", "current").unwrap();

        let router = ProviderRouter::new(db);
        let token = create_codex_role_route_token(
            "deleted-owner",
            FRONTEND_ROLE_ROUTE_VALUE,
            &CodexAgentRoleRouting::default(),
        );
        assert!(matches!(
            router
                .select_codex_frontend_route_plan("deleted-owner", "client-model", &token)
                .await,
            Err(AppError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_rejects_forged_token() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let owner = codex_role_owner("a", None, None);
        db.save_provider("codex", &owner).unwrap();

        let router = ProviderRouter::new(db);
        assert!(matches!(
            router
                .select_codex_frontend_route_plan("a", "client-model", "forged-token")
                .await,
            Err(AppError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_rejects_stale_token() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let owner = codex_role_owner("a", Some("b"), Some("initial-model"));
        let stale_token = role_route_token(&owner);
        db.save_provider("codex", &owner).unwrap();

        let changed_owner = codex_role_owner("a", Some("b"), Some("changed-model"));
        db.save_provider("codex", &changed_owner).unwrap();

        let router = ProviderRouter::new(db);
        assert!(matches!(
            router
                .select_codex_frontend_route_plan("a", "client-model", &stale_token)
                .await,
            Err(AppError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_rejects_token_for_another_owner() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let owner_a = codex_role_owner("a", None, None);
        let owner_b = codex_role_owner("b", None, None);
        let token_for_a = role_route_token(&owner_a);
        db.save_provider("codex", &owner_a).unwrap();
        db.save_provider("codex", &owner_b).unwrap();

        let router = ProviderRouter::new(db);
        assert!(matches!(
            router
                .select_codex_frontend_route_plan("b", "client-model", &token_for_a)
                .await,
            Err(AppError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_rejects_owner_without_routing() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let owner = Provider::with_id(
            "a".to_string(),
            "Provider A".to_string(),
            json!({ "model": "a-default" }),
            None,
        );
        db.save_provider("codex", &owner).unwrap();

        let router = ProviderRouter::new(db);
        assert!(matches!(
            router
                .select_codex_frontend_route_plan("a", "client-model", "any-token")
                .await,
            Err(AppError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_router_creation() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        let breaker = router.get_or_create_circuit_breaker("claude:test").await;
        assert!(breaker.allow_request().await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_disabled_uses_current_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_order_ignoring_current() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 设置 sort_index 来控制顺序：b=1, a=2
        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(2);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        db.add_to_failover_queue("claude", "b").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 2);
        // 故障转移开启时：仅按队列顺序选择（忽略当前供应商）
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[1].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_only_even_if_current_not_in_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        // 只把 b 加入故障转移队列（模拟“当前供应商不在队列里”的常见配置）
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
    }

    #[tokio::test]
    #[serial]
    async fn test_select_providers_does_not_consume_half_open_permit() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();

        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        let providers = router.select_providers("claude").await.unwrap();
        assert_eq!(providers.len(), 2);

        assert!(router.allow_provider_request("b", "claude").await.allowed());
    }

    #[tokio::test]
    #[serial]
    async fn dropping_provider_request_permit_frees_half_open_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 配置熔断器：1 次失败即熔断，0 秒超时立即进入 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 触发熔断：1 次失败
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 第一次请求：获取 HalfOpen 探测名额
        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed());
        assert!(first.used_half_open_permit());

        // 第二次请求应被拒绝（名额已被占用）
        let second = router.allow_provider_request("a", "claude").await;
        assert!(!second.allowed());

        // Dropping the guard releases the slot without changing health statistics.
        drop(first);

        // 第三次请求应被允许（名额已释放）
        let third = router.allow_provider_request("a", "claude").await;
        assert!(third.allowed());
        assert!(third.used_half_open_permit());
    }

    #[tokio::test]
    #[serial]
    async fn consuming_provider_request_permit_transfers_release_ownership_once() {
        let (_home, router) = setup_half_open_router().await;

        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed());
        assert!(first.used_half_open_permit());

        let used_half_open_permit = first.into_used_half_open_permit();
        assert!(used_half_open_permit);

        // Consuming the guard transfers release responsibility to record_result.
        let blocked_before_record = router.allow_provider_request("a", "claude").await;
        assert!(!blocked_before_record.allowed());

        router
            .record_result("a", "claude", used_half_open_permit, true, None)
            .await
            .unwrap();

        let second = router.allow_provider_request("a", "claude").await;
        assert!(second.allowed());
        assert!(second.used_half_open_permit());

        // No stale first guard remains that can release the second probe's slot.
        let third = router.allow_provider_request("a", "claude").await;
        assert!(!third.allowed());
    }

    #[tokio::test]
    #[serial]
    async fn detached_half_open_finalizer_completes_after_parent_abort() {
        let (_home, router) = setup_half_open_router().await;
        let permit = router.allow_provider_request("a", "claude").await;
        assert!(permit.allowed());
        assert!(permit.used_half_open_permit());

        let finalizer_started = Arc::new(tokio::sync::Notify::new());
        let allow_finalizer = Arc::new(tokio::sync::Notify::new());
        let finalizer_finished = Arc::new(tokio::sync::Notify::new());

        let parent = tokio::spawn({
            let router = router.clone();
            let finalizer_started = finalizer_started.clone();
            let allow_finalizer = allow_finalizer.clone();
            let finalizer_finished = finalizer_finished.clone();

            async move {
                let finalizer = tokio::spawn(async move {
                    finalizer_started.notify_one();
                    allow_finalizer.notified().await;

                    let used_half_open_permit = permit.into_used_half_open_permit();
                    router
                        .record_result("a", "claude", used_half_open_permit, true, None)
                        .await
                        .unwrap();
                    finalizer_finished.notify_one();
                });

                finalizer.await.unwrap();
            }
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            finalizer_started.notified(),
        )
        .await
        .expect("finalizer should start");

        parent.abort();
        let _ = parent.await;

        let blocked_while_finalizer_owns_permit =
            router.allow_provider_request("a", "claude").await;
        assert!(!blocked_while_finalizer_owns_permit.allowed());

        allow_finalizer.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            finalizer_finished.notified(),
        )
        .await
        .expect("detached finalizer should finish after parent abort");

        let next = router.allow_provider_request("a", "claude").await;
        assert!(next.allowed());
        assert!(next.used_half_open_permit());

        let blocked_by_next = router.allow_provider_request("a", "claude").await;
        assert!(!blocked_by_next.allowed());
    }
}
