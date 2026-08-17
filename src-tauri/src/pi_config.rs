//! Pi coding agent 官方配置投影。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::{read_json_file, write_json_file};
use crate::error::AppError;

const MODELS_FILE: &str = "models.json";
const SETTINGS_FILE: &str = "settings.json";
const DEFAULT_API: &str = "openai-completions";
const MANAGED_PROVIDER_PREFIX: &str = "cc-switch-";

/// 解析顺序：CC Switch 覆盖目录、Pi 官方环境变量、`~/.pi/agent`。
pub fn get_pi_dir() -> PathBuf {
    if let Some(override_dir) = crate::settings::get_pi_override_dir() {
        return override_dir;
    }
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    crate::config::get_home_dir().join(".pi").join("agent")
}

pub fn get_pi_models_path() -> PathBuf {
    get_pi_dir().join(MODELS_FILE)
}

pub fn get_pi_settings_path() -> PathBuf {
    get_pi_dir().join(SETTINGS_FILE)
}

pub fn config_exists() -> bool {
    get_pi_models_path().exists() || get_pi_settings_path().exists()
}

#[derive(Debug)]
struct ProviderConfig {
    base_url: String,
    api_key: Option<String>,
    model: String,
    api: String,
}

fn optional_string(
    object: &Map<String, Value>,
    keys: &[&str],
    field_name: &str,
) -> Result<Option<String>, AppError> {
    for key in keys {
        if let Some(value) = object.get(*key) {
            let value = value.as_str().ok_or_else(|| {
                AppError::localized(
                    "pi.settings.field_not_string",
                    format!("Pi {field_name} 必须是字符串"),
                    format!("Pi {field_name} must be a string"),
                )
            })?;
            return Ok(Some(value.trim().to_string()));
        }
    }
    Ok(None)
}

fn model_from_settings(object: &Map<String, Value>) -> Result<Option<String>, AppError> {
    if let Some(model) = optional_string(object, &["model"], "model")? {
        if !model.is_empty() {
            return Ok(Some(model));
        }
    }
    let Some(models) = object.get("models") else {
        return Ok(None);
    };
    let models = models.as_array().ok_or_else(|| {
        AppError::localized(
            "pi.settings.models_not_array",
            "Pi models 必须是数组",
            "Pi models must be an array",
        )
    })?;
    Ok(models
        .iter()
        .find_map(|model| model.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string))
}

fn parse_provider_config(settings: &Value) -> Result<ProviderConfig, AppError> {
    let object = settings.as_object().ok_or_else(|| {
        AppError::localized(
            "pi.settings.not_object",
            "Pi 配置必须是 JSON 对象",
            "Pi configuration must be a JSON object",
        )
    })?;
    let base_url = optional_string(object, &["baseUrl", "baseURL", "base_url"], "baseUrl")?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::localized(
                "pi.settings.base_url_missing",
                "Pi baseUrl 不能为空",
                "Pi baseUrl must not be empty",
            )
        })?;
    let model = model_from_settings(object)?.ok_or_else(|| {
        AppError::localized(
            "pi.settings.model_missing",
            "Pi model 不能为空",
            "Pi model must not be empty",
        )
    })?;
    let api = optional_string(object, &["api"], "api")?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_string());
    let api_key = optional_string(object, &["apiKey", "api_key"], "apiKey")?
        .filter(|value| !value.is_empty());

    Ok(ProviderConfig {
        base_url,
        api_key,
        model,
        api,
    })
}

fn managed_provider_id(provider_id: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_was_dash = false;

    for ch in provider_id.trim().chars().flat_map(char::to_lowercase) {
        let normalized = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
            ch
        } else {
            '-'
        };
        if normalized == '-' {
            if sanitized.is_empty() || previous_was_dash {
                continue;
            }
            previous_was_dash = true;
        } else {
            previous_was_dash = false;
        }
        sanitized.push(normalized);
    }

    while sanitized.ends_with('-') {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push_str("provider");
    }
    format!("{MANAGED_PROVIDER_PREFIX}{sanitized}")
}

fn read_json_object(path: &Path, label: &str) -> Result<Map<String, Value>, AppError> {
    if !path.exists() {
        return Ok(Map::new());
    }
    read_json_file::<Value>(path)?
        .as_object()
        .cloned()
        .ok_or_else(|| {
            AppError::localized(
                "pi.config.root_not_object",
                format!("{label} 根节点必须是 JSON 对象"),
                format!("The {label} root must be a JSON object"),
            )
        })
}

fn providers_section_mut(
    root: &mut Map<String, Value>,
) -> Result<&mut Map<String, Value>, AppError> {
    let providers = root
        .entry("providers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    providers.as_object_mut().ok_or_else(|| {
        AppError::localized(
            "pi.config.providers_not_object",
            "Pi models.json 的 providers 必须是 JSON 对象",
            "The providers field in Pi models.json must be a JSON object",
        )
    })
}

