//! Resolve only explicit provider configuration. Do not use usage scripts, auth
//! managers, process environment, endpoint selection or key-pool routing here.

use super::{input_error, TargetInput, TargetSummary, ValidationMode, ValidationProtocol};
use crate::{app_config::AppType, database::Database, error::AppError, provider::Provider};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::str::FromStr;
use url::Url;

#[derive(Clone, Copy)]
pub(super) enum KeyHeader {
    Bearer,
    Anthropic,
    Google,
}

// Deliberately neither Debug nor Serialize: this contains credentials.
pub(super) struct PinnedTarget {
    pub app: AppType,
    pub provider: Provider,
    pub summary: TargetSummary,
    pub key: String,
    pub header: KeyHeader,
    pub fingerprint: [u8; 32],
}

impl PinnedTarget {
    pub fn resolve(
        db: &Database,
        input: &TargetInput,
        mode: ValidationMode,
    ) -> Result<Self, AppError> {
        if input.model.is_empty() {
            return Err(input_error("模型名称不能为空"));
        }
        Self::resolve_config(db, input, mode)
    }

    pub fn resolve_for_models(db: &Database, input: &TargetInput) -> Result<Self, AppError> {
        Self::resolve_config(db, input, ValidationMode::Direct)
    }

    fn resolve_config(
        db: &Database,
        input: &TargetInput,
        mode: ValidationMode,
    ) -> Result<Self, AppError> {
        let app = AppType::from_str(&input.app_id).map_err(|_| input_error("应用标识无效"))?;
        if input.provider_id.is_empty() || input.provider_id.len() > 256 {
            return Err(input_error("必须选择具体的 Provider／Key 池成员"));
        }
        let provider = db
            .get_provider_by_id(&input.provider_id, app.as_str())
            .map_err(|_| input_error("无法读取供应商配置"))?
            .ok_or_else(|| input_error("供应商不存在；请选择具体成员而非 Key 池目录"))?;
        Self::from_provider(app, provider, input, mode)
    }

    pub(super) fn from_provider(
        app: AppType,
        provider: Provider,
        input: &TargetInput,
        mode: ValidationMode,
    ) -> Result<Self, AppError> {
        if input.model.len() > 256
            || input.model.trim() != input.model
            || input.model.chars().any(char::is_control)
        {
            return Err(input_error("模型名称不能为空、含控制字符或超过 256 字节"));
        }
        let settings = &provider.settings_config;
        let managed = provider
            .meta
            .as_ref()
            .and_then(|m| m.auth_binding.as_ref())
            .is_some_and(|b| b.source == crate::provider::AuthBindingSource::ManagedAccount);
        let oauth_mode = at(
            settings,
            &[
                "/auth_mode",
                "/authMode",
                "/env/AUTH_MODE",
                "/auth/auth_mode",
            ],
        )
        .is_some_and(|s| {
            matches!(
                s.to_lowercase().as_str(),
                "oauth" | "chatgpt" | "google_oauth" | "managed_account"
            )
        });
        if managed
            || oauth_mode
            || provider.uses_managed_account_auth()
            || provider
                .meta
                .as_ref()
                .and_then(|m| m.provider_type.as_deref())
                .is_some_and(|kind| kind.ends_with("_oauth") || kind == "gemini_cli")
            || app == AppType::Codex
                && crate::proxy::providers::is_codex_official_provider(&provider)
            || settings.pointer("/auth/tokens").is_some()
            || settings
                .pointer("/env/CLAUDE_CODE_USE_BEDROCK")
                .is_some_and(truthy)
            || settings
                .pointer("/env/CLAUDE_CODE_USE_VERTEX")
                .is_some_and(truthy)
        {
            return Err(input_error(
                "本次仅支持显式 API Key；OAuth、托管账号和云签名鉴权不适用",
            ));
        }

        let config = match settings.get("config").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => Some(
                s.parse::<toml::Value>()
                    .map_err(|_| input_error("供应商 TOML 无效；未尝试其他配置或凭据"))?,
            ),
            _ => None,
        };
        let selected = config.as_ref().and_then(|root| {
            if app == AppType::GrokBuild {
                root.get("models")?
                    .get("default")?
                    .as_str()
                    .and_then(|id| root.get("model")?.get(id))
            } else {
                root.get("model_provider")?
                    .as_str()
                    .and_then(|id| root.get("model_providers")?.get(id))
            }
        });
        let toml_field = |key: &str| {
            selected
                .and_then(|v| v.get(key))
                .or_else(|| config.as_ref().and_then(|v| v.get(key)))
                .and_then(toml::Value::as_str)
                .filter(|s| !s.trim().is_empty())
        };

