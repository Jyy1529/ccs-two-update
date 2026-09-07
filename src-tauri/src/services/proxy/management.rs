//! Read-only release preparation. Never restore an entire stale Live backup.
use super::*;
use crate::app_management::ReleaseEdit;
use crate::services::config_guard::release_edit;

impl ProxyService {
    pub(crate) async fn prepare_management_release(&self, app: &AppType) -> Result<Vec<ReleaseEdit>, String> {
        if !matches!(app, AppType::Claude | AppType::Codex | AppType::Gemini | AppType::GrokBuild) { return Ok(vec![]); }
        let enabled = self.db.get_proxy_config_for_app(app.as_str()).await.map_err(|e| e.to_string())?.enabled;
        if !enabled && !self.detect_takeover_in_live_config_for_app(app) { return Ok(vec![]); }
        let backup = self.db.get_live_backup(app.as_str()).await.map_err(|e| e.to_string())?
            .ok_or_else(|| "Takeover has no trustworthy backup. Ordinary writes will stop; release requires manual review.".to_string())?;
        let before: Value = serde_json::from_str(&backup.original_config).map_err(|_| "Invalid takeover backup".to_string())?;
        let mut installed = before.clone();
        let (url, codex_url) = self.build_proxy_urls().await?;
        match app {
            AppType::Claude => {
                let provider = self.require_current_provider_for_app(app)?;
                let provider = self.claude_provider_with_effective_settings(&provider)?;
                Self::apply_claude_takeover_fields_for_provider(&mut installed, &url, &provider);
            }
            AppType::Codex => {
                let provider = self.require_current_provider_for_app(app)?;
                Self::apply_codex_takeover_fields_for_provider(&mut installed, &codex_url, &provider)?;
            }
            AppType::Gemini => {
                if !installed.get("env").is_some_and(Value::is_object) { installed["env"] = json!({}); }
                installed["env"]["GOOGLE_GEMINI_BASE_URL"] = json!(url);
                installed["env"]["GEMINI_API_KEY"] = json!(PROXY_TOKEN_PLACEHOLDER);
            }
            AppType::GrokBuild => Self::apply_grok_takeover_fields(&mut installed, &format!("{}/grokbuild/v1", url.trim_end_matches('/')))?,
            _ => unreachable!(),
        }
        let mut edits = Vec::new();
        let mut add = |path: PathBuf, old: String, expected: String| -> Result<(), String> {
            if let Some(edit) = release_edit(path, &old, &expected).map_err(|e| e.to_string())? { edits.push(edit); }
            Ok(())
        };
        match app {
            AppType::Claude => add(get_claude_settings_path(), before.to_string(), installed.to_string())?,
            AppType::Codex => {
                add(crate::codex_config::get_codex_config_path(), before["config"].as_str().unwrap_or("").into(), installed["config"].as_str().unwrap_or("").into())?;
                // Official OAuth tokens can rotate while ccs runs. Only the API-key
                // placeholder is ours; never roll back the OAuth login object.
                if before.get("auth").is_some() && installed.pointer("/auth/OPENAI_API_KEY").and_then(Value::as_str) == Some(PROXY_TOKEN_PLACEHOLDER) {
                    let old = json!({"OPENAI_API_KEY": before.pointer("/auth/OPENAI_API_KEY").cloned().unwrap_or(Value::Null)});
                    add(crate::codex_config::get_codex_auth_path(), old.to_string(), json!({"OPENAI_API_KEY":PROXY_TOKEN_PLACEHOLDER}).to_string())?;
                }
            }
            AppType::Gemini => {
                let env = |value: &Value| crate::gemini_config::json_to_env(value).map(|map| crate::gemini_config::serialize_env_file(&map)).map_err(|e| e.to_string());
                add(crate::gemini_config::get_gemini_env_path(), env(&before)?, env(&installed)?)?;
            }
            AppType::GrokBuild => add(crate::grok_config::get_grok_config_path(), before["config"].as_str().unwrap_or("").into(), installed["config"].as_str().unwrap_or("").into())?,
            _ => unreachable!(),
        }
        Ok(edits)
    }

    /// The caller holds the proxy transaction and the app switch lock. No files
    /// are written here; unrelated applications keep their shared proxy service.
    pub(crate) async fn finish_management_release(&self, app: &AppType) -> Result<(), String> {
        if !matches!(app, AppType::Claude | AppType::Codex | AppType::Gemini | AppType::GrokBuild) { return Ok(()); }
        let mut config = self.db.get_proxy_config_for_app(app.as_str()).await.map_err(|e| e.to_string())?;
        config.enabled = false;
        config.auto_failover_enabled = false;
        self.db.update_proxy_config_for_app(config).await.map_err(|e| e.to_string())?;
        // Keep the backup until successful release, including crash recovery.
        self.db.delete_live_backup(app.as_str()).await.map_err(|e| e.to_string())?;
        Ok(())
    }
}
