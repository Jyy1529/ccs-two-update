use std::collections::HashSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{normalize_provider_common_config_for_storage, ProviderService};
use crate::app_config::AppType;
use crate::deeplink::{build_provider_from_request, DeepLinkImportRequest};
use crate::error::AppError;
use crate::provider::Provider;
use crate::store::AppState;

#[derive(Debug, Clone)]
pub(crate) struct PortableProviderFields {
    pub name: String,
    pub notes: Option<String>,
    pub website_url: Option<String>,
    pub api_key: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTransferRequest {
    pub source_app: String,
    pub source_provider_id: String,
    pub target_apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTransferPreview {
    pub name: String,
    pub notes: Option<String>,
    pub website_url: Option<String>,
    pub base_url: String,
    pub has_api_key: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderTransferStatus {
    Created,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTransferResult {
    pub app_id: String,
    pub status: ProviderTransferStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn portable_fields_from_provider(
    app_type: &AppType,
    provider: &Provider,
) -> PortableProviderFields {
    let (base_url, api_key) = provider.resolve_usage_credentials(app_type);
    PortableProviderFields {
        name: provider.name.trim().to_string(),
        notes: provider.notes.clone(),
        website_url: provider.website_url.clone(),
        api_key,
        base_url,
    }
}

fn validate_portable_fields(fields: &PortableProviderFields) -> Result<(), AppError> {
    if fields.name.is_empty() {
        return Err(AppError::localized(
            "provider.transfer.name_empty",
            "供应商名称不能为空",
            "Provider name cannot be empty",
        ));
    }

    let parsed = url::Url::parse(&fields.base_url).map_err(|_| {
        AppError::localized(
            "provider.transfer.base_url_invalid",
            "API 请求地址必须是有效的 HTTP 或 HTTPS URL",
            "The API endpoint must be a valid HTTP or HTTPS URL",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(AppError::localized(
            "provider.transfer.base_url_invalid",
            "API 请求地址必须是有效的 HTTP 或 HTTPS URL",
            "The API endpoint must be a valid HTTP or HTTPS URL",
        ));
    }

    Ok(())
}

pub(crate) fn build_provider_for_target(
    target: &AppType,
    fields: &PortableProviderFields,
) -> Result<Provider, AppError> {
    let request = DeepLinkImportRequest {
        version: "v1".to_string(),
        resource: "provider".to_string(),
        app: Some(target.as_str().to_string()),
        name: Some(fields.name.clone()),
        homepage: fields.website_url.clone(),
        endpoint: Some(fields.base_url.clone()),
        api_key: Some(fields.api_key.clone()),
        notes: fields.notes.clone(),
        ..Default::default()
    };

    build_provider_from_request(target, &request)
}

pub(crate) fn next_import_name<'a>(
    base_name: &str,
    existing_names: impl Iterator<Item = &'a str>,
) -> String {
    let existing: HashSet<&str> = existing_names.collect();
    if !existing.contains(base_name) {
        return base_name.to_string();
    }

    let first_copy = format!("{base_name} (导入)");
    if !existing.contains(first_copy.as_str()) {
        return first_copy;
    }

    let mut suffix = 2;
    loop {
        let candidate = format!("{base_name} (导入 {suffix})");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn imported_provider_id(name: &str) -> String {
    let slug = name
        .chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
        .collect::<String>()
        .to_lowercase();
    let slug = if slug.is_empty() { "imported" } else { &slug };
    format!("{slug}-{}", Uuid::new_v4())
}

fn redact_secret(message: String, secret: &str) -> String {
    if secret.is_empty() {
        message
    } else {
        message.replace(secret, "[REDACTED]")
    }
}

impl ProviderService {
    pub fn transfer_preview(
        state: &AppState,
        source_app: AppType,
        source_provider_id: &str,
    ) -> Result<ProviderTransferPreview, AppError> {
        let provider = state
            .db
            .get_provider_by_id(source_provider_id, source_app.as_str())?
            .ok_or_else(|| {
                AppError::localized(
                    "provider.transfer.source_missing",
                    "源供应商不存在",
                    "The source provider does not exist",
                )
            })?;
        let fields = portable_fields_from_provider(&source_app, &provider);

        Ok(ProviderTransferPreview {
            name: fields.name,
            notes: fields.notes,
            website_url: fields.website_url,
            base_url: fields.base_url,
            has_api_key: !fields.api_key.is_empty(),
        })
    }

    pub fn transfer_to_apps(
        state: &AppState,
        request: ProviderTransferRequest,
    ) -> Result<Vec<ProviderTransferResult>, AppError> {
        let source_app = AppType::from_str(&request.source_app)?;
        let source = state
            .db
            .get_provider_by_id(&request.source_provider_id, source_app.as_str())?
            .ok_or_else(|| {
                AppError::localized(
                    "provider.transfer.source_missing",
                    "源供应商不存在",
                    "The source provider does not exist",
                )
            })?;
        let fields = portable_fields_from_provider(&source_app, &source);
        validate_portable_fields(&fields)?;

        let mut seen_targets = HashSet::new();
        let mut results = Vec::with_capacity(request.target_apps.len());

        for requested_app in request.target_apps {
            let target = match AppType::from_str(&requested_app) {
                Ok(target) => target,
                Err(error) => {
                    results.push(ProviderTransferResult {
                        app_id: requested_app,
                        status: ProviderTransferStatus::Failed,
                        provider_id: None,
                        provider_name: None,
                        message: Some(error.to_string()),
                    });
                    continue;
                }
            };
            let target_id = target.as_str().to_string();

            if !seen_targets.insert(target_id.clone()) || target == source_app {
                results.push(ProviderTransferResult {
                    app_id: target_id,
                    status: ProviderTransferStatus::Skipped,
                    provider_id: None,
                    provider_name: None,
                    message: None,
                });
                continue;
            }

            let result = (|| -> Result<(String, String), AppError> {
                let existing = state.db.get_all_providers(target.as_str())?;
                let import_name = next_import_name(
                    &fields.name,
                    existing.values().map(|provider| provider.name.as_str()),
                );
                let mut target_fields = fields.clone();
                target_fields.name = import_name.clone();

                let mut provider = build_provider_for_target(&target, &target_fields)?;
                provider.id = imported_provider_id(&import_name);
                provider.created_at = Some(chrono::Utc::now().timestamp_millis());
                Self::normalize_provider_if_claude(&target, &mut provider);
                Self::validate_provider_settings(&target, &provider)?;
                normalize_provider_common_config_for_storage(
                    state.db.as_ref(),
                    &target,
                    &mut provider,
                )?;
                Self::normalize_usage_script_credential_overrides(&target, &mut provider);
                if target.is_additive_mode() {
                    Self::set_provider_live_config_managed(&mut provider, false);
                }

                let provider_id = provider.id.clone();
                state.db.save_provider(target.as_str(), &provider)?;
                Ok((provider_id, import_name))
            })();

            match result {
                Ok((provider_id, provider_name)) => results.push(ProviderTransferResult {
                    app_id: target_id,
                    status: ProviderTransferStatus::Created,
                    provider_id: Some(provider_id),
                    provider_name: Some(provider_name),
                    message: None,
                }),
                Err(error) => results.push(ProviderTransferResult {
                    app_id: target_id,
                    status: ProviderTransferStatus::Failed,
                    provider_id: None,
                    provider_name: None,
                    message: Some(redact_secret(error.to_string(), &fields.api_key)),
                }),
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        build_provider_for_target, next_import_name, portable_fields_from_provider, redact_secret,
        PortableProviderFields,
    };
    use crate::app_config::AppType;
    use crate::database::Database;
    use crate::provider::{ClaudeDesktopMode, Provider};
    use crate::services::ProviderService;
    use crate::store::AppState;

    fn portable_fields() -> PortableProviderFields {
        PortableProviderFields {
            name: "glos".to_string(),
            notes: Some("company account".to_string()),
            website_url: Some("https://example.com".to_string()),
            api_key: "sk-secret".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
        }
    }

    #[test]
    fn builds_codex_provider_from_portable_fields() {
        let provider = build_provider_for_target(&AppType::Codex, &portable_fields())
            .expect("portable fields should build a Codex provider");

        assert_eq!(provider.name, "glos");
        assert_eq!(provider.notes.as_deref(), Some("company account"));
        assert_eq!(provider.website_url.as_deref(), Some("https://example.com"));
        assert_eq!(
            provider.settings_config.pointer("/auth/OPENAI_API_KEY"),
            Some(&serde_json::json!("sk-secret"))
        );
        assert!(provider.settings_config["config"]
            .as_str()
            .expect("Codex config should be TOML")
            .contains("base_url = \"https://api.example.com/v1\""));
    }

    #[test]
    fn builds_every_supported_target() {
        for target in AppType::all() {
            let provider = build_provider_for_target(&target, &portable_fields())
                .unwrap_or_else(|error| panic!("{} adapter failed: {error}", target.as_str()));
            assert_eq!(provider.name, "glos", "{} name", target.as_str());
            assert_eq!(
                provider.resolve_usage_credentials(&target),
                (
                    "https://api.example.com/v1".to_string(),
                    "sk-secret".to_string()
                ),
                "{} credentials",
                target.as_str()
            );

            match target {
                AppType::Claude => {
                    assert_eq!(
                        provider.settings_config["env"]["ANTHROPIC_AUTH_TOKEN"],
                        "sk-secret"
                    );
                }
                AppType::ClaudeDesktop => {
                    assert_eq!(
                        provider
                            .meta
                            .as_ref()
                            .and_then(|meta| meta.claude_desktop_mode.clone()),
                        Some(ClaudeDesktopMode::Direct)
                    );
                }
                AppType::Codex => {
                    let config = provider.settings_config["config"]
                        .as_str()
                        .expect("Codex config");
                    config.parse::<toml::Value>().expect("valid Codex TOML");
                }
                AppType::Gemini => {
                    assert_eq!(
                        provider.settings_config["env"]["GOOGLE_GEMINI_BASE_URL"],
                        "https://api.example.com/v1"
                    );
                }
                AppType::GrokBuild => {
                    let config = provider.settings_config["config"]
                        .as_str()
                        .expect("Grok Build config");
                    config
                        .parse::<toml::Value>()
                        .expect("valid Grok Build TOML");
                    crate::grok_config::validate_config_toml(config)
                        .expect("valid Grok Build settings");
                }
                AppType::OpenCode => {
                    assert_eq!(provider.settings_config["npm"], "@ai-sdk/openai-compatible");
                }
                AppType::OpenClaw => {
                    assert_eq!(
                        provider.settings_config["baseUrl"],
                        portable_fields().base_url
                    );
                }
                AppType::Hermes => {
                    assert_eq!(
                        provider.settings_config["base_url"],
                        portable_fields().base_url
                    );
                    assert!(provider.settings_config.get("baseUrl").is_none());
                }
                AppType::DeepSeek | AppType::Pi => {
                    assert_eq!(
                        provider.settings_config["baseUrl"],
                        portable_fields().base_url
                    );
                    assert_eq!(provider.settings_config["apiKey"], "sk-secret");
                }
            }
        }
    }

    #[test]
    fn extracts_portable_credentials_from_every_source_shape() {
        let sources = [
            (
                AppType::Claude,
                serde_json::json!({
                    "env": {
                        "ANTHROPIC_AUTH_TOKEN": "source-key",
                        "ANTHROPIC_BASE_URL": "https://api.example.com/v1/"
                    }
                }),
            ),
            (
                AppType::ClaudeDesktop,
                serde_json::json!({
                    "env": {
                        "ANTHROPIC_AUTH_TOKEN": "source-key",
                        "ANTHROPIC_BASE_URL": "https://api.example.com/v1/"
                    }
                }),
            ),
            (
                AppType::Codex,
                serde_json::json!({
                    "auth": { "OPENAI_API_KEY": "source-key" },
                    "config": "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://api.example.com/v1/\"\n"
                }),
            ),
            (
                AppType::Gemini,
                serde_json::json!({
                    "env": {
                        "GEMINI_API_KEY": "source-key",
                        "GOOGLE_GEMINI_BASE_URL": "https://api.example.com/v1/"
                    }
                }),
            ),
            (
                AppType::GrokBuild,
                serde_json::json!({
                    "config": "[models]\ndefault = \"grok-4.5\"\n\n[model.\"grok-4.5\"]\nmodel = \"grok-4.5\"\nbase_url = \"https://api.example.com/v1/\"\nname = \"Source\"\napi_key = \"source-key\"\napi_backend = \"openai\"\ncontext_window = 131072\n"
                }),
            ),
            (
                AppType::OpenCode,
                serde_json::json!({
                    "options": {
                        "apiKey": "source-key",
                        "baseURL": "https://api.example.com/v1/"
                    }
                }),
            ),
            (
                AppType::OpenClaw,
                serde_json::json!({
                    "apiKey": "source-key",
                    "baseUrl": "https://api.example.com/v1/"
                }),
            ),
            (
                AppType::Hermes,
                serde_json::json!({
                    "api_key": "source-key",
                    "base_url": "https://api.example.com/v1/"
                }),
            ),
        ];

        for (source_app, settings) in sources {
            let mut provider = Provider::with_id(
                format!("{}-source", source_app.as_str()),
                " Source ".to_string(),
                settings,
                Some("https://example.com".to_string()),
            );
            provider.notes = Some("company account".to_string());

            let fields = portable_fields_from_provider(&source_app, &provider);
            assert_eq!(fields.name, "Source", "{} name", source_app.as_str());
            assert_eq!(fields.api_key, "source-key", "{} key", source_app.as_str());
            assert_eq!(
                fields.base_url,
                "https://api.example.com/v1",
                "{} endpoint",
                source_app.as_str()
            );
            assert_eq!(fields.notes.as_deref(), Some("company account"));
            assert_eq!(fields.website_url.as_deref(), Some("https://example.com"));
        }
    }

    #[test]
    fn grok_source_keeps_endpoint_when_api_key_is_empty() {
        let provider = build_provider_for_target(
            &AppType::GrokBuild,
            &PortableProviderFields {
                api_key: String::new(),
                ..portable_fields()
            },
        )
        .expect("build Grok provider");

        let fields = portable_fields_from_provider(&AppType::GrokBuild, &provider);
        assert_eq!(fields.base_url, "https://api.example.com/v1");
        assert!(fields.api_key.is_empty());
    }

    #[test]
    fn import_name_suffixes_do_not_overwrite_existing_names() {
        let existing = ["glos", "glos (导入)", "glos (导入 2)"];

        assert_eq!(
            next_import_name("glos", existing.iter().copied()),
            "glos (导入 3)"
        );
        assert_eq!(next_import_name("new", existing.iter().copied()), "new");
    }

    #[test]
    fn transfers_to_multiple_apps_without_changing_current_provider() {
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let source = Provider::with_id(
            "glos-source".to_string(),
            "glos".to_string(),
            serde_json::json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "sk-secret",
                    "ANTHROPIC_BASE_URL": "https://api.example.com/v1"
                }
            }),
            Some("https://example.com".to_string()),
        );
        db.save_provider("claude", &source).expect("save source");

        let existing_codex = Provider::with_id(
            "codex-current".to_string(),
            "glos".to_string(),
            serde_json::json!({ "auth": {}, "config": "" }),
            None,
        );
        db.save_provider("codex", &existing_codex)
            .expect("save existing target");
        db.set_current_provider("codex", "codex-current")
            .expect("set current target");

        let results = ProviderService::transfer_to_apps(
            &state,
            super::ProviderTransferRequest {
                source_app: "claude".to_string(),
                source_provider_id: "glos-source".to_string(),
                target_apps: vec!["codex".to_string(), "opencode".to_string()],
            },
        )
        .expect("transfer should complete per target");

        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|result| { result.status == super::ProviderTransferStatus::Created }));
        assert_eq!(results[0].provider_name.as_deref(), Some("glos (导入)"));
        assert_eq!(
            db.get_current_provider("codex").expect("current provider"),
            Some("codex-current".to_string())
        );

        let imported_open_code = db
            .get_provider_by_id(
                results[1].provider_id.as_deref().expect("OpenCode id"),
                "opencode",
            )
            .expect("load imported OpenCode")
            .expect("imported OpenCode exists");
        assert_eq!(
            imported_open_code
                .meta
                .as_ref()
                .and_then(|meta| meta.live_config_managed),
            Some(false)
        );
        assert_eq!(imported_open_code.notes.as_deref(), None);
        assert_eq!(
            imported_open_code.website_url.as_deref(),
            Some("https://example.com")
        );
        assert!(db
            .get_provider_by_id("glos-source", "claude")
            .expect("load source")
            .is_some());
    }

    #[test]
    fn empty_api_key_imports_to_every_other_app_as_db_only() {
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let source = Provider::with_id(
            "local-source".to_string(),
            "Local API".to_string(),
            serde_json::json!({
                "baseUrl": "http://127.0.0.1:11434/v1",
                "apiKey": ""
            }),
            None,
        );
        db.save_provider("openclaw", &source).expect("save source");
        let target_apps = AppType::all()
            .filter(|app| *app != AppType::OpenClaw)
            .map(|app| app.as_str().to_string())
            .collect::<Vec<_>>();

        let results = ProviderService::transfer_to_apps(
            &state,
            super::ProviderTransferRequest {
                source_app: "openclaw".to_string(),
                source_provider_id: source.id.clone(),
                target_apps,
            },
        )
        .expect("empty-key transfer");

        assert_eq!(results.len(), 9);
        assert!(results
            .iter()
            .all(|result| result.status == super::ProviderTransferStatus::Created));
    }

    #[test]
    fn returns_per_target_results_for_invalid_duplicate_and_valid_targets() {
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = AppState::new(db.clone());
        let source = Provider::with_id(
            "source".to_string(),
            "Source".to_string(),
            serde_json::json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "sk-secret",
                    "ANTHROPIC_BASE_URL": "https://api.example.com/v1"
                }
            }),
            None,
        );
        db.save_provider("claude", &source).expect("save source");

        let results = ProviderService::transfer_to_apps(
            &state,
            super::ProviderTransferRequest {
                source_app: "claude".to_string(),
                source_provider_id: source.id,
                target_apps: vec![
                    "codex".to_string(),
                    "unsupported".to_string(),
                    "gemini".to_string(),
                    "codex".to_string(),
                ],
            },
        )
        .expect("per-target transfer");

        assert_eq!(results.len(), 4);
        assert_eq!(results[0].status, super::ProviderTransferStatus::Created);
        assert_eq!(results[1].status, super::ProviderTransferStatus::Failed);
        assert_eq!(results[2].status, super::ProviderTransferStatus::Created);
        assert_eq!(results[3].status, super::ProviderTransferStatus::Skipped);
    }

    #[test]
    fn redacts_api_key_from_target_error_messages() {
        let message = redact_secret(
            "request with sk-secret failed for sk-secret".to_string(),
            "sk-secret",
        );

        assert_eq!(message, "request with [REDACTED] failed for [REDACTED]");
        assert!(!message.contains("sk-secret"));
    }
}
