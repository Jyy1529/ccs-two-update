use crate::{
    app_config::AppType,
    provider::{LocalProxyRetryErrorType, Provider},
    proxy::error::ProxyError,
};
use serde_json::Value;
use std::collections::HashSet;

const MAX_RETRIES: u32 = 100;
const MIN_RETRY_DELAY_MS: u64 = 1;
const MAX_RETRY_DELAY_MS: u64 = 60_000;
const AUTO_REVIEW_MODEL: &str = "codex-auto-review";

#[derive(Debug, Clone)]
pub(crate) struct ResolvedRetryPolicy {
    retry_limit: RetryLimit,
    retry_delay_ms: u64,
    custom_messages: Vec<String>,
    error_types: Vec<LocalProxyRetryErrorType>,
}

#[derive(Debug, Clone, Copy)]
enum RetryLimit {
    Unlimited,
    Finite(usize),
}

impl ResolvedRetryPolicy {
    #[cfg(test)]
    pub(crate) fn max_retries(&self) -> usize {
        match self.retry_limit {
            RetryLimit::Unlimited => 0,
            RetryLimit::Finite(max_retries) => max_retries,
        }
    }

    #[cfg(test)]
    fn is_unlimited(&self) -> bool {
        matches!(self.retry_limit, RetryLimit::Unlimited)
    }

    pub(crate) fn allows_retry(&self, completed_retries: usize) -> bool {
        match self.retry_limit {
            RetryLimit::Unlimited => true,
            RetryLimit::Finite(max_retries) => completed_retries < max_retries,
        }
    }

    pub(crate) fn retry_limit_label(&self) -> String {
        match self.retry_limit {
            RetryLimit::Unlimited => "unlimited".to_string(),
            RetryLimit::Finite(max_retries) => max_retries.to_string(),
        }
    }

    pub(crate) fn should_log_attempt(&self, retry_attempt: usize) -> bool {
        match self.retry_limit {
            RetryLimit::Finite(_) => true,
            RetryLimit::Unlimited => retry_attempt <= 3 || retry_attempt % 100 == 0,
        }
    }

    pub(crate) fn retry_delay_ms(&self) -> u64 {
        self.retry_delay_ms
    }

    #[cfg(test)]
    fn custom_messages(&self) -> &[String] {
        &self.custom_messages
    }

    pub(crate) fn match_error(&self, error: &ProxyError) -> Option<String> {
        let facts = ErrorFacts::from_proxy_error(error);

        if !facts.message.is_empty()
            && self
                .custom_messages
                .iter()
                .any(|needle| facts.message.contains(needle))
        {
            return Some("custom_message".to_string());
        }

        if self
            .error_types
            .contains(&LocalProxyRetryErrorType::RateLimit)
            && (facts.status == Some(429)
                || contains_any(
                    &facts.classification_text,
                    &[
                        "rate_limit",
                        "rate limit",
                        "too_many_requests",
                        "resource_exhausted",
                    ],
                ))
        {
            return Some("rate_limit".to_string());
        }

        if self
            .error_types
            .contains(&LocalProxyRetryErrorType::Overloaded)
            && (facts.status == Some(503)
                || contains_any(
                    &facts.classification_text,
                    &[
                        "overloaded",
                        "service_unavailable",
                        "service unavailable",
                        "unavailable",
                    ],
                ))
        {
            return Some("overloaded".to_string());
        }

        if self
            .error_types
            .contains(&LocalProxyRetryErrorType::ServerError)
            && (facts
                .status
                .is_some_and(|status| (500..=599).contains(&status) && status != 503)
                || contains_any(
                    &facts.classification_text,
                    &[
                        "server_error",
                        "internal_error",
                        "internal server error",
                        "bad_gateway",
                        "bad gateway",
                        "gateway_timeout",
                        "gateway timeout",
                    ],
                ))
        {
            return Some("server_error".to_string());
        }

        if self
            .error_types
            .contains(&LocalProxyRetryErrorType::Network)
            && (facts.network || facts.status == Some(408))
        {
            return Some("network".to_string());
        }

        None
    }
}

