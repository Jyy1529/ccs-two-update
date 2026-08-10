use crate::{
    app_config::AppType,
    provider::{CodexAutoReviewMode, Provider},
    proxy::{error::ProxyError, providers::codex_provider_upstream_model},
};
use serde_json::Value;

const AUTO_REVIEW_MODEL: &str = "codex-auto-review";

pub fn is_auto_review_model(app_type: &AppType, body: &Value) -> bool {
    matches!(app_type, AppType::Codex)
        && body.get("model").and_then(Value::as_str) == Some(AUTO_REVIEW_MODEL)
}

fn is_responses_endpoint(endpoint: &str) -> bool {
    endpoint
        .split('?')
        .next()
        .is_some_and(|path| path.ends_with("/responses"))
}

fn is_auto_review_request(app_type: &AppType, endpoint: &str, body: &Value) -> bool {
    is_auto_review_model(app_type, body) && is_responses_endpoint(endpoint)
}

fn configured_mode(provider: &Provider) -> CodexAutoReviewMode {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.codex_auto_review_mode)
        .unwrap_or_default()
}

fn fallback_model(provider: &Provider) -> Option<String> {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.codex_auto_review_fallback_model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty() && *model != AUTO_REVIEW_MODEL)
        .map(ToString::to_string)
        .or_else(|| {
            codex_provider_upstream_model(provider)
                .filter(|model| model.as_str() != AUTO_REVIEW_MODEL)
        })
}

fn rewrite_model(body: &mut Value, model: &str) {
    body["model"] = Value::String(model.to_string());
}

pub fn apply_initial_policy(
    app_type: &AppType,
    endpoint: &str,
    provider: &Provider,
    body: &mut Value,
) -> Option<String> {
    if !is_auto_review_request(app_type, endpoint, body)
        || configured_mode(provider) != CodexAutoReviewMode::Fallback
    {
        return None;
    }

    let model = fallback_model(provider)?;
    rewrite_model(body, &model);
    Some(model)
}

pub fn fallback_after_error(
    app_type: &AppType,
    endpoint: &str,
    provider: &Provider,
    body: &Value,
    error: &ProxyError,
) -> Option<String> {
    if !is_auto_review_request(app_type, endpoint, body)
        || configured_mode(provider) != CodexAutoReviewMode::Auto
        || !is_model_unavailable_error(error)
    {
        return None;
    }

    fallback_model(provider)
}

pub fn apply_fallback(body: &mut Value, model: &str) {
    rewrite_model(body, model);
}

