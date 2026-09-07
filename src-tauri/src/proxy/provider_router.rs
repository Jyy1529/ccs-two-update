//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// Codex Official requests carry the selected account's native Authorization
/// header. Reusing that request against another account card would cross the
/// account boundary, so these cards must never participate in provider retry.
pub(crate) fn provider_supports_failover(app_type: &str, provider: &Provider) -> bool {
    app_type != AppType::Codex.as_str()
        || !crate::proxy::providers::is_codex_official_provider(provider)
}

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
    pub key_pool_retries: HashMap<String, u32>,
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
            key_pool_retries: HashMap::new(),
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
    /// Ephemeral key-pool cursor and cooldown state. Credentials are never
    /// stored here; members are addressed only by app/group/provider IDs.
    key_pool_runtime: Arc<Mutex<HashMap<String, KeyPoolRuntime>>>,
}

#[derive(Default)]
struct KeyPoolRuntime {
    next_index: usize,
    members: HashMap<String, KeyPoolMemberRuntime>,
}

#[derive(Default)]
struct KeyPoolMemberRuntime {
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    last_failure_at: Option<i64>,
    early_probe: bool,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
            key_pool_runtime: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Select a route plan, including an optional per-group key pool.
    pub async fn select_route_plan(&self, app_type: &str) -> Result<ProviderRoutePlan, AppError> {
        let auto_failover_enabled = self.auto_failover_enabled(app_type).await;
        if let Some((group, mut providers)) = self.select_key_pool_members(app_type).await? {
            let key_pool_retries = providers
                .iter()
                .map(|provider| (provider.id.clone(), group.key_pool_max_retries))
                .collect();
            // A key pool is the preferred route. When the app-level queue is
            // also enabled, retain its compatible non-pool fallbacks without
            // duplicating members already selected from the pool.
            if auto_failover_enabled {
                if let Ok(fallbacks) = self.select_providers_base(app_type).await {
                    let mut seen: HashSet<String> = providers
                        .iter()
                        .map(|provider| provider.id.clone())
                        .collect();
                    for provider in fallbacks {
                        if provider
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.provider_group_id.as_deref())
                            == Some(group.id.as_str())
                        {
                            continue;
                        }
                        if seen.insert(provider.id.clone()) {
                            providers.push(provider);
                        }
                    }
                }
            }
            let mut plan = ProviderRoutePlan::standard(providers, true);
            plan.sync_logical_target = false;
            plan.bypass_single_provider_circuit_breaker = false;
            plan.key_pool_retries = key_pool_retries;
            return Ok(plan);
        }

