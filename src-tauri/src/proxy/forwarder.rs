//! 请求转发器
//!
//! 负责将请求转发到上游Provider，支持故障转移

use super::hyper_client::ProxyResponse;
use super::{
    body_filter::filter_private_params_with_whitelist,
    content_encoding::{decompress_body, decompress_body_limited, get_content_encoding},
    error::*,
    failover_switch::FailoverSwitchManager,
    json_canonical::{canonicalize_value, short_value_hash},
    log_codes::fwd as log_fwd,
    provider_router::{ProviderRequestPermit, ProviderRouter},
    providers::{
        codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore, get_adapter,
        AuthInfo, AuthStrategy, ProviderAdapter, ProviderType,
    },
    thinking_budget_rectifier::{rectify_thinking_budget, should_rectify_thinking_budget},
    thinking_rectifier::{
        normalize_thinking_type, rectify_anthropic_request, should_rectify_thinking_signature,
    },
    types::{CopilotOptimizerConfig, OptimizerConfig, ProxyStatus, RectifierConfig},
    ProxyError,
};
use crate::commands::{CodexOAuthState, CopilotAuthState, XaiOAuthState};
use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::proxy::providers::copilot_auth::CopilotAuthManager;
use crate::proxy::providers::xai_oauth_auth::XaiOAuthManager;
use crate::{
    app_config::AppType,
    provider::{LocalProxyRequestOverrides, Provider},
};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use http::Extensions;
use serde_json::Value;
use std::sync::Arc;
use tauri::Manager;
use tokio::sync::RwLock;

const PROXY_AUTH_PLACEHOLDER: &str = "PROXY_MANAGED";
const DEFAULT_UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 256 * 1024;

struct BoundedBody {
    bytes: Bytes,
    truncated: bool,
}

async fn collect_body_prefix(
    response: ProxyResponse,
    limit: usize,
) -> Result<BoundedBody, ProxyError> {
    let sentinel_limit = limit.saturating_add(1);
    let mut stream = Box::pin(response.bytes_stream());
    let mut body = BytesMut::with_capacity(limit.min(8 * 1024));

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            ProxyError::ForwardFailed(format!("Failed to read response body: {error}"))
        })?;
        let remaining = sentinel_limit.saturating_sub(body.len());
        let take = chunk.len().min(remaining);
        body.extend_from_slice(&chunk[..take]);
        if body.len() > limit {
            body.truncate(limit);
            return Ok(BoundedBody {
                bytes: body.freeze(),
                truncated: true,
            });
        }
    }

    Ok(BoundedBody {
        bytes: body.freeze(),
        truncated: false,
    })
}

fn bounded_error_body_text(bytes: Vec<u8>, truncated: bool) -> Option<String> {
    if !truncated {
        return String::from_utf8(bytes).ok();
    }

    let valid_end = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return None,
    };
    String::from_utf8(bytes[..valid_end].to_vec()).ok()
}

#[derive(Clone, Copy)]
struct PreOutputDeadline {
    configured: std::time::Duration,
    at: Option<tokio::time::Instant>,
}

impl PreOutputDeadline {
    fn new(configured: std::time::Duration) -> Self {
        Self {
            configured,
            at: (!configured.is_zero()).then(|| tokio::time::Instant::now() + configured),
        }
    }

    fn is_enabled(self) -> bool {
        self.at.is_some()
    }

    fn timeout_error(self, phase: &str) -> ProxyError {
        let configured = if self.configured.subsec_millis() == 0 {
            format!("{}s", self.configured.as_secs())
        } else {
            format!("{}ms", self.configured.as_millis())
        };
        ProxyError::Timeout(format!("{phase}超时（输出前总预算 {configured} 已耗尽）"))
    }

    fn check(self, phase: &str) -> Result<(), ProxyError> {
        if self.at.is_some_and(|at| at <= tokio::time::Instant::now()) {
            return Err(self.timeout_error(phase));
        }
        Ok(())
    }

    fn remaining_or(
        self,
        fallback: std::time::Duration,
        phase: &str,
    ) -> Result<std::time::Duration, ProxyError> {
        let Some(at) = self.at else {
            return Ok(fallback);
        };
        at.checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| self.timeout_error(phase))
    }

    async fn wait<F, T>(self, phase: &str, future: F) -> Result<T, ProxyError>
    where
        F: std::future::Future<Output = T>,
    {
        let Some(at) = self.at else {
            return Ok(future.await);
        };
        if at <= tokio::time::Instant::now() {
            return Err(self.timeout_error(phase));
        }
        tokio::time::timeout_at(at, future)
            .await
            .map_err(|_| self.timeout_error(phase))
    }

    async fn yield_and_check(self, phase: &str) -> Result<(), ProxyError> {
        self.wait(phase, tokio::task::yield_now()).await?;
        self.check(phase)
    }

    async fn wait_upstream_error_body<F, T>(self, status: u16, future: F) -> Result<T, ProxyError>
    where
        F: std::future::Future<Output = T>,
    {
        let Some(at) = self.at else {
            return Ok(future.await);
        };
        if at <= tokio::time::Instant::now() {
            return Err(ProxyError::UpstreamBodyTimeout {
                status,
                timeout_seconds: self.configured.as_secs(),
            });
        }
        tokio::time::timeout_at(at, future)
            .await
            .map_err(|_| ProxyError::UpstreamBodyTimeout {
                status,
                timeout_seconds: self.configured.as_secs(),
            })
    }
}

fn validate_codex_official_authorization(headers: &http::HeaderMap) -> Result<(), ProxyError> {
    let authorization = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    match authorization {
        None | Some("") => Err(ProxyError::AuthError(
            "Codex 官方登录不可用，请先在 Codex 中完成 ChatGPT 登录".to_string(),
        )),
        Some(value) if value.contains(PROXY_AUTH_PLACEHOLDER) => Err(ProxyError::AuthError(
            "已切换到 OpenAI 官方供应商，请重启 Codex 或新建会话以加载官方登录配置".to_string(),
        )),
        Some(_) => Ok(()),
    }
}

pub struct ForwardResult {
    pub response: ProxyResponse,
    pub provider: Provider,
    pub claude_api_format: Option<String>,
    /// 实际发往上游的模型名（路由接管/模型映射后的真值）。
    ///
    /// usage 归因不能依赖 ctx.request_model（映射前的客户端别名）：上游响应
    /// 缺失 model 或回显别名时，接管流量会被记成 claude-* 并按其定价计费。
    pub outbound_model: Option<String>,
    /// 活跃连接 RAII guard：随响应一起流转到 response_processor / handle_claude_transform，
    /// 最终被 move 进流式 body future（或非流式响应作用域），覆盖整个响应生命周期。
    pub(crate) connection_guard: Option<ActiveConnectionGuard>,
}

pub struct ForwardError {
    pub error: ProxyError,
    pub provider: Option<Provider>,
}

/// 活跃连接 RAII guard
///
/// 构造时把 `ProxyStatus.active_connections` +1；Drop 时在 tokio runtime 上调度
/// 一个异步任务执行 -1，从而支持把 guard move 进流式 body future（stream 自然结束
/// 时 guard 与 future 一起 drop）。
///
/// 设计动机：之前在 `forward_with_retry` 出口处同步 -1，但流式响应的 body 实际
/// 在 `create_logged_passthrough_stream` 内还会继续 yield 字节流，导致 UI 的
/// `active_connections` 计数过早归零。RAII guard 让"减量"由 Rust 类型系统驱动，
/// 不需要每条出口路径都手动调用。
pub(crate) struct ActiveConnectionGuard {
    status: Arc<RwLock<ProxyStatus>>,
}

impl ActiveConnectionGuard {
    pub(crate) async fn acquire(status: Arc<RwLock<ProxyStatus>>) -> Self {
        {
            let mut s = status.write().await;
            s.active_connections = s.active_connections.saturating_add(1);
        }
        Self { status }
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        // Drop 不能 await：把减量操作调度到 tokio runtime
        let status = self.status.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut s = status.write().await;
                s.active_connections = s.active_connections.saturating_sub(1);
            });
        }
        // 没有 runtime 时静默丢失计数（仅 UI 展示用，可接受最终一致性）
    }
}

pub struct RequestForwarder {
    /// 共享的 ProviderRouter（持有熔断器状态）
    router: Arc<ProviderRouter>,
    status: Arc<RwLock<ProxyStatus>>,
    current_providers: Arc<RwLock<std::collections::HashMap<String, (String, String)>>>,
    gemini_shadow: Arc<GeminiShadowStore>,
    codex_chat_history: Arc<CodexChatHistoryStore>,
    /// 故障转移切换管理器
    failover_manager: Arc<FailoverSwitchManager>,
    /// AppHandle，用于发射事件和更新托盘
    app_handle: Option<tauri::AppHandle>,
    /// 请求开始时的"当前供应商 ID"（用于判断是否需要同步 UI/托盘）
    current_provider_id_at_start: String,
    /// 代理会话 ID（用于 Gemini Native shadow replay）
    session_id: String,
    /// Session ID 是否由客户端提供；生成值不能作为上游缓存身份。
    session_client_provided: bool,
    /// 整流器配置
    rectifier_config: RectifierConfig,
    /// 优化器配置
    optimizer_config: OptimizerConfig,
    /// Copilot 优化器配置
    copilot_optimizer_config: CopilotOptimizerConfig,
    /// 非流式请求超时（秒）
    non_streaming_timeout: std::time::Duration,
    /// 流式请求响应头等待超时（秒）
    streaming_first_byte_timeout: std::time::Duration,
    /// 单个客户端请求最多尝试的 provider 数。
    ///
    /// 由 `AppProxyConfig.max_retries` (UI: "请求失败时的重试次数, 0-10") 派生：
    /// `max_attempts = max_retries + 1`，所以 max_retries=0 表示仅尝试一家、
    /// max_retries=3（默认）表示最多 4 家。loop 同时受 providers.len() 自然限制。
    max_attempts: usize,
    /// 全局 Provider 自动重试开关的请求级快照。
    provider_retry_enabled: bool,
    sync_logical_target: bool,
    bypass_single_provider_circuit_breaker: bool,
    outbound_model_overrides: std::collections::HashMap<String, String>,
    role_route_owner_id: Option<String>,
    role_capability_model: Option<String>,
}

impl RequestForwarder {
    /// 预防式 media 降级：发送前对 text-only 模型把图片块替换为标记。
    ///
    /// 受 `enabled && request_media_fallback` 管辖；其中"启发式模型名单预测"
    /// 再受 `request_media_heuristic` 单独管辖（显式声明 text-only 始终生效）。
    /// 返回被替换的图片块数量（0 = 未触发或开关关闭）。
    fn apply_media_prevention(&self, body: &mut Value, provider: &Provider) -> usize {
        if !(self.rectifier_config.enabled && self.rectifier_config.request_media_fallback) {
            return 0;
        }
        let replaced_images = super::media_sanitizer::replace_images_for_text_only_model(
            body,
            provider,
            self.rectifier_config.request_media_heuristic,
        );
        if replaced_images > 0 {
            let model = body.get("model").and_then(Value::as_str).unwrap_or("");
            log::info!(
                "[Media] Replaced {replaced_images} image block(s) with {} for text-only provider={}, model={}",
                super::media_sanitizer::UNSUPPORTED_IMAGE_MARKER,
                provider.id,
                model
            );
        }
        replaced_images
    }

    /// 反应式 media 重试判定：上游因图片输入报错后，是否应替换图片块并对同一供应商重试一次。
    ///
    /// 受 `enabled && request_media_fallback` 管辖；不涉及 `request_media_heuristic`——
    /// 这里是上游"实测"错误后的纯恢复，不是预测，故启发式开关与它无关。
    fn media_retry_should_trigger(
        &self,
        adapter_name: &str,
        already_retried: bool,
        provider_body: &Value,
        error: &ProxyError,
    ) -> bool {
        matches!(adapter_name, "Claude" | "Codex")
            && self.rectifier_config.enabled
            && self.rectifier_config.request_media_fallback
            && !already_retried
            && super::media_sanitizer::contains_image_blocks(provider_body)
            && super::media_sanitizer::is_unsupported_image_error(error)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: Arc<ProviderRouter>,
        non_streaming_timeout: u64,
        status: Arc<RwLock<ProxyStatus>>,
        current_providers: Arc<RwLock<std::collections::HashMap<String, (String, String)>>>,
        gemini_shadow: Arc<GeminiShadowStore>,
        codex_chat_history: Arc<CodexChatHistoryStore>,
        failover_manager: Arc<FailoverSwitchManager>,
        app_handle: Option<tauri::AppHandle>,
        current_provider_id_at_start: String,
        session_id: String,
        session_client_provided: bool,
        streaming_first_byte_timeout: u64,
        _streaming_idle_timeout: u64,
        rectifier_config: RectifierConfig,
        optimizer_config: OptimizerConfig,
        copilot_optimizer_config: CopilotOptimizerConfig,
        max_retries: u32,
        provider_retry_enabled: bool,
    ) -> Self {
        // max_retries 是「失败后重试次数」语义，attempt 上限 = retries + 1。
        // saturating_add 防止 u32::MAX + 1 溢出。
        let max_attempts = (max_retries as usize).saturating_add(1);
        Self {
            router,
            status,
            current_providers,
            gemini_shadow,
            codex_chat_history,
            failover_manager,
            app_handle,
            current_provider_id_at_start,
            session_id,
            session_client_provided,
            rectifier_config,
            optimizer_config,
            copilot_optimizer_config,
            non_streaming_timeout: std::time::Duration::from_secs(non_streaming_timeout),
            streaming_first_byte_timeout: std::time::Duration::from_secs(
                streaming_first_byte_timeout,
            ),
            max_attempts,
            provider_retry_enabled,
            sync_logical_target: true,
            bypass_single_provider_circuit_breaker: true,
            outbound_model_overrides: std::collections::HashMap::new(),
            role_route_owner_id: None,
            role_capability_model: None,
        }
    }

    pub fn with_route_plan(
        mut self,
        plan: &crate::proxy::provider_router::ProviderRoutePlan,
    ) -> Self {
        self.sync_logical_target = plan.sync_logical_target;
        self.bypass_single_provider_circuit_breaker = plan.bypass_single_provider_circuit_breaker;
        self.outbound_model_overrides = plan
            .attempts
            .iter()
            .filter_map(|attempt| {
                attempt
                    .outbound_model_override
                    .as_ref()
                    .map(|model| (attempt.provider.id.clone(), model.clone()))
            })
            .collect();
        if !plan.sync_logical_target {
            self.max_attempts = plan.attempts.len().max(1);
        }
        self
    }

    pub fn with_role_context(
        mut self,
        owner_provider_id: Option<&str>,
        capability_model: &str,
    ) -> Self {
        self.role_route_owner_id = owner_provider_id.map(ToString::to_string);
        self.role_capability_model = owner_provider_id.map(|_| capability_model.to_string());
        self
    }

    fn log_role_terminal(
        &self,
        provider: &Provider,
        outbound_model: &str,
        route_fallback: bool,
        terminal: &str,
    ) {
        let Some(owner_provider_id) = self.role_route_owner_id.as_deref() else {
            return;
        };
        log::info!(
            "[CodexRoleRoute] role=frontend owner_provider={} target_provider={} capability_model={} outbound_model={} route_fallback={} sync_logical_target={} terminal={}",
            owner_provider_id,
            provider.id,
            self.role_capability_model.as_deref().unwrap_or("unknown"),
            outbound_model,
            route_fallback,
            self.sync_logical_target,
            terminal
        );
    }

    fn pre_output_deadline(&self, request_is_streaming: bool) -> PreOutputDeadline {
        let timeout = if request_is_streaming {
            self.streaming_first_byte_timeout
        } else {
            self.non_streaming_timeout
        };
        PreOutputDeadline::new(timeout)
    }

    async fn record_success_result(
        &self,
        provider_id: &str,
        app_type: &str,
        provider_permit: &mut Option<ProviderRequestPermit>,
    ) {
        if provider_permit
            .as_ref()
            .is_some_and(|permit| permit.used_half_open_permit())
        {
            if let Err(e) = self
                .record_half_open_result_cancellation_safe(
                    provider_permit,
                    provider_id,
                    app_type,
                    true,
                    None,
                )
                .await
            {
                log::warn!(
                    "[{app_type}] 记录 Provider 成功结果失败: provider_id={provider_id}, error={e}"
                );
            }
            return;
        }

        let router = self.router.clone();
        let provider_id = provider_id.to_string();
        let app_type = app_type.to_string();
        tokio::spawn(async move {
            if let Err(e) = router
                .record_result(&provider_id, &app_type, false, true, None)
                .await
            {
                log::warn!(
                    "[{app_type}] 异步记录 Provider 成功结果失败: provider_id={provider_id}, error={e}"
                );
            }
        });
    }

    async fn record_deferred_role_stream_result(
        router: &Arc<ProviderRouter>,
        proxy_status: &Arc<RwLock<ProxyStatus>>,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        terminal_error: Option<String>,
    ) {
        let succeeded = terminal_error.is_none();
        if let Err(error) = router
            .record_result(
                provider_id,
                app_type,
                used_half_open_permit,
                succeeded,
                terminal_error.clone(),
            )
            .await
        {
            log::warn!(
                "[{app_type}] Failed to record role-routed stream result: provider_id={provider_id}, error={error}"
            );
        }

        let mut status = proxy_status.write().await;
        if succeeded {
            status.success_requests += 1;
            status.last_error = None;
        } else {
            status.failed_requests += 1;
            status.last_error = terminal_error;
        }
        if status.total_requests > 0 {
            status.success_rate =
                (status.success_requests as f32 / status.total_requests as f32) * 100.0;
        }
    }

