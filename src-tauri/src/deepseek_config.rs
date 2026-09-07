//! DeepSeek Harness 官方配置投影。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value as YamlValue};

use crate::config::atomic_write;
use crate::error::AppError;

const SETTINGS_FILE: &str = "settings.yaml";
const CREDENTIALS_FILE: &str = ".credentials.yaml";
const LLM_SECTION: &str = "llm-deepseek";
const DEFAULT_MODEL_SECTION: &str = "agent-default-model";
const PROVIDER_ROUTE: &str = "deepseek-official";
const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";
const BUILTIN_MODELS: [&str; 2] = ["deepseek-v4-flash", "deepseek-v4-pro"];

/// 获取 DeepSeek 配置目录
///
/// 解析顺序：
///   1. CCS 设置 `deepseek_config_dir`（显式覆盖）
///   2. `DSH_HOME`
///   3. 默认 `~/.dsh`
pub fn get_deepseek_dir() -> PathBuf {
    if let Some(override_dir) = crate::settings::get_deepseek_override_dir() {
        return override_dir;
    }
    if let Some(dir) = std::env::var_os("DSH_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    crate::config::get_home_dir().join(".dsh")
}

pub fn get_deepseek_settings_path() -> PathBuf {
    get_deepseek_dir().join(SETTINGS_FILE)
}

pub fn config_exists() -> bool {
    get_deepseek_settings_path().exists()
}

#[derive(Debug)]
struct ProviderConfig {
    base_url: String,
    api_key: Option<String>,
    model: String,
}

fn optional_string(
    object: &serde_json::Map<String, JsonValue>,
    keys: &[&str],
    field_name: &str,
) -> Result<Option<String>, AppError> {
    for key in keys {
        if let Some(value) = object.get(*key) {
            let value = value.as_str().ok_or_else(|| {
                AppError::localized(
                    "deepseek.settings.field_not_string",
                    format!("DeepSeek {field_name} 必须是字符串"),
                    format!("DeepSeek {field_name} must be a string"),
                )
            })?;
            return Ok(Some(value.trim().to_string()));
        }
    }
    Ok(None)
}

fn model_from_settings(
    object: &serde_json::Map<String, JsonValue>,
) -> Result<Option<String>, AppError> {
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
            "deepseek.settings.models_not_array",
            "DeepSeek models 必须是数组",
            "DeepSeek models must be an array",
        )
    })?;
    Ok(models
        .iter()
        .find_map(|model| model.get("id").and_then(JsonValue::as_str))
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string))
}

fn parse_provider_config(settings: &JsonValue) -> Result<ProviderConfig, AppError> {
    let object = settings.as_object().ok_or_else(|| {
        AppError::localized(
            "deepseek.settings.not_object",
            "DeepSeek 配置必须是 JSON 对象",
            "DeepSeek configuration must be a JSON object",
        )
    })?;
    let base_url = optional_string(object, &["baseUrl", "baseURL", "base_url"], "baseUrl")?
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let model = model_from_settings(object)?.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let api_key = optional_string(object, &["apiKey", "api_key"], "apiKey")?
        .filter(|value| !value.is_empty());

    Ok(ProviderConfig {
        base_url,
        api_key,
        model,
    })
}

fn yaml_key(key: &str) -> YamlValue {
    YamlValue::String(key.to_string())
}

fn read_yaml_mapping(path: &Path, label: &str) -> Result<Mapping, AppError> {
    if !path.exists() {
        return Ok(Mapping::new());
    }
    let source = fs::read_to_string(path).map_err(|error| AppError::io(path, error))?;
    if source.trim().is_empty() {
        return Ok(Mapping::new());
    }
    let value = serde_yaml::from_str::<YamlValue>(&source).map_err(|error| {
        AppError::localized(
            "deepseek.config.invalid_yaml",
            format!("{label} YAML 无效：{error}"),
            format!("Invalid {label} YAML: {error}"),
        )
    })?;
    value.as_mapping().cloned().ok_or_else(|| {
        AppError::localized(
            "deepseek.config.root_not_mapping",
            format!("{label} 的 YAML 根节点必须是映射"),
            format!("The {label} YAML root must be a mapping"),
        )
    })
}

