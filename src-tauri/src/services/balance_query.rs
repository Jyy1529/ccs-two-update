use crate::error::AppError;
use crate::provider::{UsageData, UsageResult};
use crate::provider_groups::BalanceQueryTemplate;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use serde_json::Value;
use std::time::Duration;

pub struct RenderedBalanceRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<String>,
    pub timeout: Duration,
}

impl RenderedBalanceRequest {
    pub fn redacted_debug(&self) -> String {
        format!(
            "{} [REDACTED] headers={} body={}",
            self.method,
            self.headers.len(),
            self.body.is_some()
        )
    }
}

fn replace_tokens(value: &str, base_url: &str, api_key: &str) -> String {
    value
        .replace("{{baseUrl}}", base_url)
        .replace("{{apiKey}}", api_key)
}

pub fn render_balance_request(
    template: &BalanceQueryTemplate,
    base_url: &str,
    api_key: &str,
) -> Result<RenderedBalanceRequest, AppError> {
    template.validate()?;
    if api_key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "API key cannot be empty".to_string(),
        ));
    }
    crate::provider_groups::normalize_base_url(base_url)?;
    let mut base = Url::parse(base_url.trim())
        .map_err(|error| AppError::InvalidInput(format!("Invalid Base URL: {error}")))?;
    base.set_query(None);
    base.set_fragment(None);
    let origin = base.origin();
    let base_prefix = base.path().trim_end_matches('/');
    let base_url = base.as_str().trim_end_matches('/');
    let raw_path = replace_tokens(&template.path, base_url, api_key);
    let url = if let Ok(candidate) = Url::parse(&raw_path) {
        if candidate.origin() != origin {
            return Err(AppError::InvalidInput(
                "Balance query URL must use the Provider Base URL origin".to_string(),
            ));
        }
        candidate
    } else {
        let mut prefix = base.clone();
        prefix.set_path(&format!("{base_prefix}/"));
        prefix.join(raw_path.trim_start_matches('/')).map_err(|_| {
            AppError::InvalidInput("[balance_template_invalid] Invalid balance query path".into())
        })?
    };

    let mut url = url;
    if url.origin() != origin || !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::InvalidInput(
            "[balance_origin] Balance query must use the Provider origin without URL credentials"
                .into(),
        ));
    }
    url.set_fragment(None);
    if !template.query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in &template.query {
            pairs.append_pair(
                &replace_tokens(key, base_url, api_key),
                &replace_tokens(value, base_url, api_key),
            );
        }
        drop(pairs);
    }

    let mut headers = HeaderMap::new();
    for (name, value) in &template.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| AppError::InvalidInput(format!("Invalid balance header: {error}")))?;
        if matches!(
            name.as_str(),
            "host" | "content-length" | "transfer-encoding"
        ) {
            return Err(AppError::InvalidInput(
                "[balance_template_invalid] Transport headers cannot be overridden".into(),
            ));
        }
        let value =
            HeaderValue::from_str(&replace_tokens(value, base_url, api_key)).map_err(|error| {
                AppError::InvalidInput(format!("Invalid balance header value: {error}"))
            })?;
        headers.insert(name, value);
    }

    let method = Method::from_bytes(template.method.as_bytes())
        .map_err(|error| AppError::InvalidInput(format!("Invalid balance method: {error}")))?;
    Ok(RenderedBalanceRequest {
        method,
        url,
        headers,
        body: template
            .body
            .as_deref()
            .map(|body| replace_tokens(body, base_url, api_key)),
        timeout: Duration::from_secs(template.timeout_secs.clamp(1, 30)),
    })
}

pub fn number_at_pointer(value: &Value, pointer: &str) -> Option<f64> {
    let value = value.pointer(pointer)?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|number| number.is_finite())
}

fn string_at_pointer(value: &Value, pointer: Option<&str>) -> Option<String> {
    pointer
        .and_then(|path| value.pointer(path))
        .and_then(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .or_else(|| value.as_i64().map(|number| number.to_string()))
        })
}

