//! Internal-only validation entry. No listener, headers granting authority, live
//! config, real database writes, provider rotation, or production runtime state.
use super::{
    failover_switch::FailoverSwitchManager,
    handlers,
    hyper_client::ProxyResponse,
    provider_router::ProviderRouter,
    providers::{codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore},
    server::ProxyState,
    types::{ProxyConfig, ProxyStatus},
};
use crate::{
    app_config::AppType, database::Database, error::AppError, provider::Provider, store::AppState,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Request},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::RwLock;

const MAX_DIAGNOSTIC_RESPONSE_BYTES: usize = 1_048_576;

#[derive(Clone)]
pub(super) struct DiagnosticContext {
    model: Arc<Mutex<Option<String>>>,
    output_limit: u64,
    controlled_temperature: Option<f64>,
    client: reqwest::Client,
}
impl DiagnosticContext {
    fn new(body: &Value) -> Result<Self, AppError> {
        let limit = [
            "/max_tokens",
            "/max_completion_tokens",
            "/max_output_tokens",
            "/generationConfig/maxOutputTokens",
        ]
        .iter()
        .filter_map(|path| body.pointer(path).and_then(Value::as_u64))
        .min()
        .filter(|limit| (1..=2048).contains(limit))
        .ok_or_else(|| {
            AppError::InvalidInput("Diagnostic requests need a bounded output token limit".into())
        })?;
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        let client = reqwest::Client::builder()
            .default_headers(default_headers)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(45))
            .pool_max_idle_per_host(0)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(|_| AppError::Config("Unable to initialize diagnostic transport".into()))?;
        Ok(Self {
            model: Arc::new(Mutex::new(None)),
            output_limit: limit,
            controlled_temperature: body
                .get("temperature")
                .or_else(|| body.pointer("/generationConfig/temperature"))
                .and_then(Value::as_f64),
            client,
        })
    }

    /// Called after protocol transforms and provider body overrides, immediately
    /// before the forwarder serializes the final upstream request.
    pub(super) fn enforce_output_limits(&self, body: &mut Value) -> Result<(), super::ProxyError> {
        let mut found_limit = false;
        for path in [
            "/max_tokens",
            "/max_completion_tokens",
            "/max_output_tokens",
            "/generationConfig/maxOutputTokens",
        ] {
            if let Some(value) = body.pointer_mut(path) {
                let limit = value.as_u64().filter(|limit| *limit > 0).ok_or_else(|| {
                    super::ProxyError::InvalidRequest(
                        "Diagnostic output limit became invalid after conversion".into(),
                    )
                })?;
                *value = Value::from(limit.min(self.output_limit));
                found_limit = true;
            }
        }
        if !found_limit {
            return Err(super::ProxyError::InvalidRequest(
                "Diagnostic output limit was removed by conversion".into(),
            ));
        }
        if let Some(n) = body.get_mut("n") {
            *n = Value::from(1);
        }
        if let Some(generation) = body
            .get_mut("generationConfig")
            .and_then(Value::as_object_mut)
        {
            generation.insert("candidateCount".into(), Value::from(1));
        }
        if let Some(expected) = self.controlled_temperature {
            let temperatures: Vec<_> = ["/temperature", "/generationConfig/temperature"]
                .iter()
                .filter_map(|path| body.pointer(path))
                .collect();
            if temperatures.is_empty()
                || temperatures
                    .iter()
                    .any(|value| value.as_f64() != Some(expected))
            {
                return Err(super::ProxyError::InvalidRequest(
                    "Diagnostic controlled temperature was changed or removed by conversion".into(),
                ));
            }
        }
        if serde_json::to_vec(body)
            .map_err(|_| super::ProxyError::InvalidRequest("Invalid diagnostic body".into()))?
            .len()
            > 262_144
        {
            return Err(super::ProxyError::InvalidRequest(
                "Diagnostic request exceeds the input bound".into(),
            ));
        }
        Ok(())
    }

    /// The diagnostic branch must not inherit production proxy settings,
    /// transparent redirects, automatic HTTP retries, or a 24-hour stream timeout.
    pub(super) fn client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// Apply before any response converter buffers the upstream body. Limiting
    /// only the converted response would let oversized discarded metadata evade
    /// the diagnostic budget. This wrapper preserves streaming/TTFC timing.
    pub(super) fn limit_response(
        &self,
        response: ProxyResponse,
    ) -> Result<ProxyResponse, super::ProxyError> {
        let status = response.status();
        let headers = response.headers().clone();
        if headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|size| size > MAX_DIAGNOSTIC_RESPONSE_BYTES as u64)
        {
            return Err(super::ProxyError::ResponseBodyTooLarge(
                MAX_DIAGNOSTIC_RESPONSE_BYTES,
            ));
        }
        // The diagnostic client explicitly requests identity. Reject an
        // unsolicited compressed body before the generic (128 MiB) decoder;
        // diagnostic tests must never pay that memory budget for a gzip bomb.
        if headers
            .get_all(http::header::CONTENT_ENCODING)
            .iter()
            .any(|value| {
                !value.to_str().is_ok_and(|value| {
                    value.trim().is_empty() || value.trim().eq_ignore_ascii_case("identity")
                })
            })
        {
            return Err(super::ProxyError::ForwardFailed(
                "Diagnostic transport requires an identity-encoded response".into(),
            ));
        }
        let source = Box::pin(response.bytes_stream());
        let bounded =
            futures::stream::try_unfold((source, 0usize), |(mut source, used)| async move {
                let Some(chunk) = source.next().await else {
                    return Ok(None);
                };
                let chunk =
                    chunk.map_err(|_| std::io::Error::other("Diagnostic response body failed"))?;
                let used = used
                    .checked_add(chunk.len())
                    .filter(|used| *used <= MAX_DIAGNOSTIC_RESPONSE_BYTES)
                    .ok_or_else(|| {
                        std::io::Error::other("Diagnostic response exceeds the 1 MiB limit")
                    })?;
                Ok(Some((chunk, (source, used))))
            });
        Ok(ProxyResponse::streamed(status, headers, bounded))
    }

    pub(super) fn record_model(&self, model: Option<String>) {
        if let Ok(mut value) = self.model.lock() {
            *value = model;
        }
    }
}