fn ensure_model(models: &mut Vec<Value>, model_id: &str) {
    let exists = models.iter().any(|model| {
        model
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id == model_id)
    });
    if !exists {
        models.push(Value::Object(Map::from_iter([(
            "id".to_string(),
            Value::String(model_id.to_string()),
        )])));
    }
}

fn build_provider_entry(
    existing: Option<&Value>,
    settings: &Value,
    provider: &ProviderConfig,
) -> Result<Value, AppError> {
    let mut entry = match existing {
        Some(value) => value.as_object().cloned().ok_or_else(|| {
            AppError::localized(
                "pi.config.managed_provider_not_object",
                "Pi 受管 provider 配置必须是 JSON 对象",
                "The managed Pi provider configuration must be a JSON object",
            )
        })?,
        None => Map::new(),
    };
    let settings = settings
        .as_object()
        .expect("provider settings were validated");

    for (key, value) in settings {
        if matches!(
            key.as_str(),
            "baseUrl" | "baseURL" | "base_url" | "apiKey" | "api_key" | "model" | "models" | "api"
        ) {
            continue;
        }
        entry.insert(key.clone(), value.clone());
    }

    let mut models = if let Some(models) = settings.get("models") {
        models.as_array().cloned().ok_or_else(|| {
            AppError::localized(
                "pi.settings.models_not_array",
                "Pi models 必须是数组",
                "Pi models must be an array",
            )
        })?
    } else {
        entry
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    ensure_model(&mut models, &provider.model);

    entry.remove("baseURL");
    entry.remove("base_url");
    entry.remove("api_key");
    entry.remove("model");
    entry.insert(
        "baseUrl".to_string(),
        Value::String(provider.base_url.clone()),
    );
    entry.insert("api".to_string(), Value::String(provider.api.clone()));
    entry.insert("models".to_string(), Value::Array(models));
    if let Some(api_key) = &provider.api_key {
        entry.insert("apiKey".to_string(), Value::String(api_key.clone()));
    }
    Ok(Value::Object(entry))
}

fn write_provider_live_at(dir: &Path, provider_id: &str, settings: &Value) -> Result<(), AppError> {
    let provider = parse_provider_config(settings)?;
    let models_path = dir.join(MODELS_FILE);
    let settings_path = dir.join(SETTINGS_FILE);

    // Parse and validate both files before the first write so malformed user
    // configuration cannot leave only one side of the Pi selection updated.
    let mut models_root = read_json_object(&models_path, "Pi models.json")?;
    let mut settings_root = read_json_object(&settings_path, "Pi settings.json")?;
    let managed_id = managed_provider_id(provider_id);
    let providers = providers_section_mut(&mut models_root)?;
    let entry = build_provider_entry(providers.get(&managed_id), settings, &provider)?;
    providers.insert(managed_id.clone(), entry);

    settings_root.insert("defaultProvider".to_string(), Value::String(managed_id));
    settings_root.insert(
        "defaultModel".to_string(),
        Value::String(provider.model.clone()),
    );

    write_json_file(&models_path, &Value::Object(models_root))?;
    write_json_file(&settings_path, &Value::Object(settings_root))
}

pub fn validate_provider_settings(settings: &Value) -> Result<(), AppError> {
    parse_provider_config(settings).map(|_| ())
}

pub fn write_provider_live(provider_id: &str, settings: &Value) -> Result<(), AppError> {
    write_provider_live_at(&get_pi_dir(), provider_id, settings)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a str, AppError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::localized(
                "pi.config.required_field_missing",
                format!("Pi {label} 不能为空"),
                format!("Pi {label} must not be empty"),
            )
        })
}

fn read_provider_live_at(dir: &Path) -> Result<Value, AppError> {
    let models_path = dir.join(MODELS_FILE);
    let settings_path = dir.join(SETTINGS_FILE);
    if !models_path.exists() || !settings_path.exists() {
        return Err(AppError::localized(
            "pi.config.missing",
            "Pi models.json 或 settings.json 不存在",
            "Pi models.json or settings.json was not found",
        ));
    }

    let models_root = read_json_object(&models_path, "Pi models.json")?;
    let settings_root = read_json_object(&settings_path, "Pi settings.json")?;
    let default_provider = required_string(&settings_root, "defaultProvider", "defaultProvider")?;
    let default_model = required_string(&settings_root, "defaultModel", "defaultModel")?;
    let providers = models_root
        .get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            AppError::localized(
                "pi.config.providers_missing",
                "Pi models.json 的 providers 配置不存在",
                "The providers configuration is missing from Pi models.json",
            )
        })?;
    let mut result = providers
        .get(default_provider)
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| {
            AppError::localized(
                "pi.config.default_provider_missing",
                format!("Pi 默认 provider '{default_provider}' 不存在"),
                format!("The default Pi provider '{default_provider}' does not exist"),
            )
        })?;

    let base_url = required_string(&result, "baseUrl", "baseUrl")?.to_string();
    let api = result
        .get("api")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_API)
        .to_string();
    let api_key = result
        .get("apiKey")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    result.insert("baseUrl".to_string(), Value::String(base_url));
    result.insert("api".to_string(), Value::String(api));
    result.insert("apiKey".to_string(), Value::String(api_key));
    result.insert(
        "model".to_string(),
        Value::String(default_model.to_string()),
    );
    Ok(Value::Object(result))
}

