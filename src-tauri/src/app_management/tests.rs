use super::*;
use crate::{services::{McpService, PromptService, ProviderService, SkillService}, store::AppState, database::Database, provider::Provider};
use serde_json::json;
use std::sync::Arc;

pub(crate) struct TestHome {
    _lock: MutexGuard<'static, ()>,
    pub dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}
impl TestHome {
    pub fn new() -> Self {
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::Builder::new().prefix("ccs-native-safety-").tempdir().unwrap();
        let previous = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
        let settings = crate::settings::AppSettings {
            hermes_config_dir: Some(dir.path().join(".hermes").display().to_string()),
            deepseek_config_dir: Some(dir.path().join(".deepseek").display().to_string()),
            pi_config_dir: Some(dir.path().join(".pi/agent").display().to_string()),
            ..Default::default()
        };
        crate::settings::update_settings(settings).unwrap();
        Self { _lock: lock, dir, previous }
    }
    pub fn state(&self) -> Arc<AppState> { Arc::new(AppState::new(Arc::new(Database::memory().unwrap()))) }
    pub fn stop(&self, app: &AppType) {
        let _guard = native_mutation_guard().unwrap();
        let mut policy = read_policy().unwrap();
        policy.managed_apps.insert(app.as_str().into(), false);
        policy.phases.insert(app.as_str().into(), ManagementPhase::Unmanaged);
        policy.revision += 1;
        save_policy(&policy).unwrap();
    }
}
impl Drop for TestHome {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
    }
}