/// Production overrides may contain real prompts, conversation IDs, remote
/// tools, or persistent-resource options. Only generation controls can be
/// inherited by a synthetic diagnostic; never silently strip content and then
/// present the result as testing the user's exact configuration.
pub(crate) fn validate_provider_config(
    app: &AppType,
    provider: &Provider,
) -> Result<(String, String), AppError> {
    // CodexAdapter also understands Grok TOML and can otherwise expand env_key.
    // Reject that fallback before asking the adapter to resolve any credentials.
    if provider
        .settings_config
        .get("config")
        .and_then(Value::as_str)
        .and_then(crate::grok_config::extract_model_config)
        .is_some_and(|config| config.api_key.is_none() && config.env_key.is_some())
    {
        return Err(AppError::InvalidInput(
            "链路检测不展开环境变量凭据；请配置显式 API Key 或使用直连检测".into(),
        ));
    }
    let body = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.local_proxy_request_overrides.as_ref())
        .and_then(|overrides| overrides.body.as_ref());
    if let Some(body) = body {
        if !body.as_object().is_some_and(|fields| {
            fields
                .iter()
                .all(|(name, value)| safe_generation_override(name, value))
        }) {
            return Err(AppError::InvalidInput(
                "链路检测仅允许生成参数覆盖；当前 body override 可能引入非合成内容、工具或远端资源，请先移除相关覆盖或使用直连检测".into(),
            ));
        }
    }
    let adapter = super::providers::get_adapter(app)
        .ok_or_else(|| AppError::InvalidInput("该应用没有 ccs 转发适配".into()))?;
    let auth = adapter.extract_auth(provider).ok_or_else(|| {
        AppError::InvalidInput("ccs 转发适配未解析出显式 API Key；请修正配置或使用直连".into())
    })?;
    if auth.access_token.is_some()
        || !matches!(
            auth.strategy,
            super::providers::AuthStrategy::Anthropic
                | super::providers::AuthStrategy::Bearer
                | super::providers::AuthStrategy::ClaudeAuth
                | super::providers::AuthStrategy::Google
        )
    {
        return Err(AppError::InvalidInput(
            "Subscription/OAuth validation is not supported".into(),
        ));
    }
    let endpoint = adapter.extract_base_url(provider).map_err(|_| {
        AppError::InvalidInput("ccs 转发适配未解析出端点；请修正配置或使用直连".into())
    })?;
    // The tuple stays backend-only and is never serialized or included in errors.
    Ok((endpoint, auth.api_key))
}

fn safe_generation_override(name: &str, value: &Value) -> bool {
    match name {
        "max_tokens"
        | "max_completion_tokens"
        | "max_output_tokens"
        | "n"
        | "temperature"
        | "top_p"
        | "top_k"
        | "seed"
        | "presence_penalty"
        | "frequency_penalty"
        | "top_logprobs" => value.is_number(),
        "parallel_tool_calls" | "logprobs" | "stream" => value.is_boolean(),
        "store" => value == &Value::Bool(false),
        "model" => value.as_str().is_some_and(|model| {
            !model.is_empty() && model.len() <= 256 && !model.chars().any(char::is_control)
        }),
        "reasoning_effort" => value.as_str().is_some_and(|value| {
            matches!(
                value,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
            )
        }),
        "reasoning" => value.as_object().is_some_and(|fields| {
            fields.iter().all(|(name, value)| match name.as_str() {
                "effort" => safe_generation_override("reasoning_effort", value),
                "summary" => matches!(value.as_str(), Some("auto" | "concise" | "detailed")),
                _ => false,
            })
        }),
        "thinking" => value.as_object().is_some_and(|fields| {
            fields.iter().all(|(name, value)| match name.as_str() {
                "type" => matches!(value.as_str(), Some("enabled" | "disabled" | "adaptive")),
                "budget_tokens" => value.is_number(),
                _ => false,
            })
        }),
        "generationConfig" => value.as_object().is_some_and(|fields| {
            fields.iter().all(|(name, value)| match name.as_str() {
                "temperature" | "topP" | "topK" | "seed" | "maxOutputTokens" | "candidateCount"
                | "presencePenalty" | "frequencyPenalty" => value.is_number(),
                "thinkingConfig" => value.as_object().is_some_and(|thinking| {
                    thinking.iter().all(|(name, value)| match name.as_str() {
                        "thinkingBudget" => value.is_number(),
                        "includeThoughts" => value.is_boolean(),
                        "thinkingLevel" => {
                            matches!(value.as_str(), Some("minimal" | "low" | "medium" | "high"))
                        }
                        _ => false,
                    })
                }),
                _ => false,
            })
        }),
        _ => false,
    }
}