pub fn read_provider_live() -> Result<Value, AppError> {
    read_provider_live_at(&get_pi_dir())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn official_projection_preserves_existing_providers_settings_and_model_metadata() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join(MODELS_FILE),
            serde_json::to_vec_pretty(&json!({
                "telemetry": false,
                "providers": {
                    "user-provider": { "baseUrl": "https://user.example/v1", "models": [{"id": "user-model"}] },
                    "cc-switch-my-provider": {
                        "baseUrl": "https://old.example/v1",
                        "apiKey": "old-key",
                        "headers": {"X-Test": "kept"},
                        "models": [{"id": "old-model", "contextWindow": 32000}]
                    }
                }
            }))
            .expect("serialize models"),
        )
        .expect("seed models");
        fs::write(
            temp.path().join(SETTINGS_FILE),
            serde_json::to_vec_pretty(&json!({"theme": "dark", "defaultProvider": "user-provider", "defaultModel": "user-model"}))
                .expect("serialize settings"),
        )
        .expect("seed settings");

        write_provider_live_at(
            temp.path(),
            "My Provider",
            &json!({
                "baseUrl": "https://gateway.example/v1",
                "apiKey": "new-key",
                "model": "new-model",
                "api": "openai-responses",
                "compat": {"supportsDeveloperRole": true}
            }),
        )
        .expect("write projection");

        let models: Value = read_json_file(&temp.path().join(MODELS_FILE)).expect("read models");
        assert_eq!(models["telemetry"], false);
        assert_eq!(
            models["providers"]["user-provider"]["models"][0]["id"],
            "user-model"
        );
        let managed = &models["providers"]["cc-switch-my-provider"];
        assert_eq!(managed["baseUrl"], "https://gateway.example/v1");
        assert_eq!(managed["apiKey"], "new-key");
        assert_eq!(managed["api"], "openai-responses");
        assert_eq!(managed["headers"]["X-Test"], "kept");
        assert_eq!(managed["compat"]["supportsDeveloperRole"], true);
        assert_eq!(managed["models"][0]["contextWindow"], 32000);
        assert!(managed["models"]
            .as_array()
            .expect("models array")
            .iter()
            .any(|model| model["id"] == "new-model"));

        let settings: Value =
            read_json_file(&temp.path().join(SETTINGS_FILE)).expect("read settings");
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["defaultProvider"], "cc-switch-my-provider");
        assert_eq!(settings["defaultModel"], "new-model");

        let live = read_provider_live_at(temp.path()).expect("read live");
        assert_eq!(live["baseUrl"], "https://gateway.example/v1");
        assert_eq!(live["apiKey"], "new-key");
        assert_eq!(live["model"], "new-model");
        assert_eq!(live["api"], "openai-responses");
    }

    #[test]
    fn empty_api_key_keeps_existing_managed_provider_key() {
        let temp = tempdir().expect("tempdir");
        write_provider_live_at(
            temp.path(),
            "provider",
            &json!({"baseUrl": "https://example.com/v1", "apiKey": "kept-key", "model": "one"}),
        )
        .expect("initial write");
        write_provider_live_at(
            temp.path(),
            "provider",
            &json!({"baseUrl": "https://example.com/v1", "apiKey": "", "model": "two"}),
        )
        .expect("write without key");

        let live = read_provider_live_at(temp.path()).expect("read live");
        assert_eq!(live["apiKey"], "kept-key");
        assert_eq!(live["model"], "two");
    }

    #[test]
    fn invalid_settings_root_is_detected_before_models_are_written() {
        let temp = tempdir().expect("tempdir");
        let models_path = temp.path().join(MODELS_FILE);
        fs::write(&models_path, "{\"providers\":{}}\n").expect("seed models");
        fs::write(temp.path().join(SETTINGS_FILE), "[]\n").expect("seed invalid settings");

        write_provider_live_at(
            temp.path(),
            "provider",
            &json!({"baseUrl": "https://example.com/v1", "model": "model"}),
        )
        .expect_err("array root must fail");
        assert_eq!(
            fs::read_to_string(models_path).expect("read original models"),
            "{\"providers\":{}}\n"
        );
    }
}