    fn defer_role_stream_result(
        &self,
        response: ProxyResponse,
        provider: &Provider,
        app_type: &str,
        outbound_model: &str,
        route_fallback: bool,
        provider_permit: &mut Option<ProviderRequestPermit>,
    ) -> (ProxyResponse, bool) {
        if self.sync_logical_target || !response.is_sse() {
            return (response, false);
        }

        let status_code = response.status();
        let headers = response.headers().clone();
        let mut upstream = Box::pin(response.bytes_stream());
        let router = Arc::clone(&self.router);
        let proxy_status = Arc::clone(&self.status);
        let provider_id = provider.id.clone();
        let app_type = app_type.to_string();
        let mut permit = provider_permit.take();
        let role_owner_provider_id = self.role_route_owner_id.clone();
        let role_capability_model = self.role_capability_model.clone();
        let outbound_model = outbound_model.to_string();

        let monitored = async_stream::stream! {
            let mut parse_buffer = String::new();
            let mut utf8_remainder = Vec::new();
            let mut terminal_error: Option<String> = None;
            let mut terminal_recorded = false;

            while let Some(next) = upstream.next().await {
                match next {
                    Ok(chunk) => {
                        crate::proxy::sse::append_utf8_safe(
                            &mut parse_buffer,
                            &mut utf8_remainder,
                            &chunk,
                        );
                        while let Some(block) = crate::proxy::sse::take_sse_block(&mut parse_buffer) {
                            if !terminal_recorded {
                                if let Some(terminal) = inspect_role_stream_terminal(&block) {
                                    terminal_error = match terminal {
                                        RoleStreamTerminal::Success => None,
                                        RoleStreamTerminal::Failure(error) => Some(error),
                                    };
                                    let used_half_open_permit = permit
                                        .take()
                                        .map(ProviderRequestPermit::into_used_half_open_permit)
                                        .unwrap_or(false);
                                    let terminal_label = if terminal_error.is_none() {
                                        "stream_succeeded"
                                    } else {
                                        "stream_failed"
                                    };
                                    Self::record_deferred_role_stream_result(
                                        &router,
                                        &proxy_status,
                                        &provider_id,
                                        &app_type,
                                        used_half_open_permit,
                                        terminal_error.clone(),
                                    )
                                    .await;
                                    if let Some(owner_provider_id) = role_owner_provider_id.as_deref() {
                                        log::info!(
                                            "[CodexRoleRoute] role=frontend owner_provider={} target_provider={} capability_model={} outbound_model={} route_fallback={} sync_logical_target=false terminal={}",
                                            owner_provider_id,
                                            provider_id,
                                            role_capability_model.as_deref().unwrap_or("unknown"),
                                            outbound_model,
                                            route_fallback,
                                            terminal_label
                                        );
                                    }
                                    terminal_recorded = true;
                                }
                            }
                        }
                        yield Ok(chunk);
                    }
                    Err(error) => {
                        if terminal_recorded {
                            yield Err(error);
                            return;
                        }
                        terminal_error.get_or_insert_with(|| {
                            format!("Role-routed stream read failed after output: {error}")
                        });
                        let used_half_open_permit = permit
                            .take()
                            .map(ProviderRequestPermit::into_used_half_open_permit)
                            .unwrap_or(false);
                        Self::record_deferred_role_stream_result(
                            &router,
                            &proxy_status,
                            &provider_id,
                            &app_type,
                            used_half_open_permit,
                            terminal_error.clone(),
                        )
                        .await;
                        if let Some(owner_provider_id) = role_owner_provider_id.as_deref() {
                            log::info!(
                                "[CodexRoleRoute] role=frontend owner_provider={} target_provider={} capability_model={} outbound_model={} route_fallback={} sync_logical_target=false terminal=stream_read_failed",
                                owner_provider_id,
                                provider_id,
                                role_capability_model.as_deref().unwrap_or("unknown"),
                                outbound_model,
                                route_fallback
                            );
                        }
                        yield Err(error);
                        return;
                    }
                }
            }

            if terminal_recorded {
                return;
            }

            if terminal_error.is_none() && !parse_buffer.trim().is_empty() {
                if let Some(Err(error)) = inspect_responses_start_event(parse_buffer.trim()) {
                    terminal_error = Some(error.to_string());
                }
            }

            let used_half_open_permit = permit
                .take()
                .map(ProviderRequestPermit::into_used_half_open_permit)
                .unwrap_or(false);
            let terminal = if terminal_error.is_none() {
                "stream_succeeded"
            } else {
                "stream_failed"
            };
            Self::record_deferred_role_stream_result(
                &router,
                &proxy_status,
                &provider_id,
                &app_type,
                used_half_open_permit,
                terminal_error,
            )
            .await;
            if let Some(owner_provider_id) = role_owner_provider_id.as_deref() {
                log::info!(
                    "[CodexRoleRoute] role=frontend owner_provider={} target_provider={} capability_model={} outbound_model={} route_fallback={} sync_logical_target=false terminal={}",
                    owner_provider_id,
                    provider_id,
                    role_capability_model.as_deref().unwrap_or("unknown"),
                    outbound_model,
                    route_fallback,
                    terminal
                );
            }
        };

        (
            ProxyResponse::streamed(status_code, headers, monitored),
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn finalize_provider_success(
        &self,
        response: ProxyResponse,
        provider: &Provider,
        app_type_str: &str,
        claude_api_format: Option<String>,
        outbound_model: Option<String>,
        provider_permit: &mut Option<ProviderRequestPermit>,
        route_fallback: bool,
    ) -> ForwardResult {
        let outbound_model_label = outbound_model.as_deref().unwrap_or("unknown");
        let (response, stream_result_deferred) = self.defer_role_stream_result(
            response,
            provider,
            app_type_str,
            outbound_model_label,
            route_fallback,
            provider_permit,
        );

        if !stream_result_deferred {
            self.record_success_result(&provider.id, app_type_str, provider_permit)
                .await;

            if self.sync_logical_target {
                let mut current_providers = self.current_providers.write().await;
                current_providers.insert(
                    app_type_str.to_string(),
                    (provider.id.clone(), provider.name.clone()),
                );
            }

            {
                let mut status = self.status.write().await;
                status.success_requests += 1;
                status.last_error = None;
                let should_switch = self.sync_logical_target
                    && self.current_provider_id_at_start.as_str() != provider.id.as_str();
                if should_switch {
                    status.failover_count += 1;

                    let fm = self.failover_manager.clone();
                    let ah = self.app_handle.clone();
                    let pid = provider.id.clone();
                    let pname = provider.name.clone();
                    let at = app_type_str.to_string();

                    tokio::spawn(async move {
                        let _ = fm.try_switch(ah.as_ref(), &at, &pid, &pname).await;
                    });
                }
                if status.total_requests > 0 {
                    status.success_rate =
                        (status.success_requests as f32 / status.total_requests as f32) * 100.0;
                }
            }

            self.log_role_terminal(provider, outbound_model_label, route_fallback, "succeeded");
        }

        ForwardResult {
            response,
            provider: provider.clone(),
            claude_api_format,
            outbound_model,
            connection_guard: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_provider_body_with_retries(
        &self,
        app_type: &AppType,
        method: &http::Method,
        provider: &Provider,
        endpoint: &str,
        provider_body: &mut Value,
        headers: &axum::http::HeaderMap,
        extensions: &Extensions,
        adapter: &dyn ProviderAdapter,
        provider_retry_policy: Option<&super::provider_retry::ResolvedRetryPolicy>,
        provider_retries: &mut usize,
        provider_retry_exhausted_with_match: &mut bool,
        app_type_str: &str,
    ) -> Result<(ProxyResponse, Option<String>, Option<String>), ProxyError> {
        *provider_retry_exhausted_with_match = false;

        loop {
            let result = self
                .forward(
                    app_type,
                    method,
                    provider,
                    endpoint,
                    provider_body,
                    headers,
                    extensions,
                    adapter,
                )
                .await;

            let result = match (result, provider_retry_policy) {
                (Ok((response, claude_api_format, outbound_model, deadline)), Some(policy)) => {
                    let request_is_streaming =
                        is_streaming_request(endpoint, provider_body, headers);
                    self.validate_provider_retry_success_response(
                        response,
                        request_is_streaming,
                        policy,
                        deadline,
                    )
                    .await
                    .map(|response| (response, claude_api_format, outbound_model))
                }
                (Ok((response, claude_api_format, outbound_model, _)), None) => {
                    Ok((response, claude_api_format, outbound_model))
                }
                (Err(error), _) => Err(error),
            };

            let Err(error) = &result else {
                return result;
            };

            if let Some(model) = super::codex_auto_review::fallback_after_error(
                app_type,
                endpoint,
                provider,
                provider_body,
                error,
            ) {
                super::codex_auto_review::apply_fallback(provider_body, &model);
                log::info!(
                    "[CodexAutoReview] native model unavailable; retrying same provider with {model}"
                );
                continue;
            }

            if let Some(policy) = provider_retry_policy {
                if let Some(reason) = policy.match_error(error) {
                    if policy.allows_retry(*provider_retries) {
                        let Some(next_retry) = provider_retries.checked_add(1) else {
                            log::error!(
                                "[ProviderRetry] retry counter overflowed; stopping same-provider retries"
                            );
                            return result;
                        };
                        *provider_retries = next_retry;
                        let delay_ms = policy.retry_delay_ms();
                        let model = retry_log_model(app_type, endpoint, provider_body);
                        if policy.should_log_attempt(*provider_retries) {
                            if let Some(owner_provider_id) = self.role_route_owner_id.as_deref() {
                                log::warn!(
                                    "[ProviderRetry] app={} role=frontend owner_provider={} provider={} capability_model={} model={} reason={} retry_attempt={} retry_limit={} delay_ms={}",
                                    app_type_str,
                                    owner_provider_id,
                                    provider.name,
                                    self.role_capability_model.as_deref().unwrap_or("unknown"),
                                    model,
                                    reason,
                                    *provider_retries,
                                    policy.retry_limit_label(),
                                    delay_ms
                                );
                            } else {
                                log::warn!(
                                    "[ProviderRetry] app={} provider={} model={} reason={} retry_attempt={} retry_limit={} delay_ms={}",
                                    app_type_str,
                                    provider.name,
                                    model,
                                    reason,
                                    *provider_retries,
                                    policy.retry_limit_label(),
                                    delay_ms
                                );
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    *provider_retry_exhausted_with_match = true;
                }
            }

            return result;
        }
    }

    async fn record_half_open_result_cancellation_safe(
        &self,
        provider_permit: &mut Option<ProviderRequestPermit>,
        provider_id: &str,
        app_type: &str,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), crate::error::AppError> {
        let permit = provider_permit.take();
        let router = self.router.clone();
        let provider_id = provider_id.to_string();
        let app_type = app_type.to_string();
        tokio::spawn(async move {
            let used_half_open_permit = permit
                .map(ProviderRequestPermit::into_used_half_open_permit)
                .unwrap_or(false);
            router
                .record_result(
                    &provider_id,
                    &app_type,
                    used_half_open_permit,
                    success,
                    error_msg,
                )
                .await
        })
        .await
        .map_err(|error| crate::error::AppError::Message(error.to_string()))?
    }

    /// 整流（thinking signature 或 budget）重试失败后的统一收尾。
    ///
    /// `None` 表示已记录熔断器、累积 `last_error`/`last_provider`，
    /// 调用方应 `continue` 让下一家 provider 继续故障转移；
    /// `Some(ForwardError)` 表示是客户端错误，没有 provider 能修复，
    /// 调用方应直接 `return` 把错误返回给客户端。
    #[allow(clippy::too_many_arguments)]
    async fn handle_rectifier_retry_failure(
        &self,
        retry_err: ProxyError,
        provider: &Provider,
        app_type_str: &str,
        provider_permit: &mut Option<ProviderRequestPermit>,
        provider_retry_exhausted_with_match: bool,
        rectifier_label: &str,
        last_error: &mut Option<ProxyError>,
        last_provider: &mut Option<Provider>,
    ) -> Option<ForwardError> {
        // Provider 错误：本家上游/网络确实出问题，下一家 provider 可能可用 → 继续故障转移。
        // 客户端错误：整流后请求仍违法，下一家也修不好 → 直接返回。
        let is_provider_error = provider_retry_exhausted_with_match
            || self.categorize_proxy_error(&retry_err, provider) == ErrorCategory::Retryable;

        if is_provider_error {
            let _ = if provider_permit
                .as_ref()
                .is_some_and(|permit| permit.used_half_open_permit())
            {
                self.record_half_open_result_cancellation_safe(
                    provider_permit,
                    &provider.id,
                    app_type_str,
                    false,
                    Some(retry_err.to_string()),
                )
                .await
            } else {
                self.router
                    .record_result(
                        &provider.id,
                        app_type_str,
                        false,
                        false,
                        Some(retry_err.to_string()),
                    )
                    .await
            };
            {
                let mut status = self.status.write().await;
                status.last_error = Some(format!(
                    "Provider {} {rectifier_label}重试失败: {}",
                    provider.name, retry_err
                ));
            }
            *last_error = Some(retry_err);
            *last_provider = Some(provider.clone());
            return None;
        }

        drop(provider_permit.take());
        let mut status = self.status.write().await;
        status.failed_requests += 1;
        status.last_error = Some(retry_err.to_string());
        if status.total_requests > 0 {
            status.success_rate =
                (status.success_requests as f32 / status.total_requests as f32) * 100.0;
        }
        Some(ForwardError {
            error: retry_err,
            provider: Some(provider.clone()),
        })
    }

    /// 转发请求（带故障转移）
    ///
    /// 这是 thin wrapper：在客户端请求维度记一次 `total_requests` / 调整
    /// `active_connections` / 刷新 `last_request_at`，无论 inner 走哪条出口路径，
    /// 出口处都会把 `active_connections` 回收。Per-attempt 维度（成功/失败/熔断
    /// 等）仍由 inner 内自行更新 `success_requests` / `failed_requests`。
    #[allow(clippy::too_many_arguments)]
    pub async fn forward_with_retry(
        &self,
        app_type: &AppType,
        method: http::Method,
        endpoint: &str,
        body: Value,
        headers: axum::http::HeaderMap,
        extensions: Extensions,
        providers: Vec<Provider>,
    ) -> Result<ForwardResult, ForwardError> {
        let guard = ActiveConnectionGuard::acquire(self.status.clone()).await;
        {
            let mut s = self.status.write().await;
            s.total_requests = s.total_requests.saturating_add(1);
            s.last_request_at = Some(chrono::Utc::now().to_rfc3339());
        }
        let result = self
            .forward_with_retry_inner(
                app_type, method, endpoint, body, headers, extensions, providers,
            )
            .await;
        // 把 guard 注入到 Ok 结果，让它随响应一起流转到 response_processor，
        // 在流式 body 的 future 内才真正 drop。
        // Err 路径：guard 在函数 scope 内随返回值落地时自动 drop。
        result.map(|mut fr| {
            fr.connection_guard = Some(guard);
            fr
        })
    }

    /// 实际转发逻辑（不包含客户端维度的入口/出口计数）
    ///
    /// # Arguments
    /// * `app_type` - 应用类型
    /// * `method` - 客户端请求的 HTTP 方法（透传给上游，支持 GET/POST 等）
    /// * `endpoint` - API 端点
    /// * `body` - 请求体
    /// * `headers` - 请求头
    /// * `providers` - 已选择的 Provider 列表（由 RequestContext 提供，避免重复调用 select_providers）
    #[allow(clippy::too_many_arguments)]
    async fn forward_with_retry_inner(
        &self,
        app_type: &AppType,
        method: http::Method,
        endpoint: &str,
        body: Value,
        headers: axum::http::HeaderMap,
        extensions: Extensions,
        providers: Vec<Provider>,
    ) -> Result<ForwardResult, ForwardError> {
        // 获取适配器
        let adapter = get_adapter(app_type);
        let app_type_str = app_type.as_str();

        if providers.is_empty() {
            return Err(ForwardError {
                error: ProxyError::NoAvailableProvider,
                provider: None,
            });
        }

        if let Some(owner_provider_id) = self.role_route_owner_id.as_deref() {
            log::info!(
                "[CodexRoleRoute] role=frontend owner_provider={} capability_model={} route_attempts={} sync_logical_target={}",
                owner_provider_id,
                self.role_capability_model.as_deref().unwrap_or("unknown"),
                providers.len(),
                self.sync_logical_target
            );
        }

        let mut last_error = None;
        let mut last_provider = None;
        let mut attempted_providers = 0usize;

        // 单 Provider 场景下跳过熔断器检查（故障转移关闭时）
        let bypass_circuit_breaker =
            self.bypass_single_provider_circuit_breaker && providers.len() == 1;

        // 依次尝试每个供应商
        for provider in providers.iter() {
            // 整流器重试标记：每个 provider 独立持有，避免标记跨 provider 短路故障转移
            // —— 首家 provider 整流后被 5xx/timeout 击落时，下家仍能用整流后的请求体走整流流程
            let mut rectifier_retried = false;
            let mut budget_rectifier_retried = false;
            let mut media_rectifier_retried = false;

            // 上限检查：尊重用户在 AppProxyConfig.max_retries 上配置的「重试次数」。
            // 放在熔断器 allow 检查之前，避免在已经超限时还占用 HalfOpen 探测名额。
            if attempted_providers >= self.max_attempts {
                log::warn!(
                    "[{app_type_str}] 已达最大尝试次数上限 ({}/{}), 停止故障转移",
                    attempted_providers,
                    self.max_attempts
                );
                break;
            }

            // 发起请求前先获取熔断器放行许可（HalfOpen 会占用探测名额）
            // 单 Provider 场景下跳过此检查，避免熔断器阻塞所有请求
            let mut provider_permit = if bypass_circuit_breaker {
                None
            } else {
                Some(
                    self.router
                        .allow_provider_request(&provider.id, app_type_str)
                        .await,
                )
            };

            if provider_permit
                .as_ref()
                .is_some_and(|permit| !permit.allowed())
            {
                continue;
            }
            // PRE-SEND 优化器：每个 provider 独立决定是否优化
            // clone body 以避免 Bedrock 优化字段泄漏到非 Bedrock provider（failover 场景）
            let mut provider_body =
                if self.optimizer_config.enabled && is_bedrock_provider(provider) {
                    let mut b = body.clone();
                    if self.optimizer_config.thinking_optimizer {
                        super::thinking_optimizer::optimize(&mut b, &self.optimizer_config);
                    }
                    if self.optimizer_config.cache_injection {
                        super::cache_injector::inject(&mut b, &self.optimizer_config);
                    }
                    b
                } else {
                    body.clone()
                };
            if let Some(model) = self.outbound_model_overrides.get(&provider.id) {
                provider_body["model"] = Value::String(model.clone());
            }
            let route_fallback = role_route_fallback(attempted_providers);

            let provider_retry_policy = super::provider_retry::resolve_retry_policy_with_global(
                app_type,
                &provider_body,
                provider,
                self.provider_retry_enabled,
            );
            if let Some(owner_provider_id) = self.role_route_owner_id.as_deref() {
                let outbound_model = provider_body
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let retry_limit = provider_retry_policy
                    .as_ref()
                    .map(|policy| policy.retry_limit_label())
                    .unwrap_or_else(|| "0".to_string());
                log::info!(
                    "[CodexRoleRoute] role=frontend owner_provider={} target_provider={} capability_model={} outbound_model={} provider_attempt={}/{} provider_retry=0/{} route_fallback={} sync_logical_target={}",
                    owner_provider_id,
                    provider.id,
                    self.role_capability_model.as_deref().unwrap_or("unknown"),
                    outbound_model,
                    attempted_providers + 1,
                    providers.len(),
                    retry_limit,
                    route_fallback,
                    self.sync_logical_target
                );
            }

            if let Some(model) = super::codex_auto_review::apply_initial_policy(
                app_type,
                endpoint,
                provider,
                &mut provider_body,
            ) {
                log::info!("[CodexAutoReview] force fallback: codex-auto-review -> {model}");
            }

            attempted_providers += 1;

            // 更新状态中的当前 Provider 信息（per-attempt 维度的标识）
            //
            // total_requests / last_request_at / active_connections 已由
            // forward_with_retry wrapper 在客户端请求维度统一处理，这里只刷
            // 新「正在尝试哪个 provider」的展示字段。
            if self.sync_logical_target {
                let mut status = self.status.write().await;
                status.current_provider = Some(provider.name.clone());
                status.current_provider_id = Some(provider.id.clone());
            }

            let mut provider_retries = 0usize;
            let mut provider_retry_exhausted_with_match = false;
            let forward_result = self
                .forward_provider_body_with_retries(
                    app_type,
                    &method,
                    provider,
                    endpoint,
                    &mut provider_body,
                    &headers,
                    &extensions,
                    adapter.as_ref(),
                    provider_retry_policy.as_ref(),
                    &mut provider_retries,
                    &mut provider_retry_exhausted_with_match,
                    app_type_str,
                )
                .await;

            match forward_result {
                Ok((response, claude_api_format, outbound_model)) => {
                    return Ok(self
                        .finalize_provider_success(
                            response,
                            provider,
                            app_type_str,
                            claude_api_format,
                            outbound_model,
                            &mut provider_permit,
                            route_fallback,
                        )
                        .await);
                }
                Err(e) => {
                    // 检测是否需要触发整流器（仅 Claude/ClaudeAuth 供应商）
                    let provider_type = ProviderType::from_app_type_and_config(app_type, provider);
                    let is_anthropic_provider = matches!(
                        provider_type,
                        ProviderType::Claude | ProviderType::ClaudeAuth
                    );
                    let mut signature_rectifier_non_retryable_client_error = false;

                    if self.media_retry_should_trigger(
                        adapter.name(),
                        media_rectifier_retried,
                        &provider_body,
                        &e,
                    ) {
                        let mut media_body = provider_body.clone();
                        let replaced_images =
                            super::media_sanitizer::replace_image_blocks_with_marker(
                                &mut media_body,
                            );

                        if replaced_images > 0 {
                            let _ = std::mem::replace(&mut media_rectifier_retried, true);
                            let model = media_body
                                .get("model")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            log::info!(
                                "[{app_type_str}] [Media] Upstream rejected image input; retrying provider={} model={} with {replaced_images} image block(s) replaced by {}",
                                provider.id,
                                model,
                                super::media_sanitizer::UNSUPPORTED_IMAGE_MARKER
                            );

                            let mut media_retry_exhausted_with_match = false;
                            match self
                                .forward_provider_body_with_retries(
                                    app_type,
                                    &method,
                                    provider,
                                    endpoint,
                                    &mut media_body,
                                    &headers,
                                    &extensions,
                                    adapter.as_ref(),
                                    provider_retry_policy.as_ref(),
                                    &mut provider_retries,
                                    &mut media_retry_exhausted_with_match,
                                    app_type_str,
                                )
                                .await
                            {
                                Ok((response, claude_api_format, outbound_model)) => {
                                    log::info!(
                                        "[{app_type_str}] [Media] Unsupported-image retry succeeded"
                                    );
                                    return Ok(self
                                        .finalize_provider_success(
                                            response,
                                            provider,
                                            app_type_str,
                                            claude_api_format,
                                            outbound_model,
                                            &mut provider_permit,
                                            route_fallback,
                                        )
                                        .await);
                                }
                                Err(retry_err) => {
                                    log::warn!(
                                        "[{app_type_str}] [Media] Unsupported-image retry still failed: {retry_err}"
                                    );
                                    if let Some(err) = self
                                        .handle_rectifier_retry_failure(
                                            retry_err,
                                            provider,
                                            app_type_str,
                                            &mut provider_permit,
                                            media_retry_exhausted_with_match,
                                            "media 降级",
                                            &mut last_error,
                                            &mut last_provider,
                                        )
                                        .await
                                    {
                                        return Err(err);
                                    }
                                    continue;
                                }
                            }
                        }
                    }

                    if is_anthropic_provider {
                        let error_message = extract_error_message(&e);
                        if should_rectify_thinking_signature(
                            error_message.as_deref(),
                            &self.rectifier_config,
                        ) {
                            // 已经重试过：直接返回错误（不可重试客户端错误）
                            if rectifier_retried {
                                log::warn!("[{app_type_str}] [RECT-005] 整流器已触发过，不再重试");
                                // 释放 HalfOpen permit（不记录熔断器，这是客户端兼容性问题）
                                drop(provider_permit.take());
                                let mut status = self.status.write().await;
                                status.failed_requests += 1;
                                status.last_error = Some(e.to_string());
                                if status.total_requests > 0 {
                                    status.success_rate = (status.success_requests as f32
                                        / status.total_requests as f32)
                                        * 100.0;
                                }
                                return Err(ForwardError {
                                    error: e,
                                    provider: Some(provider.clone()),
                                });
                            }

                            // 首次触发：整流请求体
                            let rectified = rectify_anthropic_request(&mut provider_body);

                            // 整流未生效：继续尝试 budget 整流路径，避免误判后短路
                            if !rectified.applied {
                                log::warn!(
                                    "[{app_type_str}] [RECT-006] thinking 签名整流器触发但无可整流内容，继续检查 budget；若 budget 也未命中则按客户端错误返回"
                                );
                                signature_rectifier_non_retryable_client_error = true;
                            } else {
                                log::info!(
                                    "[{}] [RECT-001] thinking 签名整流器触发, 移除 {} thinking blocks, {} redacted_thinking blocks, {} signature fields",
                                    app_type_str,
                                    rectified.removed_thinking_blocks,
                                    rectified.removed_redacted_thinking_blocks,
                                    rectified.removed_signature_fields
                                );

                                // 标记已重试（当前逻辑下重试后必定 return，保留标记以备将来扩展）
                                let _ = std::mem::replace(&mut rectifier_retried, true);

                                // 使用同一供应商重试（不计入熔断器）
                                let mut signature_retry_exhausted_with_match = false;
                                match self
                                    .forward_provider_body_with_retries(
                                        app_type,
                                        &method,
                                        provider,
                                        endpoint,
                                        &mut provider_body,
                                        &headers,
                                        &extensions,
                                        adapter.as_ref(),
                                        provider_retry_policy.as_ref(),
                                        &mut provider_retries,
                                        &mut signature_retry_exhausted_with_match,
                                        app_type_str,
                                    )
                                    .await
                                {
                                    Ok((response, claude_api_format, outbound_model)) => {
                                        log::info!("[{app_type_str}] [RECT-002] 整流重试成功");
                                        return Ok(self
                                            .finalize_provider_success(
                                                response,
                                                provider,
                                                app_type_str,
                                                claude_api_format,
                                                outbound_model,
                                                &mut provider_permit,
                                                route_fallback,
                                            )
                                            .await);
                                    }
                                    Err(retry_err) => {
                                        log::warn!(
                                            "[{app_type_str}] [RECT-003] 整流重试仍失败: {retry_err}"
                                        );
                                        if let Some(err) = self
                                            .handle_rectifier_retry_failure(
                                                retry_err,
                                                provider,
                                                app_type_str,
                                                &mut provider_permit,
                                                signature_retry_exhausted_with_match,
                                                "整流",
                                                &mut last_error,
                                                &mut last_provider,
                                            )
                                            .await
                                        {
                                            return Err(err);
                                        }
                                        continue;
                                    }
                                }
                            }
                        }
                    }

                    // 检测是否需要触发 budget 整流器（仅 Claude/ClaudeAuth 供应商）
                    if is_anthropic_provider {
                        let error_message = extract_error_message(&e);
                        if should_rectify_thinking_budget(
                            error_message.as_deref(),
                            &self.rectifier_config,
                        ) {
                            // 已经重试过：直接返回错误（不可重试客户端错误）
                            if budget_rectifier_retried {
                                log::warn!(
                                    "[{app_type_str}] [RECT-013] budget 整流器已触发过，不再重试"
                                );
                                drop(provider_permit.take());
                                let mut status = self.status.write().await;
                                status.failed_requests += 1;
                                status.last_error = Some(e.to_string());
                                if status.total_requests > 0 {
                                    status.success_rate = (status.success_requests as f32
                                        / status.total_requests as f32)
                                        * 100.0;
                                }
                                return Err(ForwardError {
                                    error: e,
                                    provider: Some(provider.clone()),
                                });
                            }

                            let budget_rectified = rectify_thinking_budget(&mut provider_body);
                            if !budget_rectified.applied {
                                log::warn!(
                                    "[{app_type_str}] [RECT-014] budget 整流器触发但无可整流内容，不做无意义重试"
                                );
                                drop(provider_permit.take());
                                let mut status = self.status.write().await;
                                status.failed_requests += 1;
                                status.last_error = Some(e.to_string());
                                if status.total_requests > 0 {
                                    status.success_rate = (status.success_requests as f32
                                        / status.total_requests as f32)
                                        * 100.0;
                                }
                                return Err(ForwardError {
                                    error: e,
                                    provider: Some(provider.clone()),
                                });
                            }

                            log::info!(
                                "[{}] [RECT-010] thinking budget 整流器触发, before={:?}, after={:?}",
                                app_type_str,
                                budget_rectified.before,
                                budget_rectified.after
                            );

                            let _ = std::mem::replace(&mut budget_rectifier_retried, true);

                            // 使用同一供应商重试（不计入熔断器）
                            let mut budget_retry_exhausted_with_match = false;
                            match self
                                .forward_provider_body_with_retries(
                                    app_type,
                                    &method,
                                    provider,
                                    endpoint,
                                    &mut provider_body,
                                    &headers,
                                    &extensions,
                                    adapter.as_ref(),
                                    provider_retry_policy.as_ref(),
                                    &mut provider_retries,
                                    &mut budget_retry_exhausted_with_match,
                                    app_type_str,
                                )
                                .await
                            {
                                Ok((response, claude_api_format, outbound_model)) => {
                                    log::info!("[{app_type_str}] [RECT-011] budget 整流重试成功");
                                    return Ok(self
                                        .finalize_provider_success(
                                            response,
                                            provider,
                                            app_type_str,
                                            claude_api_format,
                                            outbound_model,
                                            &mut provider_permit,
                                            route_fallback,
                                        )
                                        .await);
                                }
                                Err(retry_err) => {
                                    log::warn!(
                                        "[{app_type_str}] [RECT-012] budget 整流重试仍失败: {retry_err}"
                                    );
                                    if let Some(err) = self
                                        .handle_rectifier_retry_failure(
                                            retry_err,
                                            provider,
                                            app_type_str,
                                            &mut provider_permit,
                                            budget_retry_exhausted_with_match,
                                            "budget 整流",
                                            &mut last_error,
                                            &mut last_provider,
                                        )
                                        .await
                                    {
                                        return Err(err);
                                    }
                                    continue;
                                }
                            }
                        }
                    }

                    if signature_rectifier_non_retryable_client_error {
                        drop(provider_permit.take());
                        let mut status = self.status.write().await;
                        status.failed_requests += 1;
                        status.last_error = Some(e.to_string());
                        if status.total_requests > 0 {
                            status.success_rate = (status.success_requests as f32
                                / status.total_requests as f32)
                                * 100.0;
                        }
                        return Err(ForwardError {
                            error: e,
                            provider: Some(provider.clone()),
                        });
                    }

                    // 先分类错误，决定是否计入 provider 健康度
                    // —— NonRetryable / ClientAbort 是客户端层错误，无论换哪家 provider 都会被拒绝，
                    //    不应污染熔断器和数据库健康度（与 release_permit_neutral 同语义）。
                    let category = if provider_retry_exhausted_with_match {
                        ErrorCategory::Retryable
                    } else {
                        self.categorize_proxy_error(&e, provider)
                    };

                    match category {
                        ErrorCategory::Retryable => {
                            // 可重试：真正的 provider 故障 → 记录失败并更新熔断器/DB 健康度
                            let _ = if provider_permit
                                .as_ref()
                                .is_some_and(|permit| permit.used_half_open_permit())
                            {
                                self.record_half_open_result_cancellation_safe(
                                    &mut provider_permit,
                                    &provider.id,
                                    app_type_str,
                                    false,
                                    Some(e.to_string()),
                                )
                                .await
                            } else {
                                self.router
                                    .record_result(
                                        &provider.id,
                                        app_type_str,
                                        false,
                                        false,
                                        Some(e.to_string()),
                                    )
                                    .await
                            };

                            {
                                let mut status = self.status.write().await;
                                status.last_error =
                                    Some(format!("Provider {} 失败: {}", provider.name, e));
                            }

                            let (log_code, log_message) = build_retryable_failure_log(
                                &provider.name,
                                attempted_providers,
                                providers.len(),
                                &e,
                            );
                            log::warn!("[{app_type_str}] [{log_code}] {log_message}");

                            last_error = Some(e);
                            last_provider = Some(provider.clone());
                            // 继续尝试下一个供应商
                            continue;
                        }
                        ErrorCategory::NonRetryable | ErrorCategory::ClientAbort => {
                            // 不可重试：客户端层错误或客户端断连 → 不污染健康度，仅释放 HalfOpen permit
                            drop(provider_permit.take());
                            {
                                let mut status = self.status.write().await;
                                status.failed_requests += 1;
                                status.last_error = Some(e.to_string());
                                if status.total_requests > 0 {
                                    status.success_rate = (status.success_requests as f32
                                        / status.total_requests as f32)
                                        * 100.0;
                                }
                            }
                            return Err(ForwardError {
                                error: e,
                                provider: Some(provider.clone()),
                            });
                        }
                    }
                }
            }
        }

        if attempted_providers == 0 {
            // providers 列表非空，但全部被熔断器拒绝（典型：HalfOpen 探测名额被占用）
            {
                let mut status = self.status.write().await;
                status.failed_requests += 1;
                status.last_error = Some("所有供应商暂时不可用（熔断器限制）".to_string());
                if status.total_requests > 0 {
                    status.success_rate =
                        (status.success_requests as f32 / status.total_requests as f32) * 100.0;
                }
            }
            return Err(ForwardError {
                error: ProxyError::NoAvailableProvider,
                provider: None,
            });
        }

        // 所有供应商都失败了
        {
            let mut status = self.status.write().await;
            status.failed_requests += 1;
            status.last_error = Some("所有供应商都失败".to_string());
            if status.total_requests > 0 {
                status.success_rate =
                    (status.success_requests as f32 / status.total_requests as f32) * 100.0;
            }
        }

        if let Some((log_code, log_message)) =
            build_terminal_failure_log(attempted_providers, providers.len(), last_error.as_ref())
        {
            log::warn!("[{app_type_str}] [{log_code}] {log_message}");
        }

        Err(ForwardError {
            error: last_error.unwrap_or(ProxyError::MaxRetriesExceeded),
            provider: last_provider,
        })
    }

    /// 转发单个请求（使用适配器）
    ///
    /// 成功时返回 `(response, claude_api_format, outbound_model)`，其中
    /// `outbound_model` 是最终发往上游的模型名（所有映射/改写之后）。
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        app_type: &AppType,
        method: &http::Method,
        provider: &Provider,
        endpoint: &str,
        body: &Value,
        headers: &axum::http::HeaderMap,
        extensions: &Extensions,
        adapter: &dyn ProviderAdapter,
    ) -> Result<
        (
            ProxyResponse,
            Option<String>,
            Option<String>,
            PreOutputDeadline,
        ),
        ProxyError,
    > {
        // 使用适配器提取 base_url
        let mut base_url = adapter.extract_base_url(provider)?;

        let is_full_url = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.is_full_url)
            .unwrap_or(false)
            && !provider.is_codex_oauth()
            && !provider.is_xai_oauth();

        // GitHub Copilot API 使用 /chat/completions（无 /v1 前缀）
        let is_copilot = provider
            .meta
            .as_ref()
            .and_then(|m| m.provider_type.as_deref())
            == Some("github_copilot")
            || base_url.contains("githubcopilot.com");

        // Codex upstream conversion mode — computed early because the [1m]-suffix strip
        // below must be skipped on the Anthropic path (the marker has to survive to
        // catalog matching and to the transform's own strip+beta detection).
        let codex_responses_to_chat = matches!(app_type, AppType::Codex | AppType::GrokBuild)
            && super::providers::should_convert_codex_responses_to_chat(provider, endpoint);
        let codex_responses_to_anthropic = matches!(app_type, AppType::Codex | AppType::GrokBuild)
            && super::providers::should_convert_codex_responses_to_anthropic(provider, endpoint);
        let codex_official_auth_passthrough = matches!(app_type, AppType::Codex)
            && super::providers::is_codex_official_provider(provider);

        if codex_official_auth_passthrough {
            validate_codex_official_authorization(headers)?;
        }

        // 应用模型映射（独立于格式转换）
        // Claude Desktop proxy 模式必须先把 Desktop 可见的 claude-* route
        // 映射成真实上游模型名，并且未知 route 要直接报错，不能使用默认模型兜底。
        let mapped_body = if matches!(app_type, AppType::ClaudeDesktop) {
            crate::claude_desktop_config::map_proxy_request_model(body.clone(), provider)
                .map_err(|e| ProxyError::InvalidRequest(e.to_string()))?
        } else {
            let (mapped_body, _original_model, _mapped_model) =
                super::model_mapper::apply_model_mapping(body.clone(), provider);
            mapped_body
        };

        // 与 CCH 对齐：请求前不做 thinking 主动改写（仅保留兼容入口）
        let mut mapped_body = normalize_thinking_type(mapped_body);
        let outbound_model_override = self.outbound_model_overrides.get(&provider.id);
        if let Some(model) = outbound_model_override {
            mapped_body["model"] = Value::String(model.clone());
        }

        // Grok Build exposes a stable client-side model profile in config.toml.
        // Route requests to the provider's real upstream model before applying
        // the optional Responses -> Chat/Anthropic bridge.
        if matches!(app_type, AppType::GrokBuild) && outbound_model_override.is_none() {
            super::providers::apply_codex_upstream_model(provider, &mut mapped_body);
        }

        if is_copilot {
            mapped_body =
                super::providers::copilot_model_map::apply_copilot_model_normalization(mapped_body);
            self.apply_copilot_live_model_resolution(provider, &mut mapped_body)
                .await;
        } else if !codex_responses_to_anthropic {
            // Skip on the Codex→Anthropic path: stripping [1m] here would break both the
            // model-catalog match (apply_codex_upstream_model) and the transform's own
            // strip+`context-1m` beta detection. The marker is stripped later, on the
            // final anthropic_body.
            mapped_body =
                super::model_mapper::strip_one_m_suffix_for_upstream_from_body(mapped_body);
        }

        // --- Copilot 优化器：分类 + 请求体优化（在格式转换之前执行） ---
        // 注意：确定性 ID 也在此处计算，因为 mapped_body 在格式转换时会被 move
        //
        // 执行顺序（与 copilot-api 对齐）：
        //   1. 先在原始 body 上分类（保留 tool_result 语义，避免误判为 user）
        //   2. 再清洗孤立 tool_result（防止上游 API 报错）
        //   3. 再合并 tool_result + text（减少 premium 计费）
        let copilot_optimization = if is_copilot && self.copilot_optimizer_config.enabled {
            // 1. 在原始 body 上分类 — 必须在清洗/合并之前执行
            //    孤立 tool_result 仍保持 tool_result 类型，分类能正确识别为 agent
            let has_anthropic_beta = headers.contains_key("anthropic-beta");
            let classification = super::copilot_optimizer::classify_request(
                &mapped_body,
                has_anthropic_beta,
                self.copilot_optimizer_config.compact_detection,
                self.copilot_optimizer_config.subagent_detection,
            );

            log::debug!(
                "[Copilot] 优化器分类: initiator={}, is_warmup={}, is_compact={}, is_subagent={}",
                classification.initiator,
                classification.is_warmup,
                classification.is_compact,
                classification.is_subagent
            );

            // 2. 孤立 tool_result 清理 — 分类完成后再清洗
            //    防止上游 API 因不匹配的 tool_result 报错导致重试/重复计费
            mapped_body = super::copilot_optimizer::sanitize_orphan_tool_results(mapped_body);

            // 3. Tool result 合并 — 将 [tool_result, text] 变为 [tool_result(含text)]
            if self.copilot_optimizer_config.tool_result_merging {
                mapped_body = super::copilot_optimizer::merge_tool_results(mapped_body);
            }

            // 3.5. 主动剥离 thinking block — Copilot 走 OpenAI 兼容端点不识别该块
            //      避免上游拒绝后由 rectifier 反应式重试（首次请求已消耗 quota）
            if self.copilot_optimizer_config.strip_thinking {
                mapped_body = super::copilot_optimizer::strip_thinking_blocks(mapped_body);
            }

            // 4. Warmup 小模型降级
            if self.copilot_optimizer_config.warmup_downgrade && classification.is_warmup {
                log::info!(
                    "[Copilot] Warmup 请求降级到模型: {}",
                    self.copilot_optimizer_config.warmup_model
                );
                mapped_body["model"] =
                    serde_json::json!(&self.copilot_optimizer_config.warmup_model);
            }

            // 预计算确定性 Request ID（在 body 被 move 之前）
            // Session 提取优先级（与 session.rs extract_from_metadata 对齐）：
            //   1. metadata.user_id 中的 _session_ 后缀
            //   2. metadata.session_id（直接字段）
            //   3. raw metadata.user_id（整串 fallback）
            //   4. x-session-id header
            let metadata = body.get("metadata");
            let session_id = metadata
                .and_then(|m| m.get("user_id"))
                .and_then(|v| v.as_str())
                .and_then(super::session::parse_session_from_user_id)
                .or_else(|| {
                    metadata
                        .and_then(|m| m.get("session_id"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                })
                .or_else(|| {
                    metadata
                        .and_then(|m| m.get("user_id"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                })
                .or_else(|| {
                    headers
                        .get("x-session-id")
                        .and_then(|v| v.to_str().ok())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();
            let det_request_id = if self.copilot_optimizer_config.deterministic_request_id {
                Some(super::copilot_optimizer::deterministic_request_id(
                    &mapped_body,
                    &session_id,
                ))
            } else {
                None
            };

            // 从 session ID 派生稳定的 interaction ID（同一主对话共享）
            let interaction_id =
                super::copilot_optimizer::deterministic_interaction_id(&session_id);

            Some((classification, det_request_id, interaction_id))
        } else {
            None
        };

        // GitHub Copilot 动态 endpoint 路由
        // 从 CopilotAuthManager 获取缓存的 API endpoint（支持企业版等非默认 endpoint）
        if is_copilot && !is_full_url {
            if let Some(app_handle) = &self.app_handle {
                let copilot_state = app_handle.state::<CopilotAuthState>();
                let copilot_auth = copilot_state.0.read().await;

                // 从 provider.meta 获取关联的 GitHub 账号 ID
                let account_id = provider
                    .meta
                    .as_ref()
                    .and_then(|m| m.managed_account_id_for("github_copilot"));

                let dynamic_endpoint = match &account_id {
                    Some(id) => copilot_auth.get_api_endpoint(id).await,
                    None => copilot_auth.get_default_api_endpoint().await,
                };

                // 只在动态 endpoint 与当前 base_url 不同时替换
                if dynamic_endpoint != base_url {
                    log::debug!(
                        "[Copilot] 使用动态 API endpoint: {} (原: {})",
                        dynamic_endpoint,
                        base_url
                    );
                    base_url = dynamic_endpoint;
                }
            }
        }
        let resolved_claude_api_format = if adapter.name() == "Claude" {
            Some(
                self.resolve_claude_api_format(provider, &mapped_body, is_copilot)
                    .await,
            )
        } else {
            None
        };
        if adapter.name() == "Claude" {
            if let Some(api_format) = resolved_claude_api_format.as_deref() {
                super::providers::normalize_anthropic_messages_for_provider(
                    &mut mapped_body,
                    provider,
                    api_format,
                );
                self.apply_media_prevention(&mut mapped_body, provider);
            }
        }
        let needs_transform = match resolved_claude_api_format.as_deref() {
            Some(api_format) => super::providers::claude_api_format_needs_transform(api_format),
            None => adapter.needs_transform(provider),
        };
        // Codex → Anthropic: Claude Code emulation is off by default and only
        // enabled when the user explicitly turns it on in the UI, so requests can
        // pass a gateway's "Claude Code only" fingerprint check (User-Agent /
        // anthropic-beta / x-app / system prompt first line). Defaulting to off
        // avoids leaking the Claude Code fingerprint and identity prompt to
        // general-purpose gateways.
        let codex_impersonate_claude_code = codex_responses_to_anthropic
            && provider
                .meta
                .as_ref()
                .and_then(|meta| meta.impersonate_claude_code)
                == Some(true);
        let (effective_endpoint, passthrough_query) = if codex_responses_to_chat {
            rewrite_codex_responses_endpoint_to_chat(endpoint)
        } else if codex_responses_to_anthropic {
            rewrite_codex_responses_endpoint_to_anthropic(endpoint)
        } else if needs_transform && adapter.name() == "Claude" {
            let api_format = resolved_claude_api_format
                .as_deref()
                .unwrap_or_else(|| super::providers::get_claude_api_format(provider));
            rewrite_claude_transform_endpoint(endpoint, api_format, is_copilot, &mapped_body)
        } else {
            (
                endpoint.to_string(),
                split_endpoint_and_query(endpoint)
                    .1
                    .map(ToString::to_string),
            )
        };

        let codex_chat_base_is_full_endpoint =
            codex_responses_to_chat && base_url_is_full_endpoint(&base_url, "/chat/completions");

        // Defensive fallback mirroring `codex_chat_base_is_full_endpoint`: if a user pastes
        // a base URL already ending in the Anthropic `/v1/messages` endpoint but leaves the
        // "full URL" switch off, treat it as a full endpoint so we don't double-append
        // `/v1/messages` (→ `.../v1/messages/v1/messages`, a non-retryable 400). Matches the
        // exact endpoint suffix, so prefixed gateways like `.../api/v1/messages` are covered.
        let codex_anthropic_base_is_full_endpoint =
            codex_responses_to_anthropic && base_url_is_full_endpoint(&base_url, "/v1/messages");

        let url = if matches!(resolved_claude_api_format.as_deref(), Some("gemini_native")) {
            super::gemini_url::resolve_gemini_native_url(
                &base_url,
                &effective_endpoint,
                is_full_url,
            )
        } else if is_full_url
            || codex_chat_base_is_full_endpoint
            || codex_anthropic_base_is_full_endpoint
        {
            append_query_to_full_url(&base_url, passthrough_query.as_deref())
        } else {
            adapter.build_url(&base_url, &effective_endpoint)
        };

        // 记录映射后的出站模型名（此时 mapped_body 已完成接管映射 / [1m] 剥离 /
        // Copilot 归一化）。格式转换后若 body 仍带 model 字段会在下方刷新覆盖；
        // gemini_native 等模型在 URL 中的格式则保留此处的转换前真值。
        let mut outbound_model = mapped_body
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty())
            .map(str::to_string);

        // Codex→Anthropic: when the model name carries the [1m] marker, strip the
        // suffix and add the context-1m beta header.
        let mut codex_anthropic_one_m = false;

        // 转换请求体（如果需要）
        let mut request_body = if codex_responses_to_chat {
            let mut mapped_body = mapped_body;
            let explicit_prompt_cache_key = mapped_body
                .get("prompt_cache_key")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
            let restored = self
                .codex_chat_history
                .enrich_request(&mut mapped_body)
                .await;
            if restored > 0 {
                log::debug!(
                    "[Codex] Restored or enriched {restored} cached function call item(s) for Chat upstream"
                );
            }
            if outbound_model_override.is_none() {
                super::providers::apply_codex_chat_upstream_model(provider, &mut mapped_body);
            }
            let reasoning_config =
                super::providers::resolve_codex_chat_reasoning_config(provider, &mapped_body);
            let mut chat_body = super::providers::transform_codex_chat::responses_to_chat_completions_with_reasoning(
                mapped_body,
                reasoning_config.as_ref(),
            )?;
            super::providers::inject_codex_chat_prompt_cache_key(
                provider,
                &mut chat_body,
                explicit_prompt_cache_key.as_deref(),
                self.session_client_provided
                    .then_some(self.session_id.as_str()),
            );
            chat_body
        } else if codex_responses_to_anthropic {
            let mut mapped_body = mapped_body;
            if outbound_model_override.is_none() {
                super::providers::apply_codex_upstream_model(provider, &mut mapped_body);
            }
            // Per-provider output ceiling override. Codex does not forward its
            // `model_max_output_tokens` in the request body, so honor the value
            // configured on the provider here — it takes precedence over any
            // request-supplied `max_output_tokens` and over the default below.
            // Injecting it into the body (rather than overriding after transform)
            // lets the thinking-budget clamp size its headroom against the real
            // ceiling too. Kept per-provider to avoid a global large default that
            // would 400 on low-output-ceiling gateways.
            if let Some(max_out) = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.max_output_tokens)
                .filter(|v| *v > 0)
            {
                mapped_body["max_output_tokens"] = Value::from(max_out);
            }
            // Anthropic requires max_tokens; fall back to this default only when the
            // Codex request omits max_output_tokens (rare — Codex normally sends it).
            // Kept conservative so a low-output-ceiling model or relay does not hard-400
            // on the fallback (a too-high default 400s and is non-retryable); 8192 is
            // accepted by every current Claude model and virtually all gateways. The
            // transform clamps any thinking budget below this value.
            const DEFAULT_CODEX_ANTHROPIC_MAX_TOKENS: u64 = 8192;
            let mut anthropic_body =
                super::providers::transform_codex_anthropic::responses_request_to_anthropic(
                    mapped_body,
                    DEFAULT_CODEX_ANTHROPIC_MAX_TOKENS,
                )?;
            // Handle the 1M-context marker [1m]: strip the model-name suffix (the
            // gateway doesn't recognize it) and set the flag so the beta header is
            // added. apply_codex_upstream_model may have just written back a model
            // name carrying [1m] from the provider config, so strip it once more on
            // the final body here.
            if let Some(model) = anthropic_body.get("model").and_then(|v| v.as_str()) {
                let stripped = super::model_mapper::strip_one_m_suffix_for_upstream(model);
                if stripped != model {
                    codex_anthropic_one_m = true;
                    anthropic_body["model"] = Value::String(stripped.to_string());
                }
            }
            if codex_impersonate_claude_code {
                prepend_claude_code_system_prompt(&mut anthropic_body);
            }
            // Enable Anthropic prompt caching (no beta header required). Reuse the
            // configured TTL rather than silently forcing 5m on this conversion path.
            // otherwise system/tools/history are re-sent at full price every round,
            // inflating cost and first-token latency. The injector handles the
            // string→array `system` conversion and the new-breakpoint budget.
            super::cache_injector::inject(
                &mut anthropic_body,
                &codex_anthropic_cache_config(&self.optimizer_config),
            );
            anthropic_body
        } else if needs_transform {
            if adapter.name() == "Claude" {
                let api_format = resolved_claude_api_format
                    .as_deref()
                    .unwrap_or_else(|| super::providers::get_claude_api_format(provider));
                super::providers::transform_claude_request_for_api_format(
                    mapped_body,
                    provider,
                    api_format,
                    self.session_client_provided
                        .then_some(self.session_id.as_str()),
                    Some(self.gemini_shadow.as_ref()),
                )?
            } else {
                adapter.transform_request(mapped_body, provider)?
            }
        } else {
            mapped_body
        };

        let outbound_model_override_for_wire = outbound_model_override.map(|model| {
            if matches!(app_type, AppType::Codex | AppType::GrokBuild) {
                super::model_mapper::strip_one_m_suffix_for_upstream(model).to_string()
            } else {
                model.clone()
            }
        });
        if let Some(model) = outbound_model_override_for_wire.as_ref() {
            request_body["model"] = Value::String(model.clone());
        }

        // Native Responses passthrough to a strict third-party gateway (xAI):
        // flatten Codex's private `namespace`/plugin tool declarations into
        // top-level function tools so the upstream's strict serde parser does
        // not 422 on `unknown variant "namespace"`. The Chat/Anthropic paths
        // above already unwrap namespaces, so this only fires on the native
        // passthrough. The response handler restores the flat names using a map
        // re-derived from the same request tools.
        if matches!(app_type, AppType::Codex | AppType::GrokBuild)
            && !codex_responses_to_chat
            && !codex_responses_to_anthropic
            && super::providers::provider_needs_responses_namespace_flatten(provider)
            && super::providers::transform_codex_responses_namespace::flatten_request_namespaces(
                &mut request_body,
            )?
        {
            log::debug!(
                "[Codex] Flattened namespace tools for native Responses upstream (provider={})",
                provider.id
            );
        }

        // Same native-Responses path: scrub the OpenAI-backend-private fields
        // and tool carriers (`external_web_access`, `prompt_cache_retention`,
        // `additional_tools`, `tool_search`, …) that xAI's strict serde parser
        // rejects with 400/422. Deterministic field removals only, gated on the
        // xAI OAuth path, so the prompt-cache prefix stays stable and no other
        // provider is affected. Runs after the flatten above so lifted
        // `namespace` tools survive the tool-type whitelist.
        if matches!(app_type, AppType::Codex | AppType::GrokBuild)
            && !codex_responses_to_chat
            && !codex_responses_to_anthropic
            && super::providers::provider_needs_responses_namespace_flatten(provider)
            && super::providers::transform_codex_responses_xai_sanitize::sanitize_xai_responses_request(
                &mut request_body,
            )
        {
            log::debug!(
                "[Codex] Sanitized xAI-unsupported Responses fields (provider={})",
                provider.id
            );
        }

        if matches!(app_type, AppType::Codex | AppType::GrokBuild) {
            self.apply_media_prevention(&mut request_body, provider);
        }

        // 过滤私有参数（以 `_` 开头的字段），防止内部信息泄露到上游
        // 默认使用空白名单，过滤所有 _ 前缀字段
        let mut filtered_body = prepare_upstream_request_body(request_body);
        if !is_copilot {
            if let Some(overrides) = provider
                .meta
                .as_ref()
                .and_then(|meta| meta.local_proxy_request_overrides.as_ref())
            {
                if apply_local_proxy_body_overrides(&mut filtered_body, overrides) {
                    filtered_body = prepare_upstream_request_body(filtered_body);
                }
            }
        }
        if let Some(model) = outbound_model_override_for_wire.as_ref() {
            filtered_body["model"] = Value::String(model.clone());
        }
        // 出站 body 定稿后刷新真值（覆盖 Codex chat 上游模型覆写、转换层模型改写）
        if let Some(m) = filtered_body
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty())
        {
            outbound_model = Some(m.to_string());
        }
        log_prompt_cache_trace(
            app_type,
            provider,
            &effective_endpoint,
            resolved_claude_api_format.as_deref(),
            &filtered_body,
            self.session_client_provided,
        );
        let request_is_streaming =
            is_streaming_request(&effective_endpoint, &filtered_body, headers);
        let force_identity_encoding = needs_transform
            || codex_responses_to_chat
            || codex_responses_to_anthropic
            || request_is_streaming;

        // Codex OAuth 需要注入的 ChatGPT-Account-Id（在动态 token 获取期间填充）
        let mut codex_oauth_account_id: Option<String> = None;
        let mut should_send_codex_oauth_session_headers = false;

        // 获取认证头（提前准备，用于内联替换），同时保留仅用于日志脱敏的
        // 精确认证材料。实际日志永远不输出这些值。
        let mut log_secrets: Vec<String> = Vec::new();
        let mut auth_headers = if let Some(mut auth) = adapter.extract_auth(provider) {
            // GitHub Copilot 特殊处理：从 CopilotAuthManager 获取真实 token
            if auth.strategy == AuthStrategy::GitHubCopilot {
                if let Some(app_handle) = &self.app_handle {
                    let copilot_state = app_handle.state::<CopilotAuthState>();
                    let copilot_auth: tokio::sync::RwLockReadGuard<'_, CopilotAuthManager> =
                        copilot_state.0.read().await;

                    // 从 provider.meta 获取关联的 GitHub 账号 ID（多账号支持）
                    let account_id = provider
                        .meta
                        .as_ref()
                        .and_then(|m| m.managed_account_id_for("github_copilot"));

                    // 根据账号 ID 获取对应 token（向后兼容：无账号 ID 时使用第一个账号）
                    let token_result = match &account_id {
                        Some(id) => {
                            log::debug!("[Copilot] 使用指定账号 {id} 获取 token");
                            copilot_auth.get_valid_token_for_account(id).await
                        }
                        None => {
                            log::debug!("[Copilot] 使用默认账号获取 token");
                            copilot_auth.get_valid_token().await
                        }
                    };

                    match token_result {
                        Ok(token) => {
                            auth = AuthInfo::new(token, AuthStrategy::GitHubCopilot);
                            log::debug!(
                                "[Copilot] 成功获取 Copilot token (account={})",
                                account_id.as_deref().unwrap_or("default")
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "[Copilot] 获取 Copilot token 失败 (account={}): {e}",
                                account_id.as_deref().unwrap_or("default")
                            );
                            return Err(ProxyError::AuthError(format!(
                                "GitHub Copilot 认证失败: {e}"
                            )));
                        }
                    }
                } else {
                    log::error!("[Copilot] AppHandle 不可用");
                    return Err(ProxyError::AuthError(
                        "GitHub Copilot 认证不可用（无 AppHandle）".to_string(),
                    ));
                }
            }

            // Codex OAuth 特殊处理：从 CodexOAuthManager 获取真实 access_token
            if auth.strategy == AuthStrategy::CodexOAuth {
                if let Some(app_handle) = &self.app_handle {
                    let codex_state = app_handle.state::<CodexOAuthState>();
                    let codex_auth: tokio::sync::RwLockReadGuard<'_, CodexOAuthManager> =
                        codex_state.0.read().await;

                    // 从 provider.meta 获取关联的 ChatGPT 账号 ID
                    let account_id = provider
                        .meta
                        .as_ref()
                        .and_then(|m| m.managed_account_id_for("codex_oauth"));

                    let token_result = match &account_id {
                        Some(id) => {
                            log::debug!("[CodexOAuth] 使用指定账号 {id} 获取 token");
                            codex_auth.get_valid_token_for_account(id).await
                        }
                        None => {
                            log::debug!("[CodexOAuth] 使用默认账号获取 token");
                            codex_auth.get_valid_token().await
                        }
                    };

                    match token_result {
                        Ok(token) => {
                            auth = AuthInfo::new(token, AuthStrategy::CodexOAuth);
                            should_send_codex_oauth_session_headers = true;
                            // 解析使用的 account_id（用于注入 ChatGPT-Account-Id header）
                            codex_oauth_account_id = match account_id {
                                Some(id) => Some(id),
                                None => codex_auth.default_account_id().await,
                            };
                            log::debug!(
                                "[CodexOAuth] 成功获取 access_token (account={})",
                                codex_oauth_account_id.as_deref().unwrap_or("default")
                            );
                        }
                        Err(e) => {
                            log::error!("[CodexOAuth] 获取 access_token 失败: {e}");
                            return Err(ProxyError::AuthError(format!(
                                "Codex OAuth 认证失败: {e}"
                            )));
                        }
                    }
                } else {
                    log::error!("[CodexOAuth] AppHandle 不可用");
                    return Err(ProxyError::AuthError(
                        "Codex OAuth 认证不可用（无 AppHandle）".to_string(),
                    ));
                }
            }

            // xAI OAuth: resolve a managed account token immediately before
            // sending the request. Invalid refresh credentials are persisted as
            // requiring re-authentication by the manager.
            if auth.strategy == AuthStrategy::XaiOAuth {
                if let Some(app_handle) = &self.app_handle {
                    let xai_state = app_handle.state::<XaiOAuthState>();
                    let xai_auth: tokio::sync::RwLockReadGuard<'_, XaiOAuthManager> =
                        xai_state.0.read().await;
                    let account_id = provider
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.managed_account_id_for("xai_oauth"));
                    let token_result = match &account_id {
                        Some(id) => xai_auth.get_valid_token_for_account(id).await,
                        None => xai_auth.get_valid_token().await,
                    };
                    match token_result {
                        Ok(token) => {
                            auth = AuthInfo::new(token, AuthStrategy::XaiOAuth);
                            log::debug!(
                                "[XaiOAuth] 成功获取 access_token (account={})",
                                account_id.as_deref().unwrap_or("default")
                            );
                        }
                        Err(error) => {
                            log::error!("[XaiOAuth] 获取 access_token 失败: {error}");
                            return Err(ProxyError::AuthError(format!(
                                "xAI OAuth 认证失败: {error}"
                            )));
                        }
                    }
                } else {
                    return Err(ProxyError::AuthError(
                        "xAI OAuth 认证不可用（无 AppHandle）".to_string(),
                    ));
                }
            }

            for secret in std::iter::once(&auth.api_key).chain(auth.access_token.iter()) {
                if !secret.is_empty() && !log_secrets.contains(secret) {
                    log_secrets.push(secret.clone());
                }
            }

            adapter.get_auth_headers(&auth)?
        } else {
            Vec::new()
        };

        // 注入 Codex OAuth 的 ChatGPT-Account-Id header（如果有 account_id）
        if let Some(ref account_id) = codex_oauth_account_id {
            if let Ok(hv) = http::HeaderValue::from_str(account_id) {
                auth_headers.push((http::HeaderName::from_static("chatgpt-account-id"), hv));
            }
        }

        let codex_oauth_session_headers =
            if should_send_codex_oauth_session_headers && self.session_client_provided {
                build_codex_oauth_session_headers(&self.session_id)
            } else {
                Vec::new()
            };

        // 自定义 User-Agent：与 stream_check / model_fetch 共用 parse_custom_user_agent，
        // 运行时静默忽略非法值（前端在输入处给非阻断提示，不在保存时阻断）。
        // Copilot 指纹 UA 不可覆盖。
        let custom_user_agent = if is_copilot {
            None
        } else {
            provider
                .meta
                .as_ref()
                .and_then(|meta| meta.custom_user_agent_header().ok().flatten())
        };
        // Codex→Anthropic emulation: when there is no custom UA, override Codex's
        // codex_cli_rs UA with the Claude Code UA.
        let custom_user_agent = if custom_user_agent.is_none() && codex_impersonate_claude_code {
            Some(http::HeaderValue::from_static(CLAUDE_CODE_USER_AGENT))
        } else {
            custom_user_agent
        };

        // --- Copilot 优化器：动态 header 注入 ---
        if let Some((ref classification, ref det_request_id, ref interaction_id)) =
            copilot_optimization
        {
            for (name, value) in auth_headers.iter_mut() {
                match name.as_str() {
                    "x-initiator" if self.copilot_optimizer_config.request_classification => {
                        *value = http::HeaderValue::from_static(classification.initiator);
                    }
                    "x-interaction-type" if classification.is_subagent => {
                        // 子代理请求：conversation-subagent 不计 premium interaction
                        *value = http::HeaderValue::from_static("conversation-subagent");
                    }
                    "x-request-id" | "x-agent-task-id" => {
                        if let Some(ref det_id) = det_request_id {
                            if let Ok(hv) = http::HeaderValue::from_str(det_id) {
                                *value = hv;
                            }
                        }
                    }
                    _ => {}
                }
            }

            // x-interaction-id：仅在有 session 时注入（不在 get_auth_headers 中）
            if let Some(ref iid) = interaction_id {
                if let Ok(hv) = http::HeaderValue::from_str(iid) {
                    auth_headers.push((http::HeaderName::from_static("x-interaction-id"), hv));
                }
            }

            if classification.is_subagent {
                log::info!(
                    "[Copilot] 子代理请求: x-initiator=agent, x-interaction-type=conversation-subagent"
                );
            }
        }

        // Copilot 指纹头名（由 get_auth_headers 注入，需在原始头中去重）
        let copilot_fingerprint_headers: &[&str] = if is_copilot {
            &[
                "user-agent",
                "editor-version",
                "editor-plugin-version",
                "copilot-integration-id",
                "x-github-api-version",
                "openai-intent",
                // 新增 headers
                "x-initiator",
                "x-interaction-type",
                "x-interaction-id",
                "x-vscode-user-agent-library-version",
                "x-request-id",
                "x-agent-task-id",
            ]
        } else {
            &[]
        };

        // 预计算上游 host 值（用于在原位替换 host header）
        let upstream_host = url
            .parse::<http::Uri>()
            .ok()
            .and_then(|u| u.authority().map(|a| a.to_string()));

        let should_send_anthropic_headers = adapter.name() == "Claude"
            && matches!(resolved_claude_api_format.as_deref(), Some("anthropic"));

        // 预计算 anthropic-beta 值（仅 Claude）
        let anthropic_beta_value = if should_send_anthropic_headers {
            const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
            Some(if let Some(beta) = headers.get("anthropic-beta") {
                if let Ok(beta_str) = beta.to_str() {
                    if beta_str.contains(CLAUDE_CODE_BETA) {
                        beta_str.to_string()
                    } else {
                        format!("{CLAUDE_CODE_BETA},{beta_str}")
                    }
                } else {
                    CLAUDE_CODE_BETA.to_string()
                }
            } else {
                CLAUDE_CODE_BETA.to_string()
            })
        } else if codex_impersonate_claude_code || codex_anthropic_one_m {
            // Codex→Anthropic: emulation injects the claude-code marker; a [1m]
            // model injects the context-1m marker.
            let mut betas: Vec<&str> = Vec::new();
            if codex_impersonate_claude_code {
                betas.push("claude-code-20250219");
            }
            if codex_anthropic_one_m {
                betas.push("context-1m-2025-08-07");
            }
            Some(betas.join(","))
        } else {
            None
        };

        // ============================================================
        // 构建有序 HeaderMap — 内联替换，保持客户端原始顺序
        // ============================================================
        let mut ordered_headers = http::HeaderMap::new();
        let mut saw_auth = false;
        let mut saw_accept_encoding = false;
        let mut saw_accept = false;
        let mut saw_user_agent = false;
        let mut saw_anthropic_beta = false;
        let mut saw_anthropic_version = false;

        for (key, value) in headers {
            let key_str = key.as_str();

            if is_codex_role_control_header(key_str) {
                continue;
            }

            // --- host — 原位替换为上游 host（保持客户端原始位置） ---
            if key_str.eq_ignore_ascii_case("host") {
                if let Some(ref host_val) = upstream_host {
                    if let Ok(hv) = http::HeaderValue::from_str(host_val) {
                        ordered_headers.append(key.clone(), hv);
                    }
                }
                continue;
            }

            // --- 连接 / 追踪 / CDN 类 — 无条件跳过 ---
            if matches!(
                key_str,
                "content-length"
                    | "transfer-encoding"
                    | "x-forwarded-host"
                    | "x-forwarded-port"
                    | "x-forwarded-proto"
                    | "forwarded"
                    | "cf-connecting-ip"
                    | "cf-ipcountry"
                    | "cf-ray"
                    | "cf-visitor"
                    | "true-client-ip"
                    | "fastly-client-ip"
                    | "x-azure-clientip"
                    | "x-azure-fdid"
                    | "x-azure-ref"
                    | "akamai-origin-hop"
                    | "x-akamai-config-log-detail"
                    | "x-request-id"
                    | "x-correlation-id"
                    | "x-trace-id"
                    | "x-amzn-trace-id"
                    | "x-b3-traceid"
                    | "x-b3-spanid"
                    | "x-b3-parentspanid"
                    | "x-b3-sampled"
                    | "traceparent"
                    | "tracestate"
            ) {
                continue;
            }

            // --- 认证类 — 用 adapter 提供的认证头替换（在原始位置） ---
            if key_str.eq_ignore_ascii_case("authorization")
                || key_str.eq_ignore_ascii_case("x-api-key")
                || key_str.eq_ignore_ascii_case("x-goog-api-key")
            {
                // The built-in Codex official provider deliberately has no
                // credential in CC Switch. `requires_openai_auth = true` makes
                // Codex send its native ChatGPT authorization, which must reach
                // the fixed official upstream unchanged. Other credential
                // headers are still discarded.
                if codex_official_auth_passthrough && key_str.eq_ignore_ascii_case("authorization")
                {
                    saw_auth = true;
                    ordered_headers.append(key.clone(), value.clone());
                    continue;
                }
                if !saw_auth {
                    saw_auth = true;
                    for (ah_name, ah_value) in &auth_headers {
                        ordered_headers.append(ah_name.clone(), ah_value.clone());
                    }
                }
                continue;
            }

            // --- x-app — during Codex→Anthropic emulation, `cli` is injected uniformly below ---
            if codex_impersonate_claude_code && key_str.eq_ignore_ascii_case("x-app") {
                continue;
            }

            // --- Codex/OpenAI fingerprint headers — never leak to an Anthropic upstream ---
            // These are client/session identifiers from the incoming Codex request,
            // not Anthropic protocol headers. Forwarding them both leaks identity and
            // can defeat strict gateway fingerprint checks.
            // The full set lives in `is_codex_client_fingerprint_header` so it stays in one
            // place. (HeaderName is lowercased by the http crate, so a direct match is safe.)
            if codex_responses_to_anthropic && is_codex_client_fingerprint_header(key_str) {
                continue;
            }

            // --- accept — force application/json on the Codex→Anthropic path ---
            // The Codex CLI sends `Accept: text/event-stream`, whereas a native
            // Anthropic client sends `application/json` (streaming is driven by
            // the body's stream:true). Strict Anthropic gateways return 406 Not
            // Acceptable for an event-stream Accept, so normalize it here.
            if codex_responses_to_anthropic && key_str.eq_ignore_ascii_case("accept") {
                if !saw_accept {
                    saw_accept = true;
                    ordered_headers.append(
                        http::header::ACCEPT,
                        http::HeaderValue::from_static("application/json"),
                    );
                }
                continue;
            }

            // --- accept-encoding — transform / SSE 路径强制 identity，其余保留原值 ---
            if key_str.eq_ignore_ascii_case("accept-encoding") {
                if !saw_accept_encoding {
                    saw_accept_encoding = true;
                    if force_identity_encoding {
                        ordered_headers.append(
                            http::header::ACCEPT_ENCODING,
                            http::HeaderValue::from_static("identity"),
                        );
                    } else {
                        ordered_headers.append(key.clone(), value.clone());
                    }
                }
                continue;
            }

            // --- user-agent: provider-level override for local proxy routing ---
            if !is_copilot && key_str.eq_ignore_ascii_case("user-agent") {
                if !saw_user_agent {
                    saw_user_agent = true;
                    if let Some(ref ua) = custom_user_agent {
                        ordered_headers.append(http::header::USER_AGENT, ua.clone());
                    } else {
                        ordered_headers.append(key.clone(), value.clone());
                    }
                }
                continue;
            }

            // --- anthropic-beta — 用重建值替换（确保含 claude-code 标记） ---
            if key_str.eq_ignore_ascii_case("anthropic-beta") {
                if !saw_anthropic_beta {
                    saw_anthropic_beta = true;
                    if let Some(ref beta_val) = anthropic_beta_value {
                        if let Ok(hv) = http::HeaderValue::from_str(beta_val) {
                            ordered_headers.append("anthropic-beta", hv);
                        }
                    }
                }
                continue;
            }

            // --- anthropic-version — 透传客户端值 ---
            if key_str.eq_ignore_ascii_case("anthropic-version") {
                if should_send_anthropic_headers {
                    saw_anthropic_version = true;
                    ordered_headers.append(key.clone(), value.clone());
                }
                continue;
            }

            // --- Copilot 指纹头 — 跳过（由 auth_headers 提供） ---
            if copilot_fingerprint_headers
                .iter()
                .any(|h| key_str.eq_ignore_ascii_case(h))
            {
                continue;
            }

            // --- 默认：透传 ---
            ordered_headers.append(key.clone(), value.clone());
        }

        // 如果原始请求中没有认证头，在末尾追加
        if !saw_auth && !auth_headers.is_empty() {
            for (ah_name, ah_value) in &auth_headers {
                ordered_headers.append(ah_name.clone(), ah_value.clone());
            }
        }

        // transform / SSE 路径在缺失时补 identity；普通透传不主动补 accept-encoding
        if !saw_accept_encoding && force_identity_encoding {
            ordered_headers.append(
                http::header::ACCEPT_ENCODING,
                http::HeaderValue::from_static("identity"),
            );
        }

        // On the Codex→Anthropic path, add application/json when Accept is missing (matching a native Anthropic client).
        if codex_responses_to_anthropic && !saw_accept {
            ordered_headers.append(
                http::header::ACCEPT,
                http::HeaderValue::from_static("application/json"),
            );
        }

        // Codex→Anthropic emulation: inject Claude Code's x-app: cli
        if codex_impersonate_claude_code {
            ordered_headers.append("x-app", http::HeaderValue::from_static("cli"));
        }

        if !saw_user_agent {
            if let Some(ref ua) = custom_user_agent {
                ordered_headers.append(http::header::USER_AGENT, ua.clone());
            }
        }

        // 如果原始请求中没有 anthropic-beta 且有值需要添加，追加
        if !saw_anthropic_beta {
            if let Some(ref beta_val) = anthropic_beta_value {
                if let Ok(hv) = http::HeaderValue::from_str(beta_val) {
                    ordered_headers.append("anthropic-beta", hv);
                }
            }
        }

        // anthropic-version: add the default only when it is missing.
        // The Codex→Anthropic path also needs this header. Note this is independent
        // of anthropic-beta: the Claude Code-specific beta is only sent when
        // impersonation is on (handled above); on the plain Codex→Anthropic path
        // (impersonation off) anthropic-version is still required but no beta is sent.
        if (should_send_anthropic_headers || codex_responses_to_anthropic) && !saw_anthropic_version
        {
            ordered_headers.append(
                "anthropic-version",
                http::HeaderValue::from_static("2023-06-01"),
            );
        }

        // Codex OAuth 反代尽量对齐官方 Codex CLI 的会话路由信号。
        // 只发送客户端提供的 session_id；生成的 UUID 每次不同，反而会破坏前缀缓存。
        for (name, value) in codex_oauth_session_headers {
            ordered_headers.insert(name, value);
        }

        // 序列化请求体。GET/HEAD 是 idempotent/safe 方法，按 HTTP 语义不应携带 body；
        // 强行附带 JSON body 会让某些上游（如 Google Gemini 的 models.list）拒绝请求。
        let body_bytes = if matches!(method, &http::Method::GET | &http::Method::HEAD) {
            Vec::new()
        } else {
            serde_json::to_vec(&filtered_body).map_err(|e| {
                ProxyError::Internal(format!("Failed to serialize request body: {e}"))
            })?
        };

        // 确保 content-type 存在
        if !ordered_headers.contains_key(http::header::CONTENT_TYPE) {
            ordered_headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
        }

        apply_local_proxy_header_overrides(
            &mut ordered_headers,
            provider
                .meta
                .as_ref()
                .and_then(|meta| meta.local_proxy_request_overrides.as_ref()),
            is_copilot,
        );

        reject_proxy_placeholder_for_managed_account_upstream(&url, &ordered_headers)?;

        // 日志目标 URL 的脱敏分两种情形：
        // - 有已知密钥(log_secrets 非空)：记录脱敏后的完整 URL，剥 userinfo/query
        //   并抹掉已知密钥值，保留 host+path 便于诊断 base_url 配错路径导致的 404。
        // - 无已知密钥：凭据可能整个内嵌在 path 里且无从脱敏，只记 origin，
        //   避免默认 Info 级把形如 https://gw/<KEY>/v1 的 path 完整落盘。
        let target_for_log = if log_secrets.is_empty() {
            crate::redact_url_origin_for_log(&url)
        } else {
            crate::redact_url_for_log_with_secrets(&url, &log_secrets)
        };

        // 输出请求信息日志
        let tag = adapter.name();
        let request_model = filtered_body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("<none>");
        log::info!("[{tag}] >>> 请求目标: {target_for_log} (model={request_model})");
        log::debug!(
            "[{tag}] >>> 请求体已准备: bytes={}, hash={} (content omitted)",
            body_bytes.len(),
            short_value_hash(Some(&filtered_body))
        );

        // 每次上游 attempt 的响应头、正文和首个语义输出共享一条绝对 deadline。
        let transport_timeout = if self.non_streaming_timeout.is_zero() {
            DEFAULT_UPSTREAM_TIMEOUT
        } else {
            self.non_streaming_timeout
        };
        let pre_output_deadline = self.pre_output_deadline(request_is_streaming);
        let header_phase = if request_is_streaming {
            "等待流式上游响应头"
        } else {
            "等待非流式上游响应头"
        };
        let response_header_timeout =
            pre_output_deadline.remaining_or(transport_timeout, header_phase)?;

        // 获取全局代理 URL
        let upstream_proxy_url: Option<String> = super::http_client::get_current_proxy_url();

        // SOCKS5 代理不支持 CONNECT 隧道，需要用 reqwest
        let is_socks_proxy = upstream_proxy_url
            .as_deref()
            .map(|u| u.starts_with("socks5"))
            .unwrap_or(false);

        let preserve_exact_header_case = should_preserve_exact_header_case(
            adapter.name(),
            provider,
            resolved_claude_api_format.as_deref(),
            is_copilot,
        );

        // 发送请求
        let response = if is_socks_proxy || !preserve_exact_header_case {
            // OpenAI / Copilot / Codex 类后端不依赖原始 header 大小写；走 reqwest
            // 连接池，避免 raw TCP/TLS path 每次请求都重新握手。SOCKS5 也只能走 reqwest。
            log::debug!(
                "[Forwarder] Using pooled reqwest client (preserve_exact_header_case={preserve_exact_header_case}, socks_proxy={is_socks_proxy})"
            );
            let client = super::http_client::get();
            let mut request = client.request(method.clone(), &url);
            if request_is_streaming {
                // reqwest 的 timeout 是整请求超时；流式请求交给 response_processor
                // 的首包/静默期超时控制，避免长流被总时长误杀。
                request = request.timeout(std::time::Duration::from_secs(24 * 60 * 60));
            } else if !self.non_streaming_timeout.is_zero() {
                request = request.timeout(self.non_streaming_timeout);
            }
            for (key, value) in &ordered_headers {
                request = request.header(key, value);
            }
            let send = request.body(body_bytes).send();
            let send_result = if request_is_streaming && !pre_output_deadline.is_enabled() {
                tokio::time::timeout(response_header_timeout, send)
                    .await
                    .map_err(|_| {
                        ProxyError::Timeout(format!(
                            "流式响应首包超时: {}s（上游未返回响应头）",
                            response_header_timeout.as_secs()
                        ))
                    })?
            } else {
                pre_output_deadline.wait(header_phase, send).await?
            };
            let reqwest_resp = send_result.map_err(map_reqwest_send_error)?;
            ProxyResponse::Reqwest(reqwest_resp)
        } else {
            // HTTP 代理或直连：走 hyper raw write（保持 header 大小写）
            // 如果有 HTTP 代理，hyper_client 会用 CONNECT 隧道穿过代理
            let uri: http::Uri = url.parse().map_err(|e| {
                ProxyError::ForwardFailed(format!("Invalid upstream URL ({target_for_log}): {e}"))
            })?;
            let send = super::hyper_client::send_request(
                uri,
                &target_for_log,
                method.clone(),
                ordered_headers,
                extensions.clone(),
                body_bytes,
                response_header_timeout,
                upstream_proxy_url.as_deref(),
            );
            pre_output_deadline.wait(header_phase, send).await??
        };

        // 检查响应状态
        let status = response.status();

        if status.is_success() {
            let mut response = self
                .prepare_success_response_for_failover(
                    response,
                    request_is_streaming,
                    pre_output_deadline,
                )
                .await?;
            // Streaming requests normally return SSE. If a compatible gateway
            // explicitly returns JSON instead, buffer and validate it inside the retry
            // loop as well so a 2xx Anthropic error envelope can still fail over. Do
            // not buffer unknown content types: some gateways omit the SSE header.
            if codex_responses_to_anthropic && (!request_is_streaming || response.is_json()) {
                response = self
                    .validate_codex_anthropic_success_response(response, pre_output_deadline)
                    .await?;
            } else if matches!(
                resolved_claude_api_format.as_deref(),
                Some("openai_responses")
            ) {
                if !request_is_streaming || response.is_json() {
                    // Claude→Responses gateways can also return a semantic failure in an
                    // HTTP 2xx Response object. Validate buffered/JSON bodies inside the
                    // retry loop so an early failure can still select another provider.
                    response = self
                        .validate_responses_success_response(response, pre_output_deadline)
                        .await?;
                } else {
                    // Delay committing the downstream stream until the upstream emits
                    // either productive output or a valid non-failure terminal event.
                    // A response.failed/error before output remains failover-safe.
                    response = self
                        .validate_responses_stream_start(response, pre_output_deadline)
                        .await?;
                }
            }
            Ok((
                response,
                resolved_claude_api_format,
                outbound_model,
                pre_output_deadline,
            ))
        } else {
            let status_code = status.as_u16();
            // 错误响应同样可能被上游压缩（content-encoding）。reqwest 未启用任何
            // 自动解压 feature，这里拿到的是原始字节；不解压的话，压缩过的错误体会
            // 在 from_utf8 处变成非 UTF-8 而被丢弃，隐藏掉上游的限流/鉴权等详情。
            let encoding = get_content_encoding(response.headers());
            let raw = pre_output_deadline
                .wait_upstream_error_body(
                    status_code,
                    collect_body_prefix(response, MAX_UPSTREAM_ERROR_BODY_BYTES),
                )
                .await??;
            let (decoded, truncated) = match encoding {
                Some(encoding) => match decompress_body_limited(
                    &encoding,
                    &raw.bytes,
                    MAX_UPSTREAM_ERROR_BODY_BYTES,
                    raw.truncated,
                ) {
                    Ok(Some(decompressed)) => {
                        (decompressed.bytes, raw.truncated || decompressed.truncated)
                    }
                    // 不支持的编码 / 解压失败：退回原始字节，尽量保留可读信息
                    _ => (raw.bytes.to_vec(), raw.truncated),
                },
                None => (raw.bytes.to_vec(), raw.truncated),
            };
            if truncated {
                log::warn!(
                    "[Proxy] Upstream HTTP {status_code} error body exceeded the {MAX_UPSTREAM_ERROR_BODY_BYTES}-byte decoded prefix limit"
                );
            }
            let body_text = bounded_error_body_text(decoded, truncated);

            Err(ProxyError::UpstreamError {
                status: status_code,
                body: body_text,
            })
        }
    }

    /// 故障转移开启时，成功不能只看上游响应头。
    ///
    /// - 非流式：先把完整 body 读到内存，读超时/连接中断会回到 retry loop 尝试下一家。
    /// - 流式：至少等首个 chunk 到达，避免上游返回 200 后一直不吐 SSE 时被误记成功。
    async fn prepare_success_response_for_failover(
        &self,
        response: ProxyResponse,
        request_is_streaming: bool,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError> {
        if request_is_streaming {
            return self.prime_streaming_response(response, deadline).await;
        }

        if !deadline.is_enabled() {
            return Ok(response);
        }

        let status = response.status();
        let headers = response.headers().clone();
        let body = deadline
            .wait("读取非流式上游响应体", response.bytes())
            .await??;

        Ok(ProxyResponse::buffered(status, headers, body))
    }

    /// Some Anthropic-compatible gateways return an Anthropic error envelope with
    /// HTTP 2xx. Validate it inside the retry loop so the request can fail over to
    /// the next provider; the response transformer runs too late for that.
    async fn validate_codex_anthropic_success_response(
        &self,
        response: ProxyResponse,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError> {
        self.validate_success_response_envelope_if(
            response,
            codex_anthropic_error_envelope_message,
            "Anthropic upstream returned a 2xx error envelope",
            |_| true,
            deadline,
        )
        .await
    }

    async fn validate_provider_retry_success_response(
        &self,
        response: ProxyResponse,
        request_is_streaming: bool,
        policy: &super::provider_retry::ResolvedRetryPolicy,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError> {
        if !request_is_streaming || response.is_json() {
            self.validate_responses_success_response_if(
                response,
                |error| policy.match_error(error).is_some(),
                deadline,
            )
            .await
        } else {
            self.validate_responses_stream_start_if(
                response,
                |error| policy.match_error(error).is_some(),
                deadline,
            )
            .await
        }
    }

    async fn validate_responses_success_response_if<F>(
        &self,
        response: ProxyResponse,
        should_reject: F,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError>
    where
        F: Fn(&ProxyError) -> bool,
    {
        self.validate_success_response_envelope_if(
            response,
            responses_error_envelope_message,
            "Responses upstream returned a 2xx failure",
            should_reject,
            deadline,
        )
        .await
    }

    async fn validate_success_response_envelope_if<F>(
        &self,
        response: ProxyResponse,
        detect_error: fn(&[u8]) -> Option<String>,
        error_context: &str,
        should_reject: F,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError>
    where
        F: Fn(&ProxyError) -> bool,
    {
        let status = response.status();
        let headers = response.headers().clone();
        let encoding = get_content_encoding(&headers);
        let raw = deadline
            .wait("读取上游成功响应体", response.bytes())
            .await??;
        let decoded = match encoding {
            Some(encoding) => match decompress_body(&encoding, &raw) {
                Ok(Some(decompressed)) => decompressed,
                _ => raw.to_vec(),
            },
            None => raw.to_vec(),
        };

        if let Some(message) = detect_error(&decoded) {
            let error = ProxyError::TransformError(format!("{error_context}: {message}"));
            if should_reject(&error) {
                return Err(error);
            }
        }

        Ok(ProxyResponse::buffered(status, headers, raw))
    }

    async fn validate_responses_success_response(
        &self,
        response: ProxyResponse,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError> {
        self.validate_responses_success_response_if(response, |_| true, deadline)
            .await
    }

    async fn validate_responses_stream_start(
        &self,
        response: ProxyResponse,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError> {
        self.validate_responses_stream_start_if(response, |_| true, deadline)
            .await
    }

    async fn validate_responses_stream_start_if<F>(
        &self,
        response: ProxyResponse,
        should_reject: F,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError>
    where
        F: Fn(&ProxyError) -> bool,
    {
        const MAX_PRIME_BYTES: usize = 256 * 1024;
        const SCAN_BUDGET_BYTES: usize = 16 * 1024;

        let status = response.status();
        let headers = response.headers().clone();
        let mut stream = Box::pin(response.bytes_stream());
        let mut replay_chunks: Vec<Bytes> = Vec::new();
        let mut primed_bytes = 0usize;
        let mut parse_buffer = String::new();
        let mut utf8_remainder = Vec::new();
        let mut json_probe = ResponsesJsonDocumentProbe::default();
        let mut sse_cursor = crate::proxy::sse::SseBlockCursor::default();
        let filter_outcome = |outcome: Result<(), ProxyError>| match outcome {
            Err(error) if should_reject(&error) => Err(error),
            _ => Ok(()),
        };

        loop {
            let next = match deadline
                .wait("等待 Responses 首个语义输出", stream.next())
                .await
            {
                Ok(next) => next,
                Err(error) => {
                    if should_reject(&error) {
                        return Err(error);
                    }
                    let replay =
                        futures::stream::iter(replay_chunks.into_iter().map(Ok)).chain(stream);
                    return Ok(ProxyResponse::streamed(status, headers, replay));
                }
            };

            let Some(chunk) = next else {
                let remaining = sse_cursor.remaining(&parse_buffer).trim();
                if !remaining.is_empty() {
                    let sse_phase = "解析 Responses 首个语义输出前的 SSE 事件";
                    if let Err(error) = deadline.check(sse_phase) {
                        if should_reject(&error) {
                            return Err(error);
                        }
                        let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok));
                        return Ok(ProxyResponse::streamed(status, headers, replay));
                    }
                    let outcome = inspect_responses_start_event(remaining);
                    if let Err(error) = deadline.check(sse_phase) {
                        if should_reject(&error) {
                            return Err(error);
                        }
                        let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok));
                        return Ok(ProxyResponse::streamed(status, headers, replay));
                    }
                    if let Some(outcome) = outcome {
                        filter_outcome(outcome)?;
                        let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok));
                        return Ok(ProxyResponse::streamed(status, headers, replay));
                    }
                }
                let error = ProxyError::ForwardFailed(
                    "Responses stream ended before producing output or a terminal event"
                        .to_string(),
                );
                if should_reject(&error) {
                    return Err(error);
                }
                let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok));
                return Ok(ProxyResponse::streamed(status, headers, replay));
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(source) => {
                    let error = ProxyError::ForwardFailed(format!(
                        "Failed while validating Responses stream start: {source}"
                    ));
                    if should_reject(&error) {
                        return Err(error);
                    }
                    let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok))
                        .chain(futures::stream::once(async move { Err(source) }))
                        .chain(stream);
                    return Ok(ProxyResponse::streamed(status, headers, replay));
                }
            };
            let remaining_prime_bytes = MAX_PRIME_BYTES.saturating_sub(primed_bytes);
            let inspected_len = remaining_prime_bytes.min(chunk.len());
            if inspected_len > 0 {
                let mut observed = 0usize;
                while observed < inspected_len {
                    let segment_end = (observed + SCAN_BUDGET_BYTES).min(inspected_len);
                    let segment = &chunk[observed..segment_end];
                    crate::proxy::sse::append_utf8_safe(
                        &mut parse_buffer,
                        &mut utf8_remainder,
                        segment,
                    );
                    json_probe.observe(segment);
                    primed_bytes += segment.len();
                    observed = segment_end;

                    if segment.len() == SCAN_BUDGET_BYTES {
                        if let Err(error) = deadline
                            .yield_and_check("解析 Responses 首个语义输出前的 SSE/JSON 数据")
                            .await
                        {
                            if should_reject(&error) {
                                return Err(error);
                            }
                            let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok))
                                .chain(futures::stream::once(async move { Ok(chunk) }))
                                .chain(stream);
                            return Ok(ProxyResponse::streamed(status, headers, replay));
                        }
                    }
                }
            }
            replay_chunks.push(chunk);

            // Some compatible gateways ignore `stream:true` and return a complete
            // Responses JSON document without a JSON content-type. Recognize that
            // shape before looking for SSE delimiters; pretty-printed JSON may itself
            // contain blank lines and must stay intact.
            if json_probe.take_complete() {
                let json_phase = "解析 Responses 首个语义输出前的完整 JSON";
                if let Err(error) = deadline.check(json_phase) {
                    if should_reject(&error) {
                        return Err(error);
                    }
                    let replay =
                        futures::stream::iter(replay_chunks.into_iter().map(Ok)).chain(stream);
                    return Ok(ProxyResponse::streamed(status, headers, replay));
                }
                let outcome = inspect_responses_json_document(&parse_buffer);
                if let Err(error) = deadline.check(json_phase) {
                    if should_reject(&error) {
                        return Err(error);
                    }
                    let replay =
                        futures::stream::iter(replay_chunks.into_iter().map(Ok)).chain(stream);
                    return Ok(ProxyResponse::streamed(status, headers, replay));
                }
                if let Some(outcome) = outcome {
                    filter_outcome(outcome)?;
                    let replay =
                        futures::stream::iter(replay_chunks.into_iter().map(Ok)).chain(stream);
                    return Ok(ProxyResponse::streamed(status, headers, replay));
                }
            }

            if !json_probe.is_tracking() {
                let mut scan_budget = SCAN_BUDGET_BYTES;
                loop {
                    match sse_cursor.next_block_budgeted(&parse_buffer, &mut scan_budget) {
                        crate::proxy::sse::SseScanResult::Block(block) => {
                            let sse_phase = "解析 Responses 首个语义输出前的 SSE 事件";
                            if let Err(error) = deadline.check(sse_phase) {
                                if should_reject(&error) {
                                    return Err(error);
                                }
                                let replay =
                                    futures::stream::iter(replay_chunks.into_iter().map(Ok))
                                        .chain(stream);
                                return Ok(ProxyResponse::streamed(status, headers, replay));
                            }
                            let outcome = inspect_responses_start_event(block);
                            if let Err(error) = deadline.check(sse_phase) {
                                if should_reject(&error) {
                                    return Err(error);
                                }
                                let replay =
                                    futures::stream::iter(replay_chunks.into_iter().map(Ok))
                                        .chain(stream);
                                return Ok(ProxyResponse::streamed(status, headers, replay));
                            }
                            if let Some(outcome) = outcome {
                                filter_outcome(outcome)?;
                                let replay =
                                    futures::stream::iter(replay_chunks.into_iter().map(Ok))
                                        .chain(stream);
                                return Ok(ProxyResponse::streamed(status, headers, replay));
                            }
                        }
                        crate::proxy::sse::SseScanResult::NeedMoreData => break,
                        crate::proxy::sse::SseScanResult::BudgetExhausted => {
                            scan_budget = SCAN_BUDGET_BYTES;
                        }
                    }

                    if scan_budget == SCAN_BUDGET_BYTES {
                        if let Err(error) = deadline
                            .yield_and_check("解析 Responses 首个语义输出前的 SSE 事件")
                            .await
                        {
                            if should_reject(&error) {
                                return Err(error);
                            }
                            let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok))
                                .chain(stream);
                            return Ok(ProxyResponse::streamed(status, headers, replay));
                        }
                    }
                }
            }

            if primed_bytes >= MAX_PRIME_BYTES {
                let message = format!(
                    "Responses stream exceeded the semantic priming limit of {MAX_PRIME_BYTES} bytes before producing output or a terminal event"
                );
                let error = ProxyError::ForwardFailed(message.clone());
                if should_reject(&error) {
                    return Err(error);
                }
                log::warn!(
                    "[Responses] {message}; committing buffered stream because the retry policy did not match"
                );
                let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok)).chain(stream);
                return Ok(ProxyResponse::streamed(status, headers, replay));
            }
        }
    }

    async fn prime_streaming_response(
        &self,
        response: ProxyResponse,
        deadline: PreOutputDeadline,
    ) -> Result<ProxyResponse, ProxyError> {
        if !deadline.is_enabled() {
            return Ok(response);
        }

        let status = response.status();
        let headers = response.headers().clone();
        let mut stream = Box::pin(response.bytes_stream());

        let first = deadline
            .wait("等待流式响应首个数据块", stream.next())
            .await?;

        let Some(first) = first else {
            return Err(ProxyError::ForwardFailed(
                "流式响应在首包到达前结束".to_string(),
            ));
        };

        let first =
            first.map_err(|e| ProxyError::ForwardFailed(format!("读取流式响应首包失败: {e}")))?;

        let replay = futures::stream::once(async move { Ok(first) }).chain(stream);
        Ok(ProxyResponse::streamed(status, headers, replay))
    }

    async fn resolve_claude_api_format(
        &self,
        provider: &Provider,
        body: &Value,
        is_copilot: bool,
    ) -> String {
        if !is_copilot {
            return super::providers::get_claude_api_format(provider).to_string();
        }

        let model = body.get("model").and_then(|value| value.as_str());
        if let Some(model_id) = model {
            if self
                .is_copilot_openai_vendor_model(provider, model_id)
                .await
            {
                return "openai_responses".to_string();
            }
        }

        "openai_chat".to_string()
    }

    /// 用 Copilot live `/models` 列表确认 model ID 真实可用，找不到时按 family 降级。
    /// 命中缓存后是同步的；首次请求或 5 min 缓存过期后会触发一次 HTTP。
    async fn apply_copilot_live_model_resolution(
        &self,
        provider: &Provider,
        body: &mut serde_json::Value,
    ) {
        let Some(model_id) = body.get("model").and_then(|v| v.as_str()) else {
            return;
        };
        let model_id = model_id.to_string();

        let Some(app_handle) = &self.app_handle else {
            return;
        };
        let copilot_state = app_handle.state::<CopilotAuthState>();
        let copilot_auth = copilot_state.0.read().await;
        let account_id = provider
            .meta
            .as_ref()
            .and_then(|m| m.managed_account_id_for("github_copilot"));

        let models_result = match account_id.as_deref() {
            Some(id) => copilot_auth.fetch_models_for_account(id).await,
            None => copilot_auth.fetch_models().await,
        };

        let models = match models_result {
            Ok(m) => m,
            Err(err) => {
                log::debug!("[Copilot] live model list unavailable, skip resolution: {err}");
                return;
            }
        };

        if let Some(resolved) =
            super::providers::copilot_model_map::resolve_against_models(&model_id, &models)
        {
            log::info!("[Copilot] live-model resolve: {model_id} → {resolved}");
            body["model"] = serde_json::Value::String(resolved);
        }
    }

    async fn is_copilot_openai_vendor_model(&self, provider: &Provider, model_id: &str) -> bool {
        let Some(app_handle) = &self.app_handle else {
            log::debug!("[Copilot] AppHandle unavailable, fallback to chat/completions");
            return false;
        };

        let copilot_state = app_handle.state::<CopilotAuthState>();
        let copilot_auth = copilot_state.0.read().await;
        let account_id = provider
            .meta
            .as_ref()
            .and_then(|m| m.managed_account_id_for("github_copilot"));

        let vendor_result = match account_id.as_deref() {
            Some(id) => {
                copilot_auth
                    .get_model_vendor_for_account(id, model_id)
                    .await
            }
            None => copilot_auth.get_model_vendor(model_id).await,
        };

        match vendor_result {
            Ok(Some(vendor)) => vendor.eq_ignore_ascii_case("openai"),
            Ok(None) => {
                log::debug!(
                    "[Copilot] Model vendor unavailable for {model_id}, fallback to chat/completions"
                );
                false
            }
            Err(err) => {
                log::warn!(
                    "[Copilot] Failed to resolve model vendor for {model_id}, fallback to chat/completions: {err}"
                );
                false
            }
        }
    }

    fn categorize_proxy_error(&self, error: &ProxyError, provider: &Provider) -> ErrorCategory {
        // Authentication belongs to the Codex client for the built-in official
        // route. Retrying another provider would silently move the conversation
        // away from the selected official account and poison its health state.
        if super::providers::is_codex_official_provider(provider)
            && (matches!(error, ProxyError::AuthError(_))
                || matches!(
                    error,
                    ProxyError::UpstreamError {
                        status: 401 | 403,
                        ..
                    } | ProxyError::UpstreamBodyTimeout {
                        status: 401 | 403,
                        ..
                    }
                ))
        {
            return ErrorCategory::NonRetryable;
        }

        // xAI OAuth mirrors the same rule for token acquisition: a local
        // AuthError means the managed account needs re-login. Failing over
        // would silently move the conversation off the selected Grok account
        // and poison the provider's health state for an account-level issue.
        if provider.is_xai_oauth() && matches!(error, ProxyError::AuthError(_)) {
            return ErrorCategory::NonRetryable;
        }

        match error {
            // 网络和上游错误：都应该尝试下一个供应商
            ProxyError::Timeout(_) => ErrorCategory::Retryable,
            ProxyError::ForwardFailed(_) => ErrorCategory::Retryable,
            ProxyError::ProviderUnhealthy(_) => ErrorCategory::Retryable,
            // 上游 HTTP 错误：按状态码分桶。
            //
            // 客户端请求自身有问题的状态码无论换哪个 provider 都会被拒绝，
            // 继续轮询只会放大错误率、污染熔断器健康度、浪费配额：
            //   400 Bad Request / 422 Unprocessable Entity   ← 请求体格式或语义错误
            //   405 Method Not Allowed / 406 Not Acceptable  ← 方法或 Accept 错误
            //   413 Payload Too Large / 414 URI Too Long     ← 客户端构造超限
            //   415 Unsupported Media Type                    ← Content-Type 错误
            //   501 Not Implemented                           ← 上游协议确实不支持
            //
            // 其他 4xx（401/403/404/408/409/429/451 等）和全部 5xx 都保留
            // Retryable —— 换一家 provider 可能持有不同的 key、配额、地域或模型映射。
            ProxyError::UpstreamError { status, .. }
            | ProxyError::UpstreamBodyTimeout { status, .. } => match *status {
                400 | 405 | 406 | 413 | 414 | 415 | 422 | 501 => ErrorCategory::NonRetryable,
                _ => ErrorCategory::Retryable,
            },
            // Provider 级配置/转换问题：换一个 Provider 可能就能成功
            ProxyError::ConfigError(_) => ErrorCategory::Retryable,
            ProxyError::TransformError(_) => ErrorCategory::Retryable,
            ProxyError::AuthError(_) => ErrorCategory::Retryable,
            ProxyError::StreamIdleTimeout(_) => ErrorCategory::Retryable,
            // 无可用供应商：所有供应商都试过了，无法重试
            ProxyError::NoAvailableProvider => ErrorCategory::NonRetryable,
            // 其他错误（数据库/内部错误等）：不是换供应商能解决的问题
            _ => ErrorCategory::NonRetryable,
        }
    }
}