#[cfg(test)]
pub(crate) fn resolve_retry_policy(
    app_type: &AppType,
    body: &Value,
    provider: &Provider,
) -> Option<ResolvedRetryPolicy> {
    resolve_retry_policy_with_global(app_type, body, provider, true)
}

pub(crate) fn resolve_retry_policy_with_global(
    app_type: &AppType,
    body: &Value,
    provider: &Provider,
    global_enabled: bool,
) -> Option<ResolvedRetryPolicy> {
    if !global_enabled {
        return None;
    }
    if !matches!(
        app_type,
        AppType::Claude | AppType::Codex | AppType::Gemini | AppType::GrokBuild
    ) {
        return None;
    }
    if matches!(app_type, AppType::Codex)
        && body.get("model").and_then(Value::as_str) == Some(AUTO_REVIEW_MODEL)
    {
        return None;
    }

    let raw = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.local_proxy_retry_policy.as_ref())?;
    let provider_enabled = raw.enabled.unwrap_or(raw.max_retries > 0);
    if !provider_enabled {
        return None;
    }
    let retry_limit = if raw.enabled == Some(true) && raw.max_retries == 0 {
        RetryLimit::Unlimited
    } else {
        let max_retries = raw.max_retries.min(MAX_RETRIES) as usize;
        if max_retries == 0 {
            return None;
        }
        RetryLimit::Finite(max_retries)
    };

    let mut seen_messages = HashSet::new();
    let custom_messages = raw
        .custom_messages
        .iter()
        .map(|message| message.trim().to_lowercase())
        .filter(|message| !message.is_empty())
        .filter(|message| seen_messages.insert(message.clone()))
        .collect::<Vec<_>>();

    let mut error_types = Vec::new();
    for error_type in &raw.error_types {
        if !error_types.contains(error_type) {
            error_types.push(*error_type);
        }
    }
    if custom_messages.is_empty() && error_types.is_empty() {
        return None;
    }

    Some(ResolvedRetryPolicy {
        retry_limit,
        retry_delay_ms: raw
            .retry_delay_ms
            .clamp(MIN_RETRY_DELAY_MS, MAX_RETRY_DELAY_MS),
        custom_messages,
        error_types,
    })
}

struct ErrorFacts {
    status: Option<u16>,
    message: String,
    classification_text: String,
    network: bool,
}

impl ErrorFacts {
    fn from_proxy_error(error: &ProxyError) -> Self {
        match error {
            ProxyError::UpstreamError { status, body } => {
                let (message, classification_text) = body
                    .as_deref()
                    .map(error_text_from_body)
                    .unwrap_or_default();
                Self {
                    status: Some(*status),
                    message,
                    classification_text,
                    network: false,
                }
            }
            ProxyError::UpstreamBodyTimeout {
                status,
                timeout_seconds,
            } => {
                let message = format!(
                    "upstream http {status} response body timed out after {timeout_seconds}s"
                );
                Self {
                    status: Some(*status),
                    message: message.clone(),
                    classification_text: message,
                    network: true,
                }
            }
            ProxyError::Timeout(message) | ProxyError::ForwardFailed(message) => Self {
                status: None,
                message: message.to_lowercase(),
                classification_text: message.to_lowercase(),
                network: matches!(error, ProxyError::Timeout(_))
                    || is_network_forward_failure(message),
            },
            ProxyError::StreamIdleTimeout(seconds) => {
                let message = format!("stream idle timeout: {seconds}");
                Self {
                    status: None,
                    message: message.clone(),
                    classification_text: message,
                    network: true,
                }
            }
            ProxyError::TransformError(message) => Self {
                status: None,
                message: message.to_lowercase(),
                classification_text: message.to_lowercase(),
                network: false,
            },
            other => {
                let message = other.to_string().to_lowercase();
                Self {
                    status: None,
                    message: message.clone(),
                    classification_text: message,
                    network: false,
                }
            }
        }
    }
}