pub(crate) async fn forward_validation(
    production: &AppState,
    app: AppType,
    mut provider: Provider,
    _model: &str,
    endpoint: &str,
    mut body: Value,
) -> Result<Response, AppError> {
    if provider.uses_managed_account_auth()
        || provider.uses_proxy_injected_oauth()
        || app == AppType::Codex && super::providers::is_codex_official_provider(&provider)
    {
        return Err(AppError::InvalidInput(
            "Subscription/OAuth validation is not supported".into(),
        ));
    }
    if !matches!(
        app,
        AppType::Claude
            | AppType::ClaudeDesktop
            | AppType::Codex
            | AppType::Gemini
            | AppType::GrokBuild
    ) {
        return Err(AppError::InvalidInput(
            "This application has no ccs forwarding adapter".into(),
        ));
    }
    if !endpoint.starts_with('/') || endpoint.starts_with("//") || endpoint.contains(['\r', '\n']) {
        return Err(AppError::InvalidInput(
            "Invalid diagnostic protocol endpoint".into(),
        ));
    }
    validate_provider_config(&app, &provider)?;
    let context = DiagnosticContext::new(&body)?;
    if app == AppType::Gemini {
        body.as_object_mut()
            .ok_or_else(|| AppError::InvalidInput("Invalid diagnostic body".into()))?
            .remove("stream");
    }
    // Pool membership is authoritative in the real DB, not in this private
    // single-target database. Do not clone folders or resolve another member.
    if let Some(meta) = provider.meta.as_mut() {
        meta.provider_group_id = None;
        meta.key_pool_enabled = Some(false);
    }
    provider.in_failover_queue = false;
    let db = Arc::new(Database::diagnostic_memory()?);
    db.save_provider(app.as_str(), &provider)?;
    db.set_current_provider(app.as_str(), &provider.id)?;
    let mut app_config = production.db.get_proxy_config_for_app(app.as_str()).await?;
    app_config.enabled = false;
    app_config.auto_failover_enabled = false;
    app_config.max_retries = 0;
    db.update_proxy_config_for_app(app_config).await?;
    db.set_rectifier_config(&production.db.get_rectifier_config()?)?;
    db.set_optimizer_config(&production.db.get_optimizer_config()?)?;
    db.set_copilot_optimizer_config(&production.db.get_copilot_optimizer_config()?)?;
    let state = ProxyState {
        db: db.clone(),
        config: Arc::new(RwLock::new(ProxyConfig {
            enable_logging: false,
            ..Default::default()
        })),
        status: Arc::new(RwLock::new(ProxyStatus::default())),
        start_time: Arc::new(RwLock::new(None)),
        current_providers: Arc::new(RwLock::new(Default::default())),
        provider_router: Arc::new(ProviderRouter::new(db.clone())),
        gemini_shadow: Arc::new(GeminiShadowStore::default()),
        codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
        app_handle: None,
        failover_manager: Arc::new(FailoverSwitchManager::new(db)),
    };
    let mut request = Request::builder()
        .method("POST")
        .uri(endpoint)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(
            serde_json::to_vec(&body).map_err(|source| AppError::JsonSerialize { source })?,
        ))
        .map_err(|_| AppError::InvalidInput("Invalid validation request".into()))?;
    request.extensions_mut().insert(context.clone());
    let result = match app {
        AppType::Claude => handlers::handle_messages(State(state), request).await,
        AppType::ClaudeDesktop => {
            handlers::handle_messages_for_app(
                state,
                request,
                AppType::ClaudeDesktop,
                "Validation",
                "claude-desktop",
                None,
            )
            .await
        }
        AppType::Codex if endpoint.contains("chat/completions") => {
            handlers::handle_chat_completions(State(state), request).await
        }
        AppType::Codex => handlers::handle_responses(State(state), request).await,
        AppType::GrokBuild => handlers::handle_grokbuild_responses(State(state), request).await,
        AppType::Gemini => {
            handlers::handle_gemini(State(state), request.uri().clone(), request).await
        }
        _ => unreachable!(),
    };
    let mut response = result.unwrap_or_else(|error| error.into_response());
    let outbound = context
        .model
        .lock()
        .map_err(|_| AppError::Config("Diagnostic capture failed".into()))?
        .clone();
    if let Some(value) = outbound.as_deref() {
        if let Ok(value) = HeaderValue::from_str(value) {
            response
                .headers_mut()
                .insert("x-ccs-validation-upstream-model", value);
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests;