fn mapping_section_mut<'a>(
    root: &'a mut Mapping,
    section: &str,
) -> Result<&'a mut Mapping, AppError> {
    let key = yaml_key(section);
    if !root.contains_key(&key) {
        root.insert(key.clone(), YamlValue::Mapping(Mapping::new()));
    }
    root.get_mut(&key)
        .and_then(YamlValue::as_mapping_mut)
        .ok_or_else(|| {
            AppError::localized(
                "deepseek.config.section_not_mapping",
                format!("DeepSeek settings.yaml 中的 {section} 必须是映射"),
                format!("{section} in DeepSeek settings.yaml must be a mapping"),
            )
        })
}

fn apply_extra_settings(section: &mut Mapping, settings: &JsonValue) -> Result<(), AppError> {
    let Some(object) = settings.as_object() else {
        return Ok(());
    };
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "baseUrl" | "base_url" | "apiKey" | "api_key" | "model" | "api"
        ) {
            continue;
        }
        let value = serde_yaml::to_value(value).map_err(|error| {
            AppError::localized(
                "deepseek.config.yaml_serialize_failed",
                format!("DeepSeek 配置字段 {key} 无法转换为 YAML：{error}"),
                format!("DeepSeek field {key} cannot be converted to YAML: {error}"),
            )
        })?;
        section.insert(yaml_key(key), value);
    }
    Ok(())
}

fn model_entry(model: &str) -> YamlValue {
    let mut entry = Mapping::new();
    entry.insert(yaml_key("id"), YamlValue::String(model.to_string()));
    YamlValue::Mapping(entry)
}

fn ensure_model(section: &mut Mapping, model: &str) -> Result<(), AppError> {
    let models_key = yaml_key("models");
    if !section.contains_key(&models_key) {
        section.insert(
            models_key.clone(),
            YamlValue::Sequence(BUILTIN_MODELS.iter().map(|id| model_entry(id)).collect()),
        );
    }
    let models = section
        .get_mut(&models_key)
        .and_then(YamlValue::as_sequence_mut)
        .ok_or_else(|| {
            AppError::localized(
                "deepseek.config.models_not_array",
                "DeepSeek llm-deepseek.models 必须是数组",
                "DeepSeek llm-deepseek.models must be an array",
            )
        })?;
    let exists = models.iter().any(|entry| {
        entry
            .as_mapping()
            .and_then(|entry| entry.get(yaml_key("id")))
            .and_then(YamlValue::as_str)
            == Some(model)
    });
    if !exists {
        models.push(model_entry(model));
    }
    Ok(())
}

fn credential_reference(section: &mut Mapping) -> Result<String, AppError> {
    let key = yaml_key("apiKeyEnv");
    let reference = match section.get(&key) {
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        None => None,
    }
    .unwrap_or(DEFAULT_API_KEY_ENV)
    .to_string();
    let mut chars = reference.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    if !valid {
        return Err(AppError::localized(
            "deepseek.config.invalid_api_key_env",
            "DeepSeek apiKeyEnv 必须是有效的环境变量名",
            "DeepSeek apiKeyEnv must be a valid environment variable name",
        ));
    }
    section.insert(key, YamlValue::String(reference.clone()));
    Ok(reference)
}

fn write_yaml_mapping(path: &Path, mapping: &Mapping) -> Result<(), AppError> {
    let source = serde_yaml::to_string(&YamlValue::Mapping(mapping.clone())).map_err(|error| {
        AppError::localized(
            "deepseek.config.yaml_serialize_failed",
            format!("DeepSeek YAML 序列化失败：{error}"),
            format!("Failed to serialize DeepSeek YAML: {error}"),
        )
    })?;
    atomic_write(path, source.as_bytes())
}

#[cfg(unix)]
fn secure_secret_file(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::io(path, error))
}