/// 从 ProxyError 中提取错误消息
fn extract_error_message(error: &ProxyError) -> Option<String> {
    match error {
        ProxyError::UpstreamError { body, .. } => body.clone(),
        _ => Some(error.to_string()),
    }
}

/// 检测 Provider 是否为 Bedrock（通过 CLAUDE_CODE_USE_BEDROCK 环境变量判断）
fn is_bedrock_provider(provider: &Provider) -> bool {
    provider
        .settings_config
        .get("env")
        .and_then(|e| e.get("CLAUDE_CODE_USE_BEDROCK"))
        .and_then(|v| v.as_str())
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn build_retryable_failure_log(
    provider_name: &str,
    attempted_providers: usize,
    total_providers: usize,
    error: &ProxyError,
) -> (&'static str, String) {
    let error_summary = summarize_proxy_error(error);

    if total_providers <= 1 {
        (
            log_fwd::SINGLE_PROVIDER_FAILED,
            format!("Provider {provider_name} 请求失败: {error_summary}"),
        )
    } else {
        (
            log_fwd::PROVIDER_FAILED_RETRY,
            format!(
                "Provider {provider_name} 失败，继续尝试下一个 ({attempted_providers}/{total_providers}): {error_summary}"
            ),
        )
    }
}

fn build_terminal_failure_log(
    attempted_providers: usize,
    total_providers: usize,
    last_error: Option<&ProxyError>,
) -> Option<(&'static str, String)> {
    if total_providers <= 1 {
        return None;
    }

    let error_summary = last_error
        .map(summarize_proxy_error)
        .unwrap_or_else(|| "未知错误".to_string());

    Some((
        log_fwd::ALL_PROVIDERS_FAILED,
        format!(
            "已尝试 {attempted_providers}/{total_providers} 个 Provider，均失败。最后错误: {error_summary}"
        ),
    ))
}

fn summarize_proxy_error(error: &ProxyError) -> String {
    match error {
        ProxyError::UpstreamError { status, body } => {
            let body_summary = body
                .as_deref()
                .map(summarize_upstream_body)
                .filter(|summary| !summary.is_empty());

            match body_summary {
                Some(summary) => format!("上游 HTTP {status}: {summary}"),
                None => format!("上游 HTTP {status}"),
            }
        }
        ProxyError::UpstreamBodyTimeout {
            status,
            timeout_seconds,
        } => format!("上游 HTTP {status} 错误正文读取超时: {timeout_seconds}s"),
        ProxyError::Timeout(message) => {
            format!("请求超时: {}", summarize_text_for_log(message, 180))
        }
        ProxyError::ForwardFailed(message) => {
            format!("请求转发失败: {}", summarize_text_for_log(message, 180))
        }
        ProxyError::TransformError(message) => {
            format!("响应转换失败: {}", summarize_text_for_log(message, 180))
        }
        ProxyError::ConfigError(message) => {
            format!("配置错误: {}", summarize_text_for_log(message, 180))
        }
        ProxyError::AuthError(message) => {
            format!("认证失败: {}", summarize_text_for_log(message, 180))
        }
        _ => summarize_text_for_log(&error.to_string(), 180),
    }
}

fn summarize_upstream_body(body: &str) -> String {
    if let Ok(json_body) = serde_json::from_str::<Value>(body) {
        if let Some(message) = extract_json_error_message(&json_body) {
            return summarize_text_for_log(&message, 180);
        }

        if let Ok(compact_json) = serde_json::to_string(&json_body) {
            return summarize_text_for_log(&compact_json, 180);
        }
    }

    summarize_text_for_log(body, 180)
}

fn extract_json_error_message(body: &Value) -> Option<String> {
    let candidates = [
        body.pointer("/error/message"),
        body.pointer("/message"),
        body.pointer("/detail"),
        body.pointer("/error"),
    ];

    candidates
        .into_iter()
        .flatten()
        .find_map(|value| value.as_str().map(ToString::to_string))
}

fn split_endpoint_and_query(endpoint: &str) -> (&str, Option<&str>) {
    endpoint
        .split_once('?')
        .map_or((endpoint, None), |(path, query)| (path, Some(query)))
}

fn strip_beta_query(query: Option<&str>) -> Option<String> {
    let filtered = query.map(|query| {
        query
            .split('&')
            .filter(|pair| !pair.is_empty() && !pair.starts_with("beta="))
            .collect::<Vec<_>>()
            .join("&")
    });

    match filtered.as_deref() {
        Some("") | None => None,
        Some(_) => filtered,
    }
}

fn is_claude_messages_path(path: &str) -> bool {
    matches!(path, "/v1/messages" | "/claude/v1/messages")
}

fn rewrite_codex_responses_endpoint_to_chat(endpoint: &str) -> (String, Option<String>) {
    let (_path, query) = split_endpoint_and_query(endpoint);
    let passthrough_query = query.map(ToString::to_string);
    let target_path = "/chat/completions";
    let rewritten = match passthrough_query.as_deref() {
        Some(query) if !query.is_empty() => format!("{target_path}?{query}"),
        _ => target_path.to_string(),
    };

    (rewritten, passthrough_query)
}

/// Claude Code client fingerprint (used for Codex→Anthropic emulation to pass a
/// gateway's "Claude Code only" check).
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/1.0.119 (external, cli)";
const CLAUDE_CODE_SYSTEM_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Insert the Claude Code identity as the first line before the `system` field in
/// the Anthropic request body.
///
/// Anthropic subscription/OAuth plans require the first system block to be exactly
/// this identity line. After conversion `system` is a string (from Codex
/// instructions); normalize it into an array here: [identity line, original system...].
fn prepend_claude_code_system_prompt(body: &mut Value) {
    let identity = serde_json::json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM_IDENTITY });
    let mut blocks: Vec<Value> = vec![identity];
    match body.get("system") {
        Some(Value::String(existing)) if !existing.is_empty() => {
            blocks.push(serde_json::json!({ "type": "text", "text": existing }));
        }
        Some(Value::Array(existing)) => {
            // Idempotent: skip re-injection if the first block is already the identity line.
            if existing
                .first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                == Some(CLAUDE_CODE_SYSTEM_IDENTITY)
            {
                return;
            }
            blocks.extend(existing.iter().cloned());
        }
        _ => {}
    }
    body["system"] = Value::Array(blocks);
}