fn error_text_from_body(body: &str) -> (String, String) {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        let text = body.to_lowercase();
        return (text.clone(), text);
    };

    let candidate = value
        .get("error")
        .filter(|error| !error.is_null())
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
                .filter(|error| !error.is_null())
        })
        .unwrap_or(&value);

    let mut messages = Vec::new();
    let mut classifications = Vec::new();
    collect_error_fields(candidate, &mut messages, &mut classifications);
    if candidate != &value {
        collect_classification_fields(&value, &mut classifications);
    }
    (
        messages.join(" ").to_lowercase(),
        classifications.join(" ").to_lowercase(),
    )
}

fn collect_error_fields(value: &Value, messages: &mut Vec<String>, classes: &mut Vec<String>) {
    match value {
        Value::String(value) => {
            messages.push(value.clone());
            classes.push(value.clone());
        }
        Value::Number(value) => push_numeric_classification(value, classes),
        Value::Array(values) => {
            for value in values {
                collect_error_fields(value, messages, classes);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "message" | "detail" | "error_description" | "errorMessage" => {
                        if let Some(value) = value.as_str() {
                            messages.push(value.to_string());
                        } else {
                            collect_error_fields(value, messages, classes);
                        }
                    }
                    "type" | "code" | "status" | "reason" => {
                        if let Some(value) = value.as_str() {
                            classes.push(value.to_string());
                        } else if let Some(value) = value.as_number() {
                            push_numeric_classification(value, classes);
                        }
                    }
                    "error" | "errors" | "details" | "cause" | "causes" | "inner"
                    | "inner_error" | "innerError" => {
                        collect_error_fields(value, messages, classes)
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn collect_classification_fields(value: &Value, classes: &mut Vec<String>) {
    if let Some(object) = value.as_object() {
        for key in ["type", "code", "status", "reason"] {
            if let Some(value) = object.get(key) {
                if let Some(value) = value.as_str() {
                    classes.push(value.to_string());
                } else if let Some(value) = value.as_number() {
                    push_numeric_classification(value, classes);
                }
            }
        }
    }
}

fn push_numeric_classification(value: &serde_json::Number, classes: &mut Vec<String>) {
    let Some(code) = value.as_u64().and_then(|code| u16::try_from(code).ok()) else {
        classes.push(value.to_string());
        return;
    };
    classes.push(code.to_string());
    match code {
        429 => classes.push("rate_limit".to_string()),
        503 => classes.push("overloaded".to_string()),
        500..=599 => classes.push("server_error".to_string()),
        _ => {}
    }
}

fn contains_any(value: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| value.contains(pattern))
}

fn is_network_forward_failure(message: &str) -> bool {
    let message = message.to_lowercase();
    if contains_any(
        &message,
        &[
            "invalid url",
            "invalid proxy url",
            "uri has no host",
            "invalid server name",
            "invalid proxy server name",
            "failed to build request",
            "build dummy request",
        ],
    ) {
        return false;
    }

    contains_any(
        &message,
        &[
            "连接失败",
            "connection refused",
            "connection reset",
            "connection closed",
            "connect failed",
            "tcp connect failed",
            "dns",
            "name resolution",
            "socket",
            "timed out",
            "timeout",
            "failed to read response body",
            "读取流式响应首包失败",
            "failed while validating responses stream start",
            "stream ended before producing output",
            "流式响应在首包到达前结束",
            "response parse failed",
            "unexpected eof",
            "broken pipe",
            "write failed",
            "flush failed",
            "read failed",
            "tls handshake failed",
            "handshake failed",
            "error sending request",
            "failed to send request",
            "上游请求失败",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app_config::AppType,
        provider::{
            LocalProxyRetryErrorType, LocalProxyRetryPolicy, Provider, ProviderMeta,
            DEFAULT_LOCAL_PROXY_RETRY_MESSAGE,
        },
        proxy::error::ProxyError,
    };
    use serde_json::json;

    fn provider(policy: LocalProxyRetryPolicy) -> Provider {
        Provider {
            id: "provider-1".to_string(),
            name: "Provider".to_string(),
            settings_config: json!({}),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                local_proxy_retry_policy: Some(policy),
                ..ProviderMeta::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    fn resolves_normal_request_policy_and_excludes_approval_model() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 101,
            retry_delay_ms: 99_999,
            custom_messages: vec!["  Busy  ".to_string(), "busy".to_string(), "".to_string()],
            error_types: vec![],
        });

        let policy = resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("normal Codex request should use provider policy");
        assert_eq!(policy.max_retries(), 100);
        assert!(policy.allows_retry(99));
        assert!(!policy.allows_retry(100));
        assert_eq!(policy.retry_limit_label(), "100");
        assert!(policy.should_log_attempt(100));
        assert_eq!(policy.retry_delay_ms(), 60_000);
        assert_eq!(policy.custom_messages(), &["busy"]);

        assert!(resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "codex-auto-review" }),
            &provider,
        )
        .is_none());
        assert!(resolve_retry_policy(
            &AppType::OpenCode,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .is_none());
    }

    #[test]
    fn custom_message_matching_reads_error_fields_only() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 1_000,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![],
        });
        let policy = resolve_retry_policy(
            &AppType::Claude,
            &json!({ "model": "claude-sonnet" }),
            &provider,
        )
        .expect("policy");

        let matching = ProxyError::UpstreamError {
            status: 400,
            body: Some(
                json!({
                    "error": { "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE }
                })
                .to_string(),
            ),
        };
        assert_eq!(
            policy.match_error(&matching).as_deref(),
            Some("custom_message")
        );

        let output_only = ProxyError::UpstreamError {
            status: 400,
            body: Some(
                json!({
                    "status": "completed",
                    "error": null,
                    "output_text": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE
                })
                .to_string(),
            ),
        };
        assert!(policy.match_error(&output_only).is_none());

        let root_message = ProxyError::UpstreamError {
            status: 503,
            body: Some(
                json!({
                    "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE,
                    "code": "service_unavailable"
                })
                .to_string(),
            ),
        };
        assert_eq!(
            policy.match_error(&root_message).as_deref(),
            Some("custom_message")
        );
    }

    #[test]
    fn bounded_retry_matches_real_upstream_truncation_marker_literal() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: Some(true),
            max_retries: 1,
            retry_delay_ms: 1_000,
            custom_messages: vec!["upstream error body truncated".to_string()],
            error_types: vec![],
        });
        let policy = resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("bounded retry policy");
        let error = ProxyError::UpstreamError {
            status: 503,
            body: Some(
                "upstream payload\n[cc-switch: upstream error body truncated at 262144 bytes]"
                    .to_string(),
            ),
        };

        assert_eq!(
            policy.match_error(&error).as_deref(),
            Some("custom_message")
        );
    }

    #[test]
    fn unlimited_retry_matches_real_upstream_truncation_marker_literal() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: Some(true),
            max_retries: 0,
            retry_delay_ms: 1_000,
            custom_messages: vec!["upstream error body truncated".to_string()],
            error_types: vec![],
        });
        let policy = resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &provider,
        )
        .expect("unlimited retry policy");
        let error = ProxyError::UpstreamError {
            status: 503,
            body: Some(
                "upstream payload\n[cc-switch: upstream error body truncated at 262144 bytes]"
                    .to_string(),
            ),
        };

        assert!(policy.is_unlimited());
        assert_eq!(
            policy.match_error(&error).as_deref(),
            Some("custom_message")
        );
    }

    #[test]
    fn matches_each_configured_error_type() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 1_000,
            custom_messages: vec![],
            error_types: vec![
                LocalProxyRetryErrorType::RateLimit,
                LocalProxyRetryErrorType::Overloaded,
                LocalProxyRetryErrorType::ServerError,
                LocalProxyRetryErrorType::Network,
            ],
        });
        let policy = resolve_retry_policy(
            &AppType::Gemini,
            &json!({ "model": "gemini-2.5-pro" }),
            &provider,
        )
        .expect("policy");

        let cases = [
            (
                ProxyError::UpstreamError {
                    status: 429,
                    body: Some("rate limited".to_string()),
                },
                "rate_limit",
            ),
            (
                ProxyError::UpstreamError {
                    status: 503,
                    body: Some("unavailable".to_string()),
                },
                "overloaded",
            ),
            (
                ProxyError::UpstreamError {
                    status: 502,
                    body: Some("bad gateway".to_string()),
                },
                "server_error",
            ),
            (
                ProxyError::Timeout("request timed out".to_string()),
                "network",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(policy.match_error(&error).as_deref(), Some(expected));
        }
    }

    #[test]
    fn upstream_body_timeout_exposes_status_and_network_facts() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 1_000,
            custom_messages: vec![],
            error_types: vec![
                LocalProxyRetryErrorType::Overloaded,
                LocalProxyRetryErrorType::ServerError,
                LocalProxyRetryErrorType::Network,
            ],
        });
        let policy = resolve_retry_policy(
            &AppType::Claude,
            &json!({ "model": "claude-sonnet" }),
            &provider,
        )
        .expect("policy");

        assert_eq!(
            policy
                .match_error(&ProxyError::UpstreamBodyTimeout {
                    status: 503,
                    timeout_seconds: 1,
                })
                .as_deref(),
            Some("overloaded")
        );
        assert_eq!(
            policy
                .match_error(&ProxyError::UpstreamBodyTimeout {
                    status: 502,
                    timeout_seconds: 1,
                })
                .as_deref(),
            Some("server_error")
        );
        assert_eq!(
            policy
                .match_error(&ProxyError::UpstreamBodyTimeout {
                    status: 400,
                    timeout_seconds: 1,
                })
                .as_deref(),
            Some("network")
        );
    }

    #[test]
    fn disabled_or_triggerless_policies_do_not_resolve() {
        let disabled = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 0,
            retry_delay_ms: 1_000,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![],
        });
        assert!(resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &disabled,
        )
        .is_none());

        let triggerless = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 0,
            custom_messages: vec!["  ".to_string()],
            error_types: vec![],
        });
        assert!(resolve_retry_policy(
            &AppType::Claude,
            &json!({ "model": "claude-sonnet" }),
            &triggerless,
        )
        .is_none());

        let triggerless_unlimited = provider(LocalProxyRetryPolicy {
            enabled: Some(true),
            max_retries: 0,
            retry_delay_ms: 1_000,
            custom_messages: vec!["  ".to_string()],
            error_types: vec![],
        });
        assert!(resolve_retry_policy(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &triggerless_unlimited,
        )
        .is_none());

        let bounded = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 0,
            custom_messages: vec![],
            error_types: vec![LocalProxyRetryErrorType::Network],
        });
        let policy = resolve_retry_policy(
            &AppType::GrokBuild,
            &json!({ "model": "grok-4.5" }),
            &bounded,
        )
        .expect("Grok Build should use the provider policy");
        assert_eq!(policy.retry_delay_ms(), 1);
    }

    #[test]
    fn global_and_provider_switches_gate_retry_with_legacy_compatibility() {
        let explicit_enabled = provider(LocalProxyRetryPolicy {
            enabled: Some(true),
            max_retries: 2,
            retry_delay_ms: 1_000,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![],
        });
        assert!(resolve_retry_policy_with_global(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &explicit_enabled,
            false,
        )
        .is_none());
        assert!(resolve_retry_policy_with_global(
            &AppType::Codex,
            &json!({ "model": "gpt-5.6-sol" }),
            &explicit_enabled,
            true,
        )
        .is_some());

        let explicit_disabled = provider(LocalProxyRetryPolicy {
            enabled: Some(false),
            max_retries: 2,
            retry_delay_ms: 1_000,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![],
        });
        assert!(resolve_retry_policy_with_global(
            &AppType::Claude,
            &json!({ "model": "claude-sonnet" }),
            &explicit_disabled,
            true,
        )
        .is_none());

        let legacy_enabled = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 2,
            retry_delay_ms: 1_000,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![],
        });
        assert!(resolve_retry_policy_with_global(
            &AppType::Gemini,
            &json!({ "model": "gemini-3.6-flash" }),
            &legacy_enabled,
            true,
        )
        .is_some());

        let explicit_zero = provider(LocalProxyRetryPolicy {
            enabled: Some(true),
            max_retries: 0,
            retry_delay_ms: 1_000,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![],
        });
        let explicit_unlimited = resolve_retry_policy_with_global(
            &AppType::GrokBuild,
            &json!({ "model": "grok-4.5" }),
            &explicit_zero,
            true,
        )
        .expect("an explicitly enabled zero budget should mean unlimited retries");
        assert!(explicit_unlimited.is_unlimited());
        assert!(explicit_unlimited.should_log_attempt(1));
        assert!(explicit_unlimited.should_log_attempt(3));
        assert!(!explicit_unlimited.should_log_attempt(4));
        assert!(explicit_unlimited.should_log_attempt(100));
    }

    #[test]
    fn approval_model_is_excluded_even_when_both_switches_are_enabled() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: Some(true),
            max_retries: 0,
            retry_delay_ms: 1,
            custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
            error_types: vec![LocalProxyRetryErrorType::Network],
        });

        assert!(resolve_retry_policy_with_global(
            &AppType::Codex,
            &json!({ "model": AUTO_REVIEW_MODEL }),
            &provider,
            true,
        )
        .is_none());
    }
    #[test]
    fn classifies_structured_responses_anthropic_and_gemini_errors() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 1_000,
            custom_messages: vec![],
            error_types: vec![
                LocalProxyRetryErrorType::RateLimit,
                LocalProxyRetryErrorType::Overloaded,
                LocalProxyRetryErrorType::ServerError,
            ],
        });
        let policy = resolve_retry_policy(
            &AppType::Gemini,
            &json!({ "model": "gemini-2.5-pro" }),
            &provider,
        )
        .expect("policy");

        let cases = [
            (
                json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {
                            "code": "too_many_requests",
                            "message": "quota exhausted"
                        }
                    }
                }),
                "rate_limit",
            ),
            (
                json!({
                    "type": "error",
                    "error": {
                        "type": "overloaded_error",
                        "message": "capacity unavailable"
                    }
                }),
                "overloaded",
            ),
            (
                json!({
                    "error": {
                        "code": 500,
                        "status": "INTERNAL_ERROR",
                        "message": "generation failed"
                    }
                }),
                "server_error",
            ),
            (
                json!({
                    "error": {
                        "code": 429,
                        "status": "RESOURCE_EXHAUSTED",
                        "message": "quota exhausted"
                    }
                }),
                "rate_limit",
            ),
        ];

        for (body, expected) in cases {
            let error = ProxyError::UpstreamError {
                status: 200,
                body: Some(body.to_string()),
            };
            assert_eq!(policy.match_error(&error).as_deref(), Some(expected));
        }
    }

    #[test]
    fn network_type_excludes_deterministic_local_request_errors() {
        let provider = provider(LocalProxyRetryPolicy {
            enabled: None,
            max_retries: 1,
            retry_delay_ms: 1_000,
            custom_messages: vec![],
            error_types: vec![LocalProxyRetryErrorType::Network],
        });
        let policy = resolve_retry_policy(
            &AppType::Claude,
            &json!({ "model": "claude-sonnet" }),
            &provider,
        )
        .expect("policy");

        assert_eq!(
            policy
                .match_error(&ProxyError::ForwardFailed(
                    "connection reset by peer".to_string()
                ))
                .as_deref(),
            Some("network")
        );
        assert_eq!(
            policy
                .match_error(&ProxyError::ForwardFailed(
                    "流式响应在首包到达前结束".to_string()
                ))
                .as_deref(),
            Some("network")
        );
        assert_eq!(
            policy
                .match_error(&ProxyError::ForwardFailed(
                    "Response parse failed: incomplete message".to_string()
                ))
                .as_deref(),
            Some("network")
        );
        assert!(policy
            .match_error(&ProxyError::ForwardFailed(
                "Invalid URL 'not a url'".to_string()
            ))
            .is_none());
        assert!(policy
            .match_error(&ProxyError::ForwardFailed(
                "Failed to build request: invalid header".to_string()
            ))
            .is_none());
    }
}
