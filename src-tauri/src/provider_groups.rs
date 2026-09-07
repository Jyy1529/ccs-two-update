//! Shared contracts for per-application provider groups and key pools.

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderGroupKind {
    #[default]
    Manual,
    AutoBaseUrl,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyPoolStrategy {
    #[default]
    Failover,
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderGroup {
    pub id: String,
    pub app_type: String,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_color: Option<String>,
    pub kind: ProviderGroupKind,
    pub normalized_base_url: Option<String>,
    pub sort_index: usize,
    pub collapsed: bool,
    pub key_pool_enabled: bool,
    pub key_pool_strategy: KeyPoolStrategy,
    pub key_pool_max_retries: u32,
    pub key_pool_cooldown_ms: u64,
    pub balance_template_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceScope {
    #[default]
    Unknown,
    PerKey,
    Account,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceQueryTemplate {
    pub id: String,
    pub name: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub query: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub remaining_path: String,
    pub used_path: Option<String>,
    pub total_path: Option<String>,
    pub reset_path: Option<String>,
    pub error_path: Option<String>,
    pub unit: Option<String>,
    pub currency: Option<String>,
    #[serde(default)]
    pub balance_scope: BalanceScope,
    pub timeout_secs: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl BalanceQueryTemplate {
    pub fn validate(&self) -> Result<(), AppError> {
        let invalid_pointer = std::iter::once(Some(self.remaining_path.as_str()))
            .chain([
                self.used_path.as_deref(),
                self.total_path.as_deref(),
                self.reset_path.as_deref(),
                self.error_path.as_deref(),
            ])
            .flatten()
            .any(|path| !path.is_empty() && !path.starts_with('/'));
        if self.id.trim().is_empty()
            || self.name.trim().is_empty()
            || self.path.trim().is_empty()
            || self.remaining_path.is_empty()
            || invalid_pointer
            || !matches!(self.method.as_str(), "GET" | "POST")
            || !(1..=30).contains(&self.timeout_secs)
        {
            return Err(AppError::InvalidInput("[balance_template_invalid] Check template name, GET/POST method, JSON Pointer paths and 1–30s timeout".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceQueryResult {
    pub provider_id: String,
    pub provider_name: String,
    pub status: String,
    pub data: Vec<crate::provider::UsageData>,
    pub error: Option<String>,
    pub currency: Option<String>,
    pub aggregation_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceQueryByCredentialsRequest {
    pub app_type: String,
    pub base_url: String,
    pub api_key: String,
    pub template: BalanceQueryTemplate,
}

/// A redacted, operator-facing view of one group's members. It deliberately
/// contains no credential material or upstream response bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderGroupMemberStatus {
    pub provider_id: String,
    pub provider_name: String,
    pub sort_index: Option<usize>,
    pub key_pool_enabled: bool,
    pub eligible: bool,
    pub cooling_down: bool,
    #[serde(default)]
    pub cooldown_remaining_ms: u64,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_failure_at: Option<i64>,
    #[serde(default)]
    pub early_probe: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderGroupStatus {
    pub group: ProviderGroup,
    pub members: Vec<ProviderGroupMemberStatus>,
    pub eligible_member_count: usize,
    #[serde(default)]
    pub proxy_running: bool,
}

/// Normalize a Base URL for grouping only. The returned value is never used
/// as the actual upstream request URL.
pub fn normalize_base_url(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidInput(
            "Base URL cannot be empty".to_string(),
        ));
    }

    let mut url = Url::parse(trimmed)
        .map_err(|error| AppError::InvalidInput(format!("Invalid Base URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::InvalidInput(
            "Base URL must use HTTP or HTTPS".into(),
        ));
    }
    if url.host_str().is_none() {
        return Err(AppError::InvalidInput(
            "Base URL must include a host".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::InvalidInput(
            "Base URL must not contain credentials".to_string(),
        ));
    }

    url.set_query(None);
    url.set_fragment(None);
    if (url.scheme() == "http" && url.port() == Some(80))
        || (url.scheme() == "https" && url.port() == Some(443))
    {
        url.set_port(None)
            .map_err(|_| AppError::InvalidInput("Invalid default Base URL port".to_string()))?;
    }

    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let normalized = url.to_string().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        return Err(AppError::InvalidInput(
            "Base URL cannot be normalized".to_string(),
        ));
    }
    Ok(normalized)
}

/// Match the actual proxy adapter precedence, including legacy top-level fields.
/// Non-proxy apps retain their native configuration layout.
pub fn resolve_group_credentials(
    app: &crate::app_config::AppType,
    provider: &crate::provider::Provider,
) -> (String, String) {
    use crate::proxy::providers::{get_adapter, AuthStrategy};
    if app.supports_local_proxy() {
        if let Some(adapter) = get_adapter(app) {
            let base = adapter.extract_base_url(provider).unwrap_or_default();
            let key = adapter
                .extract_auth(provider)
                .filter(|auth| {
                    matches!(
                        auth.strategy,
                        AuthStrategy::Anthropic
                            | AuthStrategy::ClaudeAuth
                            | AuthStrategy::Bearer
                            | AuthStrategy::Google
                    )
                })
                .map(|auth| auth.api_key)
                .unwrap_or_default();
            return (base, key);
        }
    }
    provider.resolve_usage_credentials(app)
}

/// Shared by persistence, status and routing; never return credential material.
pub fn key_pool_identity(
    app: &crate::app_config::AppType,
    provider: &crate::provider::Provider,
) -> Result<(String, &'static str), AppError> {
    use crate::app_config::AppType;
    use crate::proxy::providers::{
        get_claude_api_format, is_codex_official_provider,
        should_convert_codex_responses_to_anthropic, should_convert_codex_responses_to_chat,
        ProviderType,
    };
    if !app.supports_local_proxy() {
        return Err(AppError::InvalidInput(
            "[pool_proxy_unsupported] Key pools require a supported local proxy app".into(),
        ));
    }
    let managed_binding = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.auth_binding.as_ref())
        .is_some_and(|binding| {
            matches!(
                binding.source,
                crate::provider::AuthBindingSource::ManagedAccount
            )
        });
    let bedrock = provider
        .settings_config
        .pointer("/env/CLAUDE_CODE_USE_BEDROCK")
        .is_some_and(|value| {
            matches!(value.as_str(), Some("1" | "true")) || value.as_bool() == Some(true)
        });
    if provider.uses_managed_account_auth()
        || managed_binding
        || bedrock
        || (matches!(app, AppType::Codex) && is_codex_official_provider(provider))
        || matches!(
            ProviderType::from_app_type_and_config(app, provider),
            Some(ProviderType::GeminiCli)
        )
    {
        return Err(AppError::InvalidInput(
            "[pool_static_key_required] Only static API-key providers can join a Key pool".into(),
        ));
    }
    let (base, key) = resolve_group_credentials(app, provider);
    if base.trim().is_empty() || key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "[pool_credentials_required] Base URL and API Key are required".into(),
        ));
    }
    let protocol = match app {
        AppType::Claude => get_claude_api_format(provider),
        AppType::Codex | AppType::GrokBuild
            if should_convert_codex_responses_to_anthropic(provider, "/responses") =>
        {
            "anthropic"
        }
        AppType::Codex | AppType::GrokBuild
            if should_convert_codex_responses_to_chat(provider, "/responses") =>
        {
            "openai_chat"
        }
        AppType::Codex | AppType::GrokBuild => "openai_responses",
        _ => "gemini",
    };
    Ok((normalize_base_url(&base)?, protocol))
}

pub fn validate_key_pool_members(
    app: &crate::app_config::AppType,
    expected_base: Option<&str>,
    members: &[crate::provider::Provider],
) -> Result<(), AppError> {
    let mut identity = None;
    for provider in members {
        let next = key_pool_identity(app, provider)?;
        if expected_base.is_some_and(|base| base != next.0)
            || identity.as_ref().is_some_and(|previous| previous != &next)
        {
            return Err(AppError::InvalidInput("[pool_base_url_mismatch] Key pool members must have the same normalized Base URL and protocol".into()));
        }
        identity = Some(next);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_url_keeps_path_but_drops_query_and_trailing_slash() {
        assert_eq!(
            normalize_base_url(" HTTPS://EXAMPLE.com/v1/?tenant=one#top ").unwrap(),
            "https://example.com/v1"
        );
    }

    #[test]
    fn legacy_provider_meta_without_group_fields_still_deserializes() {
        let meta: crate::provider::ProviderMeta = serde_json::from_str("{}").unwrap();
        assert!(meta.provider_group_id.is_none());
        assert!(!meta.key_pool_enabled.unwrap_or(false));
    }

    #[test]
    fn grouping_and_pool_validation_use_the_actual_proxy_endpoint_precedence() {
        use crate::{app_config::AppType, provider::Provider};
        let provider = Provider::with_id(
            "legacy".into(),
            "Legacy".into(),
            serde_json::json!({
                "base_url":"https://actual.example/v1", "env":{"OPENAI_API_KEY":"fixture-actual"},
                "auth":{"OPENAI_API_KEY":"fixture-stale"},
                "config":"base_url = \"https://stale.example/v1\""
            }),
            None,
        );
        let (base, key) = resolve_group_credentials(&AppType::Codex, &provider);
        assert_eq!(base, "https://actual.example/v1");
        assert_eq!(key, "fixture-actual");
        assert!(validate_key_pool_members(
            &AppType::Codex,
            Some("https://stale.example/v1"),
            &[provider]
        )
        .is_err());
    }
}