/// Headers a native Claude Code client never sends but the Codex/OpenAI CLI (and its
/// stainless SDK layer) do. Dropped for every Codex→Anthropic request so the upstream sees a
/// clean Anthropic client fingerprint. Centralized here so the set stays in one place and future
/// additions can't miss a code path. `key_str` is already lowercased by the http crate.
/// Whether `base_url` already ends in `endpoint_suffix` (e.g. `/v1/messages` or
/// `/chat/completions`), ignoring surrounding whitespace, any `?query`/`#fragment`, and a
/// trailing slash. Used to avoid double-appending the endpoint when a user pastes a full
/// URL but leaves the "full URL" switch off (`.../v1/messages` → `.../v1/messages/v1/messages`,
/// a non-retryable 400). `endpoint_suffix` must be lowercase.
fn base_url_is_full_endpoint(base_url: &str, endpoint_suffix: &str) -> bool {
    let trimmed = base_url.trim();
    // Match against the path only: a `?query`/`#fragment` on a full endpoint URL must not
    // hide the suffix (`.../v1/messages?beta=true` still ends in the endpoint).
    let path = match trimmed.split_once(['?', '#']) {
        Some((head, _)) => head,
        None => trimmed,
    };
    path.trim_end_matches('/')
        .to_ascii_lowercase()
        .ends_with(endpoint_suffix)
}

fn is_codex_client_fingerprint_header(key_str: &str) -> bool {
    matches!(
        key_str,
        "originator"
            | "session_id"
            | "session-id"
            | "thread-id"
            | "conversation_id"
            | "chatgpt-account-id"
            | "x-openai-subagent"
            | "x-client-request-id"
            | "openai-beta"
            | "openai-organization"
            | "openai-project"
    ) || key_str.starts_with("x-stainless-")
        || key_str.starts_with("x-codex-")
}

fn codex_anthropic_error_envelope_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") && value.get("error").is_none() {
        return None;
    }
    let error = value.get("error").unwrap_or(&value);
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    Some(format!("{error_type}: {message}"))
}

fn responses_error_envelope_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    responses_error_envelope_message_from_value(&value)
}

fn responses_error_envelope_message_from_value(value: &Value) -> Option<String> {
    let response = value.get("response").unwrap_or(value);
    let status = response.get("status").and_then(Value::as_str);
    let event_type = value.get("type").and_then(Value::as_str);
    let has_error = response
        .get("error")
        .or_else(|| value.get("error"))
        .is_some_and(|error| !error.is_null());
    if !matches!(status, Some("failed" | "cancelled"))
        && !matches!(event_type, Some("error" | "response.failed"))
        && !has_error
    {
        return None;
    }

    let error = response
        .get("error")
        .or_else(|| value.get("error"))
        .unwrap_or(response);
    let numeric_code = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok());
    let mut error_type = error
        .get("type")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            error
                .get("code")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            error
                .get("status")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            error
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .or_else(|| event_type.map(str::to_string))
        .unwrap_or_else(|| status.unwrap_or("error").to_string());
    if let Some(code) = numeric_code {
        let mapped = match code {
            429 => "rate_limit",
            503 => "overloaded",
            500..=599 => "server_error",
            _ => "error_code",
        };
        if error_type == status.unwrap_or("error") || error_type == "error" {
            error_type = mapped.to_string();
        }
        error_type = format!("{error_type} ({code})");
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(match status {
            Some("cancelled") => "response generation was cancelled",
            _ => "response generation failed",
        });
    Some(format!("{error_type}: {message}"))
}

/// Prompt caching is part of the Codex→Anthropic protocol bridge rather than an
/// optional Bedrock optimizer. Codex requests do not contain Anthropic
/// `cache_control`, so keep bridge caching on by default while still honoring the
/// dedicated cache-injection switch. Injected breakpoints always use Anthropic's
/// standard 5-minute TTL.
fn codex_anthropic_cache_config(config: &OptimizerConfig) -> OptimizerConfig {
    OptimizerConfig {
        enabled: true,
        thinking_optimizer: false,
        cache_injection: config.cache_injection,
    }
}

/// A streaming request may receive a whole JSON document even when the gateway
/// omits `application/json`. `None` means either "not JSON" or "not complete yet";
/// a parsed document is safe to commit unless it is a semantic failure envelope.
fn inspect_responses_json_document(buffer: &str) -> Option<Result<(), ProxyError>> {
    let trimmed = buffer.trim();
    if !matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'[')) {
        return None;
    }
    #[cfg(test)]
    RESPONSES_JSON_DOCUMENT_PARSE_COUNT.with(|count| count.set(count.get() + 1));
    let value: Value = serde_json::from_str(trimmed).ok()?;
    if let Some(message) = responses_error_envelope_message_from_value(&value) {
        return Some(Err(ProxyError::TransformError(format!(
            "Responses upstream returned a 2xx failure: {message}"
        ))));
    }
    Some(Ok(()))
}

#[cfg(test)]
thread_local! {
    static RESPONSES_JSON_DOCUMENT_PARSE_COUNT: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static RESPONSES_SSE_INSPECT_DELAY: std::cell::Cell<std::time::Duration> = const {
        std::cell::Cell::new(std::time::Duration::ZERO)
    };
}

#[cfg(test)]
fn reset_responses_json_document_parse_count() {
    RESPONSES_JSON_DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn responses_json_document_parse_count() -> usize {
    RESPONSES_JSON_DOCUMENT_PARSE_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn set_responses_sse_inspect_delay(delay: std::time::Duration) {
    RESPONSES_SSE_INSPECT_DELAY.with(|configured| configured.set(delay));
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ResponsesJsonProbeState {
    #[default]
    Undetermined,
    NotJson,
    Tracking,
    Complete,
    Consumed,
}

#[derive(Debug, Default)]
struct ResponsesJsonDocumentProbe {
    state: ResponsesJsonProbeState,
    depth: usize,
    in_string: bool,
    escaped: bool,
}

impl ResponsesJsonDocumentProbe {
    fn observe(&mut self, appended: &[u8]) {
        for &byte in appended {
            match self.state {
                ResponsesJsonProbeState::Undetermined => {
                    if byte.is_ascii_whitespace() {
                        continue;
                    }
                    if matches!(byte, b'{' | b'[') {
                        self.state = ResponsesJsonProbeState::Tracking;
                        self.depth = 1;
                    } else {
                        self.state = ResponsesJsonProbeState::NotJson;
                        return;
                    }
                }
                ResponsesJsonProbeState::Tracking if self.in_string => {
                    if self.escaped {
                        self.escaped = false;
                    } else if byte == b'\\' {
                        self.escaped = true;
                    } else if byte == b'"' {
                        self.in_string = false;
                    }
                }
                ResponsesJsonProbeState::Tracking => match byte {
                    b'"' => self.in_string = true,
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' => {
                        self.depth = self.depth.saturating_sub(1);
                        if self.depth == 0 {
                            self.state = ResponsesJsonProbeState::Complete;
                            return;
                        }
                    }
                    _ => {}
                },
                ResponsesJsonProbeState::NotJson
                | ResponsesJsonProbeState::Complete
                | ResponsesJsonProbeState::Consumed => return,
            }
        }
    }

    fn is_tracking(&self) -> bool {
        self.state == ResponsesJsonProbeState::Tracking
    }

    fn take_complete(&mut self) -> bool {
        if self.state != ResponsesJsonProbeState::Complete {
            return false;
        }
        self.state = ResponsesJsonProbeState::Consumed;
        true
    }
}

/// Inspect one complete Responses SSE block while the response is still inside
/// the retry loop. `None` means the event is lifecycle-only and priming should
/// continue; `Some(Ok(()))` means it is safe to commit/replay the stream.
fn inspect_responses_start_event(block: &str) -> Option<Result<(), ProxyError>> {
    let mut named_event = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(event) = crate::proxy::sse::strip_sse_field(line, "event") {
            named_event = Some(event.trim().to_string());
        } else if let Some(data) = crate::proxy::sse::strip_sse_field(line, "data") {
            data_lines.push(data);
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        return Some(Ok(()));
    }
    let value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(_) => return None,
    };
    #[cfg(test)]
    RESPONSES_SSE_INSPECT_DELAY.with(|configured| {
        let delay = configured.get();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    });
    let event = named_event
        .as_deref()
        .filter(|event| !event.is_empty())
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");

    if let Some(message) = responses_error_envelope_message_from_value(&value) {
        return Some(Err(ProxyError::TransformError(format!(
            "Responses upstream returned a 2xx failure: {message}"
        ))));
    }

    match event {
        "response.failed" | "error" => {
            let response = value.get("response").unwrap_or(&value);
            let error = response.get("error").unwrap_or(response);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
                .unwrap_or("Responses upstream emitted an error before output");
            let error_type = error
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| error.get("code").and_then(Value::as_str))
                .unwrap_or("upstream_error");
            Some(Err(ProxyError::TransformError(format!(
                "Responses upstream {error_type}: {message}"
            ))))
        }
        "response.created"
        | "response.in_progress"
        | "response.queued"
        | "message_start"
        | "ping" => None,
        "content_block_start" => {
            let block = value.get("content_block").unwrap_or(&Value::Null);
            let has_text = block
                .get("text")
                .or_else(|| block.get("thinking"))
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty());
            if has_text || anthropic_content_block_has_productive_output(block) {
                Some(Ok(()))
            } else {
                None
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta").unwrap_or(&Value::Null);
            let has_output = ["text", "thinking", "partial_json", "signature"]
                .iter()
                .any(|key| {
                    delta
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|output| !output.is_empty())
                });
            has_output.then_some(Ok(()))
        }
        "response.output_item.added"
        | "response.output_item.done"
        | "response.content_part.added"
        | "response.content_part.done"
        | "response.reasoning_summary_part.added"
        | "response.reasoning_summary_part.done"
        | "response.output_text.done"
        | "response.refusal.done"
        | "response.reasoning_summary_text.done"
        | "response.reasoning_text.done" => {
            responses_event_has_productive_output(&value).then_some(Ok(()))
        }
        "" => sse_json_has_productive_output(&value).then_some(Ok(())),
        "response.completed" | "response.incomplete" | "message_stop" => Some(Ok(())),
        "response.output_text.delta"
        | "response.refusal.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
        | "response.reasoning.delta"
        | "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta" => {
            responses_event_has_productive_output(&value).then_some(Ok(()))
        }
        // Unknown and lifecycle-only events stay buffered so a following failure can
        // still trigger same-provider retry before semantic output is exposed.
        _ => None,
    }
}

enum RoleStreamTerminal {
    Success,
    Failure(String),
}

fn inspect_role_stream_terminal(block: &str) -> Option<RoleStreamTerminal> {
    if let Some(Err(error)) = inspect_responses_start_event(block) {
        return Some(RoleStreamTerminal::Failure(error.to_string()));
    }

    let mut named_event = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(event) = crate::proxy::sse::strip_sse_field(line, "event") {
            named_event = Some(event.trim().to_string());
        } else if let Some(data) = crate::proxy::sse::strip_sse_field(line, "data") {
            data_lines.push(data);
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        return Some(RoleStreamTerminal::Success);
    }
    let value: Value = serde_json::from_str(&data).ok()?;
    let event = named_event
        .as_deref()
        .filter(|event| !event.is_empty())
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");
    matches!(
        event,
        "response.completed" | "response.incomplete" | "message_stop"
    )
    .then_some(RoleStreamTerminal::Success)
}

fn responses_event_has_productive_output(value: &Value) -> bool {
    fn inspect(value: &Value) -> bool {
        match value {
            Value::Array(values) => values.iter().any(inspect),
            Value::Object(object) => {
                if responses_tool_object_has_productive_output(object) {
                    return true;
                }

                for key in [
                    "text",
                    "delta",
                    "refusal",
                    "thinking",
                    "summary_text",
                    "arguments",
                    "partial_json",
                    "code",
                    "encrypted_content",
                ] {
                    if object
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|output| !output.is_empty())
                    {
                        return true;
                    }
                }

                ["item", "part", "content", "summary"]
                    .iter()
                    .any(|key| object.get(*key).is_some_and(inspect))
            }
            _ => false,
        }
    }

    inspect(value)
}

fn meaningful_tool_payload(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) => false,
        Value::Number(_) => true,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(meaningful_tool_payload),
        Value::Object(object) => object.values().any(meaningful_tool_payload),
    }
}

fn object_has_meaningful_tool_field(
    object: &serde_json::Map<String, Value>,
    fields: &[&str],
) -> bool {
    fields
        .iter()
        .any(|field| object.get(*field).is_some_and(meaningful_tool_payload))
}

fn responses_tool_object_has_productive_output(object: &serde_json::Map<String, Value>) -> bool {
    let Some(kind) = object.get("type").and_then(Value::as_str) else {
        return false;
    };

    match kind {
        "function_call" | "custom_tool_call" => object_has_meaningful_tool_field(
            object,
            &[
                "id",
                "call_id",
                "name",
                "arguments",
                "delta",
                "input",
                "partial_json",
            ],
        ),
        "tool_search_call" => object_has_meaningful_tool_field(
            object,
            &[
                "id",
                "call_id",
                "query",
                "queries",
                "arguments",
                "args",
                "status",
                "input",
            ],
        ),
        "computer_call"
        | "web_search_call"
        | "file_search_call"
        | "code_interpreter_call"
        | "image_generation_call"
        | "local_shell_call"
        | "mcp_call" => object_has_meaningful_tool_field(
            object,
            &[
                "id",
                "call_id",
                "name",
                "arguments",
                "args",
                "input",
                "action",
                "command",
                "code",
                "query",
                "status",
                "delta",
            ],
        ),
        _ => false,
    }
}

fn anthropic_content_block_has_productive_output(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("tool_use" | "server_tool_use") => object_has_meaningful_tool_field(
            object,
            &["id", "tool_use_id", "name", "input", "arguments"],
        ),
        Some("web_search_tool_result") => object_has_meaningful_tool_field(
            object,
            &["tool_use_id", "content", "result", "error_code"],
        ),
        _ => false,
    }
}

fn terminal_marker_is_productive(value: &Value) -> bool {
    match value {
        Value::String(value) => !value.trim().is_empty(),
        Value::Number(_) => true,
        _ => false,
    }
}

fn openai_function_call_has_productive_output(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object_has_meaningful_tool_field(
            object,
            &[
                "id",
                "call_id",
                "name",
                "arguments",
                "args",
                "delta",
                "input",
            ],
        )
    })
}

fn openai_tool_call_has_productive_output(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object_has_meaningful_tool_field(object, &["id", "call_id", "name", "arguments"])
            || object
                .get("function")
                .is_some_and(openai_function_call_has_productive_output)
            || object
                .get("custom")
                .is_some_and(openai_function_call_has_productive_output)
    })
}

fn gemini_part_has_productive_output(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if ["text", "thought", "thoughtSignature"].iter().any(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    }) {
        return true;
    }
    if ["functionCall", "function_call"].iter().any(|key| {
        object
            .get(*key)
            .is_some_and(openai_function_call_has_productive_output)
    }) {
        return true;
    }
    if object
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| calls.iter().any(openai_tool_call_has_productive_output))
    {
        return true;
    }

    ["inlineData", "inline_data", "fileData", "file_data"]
        .iter()
        .any(|key| {
            object.get(*key).is_some_and(|media| {
                media.as_object().is_some_and(|media| {
                    object_has_meaningful_tool_field(media, &["data", "fileUri", "file_uri"])
                })
            })
        })
}

fn sse_json_has_productive_output(value: &Value) -> bool {
    if matches!(
        value.get("status").and_then(Value::as_str),
        Some("completed" | "incomplete")
    ) {
        return true;
    }

    if value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = choice
                    .get("delta")
                    .or_else(|| choice.get("message"))
                    .unwrap_or(&Value::Null);
                let has_text = ["content", "refusal", "reasoning_content"]
                    .iter()
                    .any(|key| {
                        delta
                            .get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                    });
                has_text
                    || delta
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| {
                            calls.iter().any(openai_tool_call_has_productive_output)
                        })
                    || delta
                        .get("function_call")
                        .is_some_and(openai_function_call_has_productive_output)
                    || choice
                        .get("finish_reason")
                        .is_some_and(terminal_marker_is_productive)
            })
        })
    {
        return true;
    }

    value
        .get("candidates")
        .and_then(Value::as_array)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate
                    .get("content")
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
                    .is_some_and(|parts| parts.iter().any(gemini_part_has_productive_output))
                    || candidate
                        .get("finishReason")
                        .is_some_and(terminal_marker_is_productive)
            })
        })
}

/// Rewrite Codex's `/responses` (and variants) to Anthropic's `/v1/messages`, preserving the query.
fn rewrite_codex_responses_endpoint_to_anthropic(endpoint: &str) -> (String, Option<String>) {
    let (_path, query) = split_endpoint_and_query(endpoint);
    let passthrough_query = query.map(ToString::to_string);
    let target_path = "/v1/messages";
    let rewritten = match passthrough_query.as_deref() {
        Some(query) if !query.is_empty() => format!("{target_path}?{query}"),
        _ => target_path.to_string(),
    };

    (rewritten, passthrough_query)
}

fn rewrite_claude_transform_endpoint(
    endpoint: &str,
    api_format: &str,
    is_copilot: bool,
    body: &Value,
) -> (String, Option<String>) {
    let (path, query) = split_endpoint_and_query(endpoint);
    let passthrough_query = if is_claude_messages_path(path) {
        strip_beta_query(query)
    } else {
        query.map(ToString::to_string)
    };

    if !is_claude_messages_path(path) {
        return (endpoint.to_string(), passthrough_query);
    }

    if api_format == "gemini_native" {
        let model =
            super::providers::transform_gemini::extract_gemini_model(body).unwrap_or("unknown");
        // Accept both bare ids (`gemini-2.5-pro`) and the resource-name
        // form (`models/gemini-2.5-pro`) that Gemini SDKs emit. See
        // `normalize_gemini_model_id` for rationale.
        let model = super::gemini_url::normalize_gemini_model_id(model);
        let is_stream = body
            .get("stream")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let target_path = if is_stream {
            format!("/v1beta/models/{model}:streamGenerateContent")
        } else {
            format!("/v1beta/models/{model}:generateContent")
        };

        let rewritten_query = merge_query_params(
            passthrough_query.as_deref(),
            if is_stream { Some("alt=sse") } else { None },
        );

        let rewritten = match rewritten_query.as_deref() {
            Some(query) if !query.is_empty() => format!("{target_path}?{query}"),
            _ => target_path,
        };

        return (rewritten, rewritten_query);
    }

    let target_path = if is_copilot && api_format == "openai_responses" {
        "/v1/responses"
    } else if is_copilot {
        "/chat/completions"
    } else if api_format == "openai_responses" {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    };

    let rewritten = match passthrough_query.as_deref() {
        Some(query) if !query.is_empty() => format!("{target_path}?{query}"),
        _ => target_path.to_string(),
    };

    (rewritten, passthrough_query)
}

