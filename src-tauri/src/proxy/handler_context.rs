//! 请求上下文模块
//!
//! 提供请求生命周期的上下文管理，封装通用初始化逻辑

use crate::app_config::AppType;
use crate::provider::Provider;
use crate::proxy::{
    extract_session_id,
    forwarder::RequestForwarder,
    provider_router::ProviderRoutePlan,
    server::ProxyState,
    types::{AppProxyConfig, CopilotOptimizerConfig, OptimizerConfig, RectifierConfig},
    ProxyError,
};
use axum::http::HeaderMap;
use std::net::SocketAddr;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRoleRoute {
    Frontend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRoleRouteHeaders {
    pub route: CodexRoleRoute,
    pub owner_provider_id: String,
    pub token: String,
}

pub fn parse_codex_role_route_headers(
    headers: &HeaderMap,
) -> Result<Option<CodexRoleRouteHeaders>, ProxyError> {
    use crate::services::codex_agent_roles::{
        FRONTEND_ROLE_ROUTE_VALUE, ROLE_OWNER_HEADER, ROLE_ROUTE_HEADER, ROLE_TOKEN_HEADER,
    };

    let route = unique_role_header(headers, ROLE_ROUTE_HEADER)?;
    let owner = unique_role_header(headers, ROLE_OWNER_HEADER)?;
    let token = unique_role_header(headers, ROLE_TOKEN_HEADER)?;
    match (route, owner, token) {
        (None, None, None) => Ok(None),
        (Some(route), Some(owner_provider_id), Some(token)) => {
            let route = match route.as_str() {
                FRONTEND_ROLE_ROUTE_VALUE => CodexRoleRoute::Frontend,
                _ => {
                    return Err(ProxyError::InvalidRequest(format!(
                        "Unknown Codex role route: {route}"
                    )))
                }
            };
            Ok(Some(CodexRoleRouteHeaders {
                route,
                owner_provider_id,
                token,
            }))
        }
        _ => Err(ProxyError::InvalidRequest(
            "Codex role route, owner, and token headers must be provided together".to_string(),
        )),
    }
}

fn codex_role_route_for_request(
    app_type: &AppType,
    endpoint: &str,
    body: &serde_json::Value,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Result<Option<CodexRoleRouteHeaders>, ProxyError> {
    if super::codex_auto_review::is_auto_review_request(app_type, endpoint, body) {
        return Ok(None);
    }
    if !matches!(app_type, AppType::Codex) {
        return Ok(None);
    }

    use crate::services::codex_agent_roles::{
        ROLE_OWNER_HEADER, ROLE_ROUTE_HEADER, ROLE_TOKEN_HEADER,
    };
    let has_role_header = [ROLE_ROUTE_HEADER, ROLE_OWNER_HEADER, ROLE_TOKEN_HEADER]
        .into_iter()
        .any(|name| headers.contains_key(name));
    if has_role_header && !peer_addr.is_some_and(|addr| addr.ip().is_loopback()) {
        return Err(ProxyError::InvalidRequest(
            "Codex role routing is only accepted from the local loopback interface".to_string(),
        ));
    }
    parse_codex_role_route_headers(headers)
}

fn unique_role_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, ProxyError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(ProxyError::InvalidRequest(format!(
            "Duplicate Codex role header: {name}"
        )));
    }
    let value = value.to_str().map_err(|_| {
        ProxyError::InvalidRequest(format!("Codex role header is not valid UTF-8: {name}"))
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(ProxyError::InvalidRequest(format!(
            "Codex role header is empty: {name}"
        )));
    }
    if value.contains(',') {
        return Err(ProxyError::InvalidRequest(format!(
            "Codex role header contains multiple values: {name}"
        )));
    }
    Ok(Some(value.to_string()))
}

/// 流式超时配置
#[derive(Debug, Clone, Copy)]
pub struct StreamingTimeoutConfig {
    /// 首字节超时（秒），0 表示禁用
    pub first_byte_timeout: u64,
    /// 静默期超时（秒），0 表示禁用
    pub idle_timeout: u64,
}

/// 请求上下文
///
/// 贯穿整个请求生命周期，包含：
/// - 计时信息
/// - 应用级代理配置（per-app）
/// - 选中的 Provider 列表（用于故障转移）
/// - 请求模型名称
/// - 日志标签
/// - Session ID（用于日志关联）
pub struct RequestContext {
    /// 请求开始时间
    pub start_time: Instant,
    /// 应用级代理配置（per-app，包含重试次数和超时配置）
    pub app_config: AppProxyConfig,
    /// 选中的 Provider（故障转移链的第一个）
    pub provider: Provider,
    /// 完整的 Provider 列表（用于故障转移）
    providers: Vec<Provider>,
    route_plan: ProviderRoutePlan,
    role_route_owner_id: Option<String>,
    /// 请求开始时的"当前供应商"（用于判断是否需要同步 UI/托盘）
    ///
    /// 这里使用本地 settings 的设备级 current provider。
    /// 代理模式下如果实际使用的 provider 与此不一致，会触发切换以确保 UI 始终准确。
    pub current_provider_id: String,
    /// 请求中的模型名称
    pub request_model: String,
    /// 实际发往上游的模型名（路由接管/模型映射后的真值，forward 成功后回填）。
    ///
    /// usage 归因的兜底顺序：上游响应回显 → outbound_model → request_model。
    /// 不能直接用 request_model 兜底：接管场景下它是映射前的客户端别名。
    pub outbound_model: Option<String>,
    /// 日志标签（如 "Claude"、"Codex"、"Gemini"）
    pub tag: &'static str,
    /// 应用类型字符串（如 "claude"、"codex"、"gemini"）
    pub app_type_str: &'static str,
    /// 应用类型（预留，目前通过 app_type_str 使用）
    #[allow(dead_code)]
    pub app_type: AppType,
    /// Session ID（从客户端请求提取或新生成）
    pub session_id: String,
    /// Session ID 是否由客户端提供。生成的 UUID 不能作为上游缓存 key，否则每个请求都会换 key。
    pub session_client_provided: bool,
    /// 整流器配置
    pub rectifier_config: RectifierConfig,
    /// 优化器配置
    pub optimizer_config: OptimizerConfig,
    /// Copilot 优化器配置
    pub copilot_optimizer_config: CopilotOptimizerConfig,
    /// 全局 Provider 自动重试开关，在请求创建时快照。
    pub provider_retry_enabled: bool,
    /// 当前请求是否至少有一个 Provider 解析出有效的自动重试策略。
    ///
    /// 用于在故障转移关闭时仍保留响应输出前的正文/首包超时，使网络类错误
    /// 能进入同 Provider 重试；输出后的流式 idle timeout 仍由故障转移配置管理。
    provider_retry_active: bool,
}

impl RequestContext {
    /// 创建请求上下文
    ///
    /// # Arguments
    /// * `state` - 代理服务器状态
    /// * `body` - 请求体 JSON
    /// * `headers` - 请求头（用于提取 Session ID）
    /// * `app_type` - 应用类型
    /// * `tag` - 日志标签
    /// * `app_type_str` - 应用类型字符串
    ///
    /// # Errors
    /// 返回 `ProxyError` 如果 Provider 选择失败
    pub async fn new(
        state: &ProxyState,
        body: &serde_json::Value,
        headers: &HeaderMap,
        app_type: AppType,
        tag: &'static str,
        app_type_str: &'static str,
    ) -> Result<Self, ProxyError> {
        Self::new_with_peer_addr(state, body, headers, app_type, tag, app_type_str, "", None).await
    }

    pub async fn new_with_peer_addr(
        state: &ProxyState,
        body: &serde_json::Value,
        headers: &HeaderMap,
        app_type: AppType,
        tag: &'static str,
        app_type_str: &'static str,
        endpoint: &str,
        peer_addr: Option<SocketAddr>,
    ) -> Result<Self, ProxyError> {
        let start_time = Instant::now();
        let role_route =
            codex_role_route_for_request(&app_type, endpoint, body, headers, peer_addr)?;

        // 从数据库读取应用级代理配置（per-app）
        let app_config = state
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| ProxyError::DatabaseError(e.to_string()))?;
        let provider_retry_enabled = crate::settings::get_settings().is_provider_retry_enabled();

        // 从数据库读取整流器配置
        let rectifier_config = state.db.get_rectifier_config().unwrap_or_default();
        let optimizer_config = state.db.get_optimizer_config().unwrap_or_default();
        let copilot_optimizer_config = state.db.get_copilot_optimizer_config().unwrap_or_default();

        let current_provider_id =
            crate::settings::get_current_provider(&app_type).unwrap_or_default();

        // 从请求体提取模型名称
        let request_model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        // 提取 Session ID
        let session_result = extract_session_id(headers, body, app_type_str);
        let session_id = session_result.session_id.clone();

        log::debug!(
            "[{}] Session ID: {} (from {:?}, client_provided: {})",
            tag,
            session_id,
            session_result.source,
            session_result.client_provided
        );

        let (role_route_plan, role_route_owner_id) = match role_route {
            Some(CodexRoleRouteHeaders {
                route: CodexRoleRoute::Frontend,
                owner_provider_id,
                token,
            }) => {
                let plan = state
                    .provider_router
                    .select_codex_frontend_route_plan(&owner_provider_id, &request_model, &token)
                    .await
                    .map_err(map_provider_selection_error)?;
                (plan, Some(owner_provider_id))
            }
            None => (None, None),
        };
        let route_plan = match role_route_plan {
            Some(plan) => plan,
            None => {
                let providers = state
                    .provider_router
                    .select_providers(app_type_str)
                    .await
                    .map_err(map_provider_selection_error)?;
                ProviderRoutePlan::standard(providers, app_config.auto_failover_enabled)
            }
        };
        let providers = route_plan.providers();

        let provider = providers
            .first()
            .cloned()
            .ok_or(ProxyError::NoAvailableProvider)?;
        let provider_retry_active = providers.iter().any(|provider| {
            super::provider_retry::resolve_retry_policy_with_global(
                &app_type,
                body,
                provider,
                provider_retry_enabled,
            )
            .is_some()
        });

        log::debug!(
            "[{}] Provider: {}, model: {}, failover chain: {} providers, session: {}",
            tag,
            provider.name,
            request_model,
            providers.len(),
            session_id
        );

        Ok(Self {
            start_time,
            app_config,
            provider,
            providers,
            route_plan,
            role_route_owner_id,
            current_provider_id,
            request_model,
            outbound_model: None,
            tag,
            app_type_str,
            app_type,
            session_id,
            session_client_provided: session_result.client_provided,
            rectifier_config,
            optimizer_config,
            copilot_optimizer_config,
            provider_retry_enabled,
            provider_retry_active,
        })
    }

    /// 从 URI 提取模型名称（Gemini 专用）
    ///
    /// Gemini API 的模型名称在 URI 中，格式如：
    /// `/v1beta/models/gemini-pro:generateContent`
    pub fn with_model_from_uri(mut self, uri: &axum::http::Uri) -> Self {
        // 用 path() 而不是 path_and_query()：模型名必须从路径段中解析，
        // 否则 GET /v1beta/models/<id>?key=... 会把 query 拼到 request_model 上。
        let endpoint = uri.path();

        self.request_model =
            extract_gemini_model_from_path(endpoint).unwrap_or_else(|| "unknown".to_string());

        self
    }

    /// 创建 RequestForwarder
    ///
    /// 使用共享的 ProviderRouter，确保熔断器状态跨请求保持
    ///
    /// 配置生效规则：
    /// - 故障转移开启：输出前超时和流式 idle 配置正常生效（0 表示禁用）
    /// - 当前请求有 Provider 自动重试：输出前正文/首包超时生效，便于产生可重试网络错误
    /// - 两者均关闭：全部传入 0
    pub fn create_forwarder(&self, state: &ProxyState) -> RequestForwarder {
        let pre_output_timeouts_enabled =
            self.route_plan.use_failover_timeouts || self.provider_retry_active;
        let non_streaming_timeout = if pre_output_timeouts_enabled {
            self.app_config.non_streaming_timeout as u64
        } else {
            0
        };
        let first_byte_timeout = if pre_output_timeouts_enabled {
            self.app_config.streaming_first_byte_timeout as u64
        } else {
            0
        };
        let idle_timeout = if self.route_plan.use_failover_timeouts {
            self.app_config.streaming_idle_timeout as u64
        } else {
            0
        };
        if !pre_output_timeouts_enabled {
            log::debug!(
                "[{}] Failover and Provider retry disabled, pre-output timeouts are bypassed",
                self.tag
            );
        }

        // 故障转移关闭时强制 max_retries=0（仅尝试 1 个 provider），与「不超时 + 不切换」语义一致。
        let max_retries = if self.app_config.auto_failover_enabled {
            self.app_config.max_retries
        } else {
            0
        };

        RequestForwarder::new(
            state.provider_router.clone(),
            non_streaming_timeout,
            state.status.clone(),
            state.current_providers.clone(),
            state.gemini_shadow.clone(),
            state.codex_chat_history.clone(),
            state.failover_manager.clone(),
            state.app_handle.clone(),
            self.current_provider_id.clone(),
            self.session_id.clone(),
            self.session_client_provided,
            first_byte_timeout,
            idle_timeout,
            self.rectifier_config.clone(),
            self.optimizer_config.clone(),
            self.copilot_optimizer_config.clone(),
            max_retries,
            self.provider_retry_enabled,
        )
        .with_route_plan(&self.route_plan)
        .with_role_context(self.role_route_owner_id.as_deref(), &self.request_model)
    }

    /// 获取 Provider 列表（用于故障转移）
    ///
    /// 返回在创建上下文时已选择的 providers，避免重复调用 select_providers()
    pub fn get_providers(&self) -> Vec<Provider> {
        self.providers.clone()
    }

    /// 计算请求延迟（毫秒）
    #[inline]
    pub fn latency_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// 获取流式超时配置
    ///
    /// 配置生效规则：
    /// - 故障转移开启：返回配置的值（0 表示禁用超时检查）
    /// - 故障转移关闭：返回 0（禁用超时检查）
    #[inline]
    pub fn streaming_timeout_config(&self) -> StreamingTimeoutConfig {
        if self.route_plan.use_failover_timeouts {
            // 故障转移开启：使用配置的值（0 = 禁用超时）
            StreamingTimeoutConfig {
                first_byte_timeout: self.app_config.streaming_first_byte_timeout as u64,
                idle_timeout: self.app_config.streaming_idle_timeout as u64,
            }
        } else {
            // 故障转移关闭：禁用流式超时检查
            StreamingTimeoutConfig {
                first_byte_timeout: 0,
                idle_timeout: 0,
            }
        }
    }
}

