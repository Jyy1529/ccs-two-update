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
    body: &serde_json::Value,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> Result<Option<CodexRoleRouteHeaders>, ProxyError> {
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
    if !has_role_header && super::codex_auto_review::is_auto_review_model(app_type, body) {
        return Ok(None);
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
    /// 已通过 capability token 验证的角色配置拥有者，仅用于角色路由可观测性。
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
        Self::new_with_peer_addr(state, body, headers, app_type, tag, app_type_str, None).await
    }

    pub async fn new_with_peer_addr(
        state: &ProxyState,
        body: &serde_json::Value,
        headers: &HeaderMap,
        app_type: AppType,
        tag: &'static str,
        app_type_str: &'static str,
        peer_addr: Option<SocketAddr>,
    ) -> Result<Self, ProxyError> {
        let start_time = Instant::now();

        // 从数据库读取应用级代理配置（per-app）
        let app_config = state
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| ProxyError::DatabaseError(e.to_string()))?;

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

        let role_route = codex_role_route_for_request(&app_type, body, headers, peer_addr)?;
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
                // 使用共享的 ProviderRouter 选择 Provider（熔断器状态跨请求保持）
                // 注意：只在这里调用一次，结果传递给 forwarder，避免重复消耗 HalfOpen 名额
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
    /// - 故障转移开启：超时配置正常生效（0 表示禁用超时）
    /// - 故障转移关闭：超时配置不生效（全部传入 0）
    pub fn create_forwarder(&self, state: &ProxyState) -> RequestForwarder {
        let (non_streaming_timeout, first_byte_timeout, idle_timeout) =
            if self.route_plan.use_failover_timeouts {
                // 故障转移开启：使用配置的值（0 = 禁用超时）
                (
                    self.app_config.non_streaming_timeout as u64,
                    self.app_config.streaming_first_byte_timeout as u64,
                    self.app_config.streaming_idle_timeout as u64,
                )
            } else {
                // 故障转移关闭：不启用超时配置
                log::debug!(
                    "[{}] Failover disabled, timeout configs are bypassed",
                    self.tag
                );
                (0, 0, 0)
            };

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
        )
        .with_route_plan(&self.route_plan)
        .with_role_context(self.role_route_owner_id.clone(), self.request_model.clone())
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
        _ => ProxyError::DatabaseError(error.to_string()),
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
        codex_role_route_for_request, extract_gemini_model_from_path, map_provider_selection_error,
        parse_codex_role_route_headers, CodexRoleRoute,
    };
    use crate::app_config::AppType;
    use crate::error::AppError;
    use crate::proxy::ProxyError;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;

    #[test]
    fn codex_role_headers_require_a_complete_unique_triple() {
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

        let route = parse_codex_role_route_headers(&headers)
            .expect("valid role route headers")
            .expect("role route");
        assert_eq!(route.route, CodexRoleRoute::Frontend);
        assert_eq!(route.owner_provider_id, "provider-a");
        assert_eq!(route.token, "signed-token");

        let mut missing_owner = HeaderMap::new();
        missing_owner.insert(
            "x-cc-switch-role-route",
            HeaderValue::from_static("frontend"),
        );
        assert!(matches!(
            parse_codex_role_route_headers(&missing_owner),
            Err(ProxyError::InvalidRequest(_))
        ));

        let mut missing_token = HeaderMap::new();
        missing_token.insert(
            "x-cc-switch-role-route",
            HeaderValue::from_static("frontend"),
        );
        missing_token.insert(
            "x-cc-switch-role-owner",
            HeaderValue::from_static("provider-a"),
        );
        assert!(matches!(
            parse_codex_role_route_headers(&missing_token),
            Err(ProxyError::InvalidRequest(_))
        ));

        let mut duplicate_route = headers.clone();
        duplicate_route.append(
            "x-cc-switch-role-route",
            HeaderValue::from_static("frontend"),
        );
        assert!(matches!(
            parse_codex_role_route_headers(&duplicate_route),
            Err(ProxyError::InvalidRequest(_))
        ));

        let mut duplicate_token = headers.clone();
        duplicate_token.append(
            "x-cc-switch-role-token",
            HeaderValue::from_static("second-token"),
        );
        assert!(matches!(
            parse_codex_role_route_headers(&duplicate_token),
            Err(ProxyError::InvalidRequest(_))
        ));

        let mut combined_owner = headers;
        combined_owner.insert(
            "x-cc-switch-role-owner",
            HeaderValue::from_static("provider-a,provider-b"),
        );
        assert!(matches!(
            parse_codex_role_route_headers(&combined_owner),
            Err(ProxyError::InvalidRequest(_))
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
                HeaderValue::from_bytes(&[0xff]).expect("opaque header value"),
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
    fn invalid_role_selection_maps_to_invalid_request() {
        assert!(matches!(
            map_provider_selection_error(AppError::InvalidInput("stale token".to_string())),
            ProxyError::InvalidRequest(message) if message == "stale token"
        ));
    }

    #[test]
    fn auto_review_rejects_malformed_role_headers() {
        let mut malformed = HeaderMap::new();
        malformed.insert(
            "x-cc-switch-role-route",
            HeaderValue::from_static("unknown"),
        );

        assert!(matches!(
            codex_role_route_for_request(
                &AppType::Codex,
                &json!({ "model": "codex-auto-review" }),
                &malformed,
                None,
            ),
            Err(ProxyError::InvalidRequest(_))
        ));
        assert_eq!(
            codex_role_route_for_request(
                &AppType::Codex,
                &json!({ "model": "codex-auto-review" }),
                &HeaderMap::new(),
                None,
            )
            .expect("header-free auto review keeps its dedicated route"),
            None
        );
        assert!(matches!(
            codex_role_route_for_request(
                &AppType::Codex,
                &json!({ "model": "gpt-5.6-sol" }),
                &malformed,
                Some("127.0.0.1:15721".parse().expect("loopback address")),
            ),
            Err(ProxyError::InvalidRequest(_))
        ));
    }

    #[test]
    fn codex_role_routing_requires_a_loopback_peer() {
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
        let body = json!({ "model": "gpt-5.6-sol" });

        assert!(codex_role_route_for_request(
            &AppType::Codex,
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
                codex_role_route_for_request(&AppType::Codex, &body, &headers, peer_addr),
                Err(ProxyError::InvalidRequest(message)) if message.contains("loopback")
            ));
        }

        let mut token_only = HeaderMap::new();
        token_only.insert(
            "x-cc-switch-role-token",
            HeaderValue::from_static("signed-token"),
        );
        assert!(matches!(
            codex_role_route_for_request(&AppType::Codex, &body, &token_only, None),
            Err(ProxyError::InvalidRequest(message)) if message.contains("loopback")
        ));
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
}