fn merge_query_params(base_query: Option<&str>, extra_param: Option<&str>) -> Option<String> {
    let mut params: Vec<String> = base_query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter(|pair| !pair.is_empty())
        .filter(|pair| !pair.starts_with("alt="))
        .map(ToString::to_string)
        .collect();

    if let Some(extra_param) = extra_param {
        params.push(extra_param.to_string());
    }

    if params.is_empty() {
        None
    } else {
        Some(params.join("&"))
    }
}

fn append_query_to_full_url(base_url: &str, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => {
            if base_url.contains('?') {
                format!("{base_url}&{query}")
            } else {
                format!("{base_url}?{query}")
            }
        }
        _ => base_url.to_string(),
    }
}

fn build_codex_oauth_session_headers(
    session_id: &str,
) -> Vec<(http::HeaderName, http::HeaderValue)> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Vec::new();
    }

    let mut headers = Vec::new();
    if let Ok(value) = http::HeaderValue::from_str(session_id) {
        headers.push((http::HeaderName::from_static("session_id"), value.clone()));
        headers.push((http::HeaderName::from_static("x-client-request-id"), value));
    }

    let window_id = format!("{session_id}:0");
    if let Ok(value) = http::HeaderValue::from_str(&window_id) {
        headers.push((http::HeaderName::from_static("x-codex-window-id"), value));
    }

    headers
}

fn reject_proxy_placeholder_for_managed_account_upstream(
    url: &str,
    headers: &http::HeaderMap,
) -> Result<(), ProxyError> {
    if !is_managed_account_upstream_url(url) || !headers_contain_proxy_placeholder(headers) {
        return Ok(());
    }

    Err(ProxyError::AuthError(
        "Managed account proxy auth was not resolved; PROXY_MANAGED must not be sent upstream"
            .to_string(),
    ))
}

fn is_managed_account_upstream_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<http::Uri>() else {
        return false;
    };

    let Some(host) = uri.host().map(str::to_ascii_lowercase) else {
        return false;
    };

    host == "githubcopilot.com"
        || host.ends_with(".githubcopilot.com")
        || (host == "chatgpt.com" && uri.path().starts_with("/backend-api/codex"))
        || (host == "api.x.ai" && uri.path().starts_with("/v1/"))
}

fn headers_contain_proxy_placeholder(headers: &http::HeaderMap) -> bool {
    headers.values().any(|value| {
        value
            .to_str()
            .map(|value| value.contains(PROXY_AUTH_PLACEHOLDER))
            .unwrap_or(false)
    })
}

fn should_preserve_exact_header_case(
    adapter_name: &str,
    provider: &Provider,
    resolved_claude_api_format: Option<&str>,
    is_copilot: bool,
) -> bool {
    if matches!(adapter_name, "Codex" | "Gemini") {
        return false;
    }

    if is_copilot || provider.is_codex_oauth() || provider.is_xai_oauth() {
        return false;
    }

    matches!(resolved_claude_api_format, None | Some("anthropic"))
}

fn is_streaming_request(endpoint: &str, body: &Value, headers: &axum::http::HeaderMap) -> bool {
    if body
        .get("stream")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return true;
    }

    if endpoint.contains("streamGenerateContent") || endpoint.contains("alt=sse") {
        return true;
    }

    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|accept| accept.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false)
}

fn retry_log_model<'a>(app_type: &AppType, endpoint: &'a str, body: &'a Value) -> &'a str {
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        return model;
    }
    if matches!(app_type, AppType::Gemini) {
        let path = endpoint.split('?').next().unwrap_or(endpoint);
        if let Some(after_models) = path.rsplit_once("/models/").map(|(_, model)| model) {
            if let Some(model) = after_models
                .strip_suffix(":streamGenerateContent")
                .or_else(|| after_models.strip_suffix(":generateContent"))
            {
                return model;
            }
        }
    }
    "unknown"
}

fn role_route_fallback(attempted_providers: usize) -> bool {
    attempted_providers > 0
}

#[cfg(test)]
fn should_force_identity_encoding(
    endpoint: &str,
    body: &Value,
    headers: &axum::http::HeaderMap,
) -> bool {
    is_streaming_request(endpoint, body, headers)
}

fn map_reqwest_send_error(error: reqwest::Error) -> ProxyError {
    if error.is_timeout() {
        ProxyError::Timeout(format!("上游请求超时: {}", error.without_url()))
    } else if error.is_connect() {
        ProxyError::ForwardFailed(format!("上游连接失败: {}", error.without_url()))
    } else {
        ProxyError::ForwardFailed(format!("上游请求发送失败: {}", error.without_url()))
    }
}

fn summarize_text_for_log(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = normalized.trim();

    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let truncated: String = trimmed.chars().take(max_chars).collect();
    let truncated = truncated.trim_end();
    format!("{truncated}...")
}

fn apply_local_proxy_body_overrides(
    body: &mut Value,
    overrides: &LocalProxyRequestOverrides,
) -> bool {
    let Some(override_body) = overrides.body.as_ref() else {
        return false;
    };

    if !override_body.is_object() {
        log::warn!("[LocalProxyOverrides] Ignoring body override because it is not an object");
        return false;
    }

    merge_json_override(body, override_body)
}

fn merge_json_override(target: &mut Value, patch: &Value) -> bool {
    merge_json_override_inner(target, patch, true)
}

fn merge_json_override_inner(target: &mut Value, patch: &Value, is_top_level: bool) -> bool {
    match (target, patch) {
        (Value::Object(target_map), Value::Object(patch_map)) => {
            let mut changed = false;
            for (key, patch_value) in patch_map {
                if is_top_level && key == "stream" {
                    log::warn!(
                        "[LocalProxyOverrides] Ignoring body override for protected field: stream"
                    );
                    continue;
                }
                match target_map.get_mut(key) {
                    Some(target_value) => {
                        changed |= merge_json_override_inner(target_value, patch_value, false);
                    }
                    None => {
                        target_map.insert(key.clone(), patch_value.clone());
                        changed = true;
                    }
                }
            }
            changed
        }
        (target_value, patch_value) => {
            if target_value == patch_value {
                false
            } else {
                *target_value = patch_value.clone();
                true
            }
        }
    }
}

fn apply_local_proxy_header_overrides(
    headers: &mut http::HeaderMap,
    overrides: Option<&LocalProxyRequestOverrides>,
    is_copilot: bool,
) {
    if is_copilot {
        return;
    }

    let Some(header_overrides) = overrides.map(|overrides| &overrides.headers) else {
        return;
    };

    for (raw_name, raw_value) in header_overrides {
        let header_name = raw_name.trim().to_ascii_lowercase();
        if header_name.is_empty() {
            log::warn!("[LocalProxyOverrides] Ignoring header override with empty name");
            continue;
        }

        let Ok(name) = http::HeaderName::from_bytes(header_name.as_bytes()) else {
            log::warn!("[LocalProxyOverrides] Ignoring invalid header override name: {raw_name}");
            continue;
        };

        if is_protected_local_proxy_override_header(&name) {
            log::debug!(
                "[LocalProxyOverrides] Ignoring protected header override: {}",
                name.as_str()
            );
            continue;
        }

        let Ok(value) = http::HeaderValue::from_str(raw_value) else {
            log::warn!(
                "[LocalProxyOverrides] Ignoring invalid header override value for {}",
                name.as_str()
            );
            continue;
        };

        headers.insert(name, value);
    }
}

fn is_protected_local_proxy_override_header(name: &http::HeaderName) -> bool {
    if is_codex_role_control_header(name.as_str()) {
        return true;
    }
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "upgrade"
            | "accept-encoding"
            | "content-type"
            | "authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "chatgpt-account-id"
            | "session_id"
            | "x-client-request-id"
            | "x-codex-window-id"
            | "x-forwarded-host"
            | "x-forwarded-port"
            | "x-forwarded-proto"
            | "forwarded"
            | "cf-connecting-ip"
            | "cf-ipcountry"
            | "cf-ray"
            | "cf-visitor"
            | "true-client-ip"
            | "fastly-client-ip"
            | "x-azure-clientip"
            | "x-azure-fdid"
            | "x-azure-ref"
            | "akamai-origin-hop"
            | "x-akamai-config-log-detail"
            | "x-request-id"
            | "x-correlation-id"
            | "x-trace-id"
            | "x-amzn-trace-id"
            | "x-b3-traceid"
            | "x-b3-spanid"
            | "x-b3-parentspanid"
            | "x-b3-sampled"
            | "traceparent"
            | "tracestate"
    )
}

fn is_codex_role_control_header(name: &str) -> bool {
    use crate::services::codex_agent_roles::{
        ROLE_OWNER_HEADER, ROLE_ROUTE_HEADER, ROLE_TOKEN_HEADER,
    };

    name.eq_ignore_ascii_case(ROLE_ROUTE_HEADER)
        || name.eq_ignore_ascii_case(ROLE_OWNER_HEADER)
        || name.eq_ignore_ascii_case(ROLE_TOKEN_HEADER)
}

fn prepare_upstream_request_body(request_body: Value) -> Value {
    canonicalize_value(filter_private_params_with_whitelist(request_body, &[]))
}

fn log_prompt_cache_trace(
    app_type: &AppType,
    provider: &Provider,
    endpoint: &str,
    api_format: Option<&str>,
    body: &Value,
    session_client_provided: bool,
) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }

    let prompt_cache_key = body
        .get("prompt_cache_key")
        .and_then(|value| value.as_str())
        .map(|key| format!("present(len={})", key.len()))
        .unwrap_or_else(|| "absent".to_string());
    let store = body
        .get("store")
        .map(value_for_log)
        .unwrap_or_else(|| "absent".to_string());
    let stream = body
        .get("stream")
        .map(value_for_log)
        .unwrap_or_else(|| "absent".to_string());
    let cache_controls = cache_control_summary(body);

    log::debug!(
        "[CacheTrace] app={}, provider={}, endpoint={}, api_format={}, session_client_provided={}, prompt_cache_key={}, store={}, stream={}, instructions_hash={}, system_hash={}, tools_hash={}, input_hash={}, messages_hash={}, include_hash={}, cache_controls={}, body_hash={}",
        app_type.as_str(),
        provider.id,
        // Gemini 的 endpoint 带 ?key=<API_KEY>；脱敏剥掉 query 再落盘。
        crate::redact_url_for_log(endpoint),
        api_format.unwrap_or("native"),
        session_client_provided,
        prompt_cache_key,
        store,
        stream,
        short_value_hash(body.get("instructions")),
        short_value_hash(body.get("system")),
        short_value_hash(body.get("tools")),
        short_value_hash(body.get("input")),
        short_value_hash(body.get("messages")),
        short_value_hash(body.get("include")),
        cache_controls,
        short_value_hash(Some(body)),
    );
}

fn cache_control_summary(value: &Value) -> String {
    fn walk(value: &Value, count: &mut usize, ttls: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                if let Some(cache_control) = object.get("cache_control") {
                    *count += 1;
                    let ttl = cache_control
                        .get("ttl")
                        .and_then(Value::as_str)
                        .unwrap_or("default");
                    ttls.insert(ttl.to_string());
                }
                for child in object.values() {
                    walk(child, count, ttls);
                }
            }
            Value::Array(items) => {
                for child in items {
                    walk(child, count, ttls);
                }
            }
            _ => {}
        }
    }

    let mut count = 0;
    let mut ttls = std::collections::BTreeSet::new();
    walk(value, &mut count, &mut ttls);
    format!(
        "count={count},ttls={}",
        if ttls.is_empty() {
            "none".to_string()
        } else {
            ttls.into_iter().collect::<Vec<_>>().join("|")
        }
    )
}

