use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use cc_switch_lib::{update_settings, AppSettings, AppState, Database, MultiAppConfig};

/// 为测试设置隔离的 HOME 目录，避免污染真实用户数据。
pub fn ensure_test_home() -> &'static Path {
    static TEST_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    TEST_HOME.get_or_init(|| {
        let directory = tempfile::Builder::new().prefix("cc-switch-integration-").tempdir().expect("create isolated test home");
        let base = directory.path();
        // Windows 上 `dirs::home_dir()` 不受 HOME/USERPROFILE 影响（走 Known Folder API），
        // 用 CC_SWITCH_TEST_HOME 显式覆盖，以确保测试不会污染真实用户目录。
        std::env::set_var("CC_SWITCH_TEST_HOME", &base);
        std::env::set_var("HOME", &base);
        std::env::set_var("LOCALAPPDATA", base.join("AppData/Local"));
        std::env::set_var("HERMES_HOME", base.join(".hermes"));
        std::env::set_var("DSH_HOME", base.join(".dsh"));
        std::env::set_var("PI_CODING_AGENT_DIR", base.join(".pi/agent"));
        #[cfg(windows)]
        std::env::set_var("USERPROFILE", &base);
        directory
    })
    .path()
}

/// 清理测试目录中生成的配置文件与缓存。
pub fn reset_test_fs() {
    let home = ensure_test_home();
    for sub in [
        ".claude",
        ".codex",
        ".cc-switch",
        ".gemini",
        ".grok",
        ".config",
        ".openclaw",
        ".deepseek",
        ".dsh",
        ".hermes",
        ".claude-desktop",
        "AppData",
        ".pi",
        "profiles",
    ] {
        let path = home.join(sub);
        if path.exists() {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                eprintln!("failed to clean {}: {}", path.display(), err);
            }
        }
    }
    let claude_json = home.join(".claude.json");
    if claude_json.exists() {
        let _ = std::fs::remove_file(&claude_json);
    }

    // 重置内存中的设置缓存，确保测试环境不受上一次调用影响
    let _ = update_settings(AppSettings::default());
}

#[allow(dead_code)]
pub fn enable_codex_official_auth_preservation() {
    update_settings(AppSettings {
        preserve_codex_official_auth_on_switch: true,
        ..Default::default()
    })
    .expect("enable Codex official auth preservation");
}

/// 全局互斥锁，避免多测试并发写入相同的 HOME 目录。
pub fn test_mutex() -> &'static Mutex<()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX.get_or_init(|| Mutex::new(()))
}

/// 创建测试用的 AppState，包含一个空的数据库
#[allow(dead_code)]
pub fn create_test_state() -> Result<AppState, Box<dyn std::error::Error>> {
    let db = Arc::new(Database::init()?);
    Ok(AppState::new(db))
}

/// 创建测试用的 AppState，并从 MultiAppConfig 迁移数据
#[allow(dead_code)]
pub fn create_test_state_with_config(
    config: &MultiAppConfig,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let db = Arc::new(Database::init()?);
    db.migrate_from_json(config)?;
    Ok(AppState::new(db))
}

/// Explicitly establish a fixture's existing client files as a user-confirmed
/// baseline. Tests for first-run conflicts deliberately do not call this.
#[allow(dead_code)]
pub fn confirm_existing_configuration(app: cc_switch_lib::AppType) {
    let state = cc_switch_lib::get_config_guard_state(app.as_str().to_string()).expect("read fixture protection state");
    for file in state.files {
        if std::path::Path::new(&file.path).is_file() {
            let preview = cc_switch_lib::preview_config_change(app.as_str().to_string(), file.id).expect("preview fixture baseline");
            cc_switch_lib::apply_reviewed_configuration_change(&preview.id, "keep_local", None).expect("confirm fixture baseline");
        }
    }
}

#[allow(dead_code)]
pub fn approve_configuration_conflicts(app: cc_switch_lib::AppType, state: Option<&AppState>) {
    let mut count = 0;
    loop {
        let protection = cc_switch_lib::get_config_guard_state(app.as_str().to_string()).expect("read pending fixture changes");
        let Some(pending) = protection.pending_changes.first() else { break; };
        assert!(count < 16, "fixture review failed to converge");
        cc_switch_lib::apply_reviewed_configuration_change(&pending.id, "apply_ccs", state).expect("explicitly approve fixture change");
        count += 1;
    }
    assert!(count > 0, "expected a reviewable configuration conflict");
}