#[test]
fn safety_native_all_ten_apps_deny_files_but_keep_database_edits() {
    let home = TestHome::new();
    let state = home.state();
    let original_settings = crate::settings::get_settings();
    for app in AppType::all() {
        let root = app_roots(&app).unwrap().remove(0);
        assert!(root.starts_with(home.dir.path()), "test directory escaped isolation: {}", root.display());
        fs::create_dir_all(&root).unwrap();
        let existing = root.join("safety-user.json");
        fs::write(&existing, r#"{"user":"keep"}"#).unwrap();
        home.stop(&app);
        assert!(crate::config::atomic_write(&existing, b"{}").is_err(), "{} atomic", app.as_str());
        assert!(crate::config::write_text_file(&root.join("new/sub/file.md"), "x").is_err());
        assert!(!root.join("new").exists());
        assert!(crate::config::delete_file(&existing).is_err());
        assert_eq!(fs::read_to_string(&existing).unwrap(), r#"{"user":"keep"}"#);
        assert!(ProviderService::switch(&state, app.clone(), "synthetic").is_err());
        assert!(McpService::toggle_app(&state, "synthetic", app.clone(), true).is_err());
        assert!(McpService::sync_enabled_for_app(&state, &app).is_err());
        assert!(PromptService::enable_prompt(&state, app.clone(), "synthetic").is_err());
        assert!(SkillService::sync_to_app_dir("synthetic", &app).is_err());
        let mut provider = Provider::with_id(format!("synthetic-{}", app.as_str()), "Synthetic".into(), json!({"auth":{},"env":{"ANTHROPIC_BASE_URL":"https://synthetic.invalid", "ANTHROPIC_AUTH_TOKEN":"synthetic-key"},"config":"", "baseUrl":"https://synthetic.invalid", "apiKey":"synthetic-key", "models":[]}), None);
        provider.category = Some("official".into());
        if app == AppType::Gemini { provider.settings_config["config"] = json!({}); }
        ProviderService::add(&state, app.clone(), provider.clone(), true).unwrap();
        state.db.set_current_provider(app.as_str(), &provider.id).unwrap();
        assert!(state.db.get_provider_by_id(&provider.id, app.as_str()).unwrap().is_some());
        let prompt: crate::prompt::Prompt = serde_json::from_value(json!({"id":"synthetic", "name":"Synthetic", "content":"# Local test\n", "enabled":true})).unwrap();
        PromptService::upsert_prompt(&state, app.clone(), "synthetic", prompt).unwrap();
        assert!(state.db.get_prompts(app.as_str()).unwrap().contains_key("synthetic"));
    }
    assert_eq!(get_state().unwrap().apps.len(), 10);
    assert!(get_state().unwrap().apps.iter().all(|app| !app.enabled));
    // A stale ordinary-settings form must not resurrect management authority.
    crate::settings::update_settings(original_settings).unwrap();
    assert!(get_state().unwrap().apps.iter().all(|app| !app.enabled));
    McpService::sync_all_enabled(&state).unwrap();
    PromptService::sync_all_to_live(&state).unwrap();
    crate::services::provider::sync_current_to_live(&state).unwrap();
}

#[test]
fn safety_native_corrupt_or_partial_authority_fails_closed() {
    let _home = TestHome::new();
    fs::create_dir_all(device_state_dir()).unwrap();
    for bytes in [b"{".as_slice(), b"{}", br#"{"managedApps":{"codex":true}}"#] {
        fs::write(policy_path(), bytes).unwrap();
        assert!(get_state().is_err());
        for app in AppType::all() { assert!(require_managed(&app).is_err()); }
        assert!(crate::config::atomic_write(&crate::codex_config::get_codex_config_path(), b"model='x'").is_err());
    }
}

#[test]
fn safety_native_stop_preview_revision_pending_review_and_other_app_independence() {
    let home = TestHome::new();
    let state = home.state();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let path = crate::codex_config::get_codex_config_path();
        crate::config::atomic_write(&path, b"model='a'\n").unwrap();
        let preview = preview_change(state.clone(), AppType::Codex, false).await.unwrap();
        fs::write(&path, b"model='local'\n").unwrap();
        assert!(apply_change(state.clone(), preview.id).await.is_err());
        assert!(is_managed(&AppType::Codex));
        let ticket = WriteTicket::capture(&AppType::Codex).unwrap();
        let preview = preview_change(state.clone(), AppType::Codex, false).await.unwrap();
        let result = apply_change(state.clone(), preview.id).await.unwrap();
        assert_eq!(result.apps.iter().find(|app| app.app_id == "codex").unwrap().phase, ManagementPhase::Unmanaged);
        assert!(ticket.check().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "model='local'\n");
        crate::config::atomic_write(&crate::config::get_claude_settings_path(), b"{}").unwrap();
        let preview = preview_change(state.clone(), AppType::Codex, true).await.unwrap();
        let result = apply_change(state.clone(), preview.id).await.unwrap();
        assert_eq!(result.apps.iter().find(|app| app.app_id == "codex").unwrap().phase, ManagementPhase::PendingReview);
        assert!(crate::config::atomic_write(&path, b"model='queued'\n").is_err());
        let guard = crate::services::config_guard::get_state(&AppType::Codex).unwrap();
        for pending in guard.pending_changes {
            let file = crate::services::config_guard::get_state(&AppType::Codex).unwrap().files.into_iter().find(|file| file.path == pending.path).unwrap();
            let fresh = crate::services::config_guard::preview(&AppType::Codex, &file.id).unwrap();
            crate::services::config_guard::apply(&fresh.id, "keep_local", None).unwrap();
        }
        assert!(is_managed(&AppType::Codex));
        assert!(ticket.check().is_err(), "off/on cannot revive an old queued operation");
        assert_eq!(fs::read_to_string(&path).unwrap(), "model='local'\n");
    });
}

#[test]
fn safety_native_stop_waits_for_committing_writer_and_invalidates_queue() {
    let home = TestHome::new();
    let state = home.state();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let preview = runtime.block_on(preview_change(state.clone(), AppType::Pi, false)).unwrap();
    let ticket = WriteTicket::capture(&AppType::Pi).unwrap();
    let native = native_mutation_guard().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        tx.send(runtime.block_on(apply_change(state, preview.id)).is_ok()).unwrap();
    });
    assert!(rx.recv_timeout(std::time::Duration::from_millis(30)).is_err());
    drop(native);
    assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
    handle.join().unwrap();
    assert!(ticket.check().is_err());
}