        let (base, key, field) = match app {
            AppType::Claude | AppType::ClaudeDesktop => (
                at(
                    settings,
                    &[
                        "/env/ANTHROPIC_BASE_URL",
                        "/base_url",
                        "/baseURL",
                        "/apiEndpoint",
                    ],
                ),
                at(
                    settings,
                    &[
                        "/env/ANTHROPIC_AUTH_TOKEN",
                        "/env/ANTHROPIC_API_KEY",
                        "/env/OPENROUTER_API_KEY",
                        "/env/OPENAI_API_KEY",
                        "/env/GEMINI_API_KEY",
                        "/env/GOOGLE_API_KEY",
                        "/apiKey",
                        "/api_key",
                    ],
                ),
                if at(settings, &["/env/ANTHROPIC_AUTH_TOKEN"]).is_some() {
                    "ANTHROPIC_AUTH_TOKEN"
                } else {
                    "API Key"
                },
            ),
            AppType::Codex | AppType::GrokBuild => (
                at(settings, &["/base_url", "/baseURL", "/config/base_url"])
                    .or_else(|| toml_field("base_url")),
                at(
                    settings,
                    &[
                        "/env/OPENAI_API_KEY",
                        "/auth/OPENAI_API_KEY",
                        "/apiKey",
                        "/api_key",
                        "/config/api_key",
                        "/config/apiKey",
                    ],
                )
                .or_else(|| toml_field("api_key"))
                .or_else(|| toml_field("experimental_bearer_token")),
                "API Key",
            ),
            AppType::Gemini => (
                at(
                    settings,
                    &["/env/GOOGLE_GEMINI_BASE_URL", "/base_url", "/baseURL"],
                ),
                at(
                    settings,
                    &[
                        "/env/GEMINI_API_KEY",
                        "/env/GOOGLE_API_KEY",
                        "/apiKey",
                        "/api_key",
                    ],
                ),
                "API Key",
            ),
            AppType::OpenCode => (
                at(settings, &["/options/baseURL"]),
                at(settings, &["/options/apiKey"]),
                "API Key",
            ),
            AppType::Hermes => (
                at(settings, &["/base_url"]),
                at(settings, &["/api_key"]),
                "API Key",
            ),
            AppType::Pi => {
                let model = settings
                    .get("models")
                    .and_then(Value::as_array)
                    .and_then(|models| {
                        models.iter().find(|m| {
                            m.get("id").and_then(Value::as_str) == Some(input.model.as_str())
                        })
                    });
                (
                    model
                        .and_then(|m| at(m, &["/baseUrl"]))
                        .or_else(|| at(settings, &["/baseUrl"])),
                    at(settings, &["/apiKey"]),
                    "API Key",
                )
            }
            AppType::OpenClaw | AppType::DeepSeek => (
                at(settings, &["/baseUrl"]),
                at(settings, &["/apiKey"]),
                "API Key",
            ),
        };
        let key = key
            .ok_or_else(|| {
                input_error("没有显式 API Key；不会读取环境变量、执行脚本或使用备用 Key")
            })?
            .to_string();
        if key.len() > 8192
            || key.chars().any(char::is_control)
            || key.chars().any(char::is_whitespace)
            || key.starts_with('$')
            || key.starts_with('!')
            || key.starts_with('{')
            || key.starts_with("ya29.")
            || key.starts_with("sk-ant-oat")
            || key.contains("CC_SWITCH_PROXY")
            || key.eq_ignore_ascii_case("PROXY_MANAGED")
            || key.starts_with("{env:")
        {
            return Err(input_error(
                "凭据不是可验证的静态 API Key（不支持 OAuth、占位符或动态引用）",
            ));
        }
        if app == AppType::Pi
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(input_error(
                "Pi 环境变量名称不是显式 API Key；不会展开环境变量",
            ));
        }
        if input.model.contains(&key) || provider.id.contains(&key) {
            return Err(input_error("检测标识中含凭据；请修正供应商配置"));
        }
        let base = base.ok_or_else(|| input_error("需要供应商中显式配置的 Base URL"))?;
        let endpoint = validate_endpoint(base, &key)?;
        let inferred = infer_protocol(
            &app,
            &provider,
            &input.model,
            toml_field("wire_api").or_else(|| toml_field("api_backend")),
        );
        let protocol = match (mode, input.protocol) {
            (ValidationMode::Direct, Some(protocol)) => protocol,
            _ => inferred
                .as_ref()
                .copied()
                .map_err(|_| input_error("供应商声明的协议不在本次支持范围内"))?,
        };
        if mode == ValidationMode::Ccs {
            if !app.supports_local_proxy() {
                return Err(input_error("该应用没有 ccs 转发适配；请选择直连上游"));
            }
            if input.protocol.is_some_and(|p| p != protocol) || protocol != inferred? {
                return Err(input_error(
                    "ccs 链路检测协议必须与供应商代理配置一致；覆盖协议请使用直连",
                ));
            }
            // The actual converter owns auth/header selection. A legacy
            // alternative config field must not make it send another key or
            // endpoint than the ones identified in this preview.
            let (chain_endpoint, chain_key) =
                crate::proxy::diagnostic::validate_provider_config(&app, &provider)?;
            if chain_key != key {
                return Err(input_error(
                    "ccs 转发解析的凭据与检测目标不一致；未发送请求，请修正配置或使用直连",
                ));
            }
            if validate_endpoint(&chain_endpoint, &key)? != endpoint {
                return Err(input_error(
                    "ccs 转发解析的端点与检测目标不一致；未发送请求，请修正配置或使用直连",
                ));
            }
        }
        let header = match protocol {
            ValidationProtocol::Gemini => KeyHeader::Google,
            ValidationProtocol::Anthropic => {
                if field == "ANTHROPIC_AUTH_TOKEN"
                    || matches!(app, AppType::Codex | AppType::GrokBuild)
                        && !provider.claude_uses_api_key_field()
                    || at(settings, &["/auth_mode", "/env/AUTH_MODE"]) == Some("bearer_only")
                {
                    KeyHeader::Bearer
                } else {
                    KeyHeader::Anthropic
                }
            }
            _ => KeyHeader::Bearer,
        };
        let fingerprint = fingerprint(&provider)?;
        let credential_id = format!("{:x}", Sha256::digest(key.as_bytes()));
        let config_id = fingerprint[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let summary = TargetSummary {
            app_id: app.as_str().into(),
            provider_id: provider.id.clone(),
            provider_name: redact(&provider.name, &[&key], 160),
            endpoint,
            credential_label: format!(
                "{field} · 凭据 {} · 配置 {config_id}（不显示密钥）",
                &credential_id[..12]
            ),
            model: input.model.clone(),
            protocol,
        };
        Ok(Self {
            app,
            provider,
            summary,
            key,
            header,
            fingerprint,
        })
    }

    pub fn verify_unchanged(&self, db: &Database) -> Result<(), AppError> {
        let provider = db
            .get_provider_by_id(&self.provider.id, self.app.as_str())
            .map_err(|_| input_error("无法复核供应商配置"))?
            .ok_or_else(|| input_error("供应商已删除，请重新准备检测"))?;
        if fingerprint(&provider)? != self.fingerprint {
            return Err(input_error(
                "供应商配置或凭据已改变，请重新预览；未发送请求",
            ));
        }
        Ok(())
    }

    pub fn wire_protocol(&self, mode: ValidationMode) -> ValidationProtocol {
        if mode == ValidationMode::Direct {
            return self.summary.protocol;
        }
        match self.app {
            AppType::Claude | AppType::ClaudeDesktop => ValidationProtocol::Anthropic,
            AppType::Gemini => ValidationProtocol::Gemini,
            _ => ValidationProtocol::OpenaiResponses,
        }
    }
}