pub async fn query_balance_template(
    template: &BalanceQueryTemplate,
    base_url: &str,
    api_key: &str,
) -> Result<UsageResult, AppError> {
    let request = render_balance_request(template, base_url, api_key)?;
    log::debug!("Balance query request: {}", request.redacted_debug());
    let client = reqwest::Client::builder()
        .timeout(request.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            AppError::Message(format!("Build balance query client failed: {error}"))
        })?;
    let mut builder = client
        .request(request.method, request.url)
        .headers(request.headers);
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    let mut response = builder.send().await.map_err(|error| {
        AppError::Message(if error.is_timeout() {
            "[balance_timeout] Balance query timed out".into()
        } else {
            "[balance_network] Balance query request failed".into()
        })
    })?;
    let status = response.status();
    if !status.is_success() {
        return Ok(UsageResult {
            success: false,
            data: None,
            error: Some(format!("HTTP {}", status.as_u16())),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AppError::Message("[balance_network] Read balance response failed".into()))?
    {
        if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(AppError::Message(
                "[balance_response_large] Balance response exceeds 1 MiB".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let json: Value = serde_json::from_slice(&body).map_err(|_| {
        AppError::Message("[balance_response_invalid] Balance response is not valid JSON".into())
    })?;
    if let Some(error_path) = template.error_path.as_deref() {
        if string_at_pointer(&json, Some(error_path)).is_some_and(|error| !error.trim().is_empty())
        {
            return Ok(UsageResult {
                success: false,
                data: None,
                error: Some("[balance_upstream_error] Balance response reports an error at the configured path".into()),
            });
        }
    }
    let remaining = number_at_pointer(&json, &template.remaining_path);
    let used = template
        .used_path
        .as_deref()
        .and_then(|path| number_at_pointer(&json, path));
    let total = template
        .total_path
        .as_deref()
        .and_then(|path| number_at_pointer(&json, path));
    if remaining.is_none() {
        return Ok(UsageResult {
            success: false,
            data: None,
            error: Some(
                "[balance_numeric_missing] Remaining balance is missing or not a finite number"
                    .to_string(),
            ),
        });
    }
    let extra = template
        .reset_path
        .as_deref()
        .and_then(|path| string_at_pointer(&json, Some(path)));
    Ok(redact_balance_result(
        UsageResult {
            success: true,
            data: Some(vec![UsageData {
                plan_name: Some(template.name.clone()),
                extra,
                is_valid: Some(true),
                invalid_message: None,
                total,
                used,
                remaining,
                unit: template.unit.clone().or_else(|| template.currency.clone()),
            }]),
            error: None,
        },
        api_key,
    ))
}

pub(crate) fn redact_balance_result(mut result: UsageResult, api_key: &str) -> UsageResult {
    let encoded: String = url::form_urlencoded::byte_serialize(api_key.as_bytes()).collect();
    let escaped = serde_json::to_string(api_key).unwrap_or_default();
    let escaped = escaped.trim_matches('"');
    let clean = |text: &mut Option<String>| {
        if let Some(value) = text {
            for secret in [api_key, encoded.as_str(), escaped] {
                if !secret.is_empty() {
                    *value = value.replace(secret, "[REDACTED]");
                }
            }
            *value = value.chars().take(512).collect();
        }
    };
    clean(&mut result.error);
    for data in result.data.iter_mut().flatten() {
        clean(&mut data.plan_name);
        clean(&mut data.extra);
        clean(&mut data.invalid_message);
        clean(&mut data.unit);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn template(path: &str) -> BalanceQueryTemplate {
        BalanceQueryTemplate {
            id: "template-1".into(),
            name: "Relay".into(),
            method: "GET".into(),
            path: path.into(),
            query: HashMap::new(),
            headers: HashMap::from([("Authorization".into(), "Bearer {{apiKey}}".into())]),
            body: None,
            remaining_path: "/data/balance".into(),
            used_path: None,
            total_path: None,
            reset_path: None,
            error_path: None,
            unit: Some("USD".into()),
            currency: Some("USD".into()),
            balance_scope: Default::default(),
            timeout_secs: 10,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn renders_same_origin_request_and_redacts_key() {
        let request = render_balance_request(
            &template("/user/balance"),
            "https://relay.example/v1",
            "secret-key",
        )
        .unwrap();
        assert_eq!(
            request.url.as_str(),
            "https://relay.example/v1/user/balance"
        );
        assert!(request.redacted_debug().contains("[REDACTED]"));
        assert!(!request.redacted_debug().contains("secret-key"));
    }

    #[test]
    fn rejects_cross_origin_template_url() {
        assert!(render_balance_request(
            &template("https://other.example/balance"),
            "https://relay.example/v1",
            "secret-key"
        )
        .is_err());
    }

    #[test]
    fn reads_nested_json_pointer_as_number() {
        let body = serde_json::json!({"data": {"balance": "12.5"}});
        assert_eq!(number_at_pointer(&body, "/data/balance"), Some(12.5));
    }

    #[test]
    fn credentials_in_url_or_query_never_appear_in_debug_output() {
        let mut input = template("/balance/{{apiKey}}");
        input.query.insert("token".into(), "{{apiKey}}".into());
        let request =
            render_balance_request(&input, "http://[::1]:8080/v1", "fixture-secret").unwrap();
        assert_eq!(request.url.host_str(), Some("[::1]"));
        assert!(request.url.query().unwrap().contains("fixture-secret"));
        assert!(!request.redacted_debug().contains("fixture-secret"));
        assert!(!request.redacted_debug().contains("/balance"));
    }

    #[test]
    fn temporary_queries_validate_method_paths_origin_and_transport_headers() {
        let mut input = template("/balance");
        input.method = "DELETE".into();
        assert!(render_balance_request(&input, "https://relay.example", "key").is_err());
        input.method = "GET".into();
        input.remaining_path = "data.balance".into();
        assert!(input.validate().is_err());
        input.remaining_path = "/balance".into();
        for base in ["ftp://relay.example", "https://key@relay.example"] {
            assert!(render_balance_request(&input, base, "key").is_err());
        }
        input.path = "https://key@relay.example/balance".into();
        assert!(render_balance_request(&input, "https://relay.example", "key").is_err());
        input.path = "/balance".into();
        input.headers.insert("Host".into(), "other.example".into());
        assert!(render_balance_request(&input, "https://relay.example", "key").is_err());
        for value in ["NaN", "Infinity", "-inf"] {
            assert_eq!(
                number_at_pointer(&serde_json::json!({"balance":value}), "/balance"),
                None
            );
        }
    }

    #[tokio::test]
    async fn template_transport_bounds_response_redacts_echo_and_does_not_follow_redirects() {
        use axum::{response::Redirect, routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let redirected = Arc::new(AtomicUsize::new(0));
        let count = redirected.clone();
        let app = Router::new()
            .route("/balance", get(|| async { Json(serde_json::json!({"data":{"balance":"12.5"},"error":"","reset":"fixture-secret"})) }))
            .route("/error", get(|| async { (axum::http::StatusCode::FORBIDDEN, "fixture-secret") }))
            .route("/large", get(|| async { "x".repeat(1024 * 1024 + 1) }))
            .route("/redirect", get(|| async { Redirect::temporary("/sink") }))
            .route("/sink", get(move || { let count = count.clone(); async move { count.fetch_add(1, Ordering::SeqCst); "fixture-secret" } }))
            .route("/slow", get(|| async { tokio::time::sleep(Duration::from_secs(2)).await; "{}" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut input = template("/balance");
        input.error_path = Some("/error".into());
        input.reset_path = Some("/reset".into());
        let result = query_balance_template(&input, &base, "fixture-secret")
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.data.as_ref().unwrap()[0].remaining, Some(12.5));
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("fixture-secret"));
        for path in ["/error", "/redirect"] {
            input.path = path.into();
            let result = query_balance_template(&input, &base, "fixture-secret")
                .await
                .unwrap();
            assert!(!result.success);
            assert!(!result.error.unwrap().contains("fixture-secret"));
        }
        assert_eq!(redirected.load(Ordering::SeqCst), 0);
        input.path = "/large".into();
        let error = query_balance_template(&input, &base, "fixture-secret")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("balance_response_large"));
        input.path = "/slow".into();
        input.timeout_secs = 1;
        assert!(query_balance_template(&input, &base, "fixture-secret")
            .await
            .unwrap_err()
            .to_string()
            .contains("balance_timeout"));
        task.abort();
        let _ = task.await;
    }
}