fn value_for_log(value: &Value) -> String {
    match value {
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Null => "null".to_string(),
        Value::Array(values) => format!("array(len={})", values.len()),
        Value::Object(values) => format!("object(len={})", values.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::{
        LocalProxyRequestOverrides, LocalProxyRetryErrorType, LocalProxyRetryPolicy, ProviderMeta,
        DEFAULT_LOCAL_PROXY_RETRY_MESSAGE,
    };
    use axum::http::header::{HeaderValue, ACCEPT};
    use axum::http::HeaderMap;
    use axum::{routing::post, Json, Router};
    use bytes::Bytes;
    use http::StatusCode;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;

    fn test_provider_with_type(provider_type: Option<&str>) -> Provider {
        Provider {
            id: "provider-1".to_string(),
            name: "Provider 1".to_string(),
            settings_config: json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: provider_type.map(|value| crate::provider::ProviderMeta {
                provider_type: Some(value.to_string()),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    fn test_forwarder(
        non_streaming_timeout: Duration,
        streaming_first_byte_timeout: Duration,
    ) -> RequestForwarder {
        let db = Arc::new(Database::memory().expect("memory db"));

        RequestForwarder {
            router: Arc::new(ProviderRouter::new(db.clone())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            current_providers: Arc::new(RwLock::new(HashMap::new())),
            gemini_shadow: Arc::new(GeminiShadowStore::new()),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
            failover_manager: Arc::new(FailoverSwitchManager::new(db)),
            app_handle: None,
            current_provider_id_at_start: String::new(),
            session_id: String::new(),
            session_client_provided: false,
            rectifier_config: RectifierConfig::default(),
            optimizer_config: OptimizerConfig::default(),
            copilot_optimizer_config: CopilotOptimizerConfig::default(),
            non_streaming_timeout,
            streaming_first_byte_timeout,
            max_attempts: 1,
            provider_retry_enabled: true,
            sync_logical_target: true,
            bypass_single_provider_circuit_breaker: true,
            outbound_model_overrides: HashMap::new(),
            role_route_owner_id: None,
            role_capability_model: None,
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

    fn retry_provider(
        id: &str,
        base_url: String,
        max_retries: u32,
        custom_messages: Vec<String>,
        error_types: Vec<LocalProxyRetryErrorType>,
    ) -> Provider {
        Provider {
            id: id.to_string(),
            name: id.to_string(),
            settings_config: json!({
                "base_url": base_url,
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                local_proxy_retry_policy: Some(LocalProxyRetryPolicy {
                    enabled: None,
                    max_retries,
                    retry_delay_ms: 1,
                    custom_messages,
                    error_types,
                }),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    async fn spawn_counting_responses_server(
        status: StatusCode,
        body: Value,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let body = body.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind counting responses server");
        let address = listener
            .local_addr()
            .expect("counting responses server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve counting responses requests");
        });
        tokio::task::yield_now().await;

        (format!("http://{address}"), attempts, server)
    }

    async fn spawn_header_recording_responses_server() -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<HeaderMap>,
        tokio::task::JoinHandle<()>,
    ) {
        let (headers_tx, headers_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap, Json(_body): Json<Value>| {
                let headers_tx = headers_tx.clone();
                async move {
                    let _ = headers_tx.send(headers);
                    Json(json!({
                        "id": "resp-header-test",
                        "status": "completed",
                        "output": []
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind header recording responses server");
        let address = listener
            .local_addr()
            .expect("header recording responses server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve header recording responses requests");
        });
        tokio::task::yield_now().await;

        (format!("http://{address}"), headers_rx, server)
    }

    async fn spawn_recording_server(
        path: &'static str,
        responses: Vec<(StatusCode, Value)>,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<(HeaderMap, Value)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let responses = Arc::new(responses);
        let app = Router::new().route(
            path,
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = Arc::clone(&captured_for_handler);
                let responses = Arc::clone(&responses);
                async move {
                    let attempt = {
                        let mut captured = captured.lock().await;
                        let attempt = captured.len();
                        captured.push((headers, body));
                        attempt
                    };
                    let (status, response_body) = responses
                        .get(attempt)
                        .or_else(|| responses.last())
                        .cloned()
                        .expect("recording server response fixture");
                    (status, Json(response_body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recording server");
        let address = listener.local_addr().expect("recording server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve recording requests");
        });
        tokio::task::yield_now().await;

        (format!("http://{address}"), captured, server)
    }

    async fn spawn_sequence_responses_server(
        responses: Vec<(StatusCode, Value)>,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_sequence_json_server("/v1/responses", responses).await
    }

    async fn spawn_sequence_json_server(
        path: &'static str,
        responses: Vec<(StatusCode, Value)>,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        assert!(!responses.is_empty(), "response sequence must not be empty");
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            path,
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let responses = responses.clone();
                async move {
                    let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let index = attempt.min(responses.len() - 1);
                    let (status, body) = responses[index].clone();
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind sequenced responses server");
        let address = listener
            .local_addr()
            .expect("sequenced responses server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve sequenced responses requests");
        });
        tokio::task::yield_now().await;

        (format!("http://{address}"), attempts, server)
    }

    async fn spawn_two_attempt_sse_server(
        path: &'static str,
        first_chunks: Vec<Bytes>,
        success_body: String,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            path,
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let first_chunks = first_chunks.clone();
                let success_body = success_body.clone();
                async move {
                    let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let chunks = if attempt == 0 {
                        first_chunks
                    } else {
                        vec![Bytes::from(success_body)]
                    };
                    let stream = futures::stream::iter(
                        chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
                    );
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from_stream(stream))
                        .expect("build SSE response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind two-attempt SSE server");
        let address = listener
            .local_addr()
            .expect("two-attempt SSE server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve two-attempt SSE requests");
        });
        tokio::task::yield_now().await;

        (format!("http://{address}"), attempts, server)
    }

    #[derive(Clone)]
    enum MixedTestResponse {
        Json(StatusCode, Value),
        Sse(String),
    }

    async fn spawn_mixed_recording_server(
        path: &'static str,
        responses: Vec<MixedTestResponse>,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        assert!(!responses.is_empty(), "response sequence must not be empty");
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let responses = Arc::new(responses);
        let app = Router::new().route(
            path,
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&captured_for_handler);
                let responses = Arc::clone(&responses);
                async move {
                    let attempt = {
                        let mut captured = captured.lock().await;
                        let attempt = captured.len();
                        captured.push(body);
                        attempt
                    };
                    match responses
                        .get(attempt)
                        .or_else(|| responses.last())
                        .cloned()
                        .expect("mixed response fixture")
                    {
                        MixedTestResponse::Json(status, body) => http::Response::builder()
                            .status(status)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(axum::body::Body::from(body.to_string()))
                            .expect("build JSON response"),
                        MixedTestResponse::Sse(body) => http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "text/event-stream")
                            .body(axum::body::Body::from(body))
                            .expect("build SSE response"),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mixed recording server");
        let address = listener
            .local_addr()
            .expect("mixed recording server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve mixed recording requests");
        });
        tokio::task::yield_now().await;

        (format!("http://{address}"), captured, server)
    }

    async fn run_matching_failure_after_sse_prefix(prefix: &str) -> (Bytes, usize) {
        let failed = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        );
        let first_body = format!("{prefix}{failed}");
        let (base_url, attempts, server) = spawn_two_attempt_sse_server(
            "/v1/responses",
            vec![Bytes::from(first_body)],
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"productive-output-retry-success\",\"status\":\"completed\",\"output\":[]}}\n\n".to_string(),
        )
        .await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "productive-output-provider",
            base_url,
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|error| panic!("SSE retry case should complete: {}", error.error));
        let body = result.response.bytes().await.expect("SSE retry case body");
        abort_and_join_test_task(server).await;

        (body, attempts.load(std::sync::atomic::Ordering::SeqCst))
    }

    async fn run_custom_message_error_body_case(
        error_body: Vec<u8>,
        custom_message: &str,
    ) -> (bool, usize) {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let error_body = Bytes::from(error_body);
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let error_body = error_body.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        http::Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                            .body(axum::body::Body::from(error_body))
                            .expect("build custom-message boundary error response")
                    } else {
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(axum::body::Body::from(
                                r#"{"id":"custom-message-retry-success","status":"completed","output":[]}"#,
                            ))
                            .expect("build custom-message boundary success response")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind custom-message boundary upstream");
        let address = listener
            .local_addr()
            .expect("custom-message boundary upstream address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve custom-message boundary requests");
        });
        tokio::task::yield_now().await;

        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "custom-message-boundary-provider",
            format!("http://{address}"),
            1,
            vec![custom_message.to_string()],
            vec![],
        );
        let succeeded = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .is_ok();
        abort_and_join_test_task(server).await;

        (
            succeeded,
            attempts.load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    #[test]
    fn single_provider_retryable_log_uses_single_provider_code() {
        let error = ProxyError::UpstreamError {
            status: 429,
            body: Some(r#"{"error":{"message":"rate limit exceeded"}}"#.to_string()),
        };

        let (code, message) = build_retryable_failure_log("PackyCode-response", 1, 1, &error);

        assert_eq!(code, log_fwd::SINGLE_PROVIDER_FAILED);
        assert!(message.contains("Provider PackyCode-response 请求失败"));
        assert!(message.contains("上游 HTTP 429"));
        // 上游错误消息保留(截断)，用于诊断失败原因。
        assert!(message.contains("rate limit exceeded"));
        assert!(!message.contains("切换下一个"));
    }

    #[test]
    fn multi_provider_retryable_log_keeps_failover_wording() {
        let error = ProxyError::Timeout("upstream timed out after 30s".to_string());

        let (code, message) = build_retryable_failure_log("primary", 1, 3, &error);

        assert_eq!(code, log_fwd::PROVIDER_FAILED_RETRY);
        assert!(message.contains("继续尝试下一个 (1/3)"));
        assert!(message.contains("请求超时"));
    }

    #[test]
    fn single_provider_has_no_terminal_all_failed_log() {
        assert!(build_terminal_failure_log(1, 1, None).is_none());
    }

    #[test]
    fn multi_provider_terminal_log_contains_last_error_summary() {
        let error = ProxyError::ForwardFailed("connection reset by peer".to_string());

        let (code, message) =
            build_terminal_failure_log(2, 2, Some(&error)).expect("expected terminal log");

        assert_eq!(code, log_fwd::ALL_PROVIDERS_FAILED);
        assert!(message.contains("已尝试 2/2 个 Provider，均失败"));
        assert!(message.contains("connection reset by peer"));
    }

    #[test]
    fn summarize_text_for_log_collapses_whitespace_and_truncates() {
        let summary = summarize_text_for_log("line1\n\n line2   line3", 12);

        assert_eq!(summary, "line1 line2...");
    }

    #[test]
    fn canonical_json_sorts_object_keys_for_cache_trace_hashes() {
        let left = json!({
            "tools": [
                {
                    "parameters": {
                        "properties": {
                            "b": {"type": "string"},
                            "a": {"type": "number"}
                        },
                        "type": "object"
                    },
                    "name": "lookup"
                }
            ]
        });
        let right = json!({
            "tools": [
                {
                    "name": "lookup",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "a": {"type": "number"},
                            "b": {"type": "string"}
                        }
                    }
                }
            ]
        });

        assert_eq!(
            crate::proxy::json_canonical::canonical_json_string(&left),
            crate::proxy::json_canonical::canonical_json_string(&right)
        );
        assert_eq!(
            short_value_hash(Some(&left)),
            short_value_hash(Some(&right))
        );
    }

    #[test]
    fn prepare_upstream_request_body_filters_private_fields_and_canonicalizes_order() {
        let body = json!({
            "z": 1,
            "_internal": "drop",
            "tools": [
                {
                    "name": "lookup",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "_id": {
                                "_private_note": "drop",
                                "type": "string"
                            },
                            "b": {"type": "number"},
                            "a": {"type": "string"}
                        }
                    }
                }
            ],
            "a": 2
        });

        let prepared = prepare_upstream_request_body(body);

        assert!(prepared.get("_internal").is_none());
        assert!(prepared["tools"][0]["parameters"]["properties"]
            .get("_id")
            .is_some());
        assert!(prepared["tools"][0]["parameters"]["properties"]["_id"]
            .get("_private_note")
            .is_none());
        assert_eq!(
            serde_json::to_string(&prepared).unwrap(),
            r#"{"a":2,"tools":[{"name":"lookup","parameters":{"properties":{"_id":{"type":"string"},"a":{"type":"string"},"b":{"type":"number"}},"type":"object"}}],"z":1}"#
        );
    }

    #[test]
    fn local_proxy_body_overrides_deep_merge_final_body_without_stream() {
        let mut body = json!({
            "model": "before",
            "stream": false,
            "metadata": {
                "keep": true,
                "temperature": 1
            },
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let overrides = LocalProxyRequestOverrides {
            headers: HashMap::new(),
            body: Some(json!({
                "model": "after",
                "stream": true,
                "metadata": {
                    "temperature": 0.2,
                    "top_p": 0.9
                },
                "messages": []
            })),
        };

        assert!(apply_local_proxy_body_overrides(&mut body, &overrides));

        assert_eq!(body["model"], "after");
        assert_eq!(body["stream"], false);
        assert_eq!(body["metadata"]["keep"], true);
        assert_eq!(body["metadata"]["temperature"], 0.2);
        assert_eq!(body["metadata"]["top_p"], 0.9);
        assert_eq!(body["messages"], json!([]));
    }

    #[test]
    fn local_proxy_header_overrides_replace_allowed_headers_only() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("original"),
        );
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer good"),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        let overrides = LocalProxyRequestOverrides {
            headers: HashMap::from([
                ("User-Agent".to_string(), "custom".to_string()),
                ("X-Test".to_string(), "ok".to_string()),
                ("Authorization".to_string(), "Bearer bad".to_string()),
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("X-Bad".to_string(), "bad\nvalue".to_string()),
                ("X-CC-Switch-Role-Route".to_string(), "frontend".to_string()),
                (
                    "X-CC-Switch-Role-Owner".to_string(),
                    "forged-owner".to_string(),
                ),
                (
                    "X-CC-Switch-Role-Token".to_string(),
                    "forged-token".to_string(),
                ),
            ]),
            body: None,
        };

        apply_local_proxy_header_overrides(&mut headers, Some(&overrides), false);

        assert_eq!(
            headers
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("custom")
        );
        assert_eq!(
            headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer good")
        );
        assert_eq!(
            headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            headers.get("x-test").and_then(|value| value.to_str().ok()),
            Some("ok")
        );
        assert!(headers.get("x-bad").is_none());
        assert!(headers.get("x-cc-switch-role-route").is_none());
        assert!(headers.get("x-cc-switch-role-owner").is_none());
        assert!(headers.get("x-cc-switch-role-token").is_none());
    }

    #[tokio::test]
    async fn inbound_codex_role_control_headers_never_reach_upstream() {
        let (base_url, mut observed_headers, server) =
            spawn_header_recording_responses_server().await;
        let provider = retry_provider("header-provider", base_url, 0, vec![], vec![]);
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
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

        let result = match forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "header test" }),
                headers,
                http::Extensions::new(),
                vec![provider],
            )
            .await
        {
            Ok(result) => result,
            Err(error) => panic!("forward role request: {}", error.error),
        };
        let _ = result.response.bytes().await.expect("read response body");
        let upstream_headers =
            tokio::time::timeout(Duration::from_secs(2), observed_headers.recv())
                .await
                .expect("record upstream headers")
                .expect("upstream request headers");
        abort_and_join_test_task(server).await;

        assert!(upstream_headers.get("x-cc-switch-role-route").is_none());
        assert!(upstream_headers.get("x-cc-switch-role-owner").is_none());
        assert!(upstream_headers.get("x-cc-switch-role-token").is_none());
    }

    fn frontend_route_plan(
        provider_b: Provider,
        provider_a: Provider,
    ) -> crate::proxy::provider_router::ProviderRoutePlan {
        crate::proxy::provider_router::ProviderRoutePlan {
            attempts: vec![
                crate::proxy::provider_router::ProviderRouteAttempt {
                    provider: provider_b,
                    outbound_model_override: Some("frontend-upstream".to_string()),
                },
                crate::proxy::provider_router::ProviderRouteAttempt {
                    provider: provider_a,
                    outbound_model_override: Some("owner-default".to_string()),
                },
            ],
            use_failover_timeouts: true,
            sync_logical_target: false,
            bypass_single_provider_circuit_breaker: false,
        }
    }

    #[test]
    fn codex_role_observability_context_tracks_owner_and_capability_model() {
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2))
            .with_role_context(Some("provider-a"), "capability-model");

        assert_eq!(forwarder.role_route_owner_id.as_deref(), Some("provider-a"));
        assert_eq!(
            forwarder.role_capability_model.as_deref(),
            Some("capability-model")
        );
    }

    #[tokio::test]
    async fn codex_role_model_override_survives_chat_and_anthropic_transforms() {
        let cases = [
            (
                "openai_chat",
                "/v1/chat/completions",
                json!({ "id": "chat", "choices": [] }),
            ),
            (
                "anthropic",
                "/v1/messages",
                json!({
                    "id": "msg",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "stop_reason": "end_turn",
                    "usage": { "input_tokens": 1, "output_tokens": 1 }
                }),
            ),
        ];

        for (api_format, upstream_path, response_body) in cases {
            let (base_url, captured, server) =
                spawn_recording_server(upstream_path, vec![(StatusCode::OK, response_body)]).await;
            let mut provider = retry_provider(
                &format!("provider-{api_format}"),
                base_url,
                0,
                vec![],
                vec![],
            );
            let meta = provider.meta.as_mut().expect("provider meta");
            meta.api_format = Some(api_format.to_string());
            meta.local_proxy_request_overrides = Some(LocalProxyRequestOverrides {
                headers: HashMap::new(),
                body: Some(json!({ "model": "body-override-must-not-win" })),
            });
            let route_plan = crate::proxy::provider_router::ProviderRoutePlan {
                attempts: vec![crate::proxy::provider_router::ProviderRouteAttempt {
                    provider: provider.clone(),
                    outbound_model_override: Some("frontend-upstream".to_string()),
                }],
                use_failover_timeouts: true,
                sync_logical_target: false,
                bypass_single_provider_circuit_breaker: false,
            };
            let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2))
                .with_route_plan(&route_plan);
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
                HeaderValue::from_static("test-role-token"),
            );

            let result = forwarder
                .forward_with_retry(
                    &AppType::Codex,
                    http::Method::POST,
                    "/v1/responses",
                    json!({ "model": "capability-model", "input": "hello", "stream": false }),
                    headers,
                    Extensions::new(),
                    route_plan.providers(),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("{api_format} role request failed: {}", error.error)
                });
            assert_eq!(result.outbound_model.as_deref(), Some("frontend-upstream"));

            let captured = captured.lock().await;
            assert_eq!(captured.len(), 1, "api format {api_format}");
            assert_eq!(captured[0].1["model"], "frontend-upstream");
            assert!(captured[0].0.get("x-cc-switch-role-route").is_none());
            assert!(captured[0].0.get("x-cc-switch-role-owner").is_none());
            assert!(captured[0].0.get("x-cc-switch-role-token").is_none());
            drop(captured);
            abort_and_join_test_task(server).await;
        }
    }

    async fn assert_role_terminal_stream_health(
        chunks: Vec<Result<Bytes, std::io::Error>>,
        success: bool,
        read_succeeds: bool,
    ) {
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "role-stream-provider",
            "http://unused.example".to_string(),
            0,
            vec![],
            vec![],
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::iter(chunks),
        );
        let mut permit = None;
        let mut forwarder = forwarder;
        forwarder.sync_logical_target = false;
        let (response, deferred) = forwarder.defer_role_stream_result(
            response,
            &provider,
            AppType::Codex.as_str(),
            "frontend-upstream",
            false,
            &mut permit,
        );
        assert!(deferred);

        let read_result = response.bytes().await;
        assert_eq!(read_result.is_ok(), read_succeeds);

        let status = forwarder.status.read().await;
        assert_eq!(status.success_requests, u64::from(success));
        assert_eq!(status.failed_requests, u64::from(!success));
        drop(status);

        let stats = forwarder
            .router
            .get_circuit_breaker_stats(&provider.id, AppType::Codex.as_str())
            .await
            .expect("role stream breaker stats");
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.failed_requests, u32::from(!success));
    }

    #[test]
    fn codex_role_fallback_tracks_prior_attempts_instead_of_owner_identity() {
        assert!(!role_route_fallback(0));
        assert!(role_route_fallback(1));
    }

    #[tokio::test]
    async fn codex_role_stream_terminal_events_cover_completed_anthropic_and_root_errors() {
        let output = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        let cases = [
            (
                true,
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ),
            (
                false,
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
            ),
            (
                false,
                "data: {\"error\":{\"code\":\"server_error\",\"message\":\"failed after output\"}}\n\n",
            ),
        ];

        for (success, terminal) in cases {
            assert_role_terminal_stream_health(
                vec![Ok(Bytes::from(format!("{output}{terminal}")))],
                success,
                true,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn codex_role_terminal_event_records_health_before_transport_eof() {
        let cases = [
            (
                "terminal-success-provider",
                true,
                Bytes::from_static(
                    b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                ),
            ),
            (
                "terminal-error-provider",
                false,
                Bytes::from_static(
                    b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
                ),
            ),
        ];

        for (provider_id, success, terminal) in cases {
            let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
            forwarder.sync_logical_target = false;
            let provider = retry_provider(
                provider_id,
                "http://unused.example".to_string(),
                0,
                vec![],
                vec![],
            );
            let output = Bytes::from_static(
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            );
            let upstream = futures::stream::iter(vec![Ok(output), Ok(terminal)])
                .chain(futures::stream::pending::<Result<Bytes, std::io::Error>>());
            let response = ProxyResponse::streamed(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                )]),
                upstream,
            );
            let mut permit = None;
            let (response, deferred) = forwarder.defer_role_stream_result(
                response,
                &provider,
                AppType::Codex.as_str(),
                "frontend-upstream",
                false,
                &mut permit,
            );
            assert!(deferred);
            let mut stream = Box::pin(response.bytes_stream());
            assert!(stream.next().await.expect("output chunk").is_ok());
            assert!(stream.next().await.expect("terminal chunk").is_ok());

            let stats = forwarder
                .router
                .get_circuit_breaker_stats(&provider.id, AppType::Codex.as_str())
                .await
                .expect("terminal event breaker stats");
            assert_eq!(stats.total_requests, 1);
            assert_eq!(stats.failed_requests, u32::from(!success));
            let status = forwarder.status.read().await;
            assert_eq!(status.success_requests, u64::from(success));
            assert_eq!(status.failed_requests, u64::from(!success));
        }
    }

    #[tokio::test]
    async fn codex_role_stream_read_error_after_output_records_failure() {
        assert_role_terminal_stream_health(
            vec![
                Ok(Bytes::from_static(
                    b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                )),
                Err(std::io::Error::other("injected stream read failure")),
            ],
            false,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn codex_role_chain_runs_b_then_a_when_global_provider_retry_is_disabled() {
        let overloaded = json!({ "error": { "message": "temporarily unavailable" } });
        let success = json!({ "id": "role-success", "status": "completed", "output": [] });
        let (provider_b_url, provider_b_requests, provider_b_server) = spawn_recording_server(
            "/v1/responses",
            vec![(StatusCode::SERVICE_UNAVAILABLE, overloaded)],
        )
        .await;
        let (provider_a_url, provider_a_requests, provider_a_server) =
            spawn_recording_server("/v1/responses", vec![(StatusCode::OK, success)]).await;
        let provider_b = retry_provider(
            "provider-b",
            provider_b_url,
            3,
            vec![],
            vec![LocalProxyRetryErrorType::ServerError],
        );
        let provider_a = retry_provider(
            "provider-a",
            provider_a_url,
            3,
            vec![],
            vec![LocalProxyRetryErrorType::ServerError],
        );
        let route_plan = frontend_route_plan(provider_b, provider_a);
        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.provider_retry_enabled = false;
        let forwarder = forwarder.with_route_plan(&route_plan);

        let result = match forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "capability-model", "input": "continue" }),
                HeaderMap::new(),
                http::Extensions::new(),
                route_plan.providers(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => panic!("B to A role chain failed: {}", error.error),
        };
        let _ = result.response.bytes().await.expect("read role response");

        assert_eq!(result.provider.id, "provider-a");
        assert_eq!(provider_b_requests.lock().await.len(), 1);
        assert_eq!(provider_a_requests.lock().await.len(), 1);
        assert_eq!(
            provider_b_requests.lock().await[0].1["model"],
            "frontend-upstream"
        );
        assert_eq!(
            provider_a_requests.lock().await[0].1["model"],
            "owner-default"
        );
        abort_and_join_test_task(provider_b_server).await;
        abort_and_join_test_task(provider_a_server).await;
    }

    #[tokio::test]
    async fn codex_role_chain_does_not_fall_back_to_a_for_b_client_error() {
        let invalid = json!({ "error": { "message": "invalid request" } });
        let success = json!({ "id": "must-not-run", "status": "completed", "output": [] });
        let (provider_b_url, provider_b_requests, provider_b_server) =
            spawn_recording_server("/v1/responses", vec![(StatusCode::BAD_REQUEST, invalid)]).await;
        let (provider_a_url, provider_a_requests, provider_a_server) =
            spawn_recording_server("/v1/responses", vec![(StatusCode::OK, success)]).await;
        let provider_b = retry_provider("provider-b", provider_b_url, 0, vec![], vec![]);
        let provider_a = retry_provider("provider-a", provider_a_url, 0, vec![], vec![]);
        let route_plan = frontend_route_plan(provider_b, provider_a);
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2))
            .with_route_plan(&route_plan);

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "capability-model", "input": "continue" }),
                HeaderMap::new(),
                http::Extensions::new(),
                route_plan.providers(),
            )
            .await;
        assert!(matches!(
            result,
            Err(ForwardError {
                error: ProxyError::UpstreamError { status: 400, .. },
                ..
            })
        ));
        assert_eq!(provider_b_requests.lock().await.len(), 1);
        assert_eq!(provider_a_requests.lock().await.len(), 0);
        abort_and_join_test_task(provider_b_server).await;
        abort_and_join_test_task(provider_a_server).await;
    }

    #[tokio::test]
    async fn codex_role_chain_uses_independent_retry_budgets_without_logical_switch() {
        let overloaded = json!({ "error": { "message": "temporarily unavailable" } });
        let success = json!({ "id": "role-success", "status": "completed", "output": [] });
        let (provider_b_url, provider_b_requests, provider_b_server) = spawn_recording_server(
            "/v1/responses",
            vec![
                (StatusCode::SERVICE_UNAVAILABLE, overloaded.clone()),
                (StatusCode::SERVICE_UNAVAILABLE, overloaded.clone()),
            ],
        )
        .await;
        let (provider_a_url, provider_a_requests, provider_a_server) = spawn_recording_server(
            "/v1/responses",
            vec![
                (StatusCode::SERVICE_UNAVAILABLE, overloaded),
                (StatusCode::OK, success),
            ],
        )
        .await;
        let provider_b = retry_provider(
            "provider-b",
            provider_b_url,
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Overloaded],
        );
        let provider_a = retry_provider(
            "provider-a",
            provider_a_url,
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Overloaded],
        );
        let route_plan = frontend_route_plan(provider_b, provider_a);
        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.current_provider_id_at_start = "provider-a".to_string();
        forwarder.current_providers.write().await.insert(
            "codex".to_string(),
            ("provider-a".to_string(), "provider-a".to_string()),
        );
        {
            let mut status = forwarder.status.write().await;
            status.current_provider = Some("provider-a".to_string());
            status.current_provider_id = Some("provider-a".to_string());
            status.failover_count = 7;
        }
        let forwarder = forwarder.with_route_plan(&route_plan);

        let result = match forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "capability-model", "input": "continue" }),
                HeaderMap::new(),
                http::Extensions::new(),
                route_plan.providers(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => panic!("independent role retry budgets failed: {}", error.error),
        };
        let _ = result.response.bytes().await.expect("read role response");

        assert_eq!(result.provider.id, "provider-a");
        assert_eq!(provider_b_requests.lock().await.len(), 2);
        assert_eq!(provider_a_requests.lock().await.len(), 2);
        assert_eq!(
            forwarder.current_providers.read().await.get("codex"),
            Some(&("provider-a".to_string(), "provider-a".to_string()))
        );
        let status = forwarder.status.read().await;
        assert_eq!(status.current_provider.as_deref(), Some("provider-a"));
        assert_eq!(status.current_provider_id.as_deref(), Some("provider-a"));
        assert_eq!(status.failover_count, 7);
        drop(status);
        abort_and_join_test_task(provider_b_server).await;
        abort_and_join_test_task(provider_a_server).await;
    }

    #[tokio::test]
    async fn codex_role_rectifier_successes_preserve_logical_target() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cases = [
            (
                "media",
                "This model does not support image input",
                json!({
                    "model": "capability-model",
                    "max_tokens": 128,
                    "messages": [{
                        "role": "user",
                        "content": [{
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "abc"
                            }
                        }]
                    }]
                }),
            ),
            (
                "thinking-signature",
                "Invalid `signature` in `thinking` block",
                json!({
                    "model": "capability-model",
                    "max_tokens": 128,
                    "messages": [
                        {
                            "role": "assistant",
                            "content": [{
                                "type": "thinking",
                                "thinking": "analysis",
                                "signature": "invalid"
                            }]
                        },
                        { "role": "user", "content": "continue" }
                    ]
                }),
            ),
            (
                "thinking-budget",
                "thinking.budget_tokens: Input should be greater than or equal to 1024",
                json!({
                    "model": "capability-model",
                    "max_tokens": 128,
                    "thinking": { "type": "enabled", "budget_tokens": 10 },
                    "messages": [{ "role": "user", "content": "continue" }]
                }),
            ),
        ];

        for (label, error_message, body) in cases {
            let success = json!({
                "id": format!("{label}-success"),
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "claude-test",
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            });
            let (provider_b_url, provider_b_requests, provider_b_server) = spawn_recording_server(
                "/v1/messages",
                vec![
                    (
                        StatusCode::BAD_REQUEST,
                        json!({ "error": { "message": error_message } }),
                    ),
                    (StatusCode::OK, success),
                ],
            )
            .await;
            let mut provider_b =
                retry_provider("provider-b", provider_b_url.clone(), 0, vec![], vec![]);
            provider_b.settings_config = json!({
                "env": {
                    "ANTHROPIC_BASE_URL": provider_b_url,
                    "ANTHROPIC_API_KEY": "test-key"
                }
            });
            let provider_a = retry_provider(
                "provider-a",
                "http://127.0.0.1:9".to_string(),
                0,
                vec![],
                vec![],
            );
            let route_plan = frontend_route_plan(provider_b, provider_a);
            let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
            forwarder.current_provider_id_at_start = "provider-a".to_string();
            forwarder.current_providers.write().await.insert(
                "claude".to_string(),
                ("provider-a".to_string(), "provider-a".to_string()),
            );
            {
                let mut status = forwarder.status.write().await;
                status.current_provider = Some("provider-a".to_string());
                status.current_provider_id = Some("provider-a".to_string());
                status.failover_count = 7;
            }
            let forwarder = forwarder.with_route_plan(&route_plan);

            let result = match forwarder
                .forward_with_retry(
                    &AppType::Claude,
                    http::Method::POST,
                    "/v1/messages",
                    body,
                    HeaderMap::new(),
                    http::Extensions::new(),
                    route_plan.providers(),
                )
                .await
            {
                Ok(result) => result,
                Err(error) => panic!("{label} rectifier role request failed: {}", error.error),
            };
            let _ = result
                .response
                .bytes()
                .await
                .expect("read rectifier response");
            assert_eq!(result.provider.id, "provider-b", "{label}");
            assert_eq!(provider_b_requests.lock().await.len(), 2, "{label}");
            assert_eq!(
                forwarder.current_providers.read().await.get("claude"),
                Some(&("provider-a".to_string(), "provider-a".to_string())),
                "{label}"
            );
            let status = forwarder.status.read().await;
            assert_eq!(
                status.current_provider.as_deref(),
                Some("provider-a"),
                "{label}"
            );
            assert_eq!(
                status.current_provider_id.as_deref(),
                Some("provider-a"),
                "{label}"
            );
            assert_eq!(status.failover_count, 7, "{label}");
            drop(status);
            abort_and_join_test_task(provider_b_server).await;
        }
    }

    #[tokio::test]
    async fn codex_role_stream_failure_after_output_is_not_replayed_and_records_failure() {
        let output = Bytes::from_static(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        );
        let failed = Bytes::from(format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        ));
        let (provider_b_url, provider_b_attempts, provider_b_server) =
            spawn_two_attempt_sse_server(
                "/v1/responses",
                vec![output, failed],
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n".to_string(),
            )
            .await;
        let success = json!({ "id": "must-not-run", "status": "completed", "output": [] });
        let (provider_a_url, provider_a_requests, provider_a_server) =
            spawn_recording_server("/v1/responses", vec![(StatusCode::OK, success)]).await;
        let provider_b = retry_provider(
            "provider-b",
            provider_b_url,
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let provider_a = retry_provider("provider-a", provider_a_url, 0, vec![], vec![]);
        let route_plan = frontend_route_plan(provider_b, provider_a);
        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.current_provider_id_at_start = "provider-a".to_string();
        forwarder.current_providers.write().await.insert(
            "codex".to_string(),
            ("provider-a".to_string(), "provider-a".to_string()),
        );
        let forwarder = forwarder.with_route_plan(&route_plan);

        let result = match forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "capability-model", "input": "continue", "stream": true }),
                HeaderMap::new(),
                http::Extensions::new(),
                route_plan.providers(),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => panic!("role stream should commit after output: {}", error.error),
        };
        assert_eq!(result.provider.id, "provider-b");
        let body = result.response.bytes().await.expect("read role stream");
        assert!(String::from_utf8_lossy(&body).contains("response.failed"));
        assert_eq!(
            provider_b_attempts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(provider_a_requests.lock().await.len(), 0);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let stats = forwarder
                    .router
                    .get_circuit_breaker_stats("provider-b", AppType::Codex.as_str())
                    .await
                    .expect("provider B breaker stats");
                if stats.total_requests > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("wait for stream terminal health");
        let stats = forwarder
            .router
            .get_circuit_breaker_stats("provider-b", AppType::Codex.as_str())
            .await
            .expect("provider B breaker stats");
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.failed_requests, 1);
        let status = forwarder.status.read().await;
        assert_eq!(status.success_requests, 0);
        assert_eq!(status.failed_requests, 1);
        drop(status);
        assert_eq!(
            forwarder.current_providers.read().await.get("codex"),
            Some(&("provider-a".to_string(), "provider-a".to_string()))
        );
        abort_and_join_test_task(provider_b_server).await;
        abort_and_join_test_task(provider_a_server).await;
    }

    #[tokio::test]
    async fn codex_role_media_retry_defers_sse_terminal_failure_health() {
        let output = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        let failed = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": { "type": "server_error", "message": "failed after output" }
                }
            })
        );
        let (provider_b_url, provider_b_requests, provider_b_server) =
            spawn_mixed_recording_server(
                "/v1/responses",
                vec![
                    MixedTestResponse::Json(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": { "message": "This model does not support image input" } }),
                    ),
                    MixedTestResponse::Sse(format!("{output}{failed}")),
                ],
            )
            .await;
        let (provider_a_url, provider_a_requests, provider_a_server) = spawn_recording_server(
            "/v1/responses",
            vec![(
                StatusCode::OK,
                json!({ "id": "must-not-run", "status": "completed", "output": [] }),
            )],
        )
        .await;
        let provider_b = retry_provider("provider-b", provider_b_url, 0, vec![], vec![]);
        let provider_a = retry_provider("provider-a", provider_a_url, 0, vec![], vec![]);
        let route_plan = frontend_route_plan(provider_b, provider_a);
        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.current_provider_id_at_start = "provider-a".to_string();
        forwarder.current_providers.write().await.insert(
            "codex".to_string(),
            ("provider-a".to_string(), "provider-a".to_string()),
        );
        {
            let mut status = forwarder.status.write().await;
            status.current_provider = Some("provider-a".to_string());
            status.current_provider_id = Some("provider-a".to_string());
            status.failover_count = 7;
        }
        let forwarder = forwarder
            .with_route_plan(&route_plan)
            .with_role_context(Some("provider-a"), "capability-model");
        let mut body = body_with_codex_input_image("capability-model");
        body["stream"] = Value::Bool(true);

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                body,
                HeaderMap::new(),
                http::Extensions::new(),
                route_plan.providers(),
            )
            .await
            .unwrap_or_else(|error| panic!("media role request failed: {}", error.error));
        assert_eq!(result.provider.id, "provider-b");
        let response = result
            .response
            .bytes()
            .await
            .expect("read media role stream");
        assert!(String::from_utf8_lossy(&response).contains("response.failed"));
        assert_eq!(provider_b_requests.lock().await.len(), 2);
        assert_eq!(provider_a_requests.lock().await.len(), 0);

        let stats = forwarder
            .router
            .get_circuit_breaker_stats("provider-b", AppType::Codex.as_str())
            .await
            .expect("provider B breaker stats");
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.failed_requests, 1);
        let status = forwarder.status.read().await;
        assert_eq!(status.success_requests, 0);
        assert_eq!(status.failed_requests, 1);
        assert_eq!(status.current_provider_id.as_deref(), Some("provider-a"));
        assert_eq!(status.failover_count, 7);
        drop(status);
        assert_eq!(
            forwarder.current_providers.read().await.get("codex"),
            Some(&("provider-a".to_string(), "provider-a".to_string()))
        );
        abort_and_join_test_task(provider_b_server).await;
        abort_and_join_test_task(provider_a_server).await;
    }

    #[tokio::test]
    async fn codex_role_media_retry_uses_remaining_provider_retry_budget() {
        let failed = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        );
        let completed = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";
        let (provider_b_url, provider_b_requests, provider_b_server) =
            spawn_mixed_recording_server(
                "/v1/responses",
                vec![
                    MixedTestResponse::Json(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": { "message": "This model does not support image input" } }),
                    ),
                    MixedTestResponse::Sse(failed),
                    MixedTestResponse::Sse(completed.to_string()),
                ],
            )
            .await;
        let (provider_a_url, provider_a_requests, provider_a_server) = spawn_recording_server(
            "/v1/responses",
            vec![(
                StatusCode::OK,
                json!({ "id": "must-not-run", "status": "completed", "output": [] }),
            )],
        )
        .await;
        let provider_b = retry_provider(
            "provider-b",
            provider_b_url,
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let provider_a = retry_provider("provider-a", provider_a_url, 0, vec![], vec![]);
        let route_plan = frontend_route_plan(provider_b, provider_a);
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2))
            .with_route_plan(&route_plan)
            .with_role_context(Some("provider-a"), "capability-model");
        let mut body = body_with_codex_input_image("capability-model");
        body["stream"] = Value::Bool(true);

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                body,
                HeaderMap::new(),
                http::Extensions::new(),
                route_plan.providers(),
            )
            .await
            .unwrap_or_else(|error| panic!("media retry request failed: {}", error.error));
        assert_eq!(result.provider.id, "provider-b");
        let response = result
            .response
            .bytes()
            .await
            .expect("read completed stream");
        assert!(String::from_utf8_lossy(&response).contains("response.completed"));
        assert_eq!(provider_b_requests.lock().await.len(), 3);
        assert_eq!(provider_a_requests.lock().await.len(), 0);

        let stats = forwarder
            .router
            .get_circuit_breaker_stats("provider-b", AppType::Codex.as_str())
            .await
            .expect("provider B breaker stats");
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.failed_requests, 0);
        let status = forwarder.status.read().await;
        assert_eq!(status.success_requests, 1);
        assert_eq!(status.failed_requests, 0);
        drop(status);
        abort_and_join_test_task(provider_b_server).await;
        abort_and_join_test_task(provider_a_server).await;
    }

    #[test]
    fn local_proxy_header_overrides_are_skipped_for_copilot() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("copilot"),
        );
        let overrides = LocalProxyRequestOverrides {
            headers: HashMap::from([("User-Agent".to_string(), "custom".to_string())]),
            body: None,
        };

        apply_local_proxy_header_overrides(&mut headers, Some(&overrides), true);

        assert_eq!(
            headers
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("copilot")
        );
    }

    #[tokio::test]
    async fn non_streaming_success_is_buffered_before_marking_provider_successful() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::new(),
            futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok::<Bytes, std::io::Error>(Bytes::from_static(b"{\"ok\":true}"))
            }),
        );

        let prepared = forwarder
            .prepare_success_response_for_failover(
                response,
                false,
                forwarder.pre_output_deadline(false),
            )
            .await
            .expect("response should be buffered");

        assert_eq!(
            prepared.bytes().await.unwrap(),
            Bytes::from_static(b"{\"ok\":true}")
        );
    }

    #[tokio::test]
    async fn error_body_exact_limit_is_not_truncated() {
        let body = Bytes::from_static(b"1234567890abcdef");
        let response = ProxyResponse::streamed(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            futures::stream::once(async move { Ok::<Bytes, std::io::Error>(body) }),
        );

        let collected = collect_body_prefix(response, 16)
            .await
            .expect("body prefix");
        assert_eq!(collected.bytes, Bytes::from_static(b"1234567890abcdef"));
        assert!(!collected.truncated);
    }

    #[test]
    fn truncated_error_body_preserves_valid_utf8_prefix_without_synthetic_marker() {
        let body = bounded_error_body_text(vec![b'a', 0xe4, 0xbd], true)
            .expect("trailing partial UTF-8 should preserve its valid prefix");
        assert_eq!(body, "a");
    }

    #[tokio::test]
    async fn non_streaming_body_read_error_is_retryable_before_success_record() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::new(),
            futures::stream::once(async {
                Err::<Bytes, std::io::Error>(std::io::Error::other("body boom"))
            }),
        );

        let err = match forwarder
            .prepare_success_response_for_failover(
                response,
                false,
                forwarder.pre_output_deadline(false),
            )
            .await
        {
            Ok(_) => panic!("body read errors should fail the attempt"),
            Err(err) => err,
        };

        assert!(matches!(err, ProxyError::ForwardFailed(_)));
    }

    #[tokio::test]
    async fn streaming_success_primes_first_chunk_and_replays_it() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::new(),
            futures::stream::iter(vec![
                Ok::<Bytes, std::io::Error>(Bytes::from_static(b"first")),
                Ok::<Bytes, std::io::Error>(Bytes::from_static(b"second")),
            ]),
        );

        let prepared = forwarder
            .prepare_success_response_for_failover(
                response,
                true,
                forwarder.pre_output_deadline(true),
            )
            .await
            .expect("stream should be primed");

        assert_eq!(
            prepared.bytes().await.unwrap(),
            Bytes::from_static(b"firstsecond")
        );
    }

    #[tokio::test]
    async fn streaming_first_chunk_error_is_retryable_before_success_record() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::new(),
            futures::stream::once(async {
                Err::<Bytes, std::io::Error>(std::io::Error::other("first chunk boom"))
            }),
        );

        let err = match forwarder
            .prepare_success_response_for_failover(
                response,
                true,
                forwarder.pre_output_deadline(true),
            )
            .await
        {
            Ok(_) => panic!("first chunk errors should fail the attempt"),
            Err(err) => err,
        };

        assert!(matches!(err, ProxyError::ForwardFailed(_)));
    }

    #[tokio::test]
    async fn provider_retry_times_out_pending_hyper_error_body_before_output() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt =
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (status, body) = if attempt == 0 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            axum::body::Body::from_stream(futures::stream::pending::<Result<
                                Bytes,
                                std::convert::Infallible,
                            >>()),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            axum::body::Body::from(
                                r#"{"id":"msg-retry-success","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#,
                            ),
                        )
                    };
                    http::Response::builder()
                        .status(status)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .expect("build Hyper pending error response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Hyper pending error upstream");
        let address = listener
            .local_addr()
            .expect("Hyper pending error upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Hyper pending error upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let mut provider = retry_provider(
            "pending-hyper-error-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Network],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let watched = tokio::time::timeout(
            Duration::from_secs(3),
            forwarder.forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = match watched.expect("pending Hyper error body must honor the body timeout") {
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
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("msg-retry-success"));
    }

    #[tokio::test]
    async fn provider_retry_preserves_503_overloaded_fact_when_error_body_times_out() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt =
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (status, body) = if attempt == 0 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            axum::body::Body::from_stream(futures::stream::pending::<Result<
                                Bytes,
                                std::convert::Infallible,
                            >>()),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            axum::body::Body::from(
                                r#"{"id":"msg-retry-success","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#,
                            ),
                        )
                    };
                    http::Response::builder()
                        .status(status)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .expect("build pending 503 response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pending 503 upstream");
        let address = listener.local_addr().expect("pending 503 upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve pending 503 upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let mut provider = retry_provider(
            "pending-503-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Overloaded],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let watched = tokio::time::timeout(
            Duration::from_secs(3),
            forwarder.forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = watched
            .expect("pending 503 body must honor the body timeout")
            .unwrap_or_else(|error| {
                panic!(
                    "503 status must remain available to the overloaded retry policy: {}",
                    error.error
                )
            });
        let response_body = result
            .response
            .bytes()
            .await
            .expect("successful overloaded retry body");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("msg-retry-success"));
    }

    #[tokio::test]
    async fn pending_400_error_body_keeps_status_and_does_not_fail_over() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let primary_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary_attempts_for_handler = primary_attempts.clone();
        let primary_app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = primary_attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    http::Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(axum::body::Body::from_stream(futures::stream::pending::<
                            Result<Bytes, std::convert::Infallible>,
                        >()))
                        .expect("build pending 400 response")
                }
            }),
        );
        let primary_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pending 400 upstream");
        let primary_address = primary_listener
            .local_addr()
            .expect("pending 400 upstream address");
        let primary_server = tokio::spawn(async move {
            axum::serve(primary_listener, primary_app)
                .await
                .expect("serve pending 400 upstream");
        });

        let secondary_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let secondary_attempts_for_handler = secondary_attempts.clone();
        let secondary_app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = secondary_attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(axum::body::Body::from(
                            r#"{"id":"unexpected-failover","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#,
                        ))
                        .expect("build secondary response")
                }
            }),
        );
        let secondary_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind secondary upstream");
        let secondary_address = secondary_listener
            .local_addr()
            .expect("secondary upstream address");
        let secondary_server = tokio::spawn(async move {
            axum::serve(secondary_listener, secondary_app)
                .await
                .expect("serve secondary upstream");
        });

        let mut primary_provider = retry_provider(
            "pending-400-primary",
            format!("http://{primary_address}"),
            1,
            vec![],
            vec![],
        );
        primary_provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{primary_address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let mut secondary_provider = retry_provider(
            "pending-400-secondary",
            format!("http://{secondary_address}"),
            1,
            vec![],
            vec![],
        );
        secondary_provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{secondary_address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });

        let mut forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        forwarder.max_attempts = 2;
        let watched = tokio::time::timeout(
            Duration::from_secs(3),
            forwarder.forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![primary_provider, secondary_provider],
            ),
        )
        .await;

        abort_and_join_test_task(primary_server).await;
        abort_and_join_test_task(secondary_server).await;

        let error = match watched.expect("pending 400 body must honor the body timeout") {
            Ok(_) => panic!("known HTTP 400 must remain non-retryable"),
            Err(error) => error,
        };
        assert_eq!(
            crate::proxy::error_mapper::map_proxy_error_to_status(&error.error),
            400
        );
        assert_eq!(
            primary_attempts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            secondary_attempts.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn non_success_error_body_stops_at_limit_without_waiting_for_eof() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let first = futures::stream::once(async {
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(vec![
                            b'x';
                            256 * 1024 + 1
                        ]))
                    });
                    let pending =
                        futures::stream::pending::<Result<Bytes, std::convert::Infallible>>();
                    http::Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                        .body(axum::body::Body::from_stream(first.chain(pending)))
                        .expect("build oversized pending error response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind oversized pending error upstream");
        let address = listener
            .local_addr()
            .expect("oversized pending error upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve oversized pending error upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "oversized-pending-error-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let watched = tokio::time::timeout(
            Duration::from_millis(400),
            forwarder.forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let error = match watched.expect("oversized error body must stop at the byte limit") {
            Ok(_) => panic!("HTTP 400 must remain an error"),
            Err(error) => error,
        };
        let body = match error.error {
            ProxyError::UpstreamError {
                status: 400,
                body: Some(body),
            } => body,
            other => panic!("expected bounded HTTP 400 error body, got {other}"),
        };
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!body.contains("upstream error body truncated at 262144 bytes"));
        assert!(body.len() <= MAX_UPSTREAM_ERROR_BODY_BYTES);
    }

    #[tokio::test]
    async fn compressed_error_body_is_limited_after_decompression() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &vec![b'x'; 512 * 1024])
            .expect("compress oversized error body");
        let compressed = Arc::new(encoder.finish().expect("finish oversized gzip body"));
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let compressed = compressed.clone();
                async move {
                    http::Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                        .header(http::header::CONTENT_ENCODING, "gzip")
                        .body(axum::body::Body::from(compressed.as_ref().clone()))
                        .expect("build compressed oversized error response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind compressed oversized error upstream");
        let address = listener
            .local_addr()
            .expect("compressed oversized error upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve compressed oversized error upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let mut provider = retry_provider(
            "compressed-oversized-error-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let error = match forwarder
            .forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
        {
            Ok(_) => panic!("HTTP 400 must remain an error"),
            Err(error) => error,
        };

        abort_and_join_test_task(upstream_server).await;

        let body = match error.error {
            ProxyError::UpstreamError {
                status: 400,
                body: Some(body),
            } => body,
            other => panic!("expected bounded compressed HTTP 400 body, got {other}"),
        };
        assert!(!body.contains("upstream error body truncated at 262144 bytes"));
        assert!(body.len() <= MAX_UPSTREAM_ERROR_BODY_BYTES);
    }

    #[tokio::test]
    async fn provider_retry_matches_custom_message_in_second_gzip_member() {
        const CUSTOM_MESSAGE: &str = "retry needle in second gzip member";

        fn gzip_member(body: &[u8]) -> Vec<u8> {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, body).expect("write gzip member");
            encoder.finish().expect("finish gzip member")
        }

        let mut compressed = gzip_member(b"first member without the retry condition; ");
        compressed.extend_from_slice(&gzip_member(CUSTOM_MESSAGE.as_bytes()));
        let compressed = Arc::new(compressed);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let compressed = compressed.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        http::Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                            .header(http::header::CONTENT_ENCODING, "gzip")
                            .body(axum::body::Body::from(compressed.as_ref().clone()))
                            .expect("build multi-member gzip error response")
                    } else {
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(axum::body::Body::from(
                                r#"{"id":"multi-gzip-retry-success","status":"completed","output":[]}"#,
                            ))
                            .expect("build retry success response")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind multi-member gzip upstream");
        let address = listener
            .local_addr()
            .expect("multi-member gzip upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve multi-member gzip upstream");
        });
        tokio::task::yield_now().await;

        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "multi-member-gzip-provider",
            format!("http://{address}"),
            1,
            vec![CUSTOM_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|error| {
                panic!("second gzip member should trigger retry: {}", error.error)
            });
        let body = result.response.bytes().await.expect("retry success body");
        abort_and_join_test_task(upstream_server).await;

        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&body).contains("multi-gzip-retry-success"));
    }

    #[tokio::test]
    async fn provider_retry_matches_custom_message_fully_inside_error_body_prefix() {
        const LIMIT: usize = 256 * 1024;
        const CUSTOM_MESSAGE: &str = "custom retry boundary marker";
        let mut body = vec![b'x'; LIMIT - CUSTOM_MESSAGE.len() - 8];
        body.extend_from_slice(CUSTOM_MESSAGE.as_bytes());
        body.resize(LIMIT + 1, b'y');

        let (succeeded, attempts) = run_custom_message_error_body_case(body, CUSTOM_MESSAGE).await;

        assert!(succeeded);
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn provider_retry_does_not_match_custom_message_crossing_error_body_limit() {
        const LIMIT: usize = 256 * 1024;
        const CUSTOM_MESSAGE: &str = "custom retry boundary marker";
        let prefix_fragment_len = CUSTOM_MESSAGE.len() / 2;
        let mut body = vec![b'x'; LIMIT - prefix_fragment_len];
        body.extend_from_slice(CUSTOM_MESSAGE.as_bytes());

        let (succeeded, attempts) = run_custom_message_error_body_case(body, CUSTOM_MESSAGE).await;

        assert!(!succeeded);
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn provider_retry_does_not_scan_custom_message_after_error_body_limit() {
        const LIMIT: usize = 256 * 1024;
        const CUSTOM_MESSAGE: &str = "custom retry boundary marker";
        let mut body = vec![b'x'; LIMIT];
        body.extend_from_slice(CUSTOM_MESSAGE.as_bytes());

        let (succeeded, attempts) = run_custom_message_error_body_case(body, CUSTOM_MESSAGE).await;

        assert!(!succeeded);
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn provider_retry_truncated_body_does_not_synthesize_custom_message() {
        let body = vec![b'x'; MAX_UPSTREAM_ERROR_BODY_BYTES + 1];

        let (succeeded, attempts) =
            run_custom_message_error_body_case(body, "upstream error body truncated").await;

        assert!(!succeeded);
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn provider_retry_matches_real_upstream_truncation_marker_literal() {
        let body = b"upstream payload\n[cc-switch: upstream error body truncated at 262144 bytes]"
            .to_vec();

        let (succeeded, attempts) =
            run_custom_message_error_body_case(body, "upstream error body truncated").await;

        assert!(succeeded);
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn provider_retry_uses_first_byte_timeout_for_hyper_streaming_headers() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from(
                            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .expect("build Hyper streaming response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Hyper streaming header upstream");
        let address = listener
            .local_addr()
            .expect("Hyper streaming header upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Hyper streaming header upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(5), Duration::from_secs(1));
        let mut provider = retry_provider(
            "hyper-streaming-header-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Network],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let watched = tokio::time::timeout(
            Duration::from_secs(3),
            forwarder.forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": true
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result =
            match watched.expect("Hyper streaming headers must honor the first-byte timeout") {
                Ok(result) => result,
                Err(error) => panic!(
                    "the header timeout should enter same-provider retry: {}",
                    error.error
                ),
            };
        let response_body = result
            .response
            .bytes()
            .await
            .expect("successful retry SSE body");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("message_stop"));
    }

    #[tokio::test]
    async fn hyper_non_streaming_header_and_body_share_total_deadline() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt =
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(axum::body::Body::from_stream(futures::stream::once(
                                async {
                                    tokio::time::sleep(Duration::from_millis(150)).await;
                                    Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(
                                        br#"{"id":"slow-first-attempt","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#,
                                    ))
                                },
                            )))
                            .expect("build delayed Hyper body response")
                    } else {
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(axum::body::Body::from(
                                r#"{"id":"deadline-retry-success","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#,
                            ))
                            .expect("build Hyper deadline retry response")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Hyper total deadline upstream");
        let address = listener
            .local_addr()
            .expect("Hyper total deadline upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Hyper total deadline upstream");
        });

        let forwarder = test_forwarder(Duration::from_millis(200), Duration::from_millis(200));
        let mut provider = retry_provider(
            "hyper-total-deadline-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Network],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://{address}"),
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let watched = tokio::time::timeout(
            Duration::from_millis(600),
            forwarder.forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = watched
            .expect("Hyper total deadline test must finish")
            .unwrap_or_else(|error| panic!("deadline retry should recover: {}", error.error));
        let response_body = result
            .response
            .bytes()
            .await
            .expect("Hyper deadline retry body");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("deadline-retry-success"));
    }

    #[tokio::test]
    async fn reqwest_streaming_header_and_first_chunk_share_total_deadline() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt =
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "text/event-stream")
                            .body(axum::body::Body::from_stream(futures::stream::once(
                                async {
                                    tokio::time::sleep(Duration::from_millis(150)).await;
                                    Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(
                                        b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                                    ))
                                },
                            )))
                            .expect("build delayed reqwest SSE response")
                    } else {
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "text/event-stream")
                            .body(axum::body::Body::from(
                                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                            ))
                            .expect("build reqwest deadline retry response")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind reqwest total deadline upstream");
        let address = listener
            .local_addr()
            .expect("reqwest total deadline upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve reqwest total deadline upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(5), Duration::from_millis(200));
        let provider = retry_provider(
            "reqwest-total-deadline-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Network],
        );
        let watched = tokio::time::timeout(
            Duration::from_millis(600),
            forwarder.forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = watched
            .expect("reqwest total deadline test must finish")
            .unwrap_or_else(|error| panic!("deadline retry should recover: {}", error.error));
        let response_body = result
            .response
            .bytes()
            .await
            .expect("reqwest deadline retry SSE body");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("response.completed"));
    }

    #[tokio::test]
    async fn responses_semantic_probe_does_not_refresh_deadline_per_chunk() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt =
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if attempt == 0 {
                        let stream = futures::stream::unfold(0usize, |index| async move {
                            if index >= 3 {
                                return None;
                            }
                            tokio::time::sleep(Duration::from_millis(80)).await;
                            let chunk = match index {
                                0 => Bytes::from_static(
                                    b"event: response.in_progress\ndata: {\"type\":\"response.in_progress\"}\n\n",
                                ),
                                1 => Bytes::from_static(
                                    b"event: ping\ndata: {\"type\":\"ping\"}\n\n",
                                ),
                                _ => Bytes::from_static(
                                    b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                                ),
                            };
                            Some((Ok::<Bytes, std::convert::Infallible>(chunk), index + 1))
                        });
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "text/event-stream")
                            .body(axum::body::Body::from_stream(stream))
                            .expect("build lifecycle drip SSE response")
                    } else {
                        http::Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "text/event-stream")
                            .body(axum::body::Body::from(
                                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
                            ))
                            .expect("build semantic deadline retry response")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind semantic deadline upstream");
        let address = listener
            .local_addr()
            .expect("semantic deadline upstream address");
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve semantic deadline upstream");
        });

        let forwarder = test_forwarder(Duration::from_secs(5), Duration::from_millis(200));
        let provider = retry_provider(
            "semantic-deadline-provider",
            format!("http://{address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Network],
        );
        let watched = tokio::time::timeout(
            Duration::from_millis(600),
            forwarder.forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await;

        abort_and_join_test_task(upstream_server).await;

        let result = watched
            .expect("semantic deadline test must finish")
            .unwrap_or_else(|error| {
                panic!("semantic deadline retry should recover: {}", error.error)
            });
        let response_body = result
            .response
            .bytes()
            .await
            .expect("semantic deadline retry SSE body");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("response.completed"));
    }

    #[tokio::test]
    async fn provider_retry_prime_limit_failure_is_not_recorded_as_success() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let non_semantic_body = Bytes::from(vec![b'x'; 256 * 1024]);
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let non_semantic_body = non_semantic_body.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from(non_semantic_body))
                        .expect("build non-semantic prime-limit response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind prime-limit upstream");
        let address = listener.local_addr().expect("prime-limit upstream address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve prime-limit requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "prime-limit-provider",
            format!("http://{address}"),
            1,
            vec!["semantic priming limit".to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await;
        let status = forwarder.status.read().await;
        let success_requests = status.success_requests;
        let failed_requests = status.failed_requests;
        drop(status);
        abort_and_join_test_task(server).await;

        assert!(matches!(
            result,
            Err(ForwardError {
                error: ProxyError::ForwardFailed(message),
                ..
            }) if message.contains("262144")
        ));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(success_requests, 0);
        assert_eq!(failed_requests, 1);
    }

    #[tokio::test]
    async fn prime_limit_failure_keeps_half_open_probe_from_closing_breaker() {
        let first_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_attempts_for_handler = first_attempts.clone();
        let non_semantic_body = Bytes::from(vec![b'x'; 256 * 1024]);
        let first_app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = first_attempts_for_handler.clone();
                let non_semantic_body = non_semantic_body.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    http::Response::builder()
                        .status(StatusCode::OK)
                        .header(http::header::CONTENT_TYPE, "text/event-stream")
                        .body(axum::body::Body::from(non_semantic_body))
                        .expect("build HalfOpen prime-limit response")
                }
            }),
        );
        let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind HalfOpen prime-limit upstream");
        let first_address = first_listener
            .local_addr()
            .expect("HalfOpen prime-limit upstream address");
        let first_server = tokio::spawn(async move {
            axum::serve(first_listener, first_app)
                .await
                .expect("serve HalfOpen prime-limit requests");
        });
        let (second_url, second_attempts, second_server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "half-open-failover-success",
                "status": "completed",
                "output": []
            }),
        )
        .await;

        let first_provider = retry_provider(
            "half-open-prime-limit-provider",
            format!("http://{first_address}"),
            1,
            vec!["semantic priming limit".to_string()],
            vec![],
        );
        let second_provider =
            retry_provider("half-open-success-provider", second_url, 0, vec![], vec![]);
        let db = Arc::new(Database::memory().expect("memory db"));
        db.update_circuit_breaker_config(&crate::proxy::circuit_breaker::CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .expect("configure HalfOpen breaker");
        db.save_provider(AppType::Codex.as_str(), &first_provider)
            .expect("save first HalfOpen provider");
        db.save_provider(AppType::Codex.as_str(), &second_provider)
            .expect("save second HalfOpen provider");
        let router = Arc::new(ProviderRouter::new(db.clone()));
        router
            .record_result(
                &first_provider.id,
                AppType::Codex.as_str(),
                false,
                false,
                Some("seed breaker failure".to_string()),
            )
            .await
            .expect("open first provider breaker");

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.router = router.clone();
        forwarder.failover_manager = Arc::new(FailoverSwitchManager::new(db));
        forwarder.max_attempts = 2;
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|error| {
                panic!("prime-limit failure should fail over: {}", error.error)
            });
        let body = result
            .response
            .bytes()
            .await
            .expect("failover response bytes");
        let breaker = router
            .get_circuit_breaker_stats("half-open-prime-limit-provider", AppType::Codex.as_str())
            .await
            .expect("first provider breaker stats");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("half-open-failover-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            breaker.state,
            crate::proxy::circuit_breaker::CircuitState::Open
        );
        assert_eq!(breaker.total_requests, 2);
        assert_eq!(breaker.failed_requests, 2);
    }

    #[tokio::test]
    async fn auto_review_fallback_retries_model_unavailable_on_same_provider() {
        let requested_models = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requested_models_for_handler = requested_models.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let requested_models = requested_models_for_handler.clone();
                async move {
                    let model = body
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let attempt = {
                        let mut models = requested_models.lock().expect("request model lock");
                        let attempt = models.len();
                        models.push(model);
                        attempt
                    };

                    if attempt == 0 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "message": "No available channel for model codex-auto-review"
                                }
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({
                                "id": "resp-test",
                                "object": "response",
                                "status": "completed",
                                "model": "gpt-5.6-sol",
                                "output": []
                            })),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind retry test server");
        let address = listener.local_addr().expect("retry test server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve retry test requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = Provider {
            id: "review-provider".to_string(),
            name: "Review Provider".to_string(),
            settings_config: json!({
                "base_url": format!("http://{address}"),
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(crate::provider::ProviderMeta {
                codex_auto_review_mode: Some(crate::provider::CodexAutoReviewMode::Auto),
                codex_auto_review_fallback_model: Some("gpt-5.6-sol".to_string()),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({
                    "model": "codex-auto-review",
                    "input": "review this command",
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("fallback retry should recover"));
        let response_body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&response_body).contains("resp-test"));
        assert_eq!(
            *requested_models.lock().expect("request model lock"),
            vec!["codex-auto-review", "gpt-5.6-sol"]
        );
    }

    #[tokio::test]
    async fn normal_codex_retries_matching_provider_error_on_same_provider() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "type": "server_error",
                                    "message": crate::provider::DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                                }
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({
                                "id": "resp-test",
                                "status": "completed",
                                "model": "gpt-5.6-sol",
                                "output": []
                            })),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider retry test server");
        let address = listener
            .local_addr()
            .expect("provider retry test server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve provider retry test requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = Provider {
            id: "normal-provider".to_string(),
            name: "Normal Provider".to_string(),
            settings_config: json!({
                "base_url": format!("http://{address}"),
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(crate::provider::ProviderMeta {
                local_proxy_retry_policy: Some(crate::provider::LocalProxyRetryPolicy {
                    enabled: None,
                    max_retries: 1,
                    retry_delay_ms: 1,
                    custom_messages: vec![
                        crate::provider::DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()
                    ],
                    error_types: vec![],
                }),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "continue the conversation",
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await;
        abort_and_join_test_task(server).await;

        assert!(result.is_ok(), "matching provider error should be retried");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn explicit_unlimited_provider_retry_recovers_on_third_attempt() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                                }
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({
                                "id": "unlimited-retry-success",
                                "status": "completed",
                                "output": []
                            })),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind unlimited provider retry server");
        let address = listener
            .local_addr()
            .expect("unlimited provider retry server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve unlimited provider retry requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "unlimited-provider",
            format!("http://{address}"),
            0,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        provider
            .meta
            .as_mut()
            .and_then(|meta| meta.local_proxy_retry_policy.as_mut())
            .expect("retry policy")
            .enabled = Some(true);

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await;
        abort_and_join_test_task(server).await;

        assert!(result.is_ok(), "unlimited retry should recover");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn finite_provider_retry_is_clamped_to_100_extra_attempts() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 100 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                                }
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({
                                "id": "finite-limit-success",
                                "status": "completed",
                                "output": []
                            })),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind finite retry limit server");
        let address = listener.local_addr().expect("finite retry limit address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve finite retry limit requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "finite-limit-provider",
            format!("http://{address}"),
            101,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await;
        abort_and_join_test_task(server).await;

        assert!(result.is_ok(), "the 101st total attempt should succeed");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 101);
    }

    #[tokio::test]
    async fn aborting_unlimited_provider_retry_stops_new_attempts() {
        let (base_url, attempts, server) = spawn_counting_responses_server(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "error": {
                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                }
            }),
        )
        .await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "unlimited-abort-provider",
            base_url,
            0,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        provider
            .meta
            .as_mut()
            .and_then(|meta| meta.local_proxy_retry_policy.as_mut())
            .expect("retry policy")
            .enabled = Some(true);

        let status = forwarder.status.clone();
        let retry_task = tokio::spawn(async move {
            forwarder
                .forward_with_retry(
                    &AppType::Codex,
                    http::Method::POST,
                    "/v1/responses",
                    json!({
                        "model": "gpt-5.6-sol",
                        "input": "keep retrying",
                        "stream": false
                    }),
                    HeaderMap::new(),
                    Extensions::new(),
                    vec![provider],
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("unlimited retry should issue at least two requests");

        retry_task.abort();
        let join_error = match retry_task.await {
            Err(error) => error,
            Ok(_) => panic!("aborted retry task must not complete normally"),
        };
        assert!(join_error.is_cancelled());

        tokio::time::timeout(Duration::from_secs(1), async {
            while status.read().await.active_connections != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aborting the retry future should release the active connection");

        // Allow a request dispatched just before abort to reach the test server.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let stopped_at = attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert!(stopped_at >= 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            stopped_at,
            "dropping the retry future must stop issuing new attempts"
        );

        abort_and_join_test_task(server).await;
    }

    #[tokio::test]
    async fn normal_codex_retries_matching_200_failed_sse_on_same_provider() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = if attempt == 0 {
                        format!(
                            "event: response.failed\ndata: {}\n\n",
                            json!({
                                "type": "response.failed",
                                "response": {
                                    "status": "failed",
                                    "error": {
                                        "type": "server_error",
                                        "message": crate::provider::DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                                    }
                                }
                            })
                        )
                    } else {
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-test\",\"status\":\"completed\",\"model\":\"gpt-5.6-sol\",\"output\":[]}}\n\n".to_string()
                    };
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider retry SSE test server");
        let address = listener
            .local_addr()
            .expect("provider retry SSE test server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve provider retry SSE test requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = Provider {
            id: "normal-provider".to_string(),
            name: "Normal Provider".to_string(),
            settings_config: json!({
                "base_url": format!("http://{address}"),
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(crate::provider::ProviderMeta {
                local_proxy_retry_policy: Some(crate::provider::LocalProxyRetryPolicy {
                    enabled: None,
                    max_retries: 1,
                    retry_delay_ms: 1,
                    custom_messages: vec![
                        crate::provider::DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()
                    ],
                    error_types: vec![],
                }),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "continue the conversation",
                    "stream": true
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("matching failed SSE should be retried"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("response.completed"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unknown_lifecycle_before_fragmented_target_sse_retries_same_provider() {
        let lifecycle = Bytes::from_static(
            b"event: response.vendor_heartbeat\ndata: {\"type\":\"response.vendor_heartbeat\",\"status\":\"in_progress\"}\n\n",
        );
        let failed = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        );
        let split = failed.len() / 2;
        let (base_url, attempts, server) = spawn_two_attempt_sse_server(
            "/v1/responses",
            vec![
                lifecycle,
                Bytes::copy_from_slice(&failed.as_bytes()[..split]),
                Bytes::copy_from_slice(&failed.as_bytes()[split..]),
            ],
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-lifecycle-success\",\"status\":\"completed\",\"output\":[]}}\n\n".to_string(),
        )
        .await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "unknown-lifecycle-provider",
            base_url,
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("fragmented failure after lifecycle should retry"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("resp-lifecycle-success"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn claude_event_error_retries_same_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let error_body = format!(
            "event: error\ndata: {}\n\n",
            json!({
                "type": "error",
                "error": {
                    "type": "overloaded_error",
                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                }
            })
        );
        let (base_url, attempts, server) = spawn_two_attempt_sse_server(
            "/v1/messages",
            vec![Bytes::from(error_body)],
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        )
        .await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "claude-event-error-provider",
            base_url.clone(),
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        provider.settings_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": base_url,
                "ANTHROPIC_API_KEY": "test-key"
            }
        });
        let result = forwarder
            .forward_with_retry(
                &AppType::Claude,
                http::Method::POST,
                "/v1/messages",
                json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{ "role": "user", "content": "continue" }],
                    "stream": true
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("Claude event:error should retry"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("message_stop"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn grok_build_response_failed_retries_same_provider() {
        let error_body = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        );
        let (base_url, attempts, server) = spawn_two_attempt_sse_server(
            "/v1/responses",
            vec![Bytes::from(error_body)],
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"grok-retry-success\",\"status\":\"completed\",\"output\":[]}}\n\n".to_string(),
        )
        .await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "grok-build-provider",
            base_url,
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::GrokBuild,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "grok-code-fast-1", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("Grok Build response.failed should retry"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("grok-retry-success"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn gemini_root_error_retries_same_provider() {
        const ENDPOINT: &str = "/v1beta/models/gemini-2.5-pro:generateContent";
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            ENDPOINT,
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        Json(json!({
                            "error": {
                                "code": 503,
                                "status": "UNAVAILABLE",
                                "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                            }
                        }))
                    } else {
                        Json(json!({
                            "candidates": [{
                                "content": {
                                    "role": "model",
                                    "parts": [{ "text": "recovered" }]
                                },
                                "finishReason": "STOP"
                            }]
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Gemini retry server");
        let address = listener.local_addr().expect("Gemini retry server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Gemini retry requests");
        });
        tokio::task::yield_now().await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "gemini-root-error-provider",
            format!("http://{address}"),
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        provider.settings_config = json!({
            "env": {
                "GOOGLE_GEMINI_BASE_URL": format!("http://{address}"),
                "GEMINI_API_KEY": "test-key"
            }
        });
        let result = forwarder
            .forward_with_retry(
                &AppType::Gemini,
                http::Method::POST,
                ENDPOINT,
                json!({
                    "contents": [{
                        "role": "user",
                        "parts": [{ "text": "continue" }]
                    }]
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("Gemini root error should retry"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("recovered"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn normal_codex_retries_matching_200_json_failure_on_same_provider() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        Json(json!({
                            "status": "failed",
                            "error": {
                                "type": "server_error",
                                "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                            }
                        }))
                    } else {
                        Json(json!({
                            "id": "resp-json-success",
                            "status": "completed",
                            "model": "gpt-5.6-sol",
                            "output": []
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind JSON retry server");
        let address = listener.local_addr().expect("JSON retry server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve JSON retry requests");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "json-provider",
            format!("http://{address}"),
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({
                    "model": "gpt-5.6-sol",
                    "input": "continue",
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("matching JSON failure should be retried"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("resp-json-success"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_message_in_successful_model_output_does_not_retry() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(json!({
                        "id": "resp-output",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "content": [{
                                "type": "output_text",
                                "text": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                            }]
                        }]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind output safety server");
        let address = listener.local_addr().expect("output safety server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve output safety request");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "output-provider",
            format!("http://{address}"),
            2,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "quote it", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await;
        abort_and_join_test_task(server).await;

        assert!(result.is_ok());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_message_after_stream_output_does_not_replay_request() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let raw = format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"visible\"}}\n\nevent: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        );
        let raw_for_handler = raw.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                let raw = raw_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (StatusCode::OK, [("content-type", "text/event-stream")], raw)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind post-output server");
        let address = listener.local_addr().expect("post-output server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve post-output request");
        });

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "post-output-provider",
            format!("http://{address}"),
            2,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": true }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|_| panic!("output commits the original stream"));
        let replayed = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(server).await;

        assert_eq!(replayed, Bytes::from(raw));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn productive_output_reasoning_delta_prevents_retry_after_matching_failure() {
        let prefix = concat!(
            "event: response.reasoning.delta\n",
            "data: {\"type\":\"response.reasoning.delta\",\"delta\":\"thinking\"}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("response.reasoning.delta"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn productive_output_tool_search_call_prevents_retry_after_matching_failure() {
        let prefix = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"ts_1\",\"type\":\"tool_search_call\"}}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("tool_search_call"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn productive_output_tool_search_type_only_allows_matching_failure_retry() {
        let prefix = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"tool_search_call\"}}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("productive-output-retry-success"));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn productive_output_responses_function_call_id_prevents_retry() {
        let prefix = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\"}}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("fc_1"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn productive_output_empty_openai_tool_placeholders_allow_matching_failure_retry() {
        let prefix = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[],\"function_call\":{}}}]}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("productive-output-retry-success"));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn productive_output_openai_index_and_type_only_tool_allows_retry() {
        let prefix = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"type\":\"function\",\"function\":{\"name\":\"\",\"arguments\":\"\"}}]}}]}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("productive-output-retry-success"));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn productive_output_anthropic_tool_type_only_allows_retry() {
        let prefix = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"\",\"name\":\"\",\"input\":{}}}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("productive-output-retry-success"));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn productive_output_anthropic_tool_id_and_name_prevent_retry() {
        let prefix = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"lookup\",\"input\":{}}}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("toolu_1"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn productive_output_empty_gemini_tool_placeholders_allow_matching_failure_retry() {
        let prefix = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"tool_calls\":[],\"function_call\":{}}]}}]}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("productive-output-retry-success"));
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn productive_output_non_empty_openai_tool_payload_prevents_retry() {
        let prefix = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"lookup\"}}]}}]}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("lookup"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn productive_output_nested_gemini_tool_payload_prevents_retry() {
        let prefix = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{}}}]}}]}\n\n"
        );

        let (body, attempts) = run_matching_failure_after_sse_prefix(prefix).await;

        assert!(String::from_utf8_lossy(&body).contains("functionCall"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn responses_http_200_cancelled_without_error_retries_through_forwarder() {
        let (base_url, attempts, server) = spawn_sequence_responses_server(vec![
            (
                StatusCode::OK,
                json!({ "status": "cancelled", "error": null, "output": [] }),
            ),
            (
                StatusCode::OK,
                json!({
                    "id": "cancelled-retry-success",
                    "status": "completed",
                    "error": null,
                    "output": []
                }),
            ),
        ])
        .await;
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "responses-cancelled-provider",
            base_url,
            1,
            vec!["response generation was cancelled".to_string()],
            vec![],
        );

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|error| panic!("cancelled response should retry: {}", error.error));
        let body = result.response.bytes().await.expect("cancelled retry body");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("cancelled-retry-success"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn responses_http_200_failed_without_error_exhausts_retry_then_fails_over() {
        let (first_url, first_attempts, first_server) = spawn_sequence_responses_server(vec![(
            StatusCode::OK,
            json!({ "status": "failed", "output": [] }),
        )])
        .await;
        let (second_url, second_attempts, second_server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "failed-envelope-failover-success",
                "status": "completed",
                "error": null,
                "output": []
            }),
        )
        .await;
        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let first_provider = retry_provider(
            "responses-failed-provider",
            first_url,
            1,
            vec!["response generation failed".to_string()],
            vec![],
        );
        let second_provider =
            retry_provider("responses-failover-provider", second_url, 0, vec![], vec![]);

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|error| panic!("failed response should fail over: {}", error.error));
        let body = result.response.bytes().await.expect("failed failover body");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("failed-envelope-failover-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn responses_http_200_completed_with_null_error_is_accepted() {
        let (base_url, attempts, server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "completed-null-error",
                "status": "completed",
                "error": null,
                "output": []
            }),
        )
        .await;
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "responses-completed-provider",
            base_url,
            1,
            vec!["response generation failed".to_string()],
            vec![],
        );

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|error| panic!("completed response should pass: {}", error.error));
        let body = result
            .response
            .bytes()
            .await
            .expect("completed response body");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("completed-null-error"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn codex_anthropic_http_200_error_keeps_specialized_detector_in_forwarder() {
        let (base_url, attempts, server) = spawn_sequence_json_server(
            "/v1/messages",
            vec![
                (
                    StatusCode::OK,
                    json!({
                        "type": "error",
                        "error": {
                            "type": "overloaded_error",
                            "message": "anthropic capacity unavailable"
                        }
                    }),
                ),
                (
                    StatusCode::OK,
                    json!({
                        "id": "anthropic-retry-success",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "stop_reason": "end_turn"
                    }),
                ),
            ],
        )
        .await;
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "codex-anthropic-provider",
            base_url,
            1,
            vec![],
            vec![LocalProxyRetryErrorType::Overloaded],
        );
        provider.meta.as_mut().expect("provider meta").api_format = Some("anthropic".to_string());

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "claude-sonnet", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|error| panic!("Anthropic error should retry: {}", error.error));
        let body = result.response.bytes().await.expect("Anthropic retry body");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("anthropic-retry-success"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn non_2xx_scalar_error_classification_retries_through_forwarder() {
        let (base_url, attempts, server) = spawn_sequence_responses_server(vec![
            (
                StatusCode::BAD_REQUEST,
                json!({ "error": "resource_exhausted" }),
            ),
            (
                StatusCode::OK,
                json!({ "id": "scalar-retry-success", "status": "completed", "output": [] }),
            ),
        ])
        .await;
        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let provider = retry_provider(
            "scalar-classification-provider",
            base_url,
            1,
            vec![],
            vec![LocalProxyRetryErrorType::RateLimit],
        );

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            )
            .await
            .unwrap_or_else(|error| panic!("scalar error should retry: {}", error.error));
        let body = result.response.bytes().await.expect("scalar retry body");
        abort_and_join_test_task(server).await;

        assert!(String::from_utf8_lossy(&body).contains("scalar-retry-success"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn responses_http_200_numeric_error_codes_use_preset_retry_types() {
        let cases = [
            (429u16, LocalProxyRetryErrorType::RateLimit),
            (503u16, LocalProxyRetryErrorType::Overloaded),
            (500u16, LocalProxyRetryErrorType::ServerError),
        ];

        for (code, error_type) in cases {
            let success_id = format!("numeric-{code}-retry-success");
            let (base_url, attempts, server) = spawn_sequence_responses_server(vec![
                (
                    StatusCode::OK,
                    json!({
                        "status": "failed",
                        "error": { "code": code, "message": "numeric classified failure" },
                        "output": []
                    }),
                ),
                (
                    StatusCode::OK,
                    json!({ "id": success_id, "status": "completed", "output": [] }),
                ),
            ])
            .await;
            let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
            let provider = retry_provider(
                &format!("numeric-{code}-provider"),
                base_url,
                1,
                vec![],
                vec![error_type],
            );

            let result = forwarder
                .forward_with_retry(
                    &AppType::Codex,
                    http::Method::POST,
                    "/v1/responses",
                    json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                    HeaderMap::new(),
                    Extensions::new(),
                    vec![provider],
                )
                .await
                .unwrap_or_else(|error| panic!("numeric {code} should retry: {}", error.error));
            let body = result.response.bytes().await.expect("numeric retry body");
            abort_and_join_test_task(server).await;

            assert!(String::from_utf8_lossy(&body).contains(&success_id));
            assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn provider_retry_policy_does_not_apply_to_auto_review_model() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "error": {
                                "type": "server_error",
                                "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                            }
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind approval exclusion server");
        let address = listener
            .local_addr()
            .expect("approval exclusion server address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve approval exclusion request");
        });
        tokio::task::yield_now().await;

        let forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        let mut provider = retry_provider(
            "approval-provider",
            format!("http://{address}"),
            0,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        provider
            .meta
            .as_mut()
            .and_then(|meta| meta.local_proxy_retry_policy.as_mut())
            .expect("retry policy")
            .enabled = Some(true);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            forwarder.forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({
                    "model": "codex-auto-review",
                    "input": "review this command",
                    "stream": false
                }),
                HeaderMap::new(),
                Extensions::new(),
                vec![provider],
            ),
        )
        .await
        .expect("codex-auto-review exclusion must not enter unlimited provider retry");
        abort_and_join_test_task(server).await;

        assert!(result.is_err());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn each_provider_uses_its_own_retry_budget_before_failover() {
        let first_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_attempts_for_handler = first_attempts.clone();
        let first_app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = first_attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "error": {
                                "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                            }
                        })),
                    )
                }
            }),
        );
        let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind first failover server");
        let first_address = first_listener
            .local_addr()
            .expect("first failover server address");
        let first_server = tokio::spawn(async move {
            axum::serve(first_listener, first_app)
                .await
                .expect("serve first provider requests");
        });

        let second_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second_attempts_for_handler = second_attempts.clone();
        let second_app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = second_attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({
                                "error": {
                                    "code": "rate_limit",
                                    "message": "try again"
                                }
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({
                                "id": "second-provider-success",
                                "status": "completed",
                                "output": []
                            })),
                        )
                    }
                }
            }),
        );
        let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind second failover server");
        let second_address = second_listener
            .local_addr()
            .expect("second failover server address");
        let second_server = tokio::spawn(async move {
            axum::serve(second_listener, second_app)
                .await
                .expect("serve second provider requests");
        });

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let first_provider = retry_provider(
            "first-provider",
            format!("http://{first_address}"),
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let second_provider = retry_provider(
            "second-provider",
            format!("http://{second_address}"),
            1,
            vec![],
            vec![LocalProxyRetryErrorType::RateLimit],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|_| panic!("second provider should recover"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("second-provider-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn matching_bad_request_fails_over_after_provider_retry_budget_is_exhausted() {
        let first_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_attempts_for_handler = first_attempts.clone();
        let first_app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = first_attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                        })),
                    )
                }
            }),
        );
        let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind first bad request failover server");
        let first_address = first_listener
            .local_addr()
            .expect("first bad request failover server address");
        let first_server = tokio::spawn(async move {
            axum::serve(first_listener, first_app)
                .await
                .expect("serve first bad request provider requests");
        });
        tokio::task::yield_now().await;

        let second_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second_attempts_for_handler = second_attempts.clone();
        let second_app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = second_attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "id": "bad-request-failover-success",
                            "status": "completed",
                            "output": []
                        })),
                    )
                }
            }),
        );
        let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind second bad request failover server");
        let second_address = second_listener
            .local_addr()
            .expect("second bad request failover server address");
        let second_server = tokio::spawn(async move {
            axum::serve(second_listener, second_app)
                .await
                .expect("serve second bad request provider requests");
        });
        tokio::task::yield_now().await;

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let first_provider = retry_provider(
            "first-bad-request-provider",
            format!("http://{first_address}"),
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let second_provider = retry_provider(
            "second-success-provider",
            format!("http://{second_address}"),
            0,
            vec![],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|_| panic!("matching 400 should fail over after retry exhaustion"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("bad-request-failover-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn matching_unsupported_media_type_fails_over_after_retry_exhaustion() {
        let (first_url, first_attempts, first_server) = spawn_counting_responses_server(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            json!({
                "error": {
                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                }
            }),
        )
        .await;
        let (second_url, second_attempts, second_server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "unsupported-media-type-failover-success",
                "status": "completed",
                "output": []
            }),
        )
        .await;

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let first_provider = retry_provider(
            "first-unsupported-media-type-provider",
            first_url,
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let second_provider = retry_provider(
            "second-unsupported-media-type-provider",
            second_url,
            0,
            vec![],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|_| panic!("matching 415 should fail over after retry exhaustion"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("unsupported-media-type-failover-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn matching_unprocessable_entity_fails_over_and_records_one_breaker_failure() {
        let (first_url, first_attempts, first_server) = spawn_counting_responses_server(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "error": {
                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                }
            }),
        )
        .await;
        let (second_url, second_attempts, second_server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "unprocessable-failover-success",
                "status": "completed",
                "output": []
            }),
        )
        .await;

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let first_provider = retry_provider(
            "first-unprocessable-provider",
            first_url,
            2,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let second_provider = retry_provider(
            "second-unprocessable-provider",
            second_url,
            0,
            vec![],
            vec![],
        );
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|_| panic!("matching 422 should fail over after retry exhaustion"));
        let body = result.response.bytes().await.expect("response bytes");
        let breaker = forwarder
            .router
            .get_circuit_breaker_stats("first-unprocessable-provider", AppType::Codex.as_str())
            .await
            .expect("first provider breaker stats");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("unprocessable-failover-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(breaker.total_requests, 1);
        assert_eq!(breaker.failed_requests, 1);
    }

    #[tokio::test]
    async fn non_matching_bad_request_does_not_retry_or_fail_over() {
        let (first_url, first_attempts, first_server) = spawn_counting_responses_server(
            StatusCode::BAD_REQUEST,
            json!({
                "error": {
                    "message": "invalid request payload"
                }
            }),
        )
        .await;
        let (second_url, second_attempts, second_server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "unexpected-second-provider-success",
                "status": "completed",
                "output": []
            }),
        )
        .await;

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let first_provider = retry_provider(
            "first-non-matching-provider",
            first_url,
            2,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let second_provider =
            retry_provider("second-unused-provider", second_url, 0, vec![], vec![]);
        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await;
        let breaker = forwarder
            .router
            .get_circuit_breaker_stats("first-non-matching-provider", AppType::Codex.as_str())
            .await
            .expect("first provider breaker stats");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(matches!(
            result,
            Err(ForwardError {
                error: ProxyError::UpstreamError { status: 400, .. },
                ..
            })
        ));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(breaker.total_requests, 0);
        assert_eq!(breaker.failed_requests, 0);
    }

    #[tokio::test]
    async fn unlimited_policy_stops_on_non_matching_error_and_uses_existing_failover() {
        let first_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_attempts_for_handler = first_attempts.clone();
        let first_app = Router::new().route(
            "/v1/responses",
            post(move |Json(_body): Json<Value>| {
                let attempts = first_attempts_for_handler.clone();
                async move {
                    if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                                }
                            })),
                        )
                    } else {
                        (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({
                                "error": {
                                    "message": "different transient failure"
                                }
                            })),
                        )
                    }
                }
            }),
        );
        let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind unlimited non-matching server");
        let first_address = first_listener
            .local_addr()
            .expect("unlimited non-matching server address");
        let first_server = tokio::spawn(async move {
            axum::serve(first_listener, first_app)
                .await
                .expect("serve unlimited non-matching requests");
        });
        let (second_url, second_attempts, second_server) = spawn_counting_responses_server(
            StatusCode::OK,
            json!({
                "id": "unlimited-non-match-failover-success",
                "status": "completed",
                "output": []
            }),
        )
        .await;

        let mut forwarder = test_forwarder(Duration::from_secs(2), Duration::from_secs(2));
        forwarder.max_attempts = 2;
        let mut first_provider = retry_provider(
            "unlimited-non-matching-provider",
            format!("http://{first_address}"),
            0,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        first_provider
            .meta
            .as_mut()
            .and_then(|meta| meta.local_proxy_retry_policy.as_mut())
            .expect("retry policy")
            .enabled = Some(true);
        let second_provider = retry_provider(
            "unlimited-non-match-success-provider",
            second_url,
            0,
            vec![],
            vec![],
        );

        let result = forwarder
            .forward_with_retry(
                &AppType::Codex,
                http::Method::POST,
                "/v1/responses",
                json!({ "model": "gpt-5.6-sol", "input": "continue", "stream": false }),
                HeaderMap::new(),
                Extensions::new(),
                vec![first_provider, second_provider],
            )
            .await
            .unwrap_or_else(|_| panic!("non-matching 503 should use existing failover"));
        let body = result.response.bytes().await.expect("response bytes");
        abort_and_join_test_task(first_server).await;
        abort_and_join_test_task(second_server).await;

        assert!(String::from_utf8_lossy(&body).contains("unlimited-non-match-failover-success"));
        assert_eq!(first_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(second_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fragmented_target_sse_retries_and_non_target_sse_replays_exact_bytes() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "stream-policy-provider",
            "http://unused.example".to_string(),
            1,
            vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            vec![],
        );
        let policy = super::super::provider_retry::resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("policy");
        let headers = HeaderMap::from_iter([(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )]);

        let target = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "type": "server_error",
                        "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                    }
                }
            })
        );
        let split = target.len() / 2;
        let target_response = ProxyResponse::streamed(
            StatusCode::OK,
            headers.clone(),
            futures::stream::iter(vec![
                Ok::<_, std::io::Error>(Bytes::copy_from_slice(&target.as_bytes()[..split])),
                Ok(Bytes::copy_from_slice(&target.as_bytes()[split..])),
            ]),
        );
        let target_result = forwarder
            .validate_provider_retry_success_response(
                target_response,
                true,
                &policy,
                forwarder.pre_output_deadline(true),
            )
            .await;
        assert!(matches!(target_result, Err(ProxyError::TransformError(_))));

        let non_target = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"type\":\"invalid_request\",\"message\":\"different failure\"}}}\n\n"
        );
        let non_target_response = ProxyResponse::streamed(
            StatusCode::OK,
            headers,
            futures::stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(
                non_target.as_bytes(),
            ))]),
        );
        let replay = forwarder
            .validate_provider_retry_success_response(
                non_target_response,
                true,
                &policy,
                forwarder.pre_output_deadline(true),
            )
            .await
            .expect("non-target SSE should be replayed")
            .bytes()
            .await
            .expect("replayed bytes");
        assert_eq!(replay, Bytes::from_static(non_target.as_bytes()));
    }

    #[tokio::test]
    async fn responses_prime_limit_does_not_scan_terminal_event_after_single_chunk_boundary() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "single-chunk-prime-limit-provider",
            "http://unused.example".to_string(),
            1,
            vec!["semantic priming limit".to_string()],
            vec![],
        );
        let policy = super::super::provider_retry::resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("semantic prime-limit retry policy");
        let mut raw = vec![b'x'; 256 * 1024];
        raw.extend_from_slice(
            b"\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(raw)) }),
        );

        let result = forwarder
            .validate_provider_retry_success_response(
                response,
                true,
                &policy,
                forwarder.pre_output_deadline(true),
            )
            .await;

        assert!(matches!(
            result,
            Err(ProxyError::ForwardFailed(message)) if message.contains("262144")
        ));
    }

    #[tokio::test]
    async fn responses_prime_limit_rejects_many_small_non_semantic_chunks() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "small-chunk-prime-limit-provider",
            "http://unused.example".to_string(),
            1,
            vec!["semantic priming limit".to_string()],
            vec![],
        );
        let policy = super::super::provider_retry::resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("semantic prime-limit retry policy");
        let chunks = (0..256).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 1024])));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::iter(chunks),
        );

        let result = forwarder
            .validate_provider_retry_success_response(
                response,
                true,
                &policy,
                forwarder.pre_output_deadline(true),
            )
            .await;

        assert!(matches!(
            result,
            Err(ProxyError::ForwardFailed(message)) if message.contains("262144")
        ));
    }

    #[tokio::test]
    async fn responses_prime_limit_replays_exact_bytes_when_policy_does_not_reject() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "prime-limit-replay-provider",
            "http://unused.example".to_string(),
            1,
            vec!["retry only a specific upstream message".to_string()],
            vec![],
        );
        let policy = super::super::provider_retry::resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("custom-message retry policy");
        let raw = Bytes::from(vec![b'x'; 256 * 1024 + 17]);
        let expected = raw.clone();
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move { Ok::<_, std::io::Error>(raw) }),
        );

        let replayed = forwarder
            .validate_provider_retry_success_response(
                response,
                true,
                &policy,
                forwarder.pre_output_deadline(true),
            )
            .await
            .expect("non-matching prime-limit failure should replay")
            .bytes()
            .await
            .expect("replayed prime-limit bytes");

        assert_eq!(replayed, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_large_no_delimiter_sse_chunk_yields_before_commit() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let provider = retry_provider(
            "no-delimiter-sse-yield-provider",
            "http://unused.example".to_string(),
            1,
            vec!["retry only a specific upstream message".to_string()],
            vec![],
        );
        let policy = super::super::provider_retry::resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("custom-message retry policy");
        let raw = Bytes::from(vec![b'x'; 256 * 1024]);
        let expected = raw.clone();
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move { Ok::<_, std::io::Error>(raw) }),
        );
        let yielded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let yielded_from_task = yielded.clone();
        let observer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            yielded_from_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let replayed = forwarder
            .validate_provider_retry_success_response(
                response,
                true,
                &policy,
                forwarder.pre_output_deadline(true),
            )
            .await
            .expect("non-matching no-delimiter SSE chunk should replay")
            .bytes()
            .await
            .expect("no-delimiter SSE replay bytes");
        let yielded_before_validation_returned = yielded.load(std::sync::atomic::Ordering::SeqCst);
        observer.await.expect("yield observer task");

        assert_eq!(replayed, expected);
        assert!(yielded_before_validation_returned);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_large_productive_sse_block_rechecks_deadline_after_inspection() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let raw = Bytes::from(format!(
            ":{}\nevent: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"ready\"}}\n\n",
            "x".repeat(240 * 1024)
        ));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move { Ok::<_, std::io::Error>(raw) }),
        );

        set_responses_sse_inspect_delay(Duration::from_millis(300));
        let result = forwarder
            .validate_responses_stream_start(
                response,
                PreOutputDeadline::new(Duration::from_millis(250)),
            )
            .await;
        set_responses_sse_inspect_delay(Duration::ZERO);

        assert!(matches!(
            result,
            Err(ProxyError::Timeout(message)) if message.contains("SSE")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_large_failure_sse_block_rechecks_deadline_after_inspection() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let raw = Bytes::from(format!(
            ":{}\nevent: response.failed\ndata: {{\"type\":\"response.failed\",\"response\":{{\"status\":\"failed\",\"error\":{{\"type\":\"server_error\",\"message\":\"busy\"}}}}}}\n\n",
            "x".repeat(240 * 1024)
        ));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move { Ok::<_, std::io::Error>(raw) }),
        );

        set_responses_sse_inspect_delay(Duration::from_millis(300));
        let result = forwarder
            .validate_responses_stream_start(
                response,
                PreOutputDeadline::new(Duration::from_millis(250)),
            )
            .await;
        set_responses_sse_inspect_delay(Duration::ZERO);

        assert!(matches!(
            result,
            Err(ProxyError::Timeout(message)) if message.contains("SSE")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_no_delimiter_sse_scan_rechecks_absolute_deadline() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let raw = Bytes::from(vec![b'x'; 256 * 1024]);
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move {
                std::thread::sleep(Duration::from_millis(10));
                Ok::<_, std::io::Error>(raw)
            }),
        );

        let result = forwarder
            .validate_responses_stream_start(
                response,
                PreOutputDeadline::new(Duration::from_millis(1)),
            )
            .await;

        assert!(matches!(
            result,
            Err(ProxyError::Timeout(message)) if message.contains("SSE")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_large_json_document_rechecks_deadline_before_commit() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let raw = Bytes::from(format!(
            "{{\"status\":\"completed\",\"output\":[\"{}\"]}}",
            "x".repeat(128 * 1024)
        ));
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::once(async move {
                std::thread::sleep(Duration::from_millis(10));
                Ok::<_, std::io::Error>(raw)
            }),
        );

        let result = forwarder
            .validate_responses_stream_start(
                response,
                PreOutputDeadline::new(Duration::from_millis(1)),
            )
            .await;

        assert!(matches!(
            result,
            Err(ProxyError::Timeout(message)) if message.contains("JSON")
        ));
    }

    #[test]
    fn codex_oauth_session_headers_match_codex_cache_identity() {
        let headers = build_codex_oauth_session_headers("session-123");
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(name, value);
        }

        assert_eq!(
            map.get("session_id"),
            Some(&HeaderValue::from_static("session-123"))
        );
        assert_eq!(
            map.get("x-client-request-id"),
            Some(&HeaderValue::from_static("session-123"))
        );
        assert_eq!(
            map.get("x-codex-window-id"),
            Some(&HeaderValue::from_static("session-123:0"))
        );
    }

    #[test]
    fn managed_account_upstream_rejects_proxy_managed_placeholder_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer PROXY_MANAGED"),
        );

        let err = reject_proxy_placeholder_for_managed_account_upstream(
            "https://api.githubcopilot.com/chat/completions",
            &headers,
        )
        .expect_err("placeholder should be rejected before upstream");

        assert!(matches!(
            err,
            ProxyError::AuthError(message) if message.contains("PROXY_MANAGED")
        ));

        let xai_err = reject_proxy_placeholder_for_managed_account_upstream(
            "https://api.x.ai/v1/responses",
            &headers,
        )
        .expect_err("xAI placeholder should be rejected before upstream");
        assert!(matches!(
            xai_err,
            ProxyError::AuthError(message) if message.contains("PROXY_MANAGED")
        ));
    }

    #[test]
    fn codex_oauth_upstream_rejects_proxy_managed_placeholder_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer PROXY_MANAGED"),
        );

        let err = reject_proxy_placeholder_for_managed_account_upstream(
            "https://chatgpt.com/backend-api/codex/responses",
            &headers,
        )
        .expect_err("placeholder should be rejected before upstream");

        assert!(matches!(
            err,
            ProxyError::AuthError(message) if message.contains("PROXY_MANAGED")
        ));
    }

    #[test]
    fn non_managed_upstream_allows_proxy_managed_placeholder_guard() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer PROXY_MANAGED"),
        );

        reject_proxy_placeholder_for_managed_account_upstream(
            "https://api.example.com/v1/messages",
            &headers,
        )
        .expect("guard is scoped to managed-account upstreams");
    }

    #[test]
    fn exact_header_case_preserved_for_native_claude_only() {
        let provider = test_provider_with_type(None);

        assert!(should_preserve_exact_header_case(
            "Claude",
            &provider,
            Some("anthropic"),
            false
        ));
        assert!(!should_preserve_exact_header_case(
            "Claude",
            &provider,
            Some("openai_responses"),
            false
        ));
        assert!(!should_preserve_exact_header_case(
            "Codex", &provider, None, false
        ));
        assert!(!should_preserve_exact_header_case(
            "Gemini", &provider, None, false
        ));
    }

    #[test]
    fn exact_header_case_skipped_for_codex_oauth_and_copilot() {
        let codex_oauth = test_provider_with_type(Some("codex_oauth"));
        let copilot = test_provider_with_type(Some("github_copilot"));

        assert!(!should_preserve_exact_header_case(
            "Claude",
            &codex_oauth,
            Some("openai_responses"),
            false
        ));
        assert!(!should_preserve_exact_header_case(
            "Claude",
            &copilot,
            Some("openai_chat"),
            true
        ));
    }

    #[test]
    fn rewrite_claude_transform_endpoint_strips_beta_for_chat_completions() {
        let (endpoint, passthrough_query) = rewrite_claude_transform_endpoint(
            "/v1/messages?beta=true&foo=bar",
            "openai_chat",
            false,
            &json!({ "model": "gpt-5.4" }),
        );

        assert_eq!(endpoint, "/v1/chat/completions?foo=bar");
        assert_eq!(passthrough_query.as_deref(), Some("foo=bar"));
    }

    #[test]
    fn rewrite_claude_transform_endpoint_strips_beta_for_responses() {
        let (endpoint, passthrough_query) = rewrite_claude_transform_endpoint(
            "/claude/v1/messages?beta=true&x-id=1",
            "openai_responses",
            false,
            &json!({ "model": "gpt-5.4" }),
        );

        assert_eq!(endpoint, "/v1/responses?x-id=1");
        assert_eq!(passthrough_query.as_deref(), Some("x-id=1"));
    }

    #[test]
    fn rewrite_codex_responses_endpoint_to_chat_preserves_query() {
        let (endpoint, passthrough_query) =
            rewrite_codex_responses_endpoint_to_chat("/v1/responses?foo=bar");

        assert_eq!(endpoint, "/chat/completions?foo=bar");
        assert_eq!(passthrough_query.as_deref(), Some("foo=bar"));
    }

    #[test]
    fn prepend_claude_code_system_prompt_from_string() {
        let mut body = json!({ "system": "You are a Codex agent." });
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
        assert_eq!(system[1]["text"], "You are a Codex agent.");
    }

    #[test]
    fn prepend_claude_code_system_prompt_when_absent() {
        let mut body = json!({});
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
    }

    #[test]
    fn prepend_claude_code_system_prompt_is_idempotent() {
        let mut body = json!({ "system": "orig" });
        prepend_claude_code_system_prompt(&mut body);
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
        assert_eq!(system[1]["text"], "orig");
    }

    #[test]
    fn rewrite_codex_responses_endpoint_to_anthropic_preserves_query() {
        let (endpoint, passthrough_query) =
            rewrite_codex_responses_endpoint_to_anthropic("/responses?x=1");
        assert_eq!(endpoint, "/v1/messages?x=1");
        assert_eq!(passthrough_query.as_deref(), Some("x=1"));

        let (endpoint, _) = rewrite_codex_responses_endpoint_to_anthropic("/v1/responses");
        assert_eq!(endpoint, "/v1/messages");
    }

    #[test]
    fn codex_anthropic_full_endpoint_guard_avoids_double_messages() {
        // On the Codex→Anthropic path a base URL already ending in `/v1/messages` (switch
        // off) must be treated as a full endpoint by the real `base_url_is_full_endpoint`.

        // Without the guard, build_url would concatenate the pasted endpoint with the
        // rewritten `/v1/messages` target, producing a broken double suffix.
        use super::super::providers::ProviderAdapter;
        let doubled = super::super::providers::CodexAdapter::new()
            .build_url("https://host.example/v1/messages", "/v1/messages");
        assert_eq!(doubled, "https://host.example/v1/messages/v1/messages");

        // With the guard, the pasted URL is used verbatim (plus preserved query). Includes
        // query/fragment/whitespace suffixes, which must not hide the endpoint (fix: a base
        // like `.../v1/messages?beta=true` previously evaded the suffix check).
        for base in [
            "https://host.example/v1/messages",
            "https://host.example/v1/messages/",
            "https://host.example/api/v1/messages", // prefixed gateway
            "https://host.example/v1/messages?beta=true",
            "https://host.example/v1/messages/?beta=true",
            "https://host.example/v1/messages#frag",
            "  https://host.example/v1/messages  ",
        ] {
            assert!(
                base_url_is_full_endpoint(base, "/v1/messages"),
                "expected full-endpoint match: {base:?}"
            );
        }
        assert_eq!(
            append_query_to_full_url("https://host.example/v1/messages", Some("x=1")),
            "https://host.example/v1/messages?x=1"
        );
        // A base URL that already carries its own query is preserved verbatim (no double
        // `/v1/messages`, query kept).
        assert_eq!(
            append_query_to_full_url("https://host.example/v1/messages?beta=true", None),
            "https://host.example/v1/messages?beta=true"
        );

        // A non-endpoint base (origin/prefix) must NOT match, so build_url still appends.
        assert!(!base_url_is_full_endpoint(
            "https://host.example",
            "/v1/messages"
        ));
        assert!(!base_url_is_full_endpoint(
            "https://host.example/v1",
            "/v1/messages"
        ));
        // The shared helper also backs the Chat path's `/chat/completions` guard.
        assert!(base_url_is_full_endpoint(
            "https://host.example/v1/chat/completions?api-version=2024",
            "/chat/completions"
        ));
    }

    #[test]
    fn codex_client_fingerprint_headers_are_dropped_for_anthropic_upstreams() {
        // Codex/OpenAI fingerprints a native Claude Code client never sends → must drop.
        for header in [
            "originator",
            "session_id",
            "session-id",
            "thread-id",
            "conversation_id",
            "chatgpt-account-id",
            "x-openai-subagent",
            "x-client-request-id",
            "x-codex-window-id",
            "openai-beta",
            "openai-organization",
            "openai-project",
            "x-stainless-lang",
            "x-stainless-runtime",
            "x-codex-turn-id",
        ] {
            assert!(
                is_codex_client_fingerprint_header(header),
                "expected {header} to be dropped while impersonating Claude Code"
            );
        }

        // Headers a real Claude Code client sends (or that the forwarder rebuilds) must
        // NOT be caught by the denylist.
        for header in [
            "anthropic-version",
            "anthropic-beta",
            "user-agent",
            "accept",
            "content-type",
            "x-app",
        ] {
            assert!(
                !is_codex_client_fingerprint_header(header),
                "{header} must be preserved while impersonating Claude Code"
            );
        }
    }

    #[test]
    fn codex_anthropic_2xx_error_envelope_is_detected_for_failover() {
        let body = br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        assert_eq!(
            codex_anthropic_error_envelope_message(body).as_deref(),
            Some("overloaded_error: busy")
        );
        assert!(
            codex_anthropic_error_envelope_message(br#"{"type":"message","content":[]}"#).is_none()
        );
    }

    #[test]
    fn responses_2xx_failure_is_detected_for_failover() {
        assert_eq!(
            responses_error_envelope_message(
                br#"{"status":"failed","error":{"type":"server_error","message":"busy"},"output":[]}"#
            )
            .as_deref(),
            Some("server_error: busy")
        );
        assert_eq!(
            responses_error_envelope_message(br#"{"status":"cancelled","output":[]}"#).as_deref(),
            Some("cancelled: response generation was cancelled")
        );
        assert_eq!(
            responses_error_envelope_message(
                br#"{"type":"response.failed","response":{"status":"failed","error":{"code":"too_many_requests","message":"quota exhausted"}}}"#
            )
            .as_deref(),
            Some("too_many_requests: quota exhausted")
        );
        assert_eq!(
            responses_error_envelope_message(
                br#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota exhausted"}}"#
            )
            .as_deref(),
            Some("RESOURCE_EXHAUSTED (429): quota exhausted")
        );
        assert!(responses_error_envelope_message(
            br#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}"#
        )
        .is_none());
        assert!(responses_error_envelope_message(
            br#"{"status":"completed","error":null,"output":[]}"#
        )
        .is_none());
    }

    #[test]
    fn responses_stream_start_semantic_failure_is_retryable() {
        let created = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}"
        );
        assert!(inspect_responses_start_event(created).is_none());

        let failed = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}}"
        );
        assert!(matches!(
            inspect_responses_start_event(failed),
            Some(Err(ProxyError::TransformError(message))) if message.contains("boom")
        ));

        let anthropic_error = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"capacity unavailable\"}}"
        );
        assert!(matches!(
            inspect_responses_start_event(anthropic_error),
            Some(Err(ProxyError::TransformError(message))) if message.contains("capacity unavailable")
        ));

        let delta = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}"
        );
        assert!(matches!(inspect_responses_start_event(delta), Some(Ok(()))));

        let empty_item_added = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"content\":[]}}"
        );
        assert!(inspect_responses_start_event(empty_item_added).is_none());

        let empty_part_added = concat!(
            "event: response.content_part.added\n",
            "data: {\"type\":\"response.content_part.added\",\"part\":{\"type\":\"output_text\",\"text\":\"\"}}"
        );
        assert!(inspect_responses_start_event(empty_part_added).is_none());

        let tool_item_added = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"lookup\",\"arguments\":\"{}\"}}"
        );
        assert!(matches!(
            inspect_responses_start_event(tool_item_added),
            Some(Ok(()))
        ));

        let unknown_lifecycle = concat!(
            "event: response.vendor_heartbeat\n",
            "data: {\"type\":\"response.vendor_heartbeat\",\"status\":\"in_progress\"}"
        );
        assert!(inspect_responses_start_event(unknown_lifecycle).is_none());

        let content_block_stop = concat!(
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}"
        );
        assert!(inspect_responses_start_event(content_block_stop).is_none());
    }

    #[test]
    fn responses_stream_start_accepts_unlabelled_whole_json() {
        assert!(matches!(
            inspect_responses_json_document(
                r#"{
                    "status": "completed",

                    "output": []
                }"#
            ),
            Some(Ok(()))
        ));
        assert!(inspect_responses_json_document(r#"{"status":"completed""#).is_none());

        let failed = inspect_responses_json_document(
            r#"{"status":"failed","error":{"message":"backend unavailable"}}"#,
        );
        assert!(
            matches!(failed, Some(Err(ProxyError::TransformError(message))) if message.contains("backend unavailable"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_one_byte_fragmented_json_parses_once_when_document_closes() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let chunks = br#"{"status":"completed","output":[]}"#
            .iter()
            .map(|byte| Ok::<_, std::io::Error>(Bytes::copy_from_slice(&[*byte])))
            .collect::<Vec<_>>();
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::iter(chunks),
        );

        reset_responses_json_document_parse_count();
        let replayed = forwarder
            .validate_responses_stream_start(response, forwarder.pre_output_deadline(true))
            .await
            .expect("complete fragmented JSON should be committed")
            .bytes()
            .await
            .expect("fragmented JSON replay bytes");

        assert_eq!(
            replayed,
            Bytes::from_static(br#"{"status":"completed","output":[]}"#)
        );
        assert_eq!(responses_json_document_parse_count(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn responses_one_byte_fragmented_incomplete_json_is_never_parsed() {
        let forwarder = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        let chunks = br#"{"status":"completed","output":["#
            .iter()
            .map(|byte| Ok::<_, std::io::Error>(Bytes::copy_from_slice(&[*byte])))
            .collect::<Vec<_>>();
        let response = ProxyResponse::streamed(
            StatusCode::OK,
            HeaderMap::from_iter([(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            futures::stream::iter(chunks),
        );

        reset_responses_json_document_parse_count();
        let result = forwarder
            .validate_responses_stream_start(response, forwarder.pre_output_deadline(true))
            .await;

        assert!(matches!(
            result,
            Err(ProxyError::ForwardFailed(message)) if message.contains("ended before producing output")
        ));
        assert_eq!(responses_json_document_parse_count(), 0);
    }

    #[test]
    fn codex_anthropic_cache_is_default_on_but_honors_sub_switch() {
        let default = codex_anthropic_cache_config(&OptimizerConfig::default());
        assert!(default.enabled);
        assert!(default.cache_injection);

        let disabled = codex_anthropic_cache_config(&OptimizerConfig {
            cache_injection: false,
            ..OptimizerConfig::default()
        });
        assert!(disabled.enabled);
        assert!(!disabled.cache_injection);
    }

    #[test]
    fn invalid_client_history_is_not_retryable() {
        let forwarder = test_forwarder(Duration::ZERO, Duration::ZERO);
        let provider = test_provider_with_type(None);
        assert_eq!(
            forwarder.categorize_proxy_error(
                &ProxyError::InvalidRequest("invalid historical tool arguments".to_string()),
                &provider,
            ),
            ErrorCategory::NonRetryable
        );
    }

    #[test]
    fn official_codex_auth_failures_are_not_retryable() {
        let forwarder = test_forwarder(Duration::ZERO, Duration::ZERO);
        let mut provider = test_provider_with_type(None);
        provider.id = "codex-official".to_string();
        provider.category = Some("official".to_string());

        for error in [
            ProxyError::AuthError("restart Codex".to_string()),
            ProxyError::UpstreamError {
                status: 401,
                body: None,
            },
            ProxyError::UpstreamError {
                status: 403,
                body: None,
            },
        ] {
            assert_eq!(
                forwarder.categorize_proxy_error(&error, &provider),
                ErrorCategory::NonRetryable
            );
        }
    }

    #[test]
    fn xai_oauth_token_auth_failures_are_not_retryable() {
        let forwarder = test_forwarder(Duration::ZERO, Duration::ZERO);
        let provider = test_provider_with_type(Some("xai_oauth"));

        // 本地取 token 失败 = 账号级问题（需重新登录），failover 无济于事
        assert_eq!(
            forwarder.categorize_proxy_error(
                &ProxyError::AuthError("xAI OAuth 认证失败".to_string()),
                &provider,
            ),
            ErrorCategory::NonRetryable
        );
        // 上游 401/403 保持 Retryable：换 provider 可能持有可用的 key
        assert_eq!(
            forwarder.categorize_proxy_error(
                &ProxyError::UpstreamError {
                    status: 401,
                    body: None,
                },
                &provider,
            ),
            ErrorCategory::Retryable
        );
    }

    #[test]
    fn official_codex_rejects_stale_proxy_placeholder_with_restart_hint() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer PROXY_MANAGED"),
        );
        let error = validate_codex_official_authorization(&headers)
            .expect_err("stale placeholder must be rejected");
        assert!(matches!(error, ProxyError::AuthError(message) if message.contains("重启 Codex")));
    }

    #[test]
    fn rewrite_codex_responses_compact_endpoint_to_chat_preserves_query() {
        let (endpoint, passthrough_query) =
            rewrite_codex_responses_endpoint_to_chat("/v1/responses/compact?foo=bar");

        assert_eq!(endpoint, "/chat/completions?foo=bar");
        assert_eq!(passthrough_query.as_deref(), Some("foo=bar"));
    }

    #[test]
    fn rewrite_claude_transform_endpoint_uses_copilot_path() {
        let (endpoint, passthrough_query) = rewrite_claude_transform_endpoint(
            "/v1/messages?beta=true&x-id=1",
            "anthropic",
            true,
            &json!({ "model": "claude-sonnet-4-6" }),
        );

        assert_eq!(endpoint, "/chat/completions?x-id=1");
        assert_eq!(passthrough_query.as_deref(), Some("x-id=1"));
    }

    #[test]
    fn rewrite_claude_transform_endpoint_uses_copilot_responses_path() {
        let (endpoint, passthrough_query) = rewrite_claude_transform_endpoint(
            "/v1/messages?beta=true&x-id=1",
            "openai_responses",
            true,
            &json!({ "model": "gpt-5.4" }),
        );

        assert_eq!(endpoint, "/v1/responses?x-id=1");
        assert_eq!(passthrough_query.as_deref(), Some("x-id=1"));
    }

    #[test]
    fn rewrite_claude_transform_endpoint_maps_gemini_generate_content() {
        let (endpoint, passthrough_query) = rewrite_claude_transform_endpoint(
            "/v1/messages?beta=true&x-id=1",
            "gemini_native",
            false,
            &json!({ "model": "gemini-2.5-pro" }),
        );

        assert_eq!(
            endpoint,
            "/v1beta/models/gemini-2.5-pro:generateContent?x-id=1"
        );
        assert_eq!(passthrough_query.as_deref(), Some("x-id=1"));
    }

    /// Regression: body.model arriving as the resource-name form
    /// `models/gemini-2.5-pro` must not produce a doubled
    /// `/v1beta/models/models/...` path.
    #[test]
    fn rewrite_claude_transform_endpoint_strips_gemini_model_resource_prefix() {
        let (endpoint, _) = rewrite_claude_transform_endpoint(
            "/v1/messages",
            "gemini_native",
            false,
            &json!({ "model": "models/gemini-2.5-pro" }),
        );

        assert_eq!(endpoint, "/v1beta/models/gemini-2.5-pro:generateContent");
    }

    #[test]
    fn rewrite_claude_transform_endpoint_maps_gemini_streaming() {
        let (endpoint, passthrough_query) = rewrite_claude_transform_endpoint(
            "/v1/messages?beta=true",
            "gemini_native",
            false,
            &json!({ "model": "gemini-2.5-flash", "stream": true }),
        );

        assert_eq!(
            endpoint,
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(passthrough_query.as_deref(), Some("alt=sse"));
    }

    #[test]
    fn append_query_to_full_url_preserves_existing_query_string() {
        let url = append_query_to_full_url("https://relay.example/api?foo=bar", Some("x-id=1"));

        assert_eq!(url, "https://relay.example/api?foo=bar&x-id=1");
    }

    #[test]
    fn build_gemini_native_url_uses_origin_when_base_ends_with_v1beta() {
        let url = crate::proxy::gemini_url::build_gemini_native_url(
            "https://generativelanguage.googleapis.com/v1beta",
            "/v1beta/models/gemini-2.5-pro:generateContent",
        );

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }

    #[test]
    fn build_gemini_native_url_uses_origin_when_base_already_contains_models_prefix() {
        let url = crate::proxy::gemini_url::build_gemini_native_url(
            "https://generativelanguage.googleapis.com/v1beta/models",
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
        );

        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn resolve_gemini_native_url_keeps_opaque_full_url_as_is() {
        let url = crate::proxy::gemini_url::resolve_gemini_native_url(
            "https://relay.example/custom/generate-content",
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
            true,
        );

        assert_eq!(url, "https://relay.example/custom/generate-content?alt=sse");
    }

    #[test]
    fn force_identity_for_stream_flag_requests() {
        let headers = HeaderMap::new();

        assert!(should_force_identity_encoding(
            "/v1/responses",
            &json!({ "stream": true }),
            &headers
        ));
    }

    #[test]
    fn force_identity_for_gemini_stream_endpoints() {
        let headers = HeaderMap::new();

        assert!(should_force_identity_encoding(
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            &json!({ "model": "gemini-2.5-pro" }),
            &headers
        ));
    }

    #[test]
    fn streaming_request_detects_gemini_sse_without_body_stream_flag() {
        let headers = HeaderMap::new();

        assert!(is_streaming_request(
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            &json!({ "model": "gemini-2.5-pro" }),
            &headers
        ));
    }

    #[test]
    fn retry_log_model_reads_gemini_native_endpoint() {
        assert_eq!(
            retry_log_model(
                &AppType::Gemini,
                "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
                &json!({})
            ),
            "gemini-2.5-pro"
        );
    }

    #[test]
    fn force_identity_for_sse_accept_header() {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

        assert!(should_force_identity_encoding(
            "/v1/responses",
            &json!({ "model": "gpt-5" }),
            &headers
        ));
    }

    #[test]
    fn force_identity_for_mixed_case_sse_accept_header() {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("Text/Event-Stream"));

        assert!(is_streaming_request(
            "/v1/responses",
            &json!({ "model": "gpt-5" }),
            &headers
        ));
        assert!(should_force_identity_encoding(
            "/v1/responses",
            &json!({ "model": "gpt-5" }),
            &headers
        ));
    }

    #[test]
    fn non_streaming_requests_allow_automatic_compression() {
        let headers = HeaderMap::new();

        assert!(!should_force_identity_encoding(
            "/v1/responses",
            &json!({ "model": "gpt-5" }),
            &headers
        ));
    }

    // ==================== Copilot 动态 endpoint 路由相关测试 ====================

    /// 验证 is_copilot 检测逻辑：通过 provider_type 判断
    #[test]
    fn copilot_detection_via_provider_type() {
        use crate::provider::{Provider, ProviderMeta};

        let provider = Provider {
            id: "test".to_string(),
            name: "Test Copilot".to_string(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                provider_type: Some("github_copilot".to_string()),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };

        let is_copilot = provider
            .meta
            .as_ref()
            .and_then(|m| m.provider_type.as_deref())
            == Some("github_copilot");

        assert!(is_copilot, "应该通过 provider_type 检测为 Copilot");
    }

    /// 验证 is_copilot 检测逻辑：通过 base_url 判断
    #[test]
    fn copilot_detection_via_base_url() {
        let base_url = "https://api.githubcopilot.com";
        let is_copilot = base_url.contains("githubcopilot.com");
        assert!(is_copilot, "应该通过 base_url 检测为 Copilot");

        let non_copilot_url = "https://api.anthropic.com";
        let is_not_copilot = non_copilot_url.contains("githubcopilot.com");
        assert!(!is_not_copilot, "非 Copilot URL 不应被检测为 Copilot");
    }

    /// 验证企业版 endpoint（不包含 githubcopilot.com）场景下 is_copilot 仍然正确
    #[test]
    fn copilot_detection_for_enterprise_endpoint() {
        use crate::provider::{Provider, ProviderMeta};

        // 企业版场景：provider_type 是 github_copilot，但 base_url 可能是企业内部域名
        let provider = Provider {
            id: "enterprise".to_string(),
            name: "Enterprise Copilot".to_string(),
            settings_config: serde_json::json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                provider_type: Some("github_copilot".to_string()),
                ..Default::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        };

        let enterprise_base_url = "https://copilot-api.corp.example.com";

        // is_copilot 应该通过 provider_type 检测成功，即使 base_url 不包含 githubcopilot.com
        let is_copilot = provider
            .meta
            .as_ref()
            .and_then(|m| m.provider_type.as_deref())
            == Some("github_copilot")
            || enterprise_base_url.contains("githubcopilot.com");

        assert!(
            is_copilot,
            "企业版 Copilot 应该通过 provider_type 被正确检测"
        );
    }

    /// 验证动态 endpoint 替换条件
    #[test]
    fn dynamic_endpoint_replacement_conditions() {
        // 条件：is_copilot && !is_full_url
        let test_cases = [
            (true, false, true, "Copilot + 非 full_url 应该替换"),
            (true, true, false, "Copilot + full_url 不应替换"),
            (false, false, false, "非 Copilot 不应替换"),
            (false, true, false, "非 Copilot + full_url 不应替换"),
        ];

        for (is_copilot, is_full_url, should_replace, desc) in test_cases {
            let will_replace = is_copilot && !is_full_url;
            assert_eq!(will_replace, should_replace, "{desc}");
        }
    }

    // ===== P3: forwarder 层 media 开关回归测试 =====
    // 验证 gate 在 forwarder 这一层的"接线"，而非 media_sanitizer 纯函数本身。

    fn forwarder_with_rectifier(config: RectifierConfig) -> RequestForwarder {
        let mut fwd = test_forwarder(Duration::from_secs(1), Duration::from_secs(1));
        fwd.rectifier_config = config;
        fwd
    }

    fn provider_with_settings(settings_config: Value) -> Provider {
        let mut p = test_provider_with_type(Some("anthropic"));
        p.settings_config = settings_config;
        p
    }

    fn body_with_image(model: &str) -> Value {
        json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "abc" } }
                ]
            }]
        })
    }

    fn body_with_codex_input_image(model: &str) -> Value {
        json!({
            "model": model,
            "input": [{
                "role": "user",
                "content": [
                    { "type": "input_image", "image_url": "data:image/png;base64,abc" }
                ]
            }]
        })
    }

    fn body_with_codex_tool_output_image(stringified: bool) -> Value {
        let output = json!({
            "content": [{
                "type": "input_image",
                "image_url": "data:image/png;base64,TOOL_OUTPUT_SENTINEL"
            }]
        });
        json!({
            "model": "any-model",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": if stringified {
                    Value::String(output.to_string())
                } else {
                    output
                }
            }]
        })
    }

    fn body_with_stringified_chat_tool_image() -> Value {
        let content = json!({
            "content": [{
                "type": "image",
                "mimeType": "image/png",
                "data": "CHAT_TOOL_SENTINEL"
            }]
        })
        .to_string();
        json!({
            "model": "any-model",
            "messages": [{
                "role": "tool",
                "tool_call_id": "call_1",
                "content": content
            }]
        })
    }

    fn body_with_gemini_image() -> Value {
        json!({
            "contents": [{
                "role": "user",
                "parts": [{
                    "inlineData": {
                        "mimeType": "image/png",
                        "data": "GEMINI_SENTINEL"
                    }
                }]
            }]
        })
    }

    fn image_unsupported_error() -> ProxyError {
        ProxyError::UpstreamError {
            status: 400,
            body: Some(
                r#"{"error":{"message":"This model does not support image input"}}"#.to_string(),
            ),
        }
    }
    #[test]
    fn prevention_replaces_when_all_switches_on_and_model_in_heuristic_list() {
        let fwd = forwarder_with_rectifier(RectifierConfig::default());
        let provider = provider_with_settings(json!({}));
        let mut body = body_with_image("deepseek-v4-pro");

        let replaced = fwd.apply_media_prevention(&mut body, &provider);

        assert_eq!(replaced, 1, "默认全开 + 名单内模型应预替换");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn prevention_skipped_when_media_fallback_off() {
        // 关闭 request_media_fallback：即使名单命中也不预替换。
        let fwd = forwarder_with_rectifier(RectifierConfig {
            request_media_fallback: false,
            ..RectifierConfig::default()
        });
        let provider = provider_with_settings(json!({}));
        let mut body = body_with_image("deepseek-v4-pro");

        let replaced = fwd.apply_media_prevention(&mut body, &provider);

        assert_eq!(replaced, 0);
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
    }

    #[test]
    fn prevention_skipped_when_master_switch_off() {
        let fwd = forwarder_with_rectifier(RectifierConfig {
            enabled: false,
            ..RectifierConfig::default()
        });
        let provider = provider_with_settings(json!({}));
        let mut body = body_with_image("deepseek-v4-pro");

        assert_eq!(fwd.apply_media_prevention(&mut body, &provider), 0);
        assert_eq!(body["messages"][0]["content"][0]["type"], "image");
    }

    #[test]
    fn prevention_heuristic_off_skips_list_but_keeps_explicit_text_only() {
        // 关闭 request_media_heuristic：名单预测失效，但显式声明 text-only 仍预替换。
        let fwd = forwarder_with_rectifier(RectifierConfig {
            request_media_heuristic: false,
            ..RectifierConfig::default()
        });

        // (a) 名单内模型、无显式声明 → 不再预替换
        let bare_provider = provider_with_settings(json!({}));
        let mut list_body = body_with_image("deepseek-v4-pro");
        assert_eq!(
            fwd.apply_media_prevention(&mut list_body, &bare_provider),
            0,
            "heuristic 关闭后名单模型不应被预替换"
        );
        assert_eq!(list_body["messages"][0]["content"][0]["type"], "image");

        // (b) 显式声明 text-only → 仍预替换（声明驱动，不受 heuristic 开关影响）
        let declared_provider = provider_with_settings(json!({
            "models": [ { "id": "some-text-model", "input": ["text"] } ]
        }));
        let mut declared_body = body_with_image("some-text-model");
        assert_eq!(
            fwd.apply_media_prevention(&mut declared_body, &declared_provider),
            1,
            "显式 text-only 即使关闭 heuristic 也应预替换"
        );
        assert_eq!(declared_body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn reactive_triggers_when_all_switches_on() {
        let fwd = forwarder_with_rectifier(RectifierConfig::default());
        let body = body_with_image("any-model");
        assert!(fwd.media_retry_should_trigger("Claude", false, &body, &image_unsupported_error()));
    }

    #[test]
    fn reactive_triggers_for_codex_image_url_deserialize_errors() {
        let fwd = forwarder_with_rectifier(RectifierConfig::default());
        let body = body_with_codex_input_image("deepseek-v4-flash");
        let error = ProxyError::UpstreamError {
            status: 400,
            body: Some(
                r#"{"error":{"message":"Failed to deserialize the JSON body into the target type: messages[11]: unknown variant image_url, expected text"}}"#
                    .to_string(),
            ),
        };

        assert!(fwd.media_retry_should_trigger("Codex", false, &body, &error));
    }

    #[test]
    fn reactive_triggers_for_structured_and_stringified_codex_tool_images() {
        let fwd = forwarder_with_rectifier(RectifierConfig::default());

        for stringified in [false, true] {
            let body = body_with_codex_tool_output_image(stringified);
            assert!(
                fwd.media_retry_should_trigger("Codex", false, &body, &image_unsupported_error()),
                "tool-output image should trigger retry (stringified={stringified})"
            );
        }
    }

    #[test]
    fn reactive_triggers_for_chat_tool_and_gemini_images() {
        let fwd = forwarder_with_rectifier(RectifierConfig::default());

        assert!(fwd.media_retry_should_trigger(
            "Claude",
            false,
            &body_with_stringified_chat_tool_image(),
            &image_unsupported_error()
        ));
        assert!(fwd.media_retry_should_trigger(
            "Claude",
            false,
            &body_with_gemini_image(),
            &image_unsupported_error()
        ));
    }

    #[test]
    fn reactive_does_not_treat_context_limit_as_image_rejection() {
        let fwd = forwarder_with_rectifier(RectifierConfig::default());
        let body = body_with_codex_tool_output_image(false);
        let context_error = ProxyError::UpstreamError {
            status: 400,
            body: Some(r#"{"error":{"message":"maximum context length exceeded"}}"#.to_string()),
        };

        assert!(!fwd.media_retry_should_trigger("Codex", false, &body, &context_error));
    }

    #[test]
    fn reactive_skipped_when_media_fallback_off() {
        // 关闭 request_media_fallback：上游报图片错误也不触发兜底重试。
        let fwd = forwarder_with_rectifier(RectifierConfig {
            request_media_fallback: false,
            ..RectifierConfig::default()
        });
        let body = body_with_image("any-model");
        assert!(!fwd.media_retry_should_trigger(
            "Claude",
            false,
            &body,
            &image_unsupported_error()
        ));
    }

    #[test]
    fn reactive_skipped_when_master_switch_off() {
        let fwd = forwarder_with_rectifier(RectifierConfig {
            enabled: false,
            ..RectifierConfig::default()
        });
        let body = body_with_image("any-model");
        assert!(!fwd.media_retry_should_trigger(
            "Claude",
            false,
            &body,
            &image_unsupported_error()
        ));
    }

    #[test]
    fn reactive_unaffected_by_heuristic_switch() {
        // 关闭 request_media_heuristic 不影响反应式兜底——它是上游实测错误后的恢复，不是预测。
        let fwd = forwarder_with_rectifier(RectifierConfig {
            request_media_heuristic: false,
            ..RectifierConfig::default()
        });
        let body = body_with_image("any-model");
        assert!(fwd.media_retry_should_trigger("Claude", false, &body, &image_unsupported_error()));
    }
}