fn map_provider_selection_error(error: crate::error::AppError) -> ProxyError {
    match error {
        crate::error::AppError::AllProvidersCircuitOpen => ProxyError::AllProvidersCircuitOpen,
        crate::error::AppError::NoProvidersConfigured => ProxyError::NoProvidersConfigured,
        crate::error::AppError::InvalidInput(message) => ProxyError::InvalidRequest(message),
        error => ProxyError::DatabaseError(error.to_string()),
    }
}

/// Pull the Gemini model name out of an API path.
///
/// Accepts forms like `/v1beta/models/gemini-pro:generateContent`,
/// `/v1/models/gemini-1.5-flash`, `gemini/v1beta/models/<model>:streamGenerateContent`.
/// Returns `None` when no `models/<name>` segment is present.
pub(crate) fn extract_gemini_model_from_path(endpoint: &str) -> Option<String> {
    let segments: Vec<&str> = endpoint.split('/').collect();
    segments
        .iter()
        .position(|s| *s == "models")
        .and_then(|i| segments.get(i + 1).copied())
        // 防御性裁剪：即便调用方传入带 ? 或 :action 的字符串，也只保留 model id 本身
        .map(|s| s.split('?').next().unwrap_or(s))
        .map(|s| s.split(':').next().unwrap_or(s))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        codex_role_route_for_request, extract_gemini_model_from_path,
        parse_codex_role_route_headers, CodexRoleRoute, RequestContext,
    };
    use crate::{
        app_config::AppType,
        database::Database,
        provider::{
            CodexAgentRoleRouting, CodexFrontendAgentRoleOverride, LocalProxyRetryErrorType,
            LocalProxyRetryPolicy, Provider, ProviderMeta,
        },
        proxy::{
            failover_switch::FailoverSwitchManager,
            provider_router::ProviderRouter,
            providers::{
                codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore,
            },
            server::ProxyState,
            types::{ProxyConfig, ProxyStatus},
            ProxyError,
        },
    };
    use axum::{body::Body, routing::post, Router};
    use bytes::Bytes;
    use http::{Extensions, HeaderMap, HeaderValue, StatusCode};
    use serde_json::json;
    use serial_test::serial;
    use std::{
        convert::Infallible,
        ffi::OsString,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    use crate::services::codex_agent_roles::{
        create_codex_role_route_token, FRONTEND_ROLE_ROUTE_VALUE,
    };

    struct TestHome {
        _dir: TempDir,
        original_test_home: Option<OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create isolated handler context test home");
            let original_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload isolated handler context settings");
            Self {
                _dir: dir,
                original_test_home,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.original_test_home {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    fn build_proxy_state(db: Arc<Database>) -> ProxyState {
        ProxyState {
            db: db.clone(),
            config: Arc::new(RwLock::new(ProxyConfig::default())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            start_time: Arc::new(RwLock::new(None)),
            current_providers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            provider_router: Arc::new(ProviderRouter::new(db.clone())),
            gemini_shadow: Arc::new(GeminiShadowStore::default()),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
            app_handle: None,
            failover_manager: Arc::new(FailoverSwitchManager::new(db)),
        }
    }

    async fn abort_and_join_test_task(handle: tokio::task::JoinHandle<()>) {
        handle.abort();
        match handle.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("test fixture task join failed: {error}"),
        }
    }

    fn valid_role_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-cc-switch-role-route",
            HeaderValue::from_static("frontend"),
        );
        headers.insert(
            "x-cc-switch-role-owner",
            HeaderValue::from_static("provider-a"),
        );
        headers.insert(
            "x-cc-switch-role-token",
            HeaderValue::from_static("signed-token"),
        );
        headers
    }

    #[test]
    fn codex_role_headers_require_an_exact_complete_unique_triple() {
        let headers = valid_role_headers();
        let route = parse_codex_role_route_headers(&headers)
            .expect("valid role headers")
            .expect("role route");
        assert_eq!(route.route, CodexRoleRoute::Frontend);
        assert_eq!(route.owner_provider_id, "provider-a");
        assert_eq!(route.token, "signed-token");

        for missing in [
            "x-cc-switch-role-route",
            "x-cc-switch-role-owner",
            "x-cc-switch-role-token",
        ] {
            let mut incomplete = headers.clone();
            incomplete.remove(missing);
            assert!(matches!(
                parse_codex_role_route_headers(&incomplete),
                Err(ProxyError::InvalidRequest(message)) if message.contains("provided together")
            ));
        }

        for duplicate in [
            "x-cc-switch-role-route",
            "x-cc-switch-role-owner",
            "x-cc-switch-role-token",
        ] {
            let mut duplicated = headers.clone();
            duplicated.append(duplicate, HeaderValue::from_static("duplicate"));
            assert!(matches!(
                parse_codex_role_route_headers(&duplicated),
                Err(ProxyError::InvalidRequest(message)) if message.contains("Duplicate")
            ));
        }

        let mut combined = headers;
        combined.insert(
            "x-cc-switch-role-owner",
            HeaderValue::from_static("provider-a,provider-b"),
        );
        assert!(matches!(
            parse_codex_role_route_headers(&combined),
            Err(ProxyError::InvalidRequest(message)) if message.contains("multiple values")
        ));
    }

    #[test]
    fn codex_role_headers_reject_unknown_empty_and_non_utf8_values() {
        let cases = [
            (
                HeaderValue::from_static("backend"),
                HeaderValue::from_static("provider-a"),
                HeaderValue::from_static("signed-token"),
            ),
            (
                HeaderValue::from_static("frontend"),
                HeaderValue::from_static("   "),
                HeaderValue::from_static("signed-token"),
            ),
            (
                HeaderValue::from_bytes(&[0xff]).expect("opaque route header"),
                HeaderValue::from_static("provider-a"),
                HeaderValue::from_static("signed-token"),
            ),
            (
                HeaderValue::from_static("frontend"),
                HeaderValue::from_static("provider-a"),
                HeaderValue::from_static("   "),
            ),
        ];

        for (route, owner, token) in cases {
            let mut headers = HeaderMap::new();
            headers.insert("x-cc-switch-role-route", route);
            headers.insert("x-cc-switch-role-owner", owner);
            headers.insert("x-cc-switch-role-token", token);
            assert!(matches!(
                parse_codex_role_route_headers(&headers),
                Err(ProxyError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn codex_role_routing_requires_a_loopback_peer() {
        let headers = valid_role_headers();
        let body = json!({ "model": "gpt-5.6-sol" });
        assert!(codex_role_route_for_request(
            &AppType::Codex,
            "/v1/responses",
            &body,
            &headers,
            Some("127.0.0.1:15721".parse().expect("loopback address")),
        )
        .expect("loopback role request")
        .is_some());

        for peer_addr in [
            None,
            Some("192.0.2.10:15721".parse().expect("remote address")),
        ] {
            assert!(matches!(
                codex_role_route_for_request(
                    &AppType::Codex,
                    "/v1/responses",
                    &body,
                    &headers,
                    peer_addr,
                ),
                Err(ProxyError::InvalidRequest(message)) if message.contains("loopback")
            ));
        }
    }

    #[test]
    fn codex_auto_review_unconditionally_bypasses_role_header_validation() {
        let auto_review = json!({ "model": "codex-auto-review" });
        let mut unknown = valid_role_headers();
        unknown.insert(
            "x-cc-switch-role-route",
            HeaderValue::from_static("unknown"),
        );
        let mut duplicate = valid_role_headers();
        duplicate.append(
            "x-cc-switch-role-token",
            HeaderValue::from_static("duplicate"),
        );
        let mut non_utf8 = valid_role_headers();
        non_utf8.insert(
            "x-cc-switch-role-owner",
            HeaderValue::from_bytes(&[0xff]).expect("opaque owner header"),
        );
        let mut incomplete = HeaderMap::new();
        incomplete.insert(
            "x-cc-switch-role-route",
            HeaderValue::from_static("frontend"),
        );

        for headers in [
            HeaderMap::new(),
            valid_role_headers(),
            unknown,
            duplicate,
            non_utf8,
            incomplete,
        ] {
            for peer_addr in [
                None,
                Some("127.0.0.1:15721".parse().expect("loopback address")),
                Some("192.0.2.10:15721".parse().expect("remote address")),
            ] {
                assert_eq!(
                    codex_role_route_for_request(
                        &AppType::Codex,
                        "/v1/responses",
                        &auto_review,
                        &headers,
                        peer_addr,
                    )
                    .expect("approval routing takes priority"),
                    None
                );
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn request_context_builds_frontend_b_to_owner_a_route_plan() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create context database"));
        let routing = CodexAgentRoleRouting {
            enabled: Some(true),
            frontend: Some(CodexFrontendAgentRoleOverride {
                provider_id: Some("provider-b".to_string()),
                upstream_model: Some("frontend-upstream".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut owner = Provider::with_id(
            "provider-a".into(),
            "Provider A".into(),
            json!({ "config": "model = \"owner-default\"\n" }),
            None,
        );
        owner.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(routing.clone()),
            ..Default::default()
        });
        let provider_b = Provider::with_id(
            "provider-b".into(),
            "Provider B".into(),
            json!({ "model": "provider-b-default" }),
            None,
        );
        db.save_provider("codex", &owner).expect("save owner");
        db.save_provider("codex", &provider_b)
            .expect("save frontend provider");
        db.set_current_provider("codex", &owner.id)
            .expect("set current owner");
        let token = create_codex_role_route_token(&owner.id, FRONTEND_ROLE_ROUTE_VALUE, &routing);
        let mut headers = valid_role_headers();
        headers.insert(
            "x-cc-switch-role-owner",
            HeaderValue::from_str(&owner.id).expect("owner header"),
        );
        headers.insert(
            "x-cc-switch-role-token",
            HeaderValue::from_str(&token).expect("token header"),
        );

        let state = build_proxy_state(db);
        let body = json!({ "model": "capability-model", "input": "continue" });
        let context = RequestContext::new_with_peer_addr(
            &state,
            &body,
            &headers,
            AppType::Codex,
            "Codex",
            "codex",
            "/v1/responses",
            Some("127.0.0.1:15721".parse().expect("loopback peer")),
        )
        .await
        .expect("create role request context");

        assert_eq!(
            context
                .route_plan
                .attempts
                .iter()
                .map(|attempt| attempt.provider.id.as_str())
                .collect::<Vec<_>>(),
            vec!["provider-b", "provider-a"]
        );
        assert!(!context.route_plan.sync_logical_target);
        assert_eq!(context.role_route_owner_id.as_deref(), Some("provider-a"));
    }

    #[test]
    fn extract_model_with_action() {
        assert_eq!(
            extract_gemini_model_from_path("/v1beta/models/gemini-pro:generateContent").as_deref(),
            Some("gemini-pro"),
        );
    }

    #[test]
    fn extract_model_with_dotted_version() {
        assert_eq!(
            extract_gemini_model_from_path("/v1beta/models/gemini-1.5-flash:streamGenerateContent")
                .as_deref(),
            Some("gemini-1.5-flash"),
        );
    }

    #[test]
    fn extract_model_without_action() {
        assert_eq!(
            extract_gemini_model_from_path("/v1/models/gemini-1.5-pro").as_deref(),
            Some("gemini-1.5-pro"),
        );
    }

    #[test]
    fn extract_model_with_proxy_prefix() {
        assert_eq!(
            extract_gemini_model_from_path("/gemini/v1beta/models/gemini-2.0-flash:countTokens")
                .as_deref(),
            Some("gemini-2.0-flash"),
        );
    }

    #[test]
    fn extract_model_with_query_string() {
        assert_eq!(
            extract_gemini_model_from_path("/v1beta/models/gemini-pro:generateContent?key=abc")
                .as_deref(),
            Some("gemini-pro"),
        );
    }

    #[test]
    fn extract_model_missing_segment() {
        assert_eq!(extract_gemini_model_from_path("/v1beta/operations"), None);
    }

    #[test]
    fn extract_model_trailing_models_segment() {
        // `/v1beta/models` (list endpoint) has no following segment → None.
        assert_eq!(extract_gemini_model_from_path("/v1beta/models"), None);
    }

    #[test]
    fn extract_model_get_with_query_only() {
        // GET /v1beta/models/<id>?key=... 无 action verb，仅靠 ':' 拆分会把 query 带进 model 名。
        // 修复后应该把 query 剥掉。
        assert_eq!(
            extract_gemini_model_from_path("/v1beta/models/gemini-pro?key=abc").as_deref(),
            Some("gemini-pro"),
        );
    }

    #[test]
    fn extract_model_get_with_proxy_prefix_and_query() {
        assert_eq!(
            extract_gemini_model_from_path("/gemini/v1beta/models/gemini-2.0-flash?key=abc")
                .as_deref(),
            Some("gemini-2.0-flash"),
        );
    }

    #[tokio::test]
    #[serial]
    async fn provider_retry_times_out_pending_200_body_when_auto_failover_is_disabled() {
        let _home = TestHome::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    let body = if attempt == 0 {
                        Body::from_stream(futures::stream::pending::<Result<Bytes, Infallible>>())
                    } else {
                        Body::from(
                            r#"{"id":"resp-retry-success","status":"completed","output":[]}"#,
                        )
                    };
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .expect("build pending body response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pending body upstream");
        let address = listener
            .local_addr()
            .expect("pending body upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve pending body upstream");
        });

        let db = Arc::new(Database::memory().expect("create handler context test database"));
        let mut provider = Provider::with_id(
            "pending-body-provider".to_string(),
            "Pending body provider".to_string(),
            json!({
                "base_url": format!("http://{address}"),
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            local_proxy_retry_policy: Some(LocalProxyRetryPolicy {
                enabled: Some(true),
                max_retries: 1,
                retry_delay_ms: 1,
                custom_messages: vec![],
                error_types: vec![LocalProxyRetryErrorType::Network],
            }),
            ..Default::default()
        });
        db.save_provider("codex", &provider)
            .expect("save pending body provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select pending body provider");
        let mut app_config = db
            .get_proxy_config_for_app("codex")
            .await
            .expect("load codex proxy config");
        app_config.auto_failover_enabled = false;
        app_config.non_streaming_timeout = 1;
        db.update_proxy_config_for_app(app_config)
            .await
            .expect("save codex timeout config");

        let state = build_proxy_state(db);
        let request_body = json!({
            "model": "gpt-5.6-sol",
            "input": "continue",
            "stream": false
        });
        let ctx = RequestContext::new(
            &state,
            &request_body,
            &HeaderMap::new(),
            AppType::Codex,
            "Codex",
            "codex",
        )
        .await
        .expect("create request context");
        let forwarder = ctx.create_forwarder(&state);
        let watched = tokio::time::timeout(
            Duration::from_secs(3),
            forwarder.forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                request_body,
                HeaderMap::new(),
                Extensions::new(),
                ctx.get_providers(),
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = match watched
            .expect("configured body timeout must prevent a 200 response from hanging")
        {
            Ok(result) => result,
            Err(error) => panic!(
                "the timeout should enter same-provider retry: {}",
                error.error
            ),
        };
        let response_body = result
            .response
            .bytes()
            .await
            .expect("successful retry body");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("resp-retry-success"));
    }

    #[tokio::test]
    #[serial]
    async fn provider_retry_times_out_pending_sse_first_chunk_when_auto_failover_is_disabled() {
        let _home = TestHome::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    let body = if attempt == 0 {
                        Body::from_stream(futures::stream::pending::<
                            Result<Bytes, Infallible>,
                        >())
                    } else {
                        Body::from(
                            "event: response.output_text.delta\n\
                             data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n\
                             event: response.completed\n\
                             data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n",
                        )
                    };
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(body)
                        .expect("build pending SSE response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pending SSE upstream");
        let address = listener.local_addr().expect("pending SSE upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve pending SSE upstream");
        });

        let db = Arc::new(Database::memory().expect("create handler context test database"));
        let mut provider = Provider::with_id(
            "pending-sse-provider".to_string(),
            "Pending SSE provider".to_string(),
            json!({
                "base_url": format!("http://{address}"),
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            local_proxy_retry_policy: Some(LocalProxyRetryPolicy {
                enabled: Some(true),
                max_retries: 1,
                retry_delay_ms: 1,
                custom_messages: vec![],
                error_types: vec![LocalProxyRetryErrorType::Network],
            }),
            ..Default::default()
        });
        db.save_provider("codex", &provider)
            .expect("save pending SSE provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select pending SSE provider");
        let mut app_config = db
            .get_proxy_config_for_app("codex")
            .await
            .expect("load codex proxy config");
        app_config.auto_failover_enabled = false;
        app_config.streaming_first_byte_timeout = 1;
        db.update_proxy_config_for_app(app_config)
            .await
            .expect("save codex first-byte timeout config");

        let state = build_proxy_state(db);
        let request_body = json!({
            "model": "gpt-5.6-sol",
            "input": "continue",
            "stream": true
        });
        let ctx = RequestContext::new(
            &state,
            &request_body,
            &HeaderMap::new(),
            AppType::Codex,
            "Codex",
            "codex",
        )
        .await
        .expect("create request context");
        let forwarder = ctx.create_forwarder(&state);
        let watched = tokio::time::timeout(
            Duration::from_secs(3),
            forwarder.forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                request_body,
                HeaderMap::new(),
                Extensions::new(),
                ctx.get_providers(),
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = match watched
            .expect("configured first-byte timeout must prevent a 200 SSE response from hanging")
        {
            Ok(result) => result,
            Err(error) => panic!(
                "the timeout should enter same-provider retry: {}",
                error.error
            ),
        };
        let response_body = result
            .response
            .bytes()
            .await
            .expect("successful retry SSE body");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("\"delta\":\"ok\""));
    }
}