#[test]
fn safety_native_untrusted_takeover_stops_with_durable_pending_release() {
    let home = TestHome::new();
    let state = home.state();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut config = state.db.get_proxy_config_for_app("codex").await.unwrap();
        config.enabled = true;
        state.db.update_proxy_config_for_app(config).await.unwrap();
        let preview = preview_change(state.clone(), AppType::Codex, false).await.unwrap();
        assert!(!preview.conflicts.is_empty());
        apply_change(state.clone(), preview.id).await.unwrap();
        assert_eq!(read_policy().unwrap().app(&AppType::Codex).phase, ManagementPhase::PendingRelease);
        assert!(!is_managed(&AppType::Codex));
        // Fresh service objects read the durable stop, not old proxy flags.
        let restarted = AppState::new(state.db.clone());
        assert!(ProviderService::switch(&restarted, AppType::Codex, "synthetic").is_err());
        assert!(is_managed(&AppType::Claude));
    });
}

#[test]
fn safety_native_shared_physical_roots_block_other_application_writes() {
    let home = TestHome::new();
    let shared = home.dir.path().join("shared-config");
    let mut settings = crate::settings::get_settings();
    settings.codex_config_dir = Some(shared.display().to_string());
    settings.gemini_config_dir = Some(shared.display().to_string());
    crate::settings::update_settings(settings).unwrap();
    let state = home.state();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let preview = preview_change(state.clone(), AppType::Codex, false).await.unwrap();
        assert!(preview.conflicts.iter().any(|text| text.contains("share a physical")));
        apply_change(state, preview.id).await.unwrap();
    });
    assert_eq!(read_policy().unwrap().app(&AppType::Codex).phase, ManagementPhase::PendingRelease);
    assert!(crate::config::atomic_write(&shared.join("settings.json"), b"{}").is_err());
    assert!(!shared.exists());
    assert!(is_managed(&AppType::Gemini));
}

#[test]
fn safety_native_profile_apply_reports_unmanaged_skips() {
    use crate::services::profile::{ProfileScope, ProfileService};
    let home = TestHome::new();
    let state = home.state();
    for scope in ProfileScope::ALL {
        let profile = ProfileService::create(&state, "Synthetic", scope).unwrap();
        let app = scope.apps()[0].clone();
        home.stop(&app);
        let (warnings, _) = ProfileService::apply(&state, &profile.id, scope).unwrap();
        assert!(warnings.iter().any(|warning| warning.contains("skipped: local management")));
    }
}

#[test]
fn safety_native_schema_migration_and_sync_import_preserve_local_evidence_and_stop() {
    let home = TestHome::new();
    let db = Database::memory().unwrap();
    {
        let conn = db.conn.lock().unwrap();
        conn.execute("INSERT INTO model_validation_runs VALUES ('local', 'local-plan', 'codex', 'a', '2026-01-01', '{}')", []).unwrap();
        conn.pragma_update(None, "user_version", 20).unwrap();
        Database::apply_schema_migrations_on_conn(&conn).unwrap();
        Database::apply_schema_migrations_on_conn(&conn).unwrap();
        assert_eq!(conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0)).unwrap(), 21);
    }
    assert!(!db.export_sql_string_for_sync().unwrap().contains("local-plan"));
    let remote = Database::memory().unwrap();
    remote.conn.lock().unwrap().execute("INSERT INTO model_validation_runs VALUES ('remote', 'remote-plan', 'codex', 'b', '2026-01-02', '{}')", []).unwrap();
    home.stop(&AppType::Codex);
    db.import_sql_string_for_sync(&remote.export_sql_string_for_sync().unwrap()).unwrap();
    db.import_sql_string(&remote.export_sql_string().unwrap()).unwrap();
    let conn = db.conn.lock().unwrap();
    assert_eq!(conn.query_row("SELECT id FROM model_validation_runs", [], |row| row.get::<_, String>(0)).unwrap(), "local");
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM model_validation_runs", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert!(!is_managed(&AppType::Codex));
}