#[cfg(not(unix))]
fn secure_secret_file(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

fn write_provider_live_at(dir: &Path, settings: &JsonValue) -> Result<(), AppError> {
    let provider = parse_provider_config(settings)?;
    let settings_path = dir.join(SETTINGS_FILE);
    let credentials_path = dir.join(CREDENTIALS_FILE);
    let mut root = read_yaml_mapping(&settings_path, "DeepSeek settings.yaml")?;

    let credential_name = {
        let section = mapping_section_mut(&mut root, LLM_SECTION)?;
        apply_extra_settings(section, settings)?;
        section.insert(
            yaml_key("baseURL"),
            YamlValue::String(provider.base_url.clone()),
        );
        ensure_model(section, &provider.model)?;
        credential_reference(section)?
    };
    let selection = mapping_section_mut(&mut root, DEFAULT_MODEL_SECTION)?;
    selection.insert(
        yaml_key("provider"),
        YamlValue::String(PROVIDER_ROUTE.to_string()),
    );
    selection.insert(yaml_key("model"), YamlValue::String(provider.model.clone()));

    let credentials = if let Some(api_key) = provider.api_key {
        let mut credentials = read_yaml_mapping(&credentials_path, "DeepSeek credentials")?;
        credentials.insert(yaml_key(&credential_name), YamlValue::String(api_key));
        Some(credentials)
    } else {
        None
    };

    if dir == get_deepseek_dir() {
        let mut files = vec![(settings_path, serde_yaml::to_string(&root).map_err(|error| AppError::Config(error.to_string()))?.into_bytes())];
        if let Some(credentials) = credentials {
            files.push((credentials_path, serde_yaml::to_string(&credentials).map_err(|error| AppError::Config(error.to_string()))?.into_bytes()));
        }
        crate::services::config_guard::commit_files(&crate::app_config::AppType::DeepSeek, &files)
    } else {
        // This private path-parameterized helper also serves isolated fixtures;
        // public native projection always uses the registered directory above.
        if let Some(credentials) = credentials {
            write_yaml_mapping(&credentials_path, &credentials)?;
            secure_secret_file(&credentials_path)?;
        }
        write_yaml_mapping(&settings_path, &root)
    }
}

pub fn validate_provider_settings(settings: &JsonValue) -> Result<(), AppError> {
    parse_provider_config(settings).map(|_| ())
}

pub fn write_provider_live(settings: &JsonValue) -> Result<(), AppError> {
    write_provider_live_at(&get_deepseek_dir(), settings)
}

fn read_provider_live_at(dir: &Path) -> Result<JsonValue, AppError> {
    let settings_path = dir.join(SETTINGS_FILE);
    if !settings_path.exists() {
        return Err(AppError::localized(
            "deepseek.config.missing",
            "DeepSeek settings.yaml 不存在",
            "DeepSeek settings.yaml was not found",
        ));
    }
    let root = read_yaml_mapping(&settings_path, "DeepSeek settings.yaml")?;
    let llm = root
        .get(yaml_key(LLM_SECTION))
        .and_then(YamlValue::as_mapping)
        .cloned()
        .unwrap_or_default();
    let selection = root
        .get(yaml_key(DEFAULT_MODEL_SECTION))
        .and_then(YamlValue::as_mapping)
        .ok_or_else(|| {
            AppError::localized(
                "deepseek.config.selection_missing",
                "DeepSeek agent-default-model 配置不存在",
                "DeepSeek agent-default-model configuration is missing",
            )
        })?;
    if selection
        .get(yaml_key("provider"))
        .and_then(YamlValue::as_str)
        != Some(PROVIDER_ROUTE)
    {
        return Err(AppError::localized(
            "deepseek.config.provider_not_active",
            "DeepSeek 当前模型未使用 deepseek-official 供应商",
            "The active DeepSeek model does not use the deepseek-official provider",
        ));
    }
    let model = selection
        .get(yaml_key("model"))
        .and_then(YamlValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::localized(
                "deepseek.config.model_missing",
                "DeepSeek 当前模型为空",
                "The active DeepSeek model is empty",
            )
        })?;

    let mut result = serde_json::to_value(YamlValue::Mapping(llm.clone())).map_err(|error| {
        AppError::localized(
            "deepseek.config.json_conversion_failed",
            format!("DeepSeek 配置无法转换为 JSON：{error}"),
            format!("DeepSeek configuration cannot be converted to JSON: {error}"),
        )
    })?;
    let object = result
        .as_object_mut()
        .expect("YAML mapping converts to object");
    let base_url = object
        .remove("baseURL")
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let credential_name = llm
        .get(yaml_key("apiKeyEnv"))
        .and_then(YamlValue::as_str)
        .unwrap_or(DEFAULT_API_KEY_ENV);
    let credentials = read_yaml_mapping(&dir.join(CREDENTIALS_FILE), "DeepSeek credentials")?;
    let api_key = credentials
        .get(yaml_key(credential_name))
        .and_then(YamlValue::as_str)
        .unwrap_or_default();
    object.insert("baseUrl".to_string(), JsonValue::String(base_url));
    object.insert("apiKey".to_string(), JsonValue::String(api_key.to_string()));
    object.insert("model".to_string(), JsonValue::String(model.to_string()));
    Ok(result)
}