pub fn is_model_unavailable_error(error: &ProxyError) -> bool {
    let ProxyError::UpstreamError { status, body } = error else {
        return false;
    };
    if !matches!(*status, 400 | 404 | 422 | 500 | 502 | 503 | 504) {
        return false;
    }

    let message = body.as_deref().unwrap_or_default().to_ascii_lowercase();
    let explicitly_model_specific = [
        "model_not_found",
        "model not found",
        "unknown model",
        "unsupported model",
        "invalid model",
        "model is not available",
        "model unavailable",
        "no available channel for model",
        "does not support model",
    ]
    .iter()
    .any(|pattern| message.contains(pattern));

    explicitly_model_specific
        || (message.contains("does not exist")
            && (message.contains("model") || message.contains(AUTO_REVIEW_MODEL)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderMeta};
    use serde_json::json;

    fn provider(mode: Option<CodexAutoReviewMode>, fallback: Option<&str>) -> Provider {
        Provider {
            id: "provider-1".to_string(),
            name: "Provider".to_string(),
            settings_config: json!({
                "config": "model = \"provider-default\"\n"
            }),
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta {
                codex_auto_review_mode: mode,
                codex_auto_review_fallback_model: fallback.map(ToString::to_string),
                ..ProviderMeta::default()
            }),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    fn unavailable_error(status: u16, message: &str) -> ProxyError {
        ProxyError::UpstreamError {
            status,
            body: Some(message.to_string()),
        }
    }

    #[test]
    fn native_mode_preserves_internal_model() {
        let provider = provider(None, Some("gpt-5.6-sol"));
        let mut body = json!({ "model": AUTO_REVIEW_MODEL });

        assert_eq!(
            apply_initial_policy(&AppType::Codex, "/v1/responses", &provider, &mut body),
            None
        );
        assert_eq!(body["model"], AUTO_REVIEW_MODEL);
    }

    #[test]
    fn fallback_mode_rewrites_before_first_request() {
        let provider = provider(Some(CodexAutoReviewMode::Fallback), Some("gpt-5.6-sol"));
        let mut body = json!({ "model": AUTO_REVIEW_MODEL, "input": "review" });

        assert_eq!(
            apply_initial_policy(&AppType::Codex, "/v1/responses", &provider, &mut body),
            Some("gpt-5.6-sol".to_string())
        );
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["input"], "review");
    }

    #[test]
    fn fallback_mode_uses_provider_default_when_explicit_model_is_empty() {
        let provider = provider(Some(CodexAutoReviewMode::Fallback), Some("  "));
        let mut body = json!({ "model": AUTO_REVIEW_MODEL });

        assert_eq!(
            apply_initial_policy(&AppType::Codex, "/responses", &provider, &mut body),
            Some("provider-default".to_string())
        );
        assert_eq!(body["model"], "provider-default");
    }

    #[test]
    fn auto_mode_retries_explicit_model_unavailable_error() {
        let provider = provider(Some(CodexAutoReviewMode::Auto), Some("gpt-5.6-sol"));
        let body = json!({ "model": AUTO_REVIEW_MODEL });
        let error = unavailable_error(
            503,
            "No available channel for model codex-auto-review under group default",
        );

        assert_eq!(
            fallback_after_error(&AppType::Codex, "/v1/responses", &provider, &body, &error,),
            Some("gpt-5.6-sol".to_string())
        );
    }

    #[test]
    fn auto_mode_does_not_retry_unrelated_errors() {
        let provider = provider(Some(CodexAutoReviewMode::Auto), Some("gpt-5.6-sol"));
        let body = json!({ "model": AUTO_REVIEW_MODEL });
        for error in [
            unavailable_error(401, "model not found"),
            unavailable_error(403, "forbidden"),
            unavailable_error(429, "rate limited"),
            unavailable_error(503, "temporarily unavailable"),
            unavailable_error(404, "The requested endpoint does not exist"),
            unavailable_error(422, "Tool web_search does not exist"),
        ] {
            assert_eq!(
                fallback_after_error(&AppType::Codex, "/v1/responses", &provider, &body, &error,),
                None
            );
        }
    }

    #[test]
    fn auto_mode_retries_model_does_not_exist_error() {
        let provider = provider(Some(CodexAutoReviewMode::Auto), Some("gpt-5.6-sol"));
        let body = json!({ "model": AUTO_REVIEW_MODEL });
        let error = unavailable_error(404, "The model codex-auto-review does not exist");

        assert_eq!(
            fallback_after_error(&AppType::Codex, "/v1/responses", &provider, &body, &error),
            Some("gpt-5.6-sol".to_string())
        );
    }

    #[test]
    fn auto_mode_does_not_retry_generic_gateway_failures() {
        let provider = provider(Some(CodexAutoReviewMode::Auto), Some("gpt-5.6-sol"));
        let body = json!({ "model": AUTO_REVIEW_MODEL });

        for (status, message) in [
            (500, "upstream internal error"),
            (502, "bad gateway"),
            (504, "gateway timeout"),
        ] {
            let error = unavailable_error(status, message);
            assert_eq!(
                fallback_after_error(&AppType::Codex, "/v1/responses", &provider, &body, &error),
                None,
                "generic gateway status {status} must not trigger model fallback"
            );
        }
    }

    #[test]
    fn auto_mode_retries_model_specific_gateway_failure() {
        let provider = provider(Some(CodexAutoReviewMode::Auto), Some("gpt-5.6-sol"));
        let body = json!({ "model": AUTO_REVIEW_MODEL });
        let error = unavailable_error(502, "model not found: codex-auto-review");

        assert_eq!(
            fallback_after_error(&AppType::Codex, "/v1/responses", &provider, &body, &error),
            Some("gpt-5.6-sol".to_string())
        );
    }

    #[test]
    fn policy_ignores_other_models_apps_and_endpoints() {
        let provider = provider(Some(CodexAutoReviewMode::Fallback), Some("gpt-5.6-sol"));
        let cases = [
            (AppType::Codex, "/v1/responses", "gpt-5.6-sol"),
            (AppType::Claude, "/v1/responses", AUTO_REVIEW_MODEL),
            (AppType::Codex, "/v1/models", AUTO_REVIEW_MODEL),
        ];

        for (app_type, endpoint, model) in cases {
            let mut body = json!({ "model": model });
            assert_eq!(
                apply_initial_policy(&app_type, endpoint, &provider, &mut body),
                None
            );
            assert_eq!(body["model"], model);
        }
    }
}