#[cfg(any(unix, windows))]
fn skill_fixture(home: &TestHome) -> (Arc<AppState>, PathBuf, PathBuf) {
    let state = home.state();
    let source = skills::ssot_path().join("synthetic-skill");
    fs::create_dir_all(source.join("empty/nested")).unwrap();
    fs::write(source.join("SKILL.md"), "---\nname: synthetic\n---\n# Test\n").unwrap();
    let target = SkillService::get_app_skills_dir(&AppType::Codex).unwrap().join("synthetic-skill");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    #[cfg(unix)] std::os::unix::fs::symlink(&source, &target).unwrap();
    #[cfg(windows)] {
        if let Err(error) = std::os::windows::fs::symlink_dir(&source, &target) {
            assert_eq!(error.raw_os_error(), Some(1314), "unexpected symlink failure: {error}");
            // A junction is the native directory-link equivalent available
            // without SeCreateSymbolicLinkPrivilege on ordinary Windows hosts.
            use std::os::windows::process::CommandExt;
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", "New-Item -ItemType Junction -Path $env:CCS_TEST_LINK -Target $env:CCS_TEST_SOURCE -ErrorAction Stop | Out-Null"])
                .env("CCS_TEST_LINK", &target).env("CCS_TEST_SOURCE", &source)
                .creation_flags(0x08000000).output().unwrap();
            assert!(output.status.success(), "junction fixture: {}", String::from_utf8_lossy(&output.stderr));
        }
    }
    let skill: crate::app_config::InstalledSkill = serde_json::from_value(json!({
        "id":"local:synthetic-skill", "directory":"synthetic-skill", "name":"Synthetic", "apps":{"codex":true}, "installedAt":0
    })).unwrap();
    state.db.save_skill(&skill).unwrap();
    (state, source, target)
}

#[cfg(any(unix, windows))]
#[test]
fn safety_native_skill_detachment_preserves_empty_directories_and_separates_source() {
    let home = TestHome::new();
    let (state, source, target) = skill_fixture(&home);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let plan = preview_change(state.clone(), AppType::Codex, false).await.unwrap();
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        assert!(plan.files.iter().any(|file| file.action == "detach_skill_link_preserving_content"));
        apply_change(state, plan.id).await.unwrap();
    });
    assert!(fs::read_link(&target).is_err());
    assert!(target.join("empty/nested").is_dir());
    let before = fs::read(target.join("SKILL.md")).unwrap();
    fs::write(source.join("SKILL.md"), "Changed shared source").unwrap();
    assert_eq!(fs::read(target.join("SKILL.md")).unwrap(), before);
    assert_eq!(read_policy().unwrap().app(&AppType::Codex).phase, ManagementPhase::Unmanaged);
}

#[cfg(any(unix, windows))]
#[test]
fn safety_native_interrupted_skill_rename_can_resume_from_local_journal() {
    let home = TestHome::new();
    let (state, source, target) = skill_fixture(&home);
    let (mut links, conflicts, _) = skills::inspect(&state.db, &AppType::Codex).unwrap();
    assert!(conflicts.is_empty());
    let mut link = links.remove(0);
    let id = uuid::Uuid::new_v4().to_string();
    link.recovery_id = Some(id.clone());
    let parent = target.parent().unwrap();
    let stage = parent.join(format!(".ccs-detach-{id}.copy"));
    fs::create_dir_all(stage.join("empty/nested")).unwrap();
    fs::copy(source.join("SKILL.md"), stage.join("SKILL.md")).unwrap();
    fs::write(parent.join(format!(".ccs-detach-{id}.json")), serde_json::to_vec(&link).unwrap()).unwrap();
    fs::rename(&target, parent.join(format!(".ccs-detach-{id}.link"))).unwrap();
    home.stop(&AppType::Codex);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let plan = preview_change(state.clone(), AppType::Codex, false).await.unwrap();
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        apply_change(state, plan.id).await.unwrap();
    });
    assert!(target.join("SKILL.md").is_file());
    assert!(target.join("empty/nested").is_dir());
    assert!(fs::read_link(target).is_err());
    assert!(!stage.exists());
}