fn fingerprint(provider: &Provider) -> Result<[u8; 32], AppError> {
    // Provider metadata contains HashMaps. Their iteration order can change on
    // every database read without any actual configuration change. Canonicalize
    // object keys recursively, while preserving the order of arrays.
    let mut value =
        serde_json::to_value(provider).map_err(|_| input_error("无法创建供应商配置快照"))?;
    value.sort_all_objects();
    let encoded = serde_json::to_vec(&value).map_err(|_| input_error("无法创建供应商配置快照"))?;
    Ok(Sha256::digest(encoded).into())
}

fn at<'a>(v: &'a Value, paths: &[&str]) -> Option<&'a str> {
    paths.iter().find_map(|p| {
        v.pointer(p)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}

fn truthy(v: &Value) -> bool {
    v.as_bool() == Some(true) || matches!(v.as_str(), Some("true" | "1"))
}

fn infer_protocol(
    app: &AppType,
    provider: &Provider,
    model: &str,
    wire: Option<&str>,
) -> Result<ValidationProtocol, AppError> {
    use ValidationProtocol::*;
    let model_api = (app == &AppType::Pi)
        .then(|| {
            provider
                .settings_config
                .get("models")
                .and_then(Value::as_array)
                .and_then(|items| {
                    items
                        .iter()
                        .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
                })
                .and_then(|m| m.get("api"))
                .and_then(Value::as_str)
        })
        .flatten();
    let explicit = model_api
        .or_else(|| provider.meta.as_ref().and_then(|m| m.api_format.as_deref()))
        .or_else(|| {
            at(
                &provider.settings_config,
                &["/api_format", "/apiFormat", "/api", "/options/api"],
            )
        })
        .or(wire);
    if let Some(format) = explicit {
        return match format.to_lowercase().as_str() {
            "openai_chat" | "openai" | "chat" | "chat_completions" | "openai-completions" => {
                Ok(OpenaiChat)
            }
            "openai_responses" | "responses" | "openai-responses" => Ok(OpenaiResponses),
            "anthropic" | "anthropic_messages" | "anthropic-messages" => Ok(Anthropic),
            "gemini" | "gemini_native" | "google" | "google-generative-ai" => Ok(Gemini),
            _ => Err(input_error("供应商声明的协议不在本次支持范围内")),
        };
    }
    Ok(match app {
        AppType::Claude | AppType::ClaudeDesktop => {
            match crate::proxy::providers::get_claude_api_format(provider) {
                "openai_chat" => OpenaiChat,
                "openai_responses" => OpenaiResponses,
                "gemini_native" => Gemini,
                _ => Anthropic,
            }
        }
        AppType::Codex | AppType::GrokBuild => OpenaiResponses,
        AppType::Gemini => Gemini,
        _ => OpenaiChat,
    })
}

fn validate_endpoint(raw: &str, key: &str) -> Result<String, AppError> {
    let url = Url::parse(raw).map_err(|_| input_error("Base URL 无效"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || raw.len() > 2048
        || raw.contains(key)
        || raw.to_ascii_lowercase().contains(
            &url::form_urlencoded::byte_serialize(key.as_bytes())
                .collect::<String>()
                .to_ascii_lowercase(),
        )
        || raw.chars().any(char::is_control)
    {
        return Err(input_error(
            "Base URL 必须是 HTTP(S) 地址，且不得包含凭据、查询参数或片段",
        ));
    }
    let host = url.host_str().unwrap_or_default().to_lowercase();
    if host == "chatgpt.com"
        || host.ends_with(".githubcopilot.com")
        || host == "githubcopilot.com"
        || url.path().contains("backend-api")
        || url.path().contains("oauth")
    {
        return Err(input_error("订阅或 OAuth 端点不支持本次 API Key 验证"));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Used on every untrusted string before it can enter IPC or history. Never
/// persists response headers, raw bodies, signatures or thinking text.
pub(super) fn redact(text: &str, keys: &[&str], max_chars: usize) -> String {
    let mut safe = text.to_string();
    for key in keys.iter().filter(|key| !key.is_empty()) {
        safe = safe.replace(key, "[redacted]");
        let encoded: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        safe = safe.replace(&encoded, "[redacted]");
    }
    static SECRET: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)(?:bearer\s+[^\s\x22]+|sk-[a-z0-9_-]{8,}|AIza[a-z0-9_-]{12,})")
            .expect("constant regex")
    });
    SECRET
        .replace_all(&safe, "[redacted]")
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}