pub fn read_provider_live() -> Result<JsonValue, AppError> {
    read_provider_live_at(&get_deepseek_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn official_projection_preserves_existing_yaml_and_credentials() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join(SETTINGS_FILE),
            r#"theme:
  mode: dark
llm-deepseek:
  thinking: enabled
  models:
    - id: existing-model
      name: Existing
agent-default-model:
  reasoningEffort: high
"#,
        )
        .expect("seed settings");
        fs::write(
            temp.path().join(CREDENTIALS_FILE),
            "OTHER_KEY: other\nDEEPSEEK_API_KEY: old\n",
        )
        .expect("seed credentials");

        write_provider_live_at(
            temp.path(),
            &json!({
                "baseUrl": "https://gateway.example/v1",
                "apiKey": "new-key",
                "model": "custom-model",
                "maxTokens": 8192
            }),
        )
        .expect("write projection");

        let settings =
            read_yaml_mapping(&temp.path().join(SETTINGS_FILE), "DeepSeek settings.yaml")
                .expect("read settings");
        assert_eq!(
            settings
                .get(yaml_key("theme"))
                .and_then(YamlValue::as_mapping)
                .and_then(|theme| theme.get(yaml_key("mode")))
                .and_then(YamlValue::as_str),
            Some("dark")
        );
        let llm = settings
            .get(yaml_key(LLM_SECTION))
            .and_then(YamlValue::as_mapping)
            .expect("llm section");
        assert_eq!(
            llm.get(yaml_key("baseURL")).and_then(YamlValue::as_str),
            Some("https://gateway.example/v1")
        );
        assert_eq!(
            llm.get(yaml_key("thinking")).and_then(YamlValue::as_str),
            Some("enabled")
        );
        assert_eq!(
            llm.get(yaml_key("maxTokens")).and_then(YamlValue::as_i64),
            Some(8192)
        );
        let models = llm
            .get(yaml_key("models"))
            .and_then(YamlValue::as_sequence)
            .expect("models");
        for expected in ["existing-model", "custom-model"] {
            assert!(models.iter().any(|entry| {
                entry
                    .as_mapping()
                    .and_then(|entry| entry.get(yaml_key("id")))
                    .and_then(YamlValue::as_str)
                    == Some(expected)
            }));
        }

        let credentials =
            read_yaml_mapping(&temp.path().join(CREDENTIALS_FILE), "DeepSeek credentials")
                .expect("read credentials");
        assert_eq!(
            credentials
                .get(yaml_key("OTHER_KEY"))
                .and_then(YamlValue::as_str),
            Some("other")
        );
        assert_eq!(
            credentials
                .get(yaml_key(DEFAULT_API_KEY_ENV))
                .and_then(YamlValue::as_str),
            Some("new-key")
        );

        let live = read_provider_live_at(temp.path()).expect("read live projection");
        assert_eq!(live["baseUrl"], "https://gateway.example/v1");
        assert_eq!(live["apiKey"], "new-key");
        assert_eq!(live["model"], "custom-model");
        assert_eq!(live["maxTokens"], 8192);
    }

    #[test]
    fn empty_api_key_keeps_existing_credential() {
        let temp = tempdir().expect("tempdir");
        write_provider_live_at(
            temp.path(),
            &json!({"apiKey": "kept-key", "model": DEFAULT_MODEL}),
        )
        .expect("initial write");
        write_provider_live_at(
            temp.path(),
            &json!({"apiKey": "", "model": "deepseek-v4-pro"}),
        )
        .expect("write without key");

        let live = read_provider_live_at(temp.path()).expect("read live");
        assert_eq!(live["apiKey"], "kept-key");
        assert_eq!(live["model"], "deepseek-v4-pro");
    }

    #[test]
    fn invalid_yaml_root_is_not_overwritten() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(SETTINGS_FILE);
        fs::write(&path, "[]\n").expect("seed invalid root");

        write_provider_live_at(temp.path(), &json!({"model": DEFAULT_MODEL}))
            .expect_err("array root must fail");
        assert_eq!(fs::read_to_string(path).expect("read original"), "[]\n");
    }
}
