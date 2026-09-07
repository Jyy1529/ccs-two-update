use crate::{app_config::AppType, error::AppError};
use std::path::{Path, PathBuf};

pub(crate) fn app_roots(app: &AppType) -> Result<Vec<PathBuf>, AppError> {
    let root = match app {
        AppType::Claude => crate::config::get_claude_config_dir(),
        AppType::ClaudeDesktop => return crate::claude_desktop_config::management_roots(),
        AppType::Codex => crate::codex_config::get_codex_config_dir(),
        AppType::Gemini => crate::gemini_config::get_gemini_dir(),
        AppType::GrokBuild => crate::grok_config::get_grok_config_dir(),
        AppType::OpenCode => crate::opencode_config::get_opencode_dir(),
        AppType::OpenClaw => crate::openclaw_config::get_openclaw_dir(),
        AppType::Hermes => crate::hermes_config::get_hermes_dir(),
        AppType::DeepSeek => crate::deepseek_config::get_deepseek_dir(),
        AppType::Pi => crate::pi_config::get_pi_agent_dir()?,
    };
    Ok(vec![root])
}
pub(crate) fn app_files(app: &AppType) -> Result<Vec<PathBuf>, AppError> {
    let mut files = match app {
        AppType::Claude => vec![crate::config::get_claude_settings_path(), crate::config::get_claude_mcp_path()],
        AppType::ClaudeDesktop => crate::claude_desktop_config::management_files()?,
        AppType::Codex => vec![crate::codex_config::get_codex_auth_path(), crate::codex_config::get_codex_config_path(), crate::codex_config::get_codex_config_dir().join(crate::codex_config::CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME)],
        AppType::Gemini => vec![crate::gemini_config::get_gemini_env_path(), crate::gemini_config::get_gemini_settings_path()],
        AppType::GrokBuild => vec![crate::grok_config::get_grok_config_path()],
        AppType::OpenCode => vec![crate::opencode_config::get_opencode_config_path()],
        AppType::OpenClaw => vec![crate::openclaw_config::get_openclaw_config_path()],
        AppType::Hermes => vec![crate::hermes_config::get_hermes_config_path()],
        AppType::DeepSeek => vec![crate::deepseek_config::get_deepseek_settings_path(), crate::deepseek_config::get_deepseek_dir().join(".credentials.yaml")],
        AppType::Pi => vec![crate::pi_config::get_pi_models_path()?, crate::pi_config::get_pi_settings_path()?],
    };
    if !matches!(app, AppType::ClaudeDesktop) { files.push(crate::prompt_files::prompt_file_path(app)?); }
    Ok(files)
}

fn resolved(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut tail = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else { break; };
        tail.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else { break; };
        ancestor = parent;
    }
    let mut result = ancestor.canonicalize().unwrap_or_else(|_| ancestor.to_path_buf());
    for name in tail.into_iter().rev() { result.push(name); }
    result
}
pub(crate) fn paths_overlap(left: &Path, right: &Path) -> bool {
    let overlaps = |a: &Path, b: &Path| crate::config::path_is_within(a, b) || crate::config::path_is_within(b, a);
    overlaps(left, right) || overlaps(&resolved(left), &resolved(right))
}
pub(crate) fn owners_of_path(path: &Path) -> Result<Vec<AppType>, AppError> {
    let mut owners = Vec::new();
    for app in AppType::all() {
        let in_root = app_roots(&app)?.iter().any(|root| crate::config::path_is_within(root, path) || crate::config::path_is_within(&resolved(root), &resolved(path)));
        let exact = app_files(&app)?.iter().any(|file| file == path || resolved(file) == resolved(path));
        if in_root || exact { owners.push(app); }
    }
    Ok(owners)
}