        let providers = self.select_providers_base(app_type).await?;
        Ok(ProviderRoutePlan::standard(
            providers,
            auto_failover_enabled,
        ))
    }

    async fn auto_failover_enabled(&self, app_type: &str) -> bool {
        match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(error) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {error}，默认禁用故障转移");
                false
            }
        }
    }

    /// Select members for the current provider's enabled key pool.
    /// `None` means the current provider is not an active pool anchor and the
    /// caller should use the legacy provider route.
    async fn select_key_pool_members(
        &self,
        app_type: &str,
    ) -> Result<Option<(crate::provider_groups::ProviderGroup, Vec<Provider>)>, AppError> {
        let app = AppType::from_str(app_type)?;
        if !app.supports_local_proxy() {
            return Ok(None);
        }
        let current_id = crate::settings::get_effective_current_provider(&self.db, &app)
            .ok()
            .flatten()
            .or_else(|| self.db.get_current_provider(app_type).ok().flatten());
        let Some(current_id) = current_id else {
            return Ok(None);
        };
        let Some(current) = self.db.get_provider_by_id(&current_id, app_type)? else {
            return Ok(None);
        };
        if !current
            .meta
            .as_ref()
            .and_then(|meta| meta.key_pool_enabled)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let Some(group_id) = current
            .meta
            .as_ref()
            .and_then(|meta| meta.provider_group_id.as_deref())
        else {
            return Ok(None);
        };
        let Some(group) = self.db.get_provider_group(group_id)? else {
            return Ok(None);
        };
        if group.app_type != app_type || !group.key_pool_enabled {
            return Ok(None);
        }

        let mut members =
            crate::services::provider_groups::ProviderGroupService::validate_pool_members(
                &self.db, &group.id,
            )?;
        members.sort_by(|left, right| {
            let left_index = left
                .meta
                .as_ref()
                .and_then(|meta| meta.provider_group_sort_index)
                .unwrap_or(usize::MAX);
            let right_index = right
                .meta
                .as_ref()
                .and_then(|meta| meta.provider_group_sort_index)
                .unwrap_or(usize::MAX);
            left_index
                .cmp(&right_index)
                .then_with(|| {
                    left.sort_index
                        .unwrap_or(usize::MAX)
                        .cmp(&right.sort_index.unwrap_or(usize::MAX))
                })
                .then_with(|| left.id.cmp(&right.id))
        });

        let runtime_key = format!("{app_type}:{}", group.id);
        let now = Instant::now();
        let mut runtime = self.key_pool_runtime.lock().await;
        let state = runtime.entry(runtime_key).or_default();
        state
            .members
            .retain(|id, _| members.iter().any(|member| &member.id == id));
        let start = match group.key_pool_strategy {
            crate::provider_groups::KeyPoolStrategy::Failover => 0,
            crate::provider_groups::KeyPoolStrategy::RoundRobin => state.next_index % members.len(),
        };

        let mut selected = Vec::with_capacity(members.len());
        for offset in 0..members.len() {
            let index = (start + offset) % members.len();
            let provider = &members[index];
            if state
                .members
                .get(&provider.id)
                .and_then(|member| member.cooldown_until)
                .is_some_and(|until| until > now)
            {
                continue;
            }
            selected.push(provider.clone());
        }
        if selected.is_empty() {
            // Avoid making a request hang indefinitely. Probe the member whose
            // cooldown expires first and remove only that member's cooldown.
            if let Some((provider, _)) = members
                .iter()
                .filter_map(|provider| {
                    state
                        .members
                        .get(&provider.id)
                        .and_then(|member| member.cooldown_until.map(|until| (provider, until)))
                })
                .min_by_key(|(_, until)| *until)
            {
                let member = state.members.entry(provider.id.clone()).or_default();
                member.cooldown_until = None;
                member.early_probe = true;
                log::warn!(
                    "[KeyPool] app={} group={} provider={} probing earliest cooldown member",
                    app_type,
                    group.id,
                    provider.id
                );
                selected.push(provider.clone());
            }
        }
        if let Some(first) = selected.first() {
            state.next_index = (members
                .iter()
                .position(|member| member.id == first.id)
                .unwrap_or(0)
                + 1)
                % members.len();
        }
        Ok(Some((group, selected)))
    }

    /// 选择可用的供应商（支持故障转移）。保留此 API 供非上下文调用方使用。
    pub async fn select_providers(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        Ok(self.select_route_plan(app_type).await?.providers())
    }

    /// 旧的全局故障转移选择逻辑；池路由在 `select_route_plan` 中优先处理。
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：仅使用故障转移队列，按队列顺序依次尝试（P1 → P2 → ...）
    async fn select_providers_base(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;
        let current_id = AppType::from_str(app_type)
            .ok()
            .and_then(|app_enum| {
                crate::settings::get_effective_current_provider(&self.db, &app_enum)
                    .ok()
                    .flatten()
            })
            .or_else(|| self.db.get_current_provider(app_type).ok().flatten());
        let current_provider = current_id
            .as_deref()
            .map(|id| self.db.get_provider_by_id(id, app_type))
            .transpose()?
            .flatten();

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled
            && current_provider
                .as_ref()
                .is_some_and(|provider| !provider_supports_failover(app_type, provider))
        {
            // A selected Codex Official account is an explicit account choice.
            // Keep it as a single route even if an old failover setting remains
            // enabled; retrying would reuse its inbound token for another card.
            total_providers = 1;
            result.push(current_provider.expect("checked above"));
        } else if auto_failover_enabled {
            // 故障转移开启：仅按队列顺序依次尝试（P1 → P2 → ...）
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致
            let ordered_ids: Vec<String> = self
                .db
                .get_failover_queue(app_type)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();

            for provider_id in ordered_ids {
                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };
                if !provider_supports_failover(app_type, &provider) {
                    continue;
                }
                total_providers += 1;

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
            if let Some(current) = current_provider {
                total_providers = 1;
                result.push(current);
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

        if !crate::settings::agent_role_routing_allowed() {
            return Ok(None);
        }

        let Some(owner) = self.db.get_provider_by_id(owner_provider_id, APP_TYPE)? else {
            let current_id =
                crate::settings::get_effective_current_provider(&self.db, &AppType::Codex)?
                    .ok_or(AppError::NoProvidersConfigured)?;
            let current = self
                .db
                .get_provider_by_id(&current_id, APP_TYPE)?
                .ok_or(AppError::NoProvidersConfigured)?;
            log::warn!(
                "[CodexRoleRoute] owner {} is unavailable; using current Codex provider {}",
                owner_provider_id,
                current.id
            );
            return Ok(Some(ProviderRoutePlan {
                attempts: vec![ProviderRouteAttempt {
                    outbound_model_override: provider_default_or_request(&current, request_model),
                    provider: current,
                }],
                use_failover_timeouts: true,
                sync_logical_target: false,
                bypass_single_provider_circuit_breaker: false,
                key_pool_retries: HashMap::new(),
            }));
        };

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
            key_pool_retries: HashMap::new(),
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

    /// Record pool state against the provider snapshot that issued the request.
    pub async fn record_result_for_provider(
        &self,
        provider: &Provider,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        self.record_result(
            &provider.id,
            app_type,
            used_half_open_permit,
            success,
            error_msg,
        )
        .await?;
        let Some(meta) = provider
            .meta
            .as_ref()
            .filter(|meta| meta.key_pool_enabled == Some(true))
        else {
            return Ok(());
        };
        let Some(group_id) = meta.provider_group_id.as_deref() else {
            return Ok(());
        };
        let Some(current) = self.db.get_provider_by_id(&provider.id, app_type)? else {
            return Ok(());
        };
        if current.settings_config != provider.settings_config
            || current
                .meta
                .as_ref()
                .and_then(|meta| meta.provider_group_id.as_deref())
                != Some(group_id)
            || current.meta.as_ref().and_then(|meta| meta.key_pool_enabled) != Some(true)
        {
            return Ok(());
        }
        let Some(group) = self.db.get_provider_group(group_id)? else {
            return Ok(());
        };
        if !group.key_pool_enabled || group.app_type != app_type {
            return Ok(());
        }
        let key = format!("{app_type}:{group_id}");
        let mut runtime = self.key_pool_runtime.lock().await;
        if success {
            if let Some(state) = runtime.get_mut(&key) {
                state
                    .members
                    .insert(provider.id.clone(), KeyPoolMemberRuntime::default());
            }
        } else {
            let member = runtime
                .entry(key)
                .or_default()
                .members
                .entry(provider.id.clone())
                .or_default();
            member.cooldown_until =
                Some(Instant::now() + Duration::from_millis(group.key_pool_cooldown_ms));
            member.consecutive_failures = member.consecutive_failures.saturating_add(1);
            member.last_failure_at = Some(chrono::Utc::now().timestamp_millis());
            member.early_probe = false;
        }
        Ok(())
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

    pub async fn clear_key_pool_runtime(&self, app_type: &str, group_id: &str) {
        self.key_pool_runtime
            .lock()
            .await
            .remove(&format!("{app_type}:{group_id}"));
    }

    pub async fn fill_key_pool_status(
        &self,
        status: &mut crate::provider_groups::ProviderGroupStatus,
    ) {
        status.proxy_running = true;
        let key = format!("{}:{}", status.group.app_type, status.group.id);
        let runtime = self.key_pool_runtime.lock().await;
        let Some(state) = runtime.get(&key).filter(|_| status.group.key_pool_enabled) else {
            return;
        };
        for member in status.members.iter_mut().filter(|member| member.eligible) {
            if let Some(live) = state.members.get(&member.provider_id) {
                member.cooldown_remaining_ms = live
                    .cooldown_until
                    .map(|until| until.saturating_duration_since(Instant::now()).as_millis() as u64)
                    .unwrap_or(0);
                member.cooling_down = member.cooldown_remaining_ms > 0;
                member.consecutive_failures = live.consecutive_failures;
                member.last_failure_at = live.last_failure_at;
                member.early_probe = live.early_probe;
            }
        }
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
    crate::proxy::providers::codex_provider_upstream_model(provider)
        .or_else(|| {
            provider
                .settings_config
                .get("modelCatalog")
                .and_then(|catalog| catalog.get("models"))
                .and_then(|models| models.as_array())
                .and_then(|models| models.first())
                .and_then(|model| {
                    model
                        .as_str()
                        .or_else(|| model.get("model").and_then(|value| value.as_str()))
                })
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToString::to_string)
        })
        .or_else(|| {
            let request_model = request_model.trim();
            (!request_model.is_empty()).then(|| request_model.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::{
        AuthBinding, AuthBindingSource, CodexAgentRoleRouting, CodexFrontendAgentRoleOverride,
        ProviderMeta,
    };
    use crate::services::codex_agent_roles::{
        create_codex_role_route_token, FRONTEND_ROLE_ROUTE_VALUE,
    };
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    fn managed_codex_official(id: &str, account_id: &str) -> Provider {
        let mut provider = Provider::with_id(
            id.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some(account_id.to_string()),
            }),
            ..Default::default()
        });
        provider
    }

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
    async fn disabled_agent_role_scope_skips_frontend_route_selection() {
        let _home = TempHome::new();
        let mut settings = crate::settings::get_settings();
        let scopes = crate::settings::ProviderFeatureScopes {
            agent_role_routing: crate::settings::FeatureScope {
                enabled: false,
                apps: vec!["codex".to_string()],
            },
            ..Default::default()
        };
        settings.provider_feature_scopes = Some(scopes);
        crate::settings::update_settings(settings).expect("disable agent role routing scope");

        let router = ProviderRouter::new(Arc::new(Database::memory().expect("memory db")));
        let plan = router
            .select_codex_frontend_route_plan("old-owner", "client-model", "old-token")
            .await
            .expect("disabled role route selection");

        assert!(plan.is_none());
        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("restore default settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_is_fixed_to_b_then_owner_a() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("memory db"));
        let owner = codex_role_owner("a", Some("b"), Some("frontend-custom"));
        let provider_b = Provider::with_id(
            "b".into(),
            "Provider B".into(),
            json!({
                "modelCatalog": {
                    "models": [{ "model": "b-catalog-default" }]
                }
            }),
            None,
        );
        let provider_c = Provider::with_id(
            "c".into(),
            "Provider C".into(),
            json!({ "model": "c-default" }),
            None,
        );
        db.save_provider("codex", &owner).expect("save owner");
        db.save_provider("codex", &provider_b).expect("save B");
        db.save_provider("codex", &provider_c).expect("save C");
        db.set_current_provider("codex", "c")
            .expect("set current C");
        db.add_to_failover_queue("codex", "c").expect("queue C");

        let router = ProviderRouter::new(db);
        let plan = router
            .select_codex_frontend_route_plan(
                "a",
                "client-capability-model",
                &role_route_token(&owner),
            )
            .await
            .expect("select role route")
            .expect("enabled role route");

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
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_uses_target_and_owner_catalog_defaults() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("memory db"));
        let mut owner = codex_role_owner("a", Some("b"), None);
        owner.settings_config = json!({
            "modelCatalog": {
                "models": ["  a-catalog-default  "]
            }
        });
        let provider_b = Provider::with_id(
            "b".into(),
            "Provider B".into(),
            json!({
                "modelCatalog": {
                    "models": [{ "model": "  b-catalog-default  " }]
                }
            }),
            None,
        );
        db.save_provider("codex", &owner).expect("save owner");
        db.save_provider("codex", &provider_b).expect("save B");

        let router = ProviderRouter::new(db);
        let plan = router
            .select_codex_frontend_route_plan(
                "a",
                "client-capability-model",
                &role_route_token(&owner),
            )
            .await
            .expect("select role route")
            .expect("enabled role route");

        assert_eq!(
            plan.attempts[0].outbound_model_override.as_deref(),
            Some("b-catalog-default")
        );
        assert_eq!(
            plan.attempts[1].outbound_model_override.as_deref(),
            Some("a-catalog-default")
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_frontend_route_plan_recovers_missing_target_with_owner_default() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("memory db"));
        let mut owner = codex_role_owner("a", Some("missing"), None);
        owner.settings_config = json!({
            "modelCatalog": {
                "models": [{ "model": "owner-catalog-default" }]
            }
        });
        db.save_provider("codex", &owner).expect("save owner");
        let router = ProviderRouter::new(db);

        let plan = router
            .select_codex_frontend_route_plan("a", "client-model", &role_route_token(&owner))
            .await
            .expect("select missing target route")
            .expect("role route plan");
        assert_eq!(plan.attempts.len(), 1);
        assert_eq!(plan.attempts[0].provider.id, "a");
        assert_eq!(
            plan.attempts[0].outbound_model_override.as_deref(),
            Some("owner-catalog-default")
        );
    }

    #[tokio::test]
    #[serial]
    async fn missing_role_owner_falls_back_to_current_codex_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("memory db"));
        let current = Provider::with_id(
            "current".into(),
            "Current".into(),
            json!({
                "modelCatalog": {
                    "models": ["  current-catalog-default  "]
                }
            }),
            None,
        );
        db.save_provider("codex", &current).expect("save current");
        db.set_current_provider("codex", "current")
            .expect("set database current");
        let router = ProviderRouter::new(db);

        let plan = router
            .select_codex_frontend_route_plan(
                "deleted-owner",
                "client-model",
                "unverifiable-deleted-owner-token",
            )
            .await
            .expect("missing owner falls back")
            .expect("fallback route plan");
        assert_eq!(plan.attempts.len(), 1);
        assert_eq!(plan.attempts[0].provider.id, "current");
        assert_eq!(
            plan.attempts[0].outbound_model_override.as_deref(),
            Some("current-catalog-default")
        );
        assert!(!plan.sync_logical_target);
    }

    #[test]
    fn provider_default_or_request_uses_role_model_priority() {
        let direct = Provider::with_id(
            "direct".into(),
            "Direct".into(),
            json!({
                "model": "  settings-model  ",
                "config": "model = \"toml-model\"\n",
                "modelCatalog": { "models": [{ "model": "catalog-model" }] }
            }),
            None,
        );
        assert_eq!(
            provider_default_or_request(&direct, "request-model").as_deref(),
            Some("settings-model")
        );

        let toml = Provider::with_id(
            "toml".into(),
            "TOML".into(),
            json!({
                "model": "   ",
                "config": "model = \"  toml-model  \"\n",
                "modelCatalog": { "models": [{ "model": "catalog-model" }] }
            }),
            None,
        );
        assert_eq!(
            provider_default_or_request(&toml, "request-model").as_deref(),
            Some("toml-model")
        );

        let catalog = Provider::with_id(
            "catalog".into(),
            "Catalog".into(),
            json!({
                "model": "   ",
                "config": "model = \"   \"\n",
                "modelCatalog": { "models": [{ "model": "  catalog-model  " }] }
            }),
            None,
        );
        assert_eq!(
            provider_default_or_request(&catalog, "request-model").as_deref(),
            Some("catalog-model")
        );

        let empty_catalog = Provider::with_id(
            "request".into(),
            "Request".into(),
            json!({
                "modelCatalog": { "models": ["   ", { "model": "unused-second-model" }] }
            }),
            None,
        );
        assert_eq!(
            provider_default_or_request(&empty_catalog, "  request-model  ").as_deref(),
            Some("request-model")
        );
    }

    #[tokio::test]
    #[serial]
    async fn existing_role_owner_rejects_forged_and_stale_tokens() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("memory db"));
        let owner = codex_role_owner("a", Some("b"), Some("initial-model"));
        let stale_token = role_route_token(&owner);
        db.save_provider("codex", &owner).expect("save owner");
        let changed_owner = codex_role_owner("a", Some("b"), Some("changed-model"));
        db.save_provider("codex", &changed_owner)
            .expect("save changed owner");
        let router = ProviderRouter::new(db);

        for token in ["forged-token", stale_token.as_str()] {
            assert!(matches!(
                router
                    .select_codex_frontend_route_plan("a", "client-model", token)
                    .await,
                Err(AppError::InvalidInput(message)) if message.contains("token")
            ));
        }
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
    async fn codex_official_current_stays_single_route_when_failover_is_stale() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let official = managed_codex_official("official-a", "account-a");
        let fallback = Provider::with_id(
            "fallback".to_string(),
            "Fallback".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &fallback).unwrap();
        db.set_current_provider("codex", &official.id).unwrap();
        db.add_to_failover_queue("codex", &fallback.id).unwrap();

        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let providers = ProviderRouter::new(db)
            .select_providers("codex")
            .await
            .unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, official.id);
    }

    #[tokio::test]
    #[serial]
    async fn stale_codex_official_queue_entries_are_not_retry_targets() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let current = Provider::with_id(
            "third-party".to_string(),
            "Third Party".to_string(),
            json!({}),
            None,
        );
        let official = managed_codex_official("official-a", "account-a");
        let fallback = Provider::with_id(
            "fallback".to_string(),
            "Fallback".to_string(),
            json!({}),
            None,
        );
        db.save_provider("codex", &current).unwrap();
        db.save_provider("codex", &official).unwrap();
        db.save_provider("codex", &fallback).unwrap();
        db.set_current_provider("codex", &current.id).unwrap();
        db.add_to_failover_queue("codex", &official.id).unwrap();
        db.add_to_failover_queue("codex", &fallback.id).unwrap();

        let mut config = db.get_proxy_config_for_app("codex").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let providers = ProviderRouter::new(db)
            .select_providers("codex")
            .await
            .unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fallback"]
        );
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

    fn key_pool_fixture() -> (Arc<Database>, ProviderRouter) {
        let db = Arc::new(Database::memory().expect("memory db"));
        let group = crate::provider_groups::ProviderGroup {
            id: "group-1".to_string(),
            app_type: "codex".to_string(),
            name: "Relay".to_string(),
            kind: crate::provider_groups::ProviderGroupKind::Manual,
            normalized_base_url: Some("https://relay.example/v1".to_string()),
            sort_index: 0,
            collapsed: false,
            key_pool_enabled: true,
            key_pool_strategy: crate::provider_groups::KeyPoolStrategy::RoundRobin,
            icon: None,
            icon_color: None,
            key_pool_max_retries: 0,
            key_pool_cooldown_ms: 1000,
            balance_template_id: None,
            created_at: 1,
            updated_at: 1,
        };
        db.create_provider_group(&group).expect("create group");

        for (index, id) in ["p1", "p2", "p3"].into_iter().enumerate() {
            let mut provider = Provider::with_id(
                id.to_string(),
                format!("Relay {id}"),
                json!({
                    "auth": {"OPENAI_API_KEY": format!("key-{id}")},
                    "config": format!(
                        "model_provider = \"relay\"\n[model_providers.relay]\nbase_url = \"https://relay.example/v1\"\n"
                    )
                }),
                None,
            );
            provider.meta = Some(ProviderMeta {
                provider_group_id: Some(group.id.clone()),
                provider_group_sort_index: Some(index),
                key_pool_enabled: Some(true),
                ..Default::default()
            });
            db.save_provider("codex", &provider).expect("save provider");
        }
        db.set_current_provider("codex", "p1")
            .expect("set current provider");
        let saved = db
            .get_provider_by_id("p1", "codex")
            .expect("read saved provider")
            .expect("saved provider exists");
        let (saved_base_url, saved_key) = saved.resolve_usage_credentials(&AppType::Codex);
        assert_eq!(saved_base_url, "https://relay.example/v1");
        assert!(!saved_key.is_empty());
        assert_eq!(
            saved
                .meta
                .as_ref()
                .and_then(|meta| meta.provider_group_id.as_deref()),
            Some("group-1")
        );

        let router = ProviderRouter::new(db.clone());
        (db, router)
    }

    #[tokio::test]
    #[serial]
    async fn round_robin_pool_rotates_without_changing_logical_current() {
        let _home = TempHome::new();
        let (db, router) = key_pool_fixture();
        let first = router
            .select_route_plan("codex")
            .await
            .expect("first route plan");
        let second = router
            .select_route_plan("codex")
            .await
            .expect("second route plan");

        assert_eq!(
            first.attempts.len(),
            3,
            "zero member retries must still allow every key"
        );
        assert_eq!(first.key_pool_retries.len(), 3);

        assert_ne!(
            first.attempts[0].provider.id,
            second.attempts[0].provider.id
        );
        assert!(!first.sync_logical_target);
        assert_eq!(
            db.get_current_provider("codex")
                .expect("read current provider")
                .as_deref(),
            Some("p1")
        );
    }

    #[tokio::test]
    #[serial]
    async fn key_pool_failover_order_cooldown_and_early_probe_are_observable() {
        let _home = TempHome::new();
        let (db, router) = key_pool_fixture();
        let mut group = db.get_provider_group("group-1").unwrap().unwrap();
        group.key_pool_strategy = crate::provider_groups::KeyPoolStrategy::Failover;
        group.key_pool_cooldown_ms = 60_000;
        db.update_provider_group(&group).unwrap();
        db.set_current_provider("codex", "p3").unwrap();
        assert_eq!(
            router.select_route_plan("codex").await.unwrap().attempts[0]
                .provider
                .id,
            "p1"
        );
        router
            .record_result_for_provider(
                &db.get_provider_by_id("p1", "codex").unwrap().unwrap(),
                "codex",
                false,
                false,
                None,
            )
            .await
            .unwrap();
        let selected = router.select_route_plan("codex").await.unwrap();
        assert_eq!(
            selected
                .providers()
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["p2", "p3"]
        );
        let mut status =
            crate::services::provider_groups::ProviderGroupService::group_status(&db, "group-1")
                .unwrap();
        assert!(!status.proxy_running);
        router.fill_key_pool_status(&mut status).await;
        assert!(status.proxy_running && status.members[0].cooling_down);
        assert!(status.members[0].cooldown_remaining_ms > 0);
        assert_eq!(status.members[0].consecutive_failures, 1);
        assert!(status.members[0].last_failure_at.is_some());
        router
            .record_result_for_provider(
                &db.get_provider_by_id("p2", "codex").unwrap().unwrap(),
                "codex",
                false,
                false,
                None,
            )
            .await
            .unwrap();
        router
            .record_result_for_provider(
                &db.get_provider_by_id("p3", "codex").unwrap().unwrap(),
                "codex",
                false,
                false,
                None,
            )
            .await
            .unwrap();
        let probe = router.select_route_plan("codex").await.unwrap();
        assert_eq!(probe.attempts.len(), 1);
        assert_eq!(probe.attempts[0].provider.id, "p1");
        router.fill_key_pool_status(&mut status).await;
        assert!(status.members[0].early_probe);
        router
            .record_result_for_provider(
                &db.get_provider_by_id("p1", "codex").unwrap().unwrap(),
                "codex",
                false,
                true,
                None,
            )
            .await
            .unwrap();
        router.fill_key_pool_status(&mut status).await;
        assert!(!status.members[0].cooling_down && !status.members[0].early_probe);
        assert_eq!(status.members[0].consecutive_failures, 0);
        router.clear_key_pool_runtime("codex", "group-1").await;
        assert!(router.key_pool_runtime.lock().await.is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn key_pool_round_robin_skips_cooling_key_without_repeating_next_key() {
        let _home = TempHome::new();
        let (db, router) = key_pool_fixture();
        assert_eq!(
            router.select_route_plan("codex").await.unwrap().attempts[0]
                .provider
                .id,
            "p1"
        );
        router
            .record_result_for_provider(
                &db.get_provider_by_id("p2", "codex").unwrap().unwrap(),
                "codex",
                false,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            router.select_route_plan("codex").await.unwrap().attempts[0]
                .provider
                .id,
            "p3"
        );
        assert_eq!(
            router.select_route_plan("codex").await.unwrap().attempts[0]
                .provider
                .id,
            "p1"
        );
    }

    #[tokio::test]
    #[serial]
    async fn key_pool_excluded_anchor_uses_legacy_independent_route() {
        let _home = TempHome::new();
        let (db, router) = key_pool_fixture();
        db.assign_provider_group("codex", "p1", Some("group-1"), None, Some(false), None)
            .unwrap();
        let plan = router.select_route_plan("codex").await.unwrap();
        assert!(plan.key_pool_retries.is_empty() && plan.sync_logical_target);
        assert_eq!(plan.attempts.len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn key_pool_late_result_does_not_cool_a_new_folder_after_member_move() {
        let _home = TempHome::new();
        let (db, router) = key_pool_fixture();
        let original = router.select_route_plan("codex").await.unwrap().attempts[0]
            .provider
            .clone();
        let mut other = db.get_provider_group("group-1").unwrap().unwrap();
        other.id = "group-2".into();
        other.name = "Second pool".into();
        other.key_pool_enabled = false;
        db.create_provider_group(&other).unwrap();
        db.assign_provider_group(
            "codex",
            &original.id,
            Some(&other.id),
            None,
            Some(true),
            Some(true),
        )
        .unwrap();
        other.key_pool_enabled = true;
        db.update_provider_group(&other).unwrap();
        router.select_route_plan("codex").await.unwrap();

        router
            .record_result_for_provider(&original, "codex", false, false, None)
            .await
            .unwrap();

        let mut status =
            crate::services::provider_groups::ProviderGroupService::group_status(&db, &other.id)
                .unwrap();
        router.fill_key_pool_status(&mut status).await;
        assert_eq!(status.members.len(), 1);
        assert!(
            !status.members[0].cooling_down,
            "an old request must not cool its member's new pool"
        );
        assert_eq!(status.members[0].consecutive_failures, 0);
        let current = db
            .get_provider_by_id(&original.id, "codex")
            .unwrap()
            .unwrap();
        router
            .record_result_for_provider(&current, "codex", false, false, None)
            .await
            .unwrap();
        router
            .record_result_for_provider(&original, "codex", false, true, None)
            .await
            .unwrap();
        router.fill_key_pool_status(&mut status).await;
        assert!(
            status.members[0].cooling_down,
            "an old success must not clear the new pool's cooldown"
        );
    }

    #[tokio::test]
    #[serial]
    async fn key_pool_late_failure_does_not_cool_replaced_credentials() {
        let _home = TempHome::new();
        let (db, router) = key_pool_fixture();
        let original = router.select_route_plan("codex").await.unwrap().attempts[0]
            .provider
            .clone();
        let mut current = original.clone();
        current.settings_config["auth"]["OPENAI_API_KEY"] = json!("replacement-fixture-key");
        db.save_provider("codex", &current).unwrap();
        router
            .record_result_for_provider(&original, "codex", false, false, None)
            .await
            .unwrap();
        let mut status =
            crate::services::provider_groups::ProviderGroupService::group_status(&db, "group-1")
                .unwrap();
        router.fill_key_pool_status(&mut status).await;
        assert!(!status.members[0].cooling_down);
        assert_eq!(status.members[0].consecutive_failures, 0);
    }
}
