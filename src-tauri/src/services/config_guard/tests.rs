use super::*;
use crate::app_management::tests::TestHome;
use serde_json::json;

type ReplaceHook = Box<dyn FnMut(&Path)>;
thread_local! {
    static REPLACE_HOOK: RefCell<Option<ReplaceHook>> = const { RefCell::new(None) };
}

pub(super) fn before_replace(path: &Path) {
    REPLACE_HOOK.with(|hook| {
        if let Some(operation) = hook.borrow_mut().as_mut() { operation(path); }
    });
}

fn with_replace_hook<ResultValue>(hook: impl FnMut(&Path) + 'static, operation: impl FnOnce() -> ResultValue) -> ResultValue {
    struct Reset(Option<ReplaceHook>);
    impl Drop for Reset {
        fn drop(&mut self) { REPLACE_HOOK.with(|hook| *hook.borrow_mut() = self.0.take()); }
    }
    let _reset = Reset(REPLACE_HOOK.with(|current| current.replace(Some(Box::new(hook)))));
    operation()
}

fn codex_path() -> PathBuf { crate::codex_config::get_codex_config_path() }
fn seed(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
fn keep_local(app: &AppType, path: &Path) {
    let preview = preview(app, &file_id(app, path)).unwrap();
    apply(&preview.id, "keep_local", None).unwrap();
}

#[test]
fn safety_native_guard_without_baseline_requires_confirmation_and_preserves_secrets() {
    let _home = TestHome::new();
    let path = codex_path();
    seed(&path, "model='local'\n[model_providers.custom]\nbase_url='https://synthetic.invalid'\nexperimental_bearer_token='synthetic-secret'\n");
    assert!(crate::config::atomic_write(&path, b"model='remote'\n").is_err());
    let state = get_state(&AppType::Codex).unwrap();
    assert_eq!(state.pending_changes.len(), 1);
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("synthetic-secret"));
    assert!(!json.contains("synthetic.invalid"));
    keep_local(&AppType::Codex, &path);
    assert!(fs::read_to_string(&path).unwrap().contains("model='local'"));
}

#[test]
fn safety_native_guard_connection_change_cannot_retain_foreign_credentials() {
    let _home = TestHome::new();
    let app = AppType::Claude;
    let path = crate::config::get_claude_settings_path();
    let original = json!({"env":{"ANTHROPIC_API_KEY":"synthetic-a","OPENROUTER_API_KEY":"synthetic-foreign"},"user_setting":true});
    commit_files(&app, &[(path.clone(), serde_json::to_vec(&original).unwrap())]).unwrap();
    let next = json!({"env":{"ANTHROPIC_API_KEY":"synthetic-b"}});
    assert!(commit_files(&app, &[(path.clone(), serde_json::to_vec(&next).unwrap())]).is_err());
    assert_eq!(serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(), original);
    let pending = get_state(&app).unwrap().pending_changes.remove(0);
    assert!(pending.conflicts.contains(&"/env/OPENROUTER_API_KEY".to_string()));
    assert!(pending.changes.iter().any(|change| change.path == "/user_setting" && change.kind == "remove"));
    assert!(!serde_json::to_string(&pending).unwrap().contains("synthetic-foreign"));
    apply(&pending.id, "apply_ccs", None).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(), next);
}

#[test]
fn safety_native_guard_read_detects_external_changes_and_refuses_recreating_deleted_file() {
    let _home = TestHome::new();
    let path = codex_path();
    crate::config::atomic_write(&path, b"model='a'\n").unwrap();
    seed(&path, "model='external'\n");
    let state = get_state(&AppType::Codex).unwrap();
    assert!(!state.pending_changes.is_empty());
    assert_eq!(fs::read_to_string(&path).unwrap(), "model='external'\n");
    fs::remove_file(&path).unwrap(); // test-owned fixture simulates external deletion
    assert!(crate::config::atomic_write(&path, b"model='b'\n").is_err());
    assert!(!path.exists());
}

#[test]
fn safety_native_release_restores_only_owned_fields_and_preserves_rotated_oauth() {
    let _home = TestHome::new();
    let path = crate::codex_config::get_codex_auth_path();
    seed(&path, r#"{"OPENAI_API_KEY":"PROXY_MANAGED","tokens":{"access_token":"rotated-user-token"},"extension":7}"#);
    let edit = release_edit(path.clone(), r#"{"OPENAI_API_KEY":"synthetic-old"}"#, r#"{"OPENAI_API_KEY":"PROXY_MANAGED"}"#).unwrap().unwrap();
    let value: Value = serde_json::from_str(&edit.content).unwrap();
    assert_eq!(value["OPENAI_API_KEY"], "synthetic-old");
    assert_eq!(value["tokens"]["access_token"], "rotated-user-token");
    assert_eq!(value["extension"], 7);
    seed(&path, r#"{"OPENAI_API_KEY":"external-key"}"#);
    assert!(release_edit(path, r#"{"OPENAI_API_KEY":"synthetic-old"}"#, r#"{"OPENAI_API_KEY":"PROXY_MANAGED"}"#).is_err());
}

#[test]
fn safety_native_codex_provider_conflict_confirmation_commits_real_provider_transaction() {
    let home = TestHome::new();
    let state = home.state();
    let make_provider = |id: &str| crate::provider::Provider::with_id(id.into(), id.into(), json!({
        "auth":{"OPENAI_API_KEY":format!("synthetic-{id}")},
        "config":format!("model='{id}'\nmodel_provider='custom'\n[model_providers.custom]\nname='Synthetic'\nbase_url='https://synthetic.invalid/v1'\nwire_api='responses'\nrequires_openai_auth=true\n")
    }), None);
    state.db.save_provider("codex", &make_provider("a")).unwrap();
    state.db.save_provider("codex", &make_provider("b")).unwrap();
    crate::services::ProviderService::switch(&state, AppType::Codex, "a").unwrap();
    let path = codex_path();
    let content = fs::read_to_string(&path).unwrap();
    let mut value: toml_edit::DocumentMut = content.parse().unwrap();
    value["model"] = toml_edit::value("external");
    seed(&path, &value.to_string());
    assert!(crate::services::ProviderService::switch(&state, AppType::Codex, "b").is_err());
    assert_eq!(crate::services::ProviderService::current(&state, AppType::Codex).unwrap(), "a");
    let pending = get_state(&AppType::Codex).unwrap().pending_changes;
    assert!(!pending.is_empty());
    let reviewed = pending.iter().find(|pending| pending.path == path.display().to_string()).unwrap();
    apply(&reviewed.id, "apply_ccs", Some(&state)).unwrap();
    assert_eq!(crate::services::ProviderService::current(&state, AppType::Codex).unwrap(), "b");
    assert_eq!(fs::read_to_string(path).unwrap().parse::<toml::Value>().unwrap()["model"].as_str(), Some("b"));
}

#[test]
fn safety_native_guard_three_way_merge_and_stale_preview() {
    let _home = TestHome::new();
    let path = codex_path();
    crate::config::atomic_write(&path, b"model='a'\ncustom=1\n").unwrap();
    seed(&path, "model='a'\ncustom=2\n");
    crate::config::atomic_write(&path, b"model='b'\ncustom=3\n").unwrap();
    let merged: toml::Value = fs::read_to_string(&path).unwrap().parse().unwrap();
    assert_eq!(merged["model"].as_str(), Some("b"));
    assert_eq!(merged["custom"].as_integer(), Some(2));
    seed(&path, "model='local'\ncustom=2\n");
    assert!(crate::config::atomic_write(&path, b"model='c'\n").is_err());
    let pending = get_state(&AppType::Codex).unwrap().pending_changes.remove(0);
    seed(&path, "model='newer'\ncustom=2\n");
    assert!(apply(&pending.id, "apply_ccs", None).is_err());
    assert!(fs::read_to_string(&path).unwrap().contains("newer"));
}

#[test]
fn safety_native_guard_rules_have_their_own_cas_and_invalidate_previews() {
    let _home = TestHome::new();
    let app = AppType::Codex;
    let path = codex_path();
    crate::config::atomic_write(&path, b"model='a'\n").unwrap();
    let file = get_state(&app).unwrap().files.into_iter().find(|file| file.path == path.display().to_string()).unwrap();
    let pending = preview(&app, &file.id).unwrap();
    set_protection(&app, &file.id, vec!["/model".into()], false, &file.revision).unwrap();
    assert!(set_protection(&app, &file.id, vec![], false, &file.revision).is_err());
    assert!(apply(&pending.id, "apply_ccs", None).is_err());
    assert!(crate::config::atomic_write(&path, b"model='b'\n").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "model='a'\n");
}

#[test]
fn safety_native_guard_codex_catalog_reference_and_unknown_fields_survive() {
    let _home = TestHome::new();
    let app = AppType::Codex;
    let path = codex_path();
    let external = crate::config::get_home_dir().join("personal-catalog.json");
    seed(&external, r#"{"models":[{"slug":"synthetic","input_modalities":["text","image"],"extra":7}]}"#);
    let content = format!("model='a'\nmodel_catalog_json='{}'\n", external.display());
    crate::config::atomic_write(&path, content.as_bytes()).unwrap();
    crate::config::atomic_write(&path, b"model='b'\nmodel_catalog_json='cc-switch-models.json'\n").unwrap();
    let next: toml::Value = fs::read_to_string(&path).unwrap().parse().unwrap();
    assert_eq!(next["model_catalog_json"].as_str(), Some(external.to_str().unwrap()));
    assert!(fs::read_to_string(&external).unwrap().contains("image"));
    let catalog = crate::codex_config::get_codex_config_dir().join(crate::codex_config::CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME);
    let base = json!({"models":[{"slug":"synthetic", "context_window":100, "input_modalities":["text"]}]});
    commit_files(&app, &[(catalog.clone(), serde_json::to_vec(&base).unwrap())]).unwrap();
    seed(&catalog, &json!({"models":[{"slug":"synthetic", "context_window":100, "input_modalities":["text","image"], "future_field":true}]}).to_string());
    let proposal = json!({"models":[{"slug":"synthetic", "context_window":200, "input_modalities":["text"]}]});
    commit_files(&app, &[(catalog.clone(), serde_json::to_vec(&proposal).unwrap())]).unwrap();
    let result: Value = serde_json::from_str(&fs::read_to_string(catalog).unwrap()).unwrap();
    assert_eq!(result["models"][0]["input_modalities"], json!(["text","image"]));
    assert_eq!(result["models"][0]["future_field"], true);
    assert_eq!(result["models"][0]["context_window"], 200);
}

#[test]
fn safety_native_guard_connection_bundle_conflict_writes_nothing_then_reviews_together() {
    let home = TestHome::new();
    let app = AppType::Codex;
    let config = codex_path();
    let auth = crate::codex_config::get_codex_auth_path();
    let first = vec![(config.clone(), b"model='a'\n".to_vec()), (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-a"}"#.to_vec())];
    commit_files(&app, &first).unwrap();
    seed(&auth, r#"{"OPENAI_API_KEY":"user-key"}"#);
    let next = vec![(config.clone(), b"model='b'\n".to_vec()), (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-b"}"#.to_vec())];
    assert!(commit_files(&app, &next).is_err());
    assert_eq!(fs::read(&config).unwrap(), first[0].1);
    assert!(fs::read_to_string(&auth).unwrap().contains("user-key"));
    let pending = get_state(&app).unwrap().pending_changes;
    assert_eq!(pending.len(), 2);
    apply(&pending[0].id, "apply_ccs", Some(&home.state())).unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap().parse::<toml::Value>().unwrap()["model"].as_str(), Some("b"));
    assert_eq!(serde_json::from_slice::<Value>(&fs::read(&auth).unwrap()).unwrap()["OPENAI_API_KEY"], "synthetic-b");
    assert!(get_state(&app).unwrap().pending_changes.is_empty());
}

#[test]
fn safety_native_guard_backup_restore_is_grouped_and_version_checked() {
    let _home = TestHome::new();
    let app = AppType::Codex;
    let config = codex_path();
    let auth = crate::codex_config::get_codex_auth_path();
    let before = vec![(config.clone(), b"model='a'\n".to_vec()), (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-a"}"#.to_vec())];
    let after = vec![(config.clone(), b"model='b'\n".to_vec()), (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-b"}"#.to_vec())];
    commit_files(&app, &before).unwrap();
    commit_files(&app, &after).unwrap();
    let state = get_state(&app).unwrap();
    assert_eq!(state.backups.len(), 4);
    assert_eq!(state.backups.iter().filter(|backup| backup.group_id == state.backups[0].group_id).count(), 2);
    assert!(state.history.iter().any(|entry| entry.result == "applied"));
    let preview = preview_restore(&app, &state.backups[0].id).unwrap();
    seed(&auth, r#"{"OPENAI_API_KEY":"external"}"#);
    assert!(apply(&preview.id, "apply_ccs", None).is_err());
    assert!(super::preview(&app, &state.backups[0].file_id).is_err());
    assert!(preview_restore(&app, &state.backups[0].id).is_err());
    seed(&auth, std::str::from_utf8(&after[1].1).unwrap());
    let preview = preview_restore(&app, &state.backups[0].id).unwrap();
    apply(&preview.id, "apply_ccs", None).unwrap();
    let config_value: toml::Value = fs::read_to_string(config).unwrap().parse().unwrap();
    assert_eq!(config_value["model"].as_str(), Some("a"));
    assert_eq!(serde_json::from_slice::<Value>(&fs::read(auth).unwrap()).unwrap()["OPENAI_API_KEY"], "synthetic-a");
}

#[test]
fn safety_native_guard_stop_has_priority_over_review_and_delete() {
    let home = TestHome::new();
    let path = codex_path();
    crate::config::atomic_write(&path, b"model='a'\n").unwrap();
    let pending = preview(&AppType::Codex, &file_id(&AppType::Codex, &path)).unwrap();
    home.stop(&AppType::Codex);
    assert!(apply(&pending.id, "apply_ccs", None).is_err());
    assert!(crate::config::delete_file(&path).is_err());
}

#[test]
fn safety_native_gemini_and_deepseek_connections_preflight_credentials_together() {
    let home = TestHome::new();
    let state = home.state();
    let mut gemini = crate::provider::Provider::with_id("synthetic-gemini".into(), "Synthetic API".into(), json!({
        "env":{"GEMINI_API_KEY":"synthetic-a", "GOOGLE_GEMINI_BASE_URL":"https://synthetic.invalid"}, "config":{}
    }), None);
    crate::services::provider::write_live_with_common_config_for_state(&state, &AppType::Gemini, &gemini).unwrap();
    let env_path = crate::gemini_config::get_gemini_env_path();
    let settings_path = crate::gemini_config::get_gemini_settings_path();
    let before_env = fs::read(&env_path).unwrap();
    seed(&settings_path, r#"{"security":{"auth":{"selectedType":"external-login"}}}"#);
    gemini.settings_config["env"]["GEMINI_API_KEY"] = json!("synthetic-b");
    assert!(crate::services::provider::write_live_with_common_config_for_state(&state, &AppType::Gemini, &gemini).is_err());
    assert_eq!(fs::read(env_path).unwrap(), before_env);
    let first = json!({"baseUrl":"https://synthetic.invalid/a", "apiKey":"synthetic-a", "model":"synthetic-model"});
    crate::deepseek_config::write_provider_live(&first).unwrap();
    let credentials = crate::deepseek_config::get_deepseek_dir().join(".credentials.yaml");
    let settings_path = crate::deepseek_config::get_deepseek_settings_path();
    let before_settings = fs::read(&settings_path).unwrap();
    seed(&credentials, "DEEPSEEK_API_KEY: external\n");
    let next = json!({"baseUrl":"https://synthetic.invalid/b", "apiKey":"synthetic-b", "model":"synthetic-next"});
    assert!(crate::deepseek_config::write_provider_live(&next).is_err());
    assert_eq!(fs::read(settings_path).unwrap(), before_settings);
    assert!(fs::read_to_string(credentials).unwrap().contains("external"));
}

#[cfg(windows)]
#[test]
fn safety_native_guard_failed_second_replace_rolls_back_first_without_losing_backup() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    let _home = TestHome::new();
    let app = AppType::Codex;
    let config = codex_path();
    let auth = crate::codex_config::get_codex_auth_path();
    let before = vec![(config.clone(), b"model='a'\n".to_vec()), (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-a"}"#.to_vec())];
    commit_files(&app, &before).unwrap();
    let held = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&auth).unwrap();
    let after = vec![(config.clone(), b"model='b'\n".to_vec()), (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-b"}"#.to_vec())];
    assert!(commit_files(&app, &after).is_err());
    drop(held);
    assert_eq!(fs::read(&config).unwrap(), before[0].1);
    assert_eq!(fs::read(&auth).unwrap(), before[1].1);
    assert!(get_state(&app).unwrap().history.iter().any(|entry| entry.result == "failed"));
}

#[test]
fn safety_native_guard_backup_restore_removes_new_files_in_the_same_bundle() {
    let _home = TestHome::new();
    let app = AppType::Codex;
    let config = codex_path();
    let auth = crate::codex_config::get_codex_auth_path();
    commit_files(&app, &[(config.clone(), b"model='before'\n".to_vec())]).unwrap();
    commit_files(&app, &[
        (config.clone(), b"model='after'\n".to_vec()),
        (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-created"}"#.to_vec()),
    ]).unwrap();
    let state = get_state(&app).unwrap();
    let created = state.backups.iter().find(|backup| backup.file_id == file_id(&app, &auth)).unwrap();
    assert_eq!(created.before_revision, "missing");
    assert_eq!(state.backups.iter().filter(|backup| backup.group_id == created.group_id).count(), 2);
    let preview = preview_restore(&app, &created.id).unwrap();
    assert!(preview.changes.iter().all(|change| change.kind == "remove"));
    apply(&preview.id, "apply_ccs", None).unwrap();
    assert!(!auth.exists());
    assert_eq!(fs::read_to_string(&config).unwrap().parse::<toml::Value>().unwrap()["model"].as_str(), Some("before"));
    assert!(get_state(&app).unwrap().pending_changes.is_empty());
    let restoration = get_state(&app).unwrap().backups.into_iter().find(|backup|
        backup.file_id == file_id(&app, &auth) && backup.after_revision == "missing"
    ).unwrap();
    let preview = preview_restore(&app, &restoration.id).unwrap();
    apply(&preview.id, "apply_ccs", None).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&fs::read(&auth).unwrap()).unwrap()["OPENAI_API_KEY"], "synthetic-created");
    assert_eq!(fs::read_to_string(config).unwrap().parse::<toml::Value>().unwrap()["model"].as_str(), Some("after"));
}

#[test]
fn safety_native_guard_deletion_cannot_bypass_protected_fields() {
    let _home = TestHome::new();
    let app = AppType::Codex;
    let path = codex_path();
    commit_files(&app, &[(path.clone(), b"model='protected'\n".to_vec())]).unwrap();
    let file = get_state(&app).unwrap().files.into_iter().find(|file| file.id == file_id(&app, &path)).unwrap();
    set_protection(&app, &file.id, vec!["/model".into()], false, &file.revision).unwrap();
    assert!(commit_file_changes(&app, &[(path.clone(), None)]).is_err());
    let preview = get_state(&app).unwrap().pending_changes.remove(0);
    assert!(apply(&preview.id, "apply_ccs", None).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "model='protected'\n");
}

#[test]
fn safety_native_guard_revision_read_failure_rolls_back_only_its_own_writes() {
    for preserve_external in [false, true] {
        let _home = TestHome::new();
        let app = AppType::Codex;
        let config = codex_path();
        let auth = crate::codex_config::get_codex_auth_path();
        let before = vec![
            (config.clone(), b"model='before'\n".to_vec()),
            (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-before"}"#.to_vec()),
        ];
        commit_files(&app, &before).unwrap();
        let hook_config = config.clone();
        let hook_auth = auth.clone();
        let moved_auth = auth.with_extension("saved.json");
        let hook_moved_auth = moved_auth.clone();
        let error = with_replace_hook(move |path| {
            if path == hook_auth {
                if preserve_external { seed(&hook_config, "model='external'\n"); }
                fs::rename(&hook_auth, &hook_moved_auth).unwrap();
                fs::create_dir(&hook_auth).unwrap();
            }
        }, || commit_files(&app, &[
            (config.clone(), b"model='after'\n".to_vec()),
            (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-after"}"#.to_vec()),
        ])).unwrap_err();
        assert!(error.to_string().contains("bounded regular file"));
        let expected = if preserve_external { b"model='external'\n".as_slice() } else { before[0].1.as_slice() };
        assert_eq!(fs::read(&config).unwrap(), expected);
        assert_eq!(fs::read(&moved_auth).unwrap(), before[1].1);
        assert!(auth.is_dir());
        assert_eq!(error.to_string().contains("External edit preserved"), preserve_external);
        assert!(read_store().unwrap().history.iter().any(|entry| entry.result == "failed"));
    }
}

#[test]
fn safety_native_guard_metadata_failure_rolls_back_the_completed_bundle() {
    let _home = TestHome::new();
    let app = AppType::Codex;
    let config = codex_path();
    let auth = crate::codex_config::get_codex_auth_path();
    let before = vec![
        (config.clone(), b"model='before'\n".to_vec()),
        (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-before"}"#.to_vec()),
    ];
    commit_files(&app, &before).unwrap();
    let hook_auth = auth.clone();
    let error = with_replace_hook(move |path| {
        if path == hook_auth {
            let metadata = store_path();
            fs::rename(&metadata, metadata.with_extension("saved.json")).unwrap();
            fs::create_dir(&metadata).unwrap();
        }
    }, || commit_files(&app, &[
        (config.clone(), b"model='after'\n".to_vec()),
        (auth.clone(), br#"{"OPENAI_API_KEY":"synthetic-after"}"#.to_vec()),
    ])).unwrap_err();
    assert!(error.to_string().contains("rollback"));
    assert_eq!(fs::read(config).unwrap(), before[0].1);
    assert_eq!(fs::read(auth).unwrap(), before[1].1);
    assert!(store_path().with_extension("saved.json").is_file());
}

#[cfg(windows)]
#[test]
fn safety_native_codex_auth_delete_failure_rolls_back_both_native_write_paths() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    for with_catalog in [false, true] {
        let _home = TestHome::new();
        let config = codex_path();
        let auth = crate::codex_config::get_codex_auth_path();
        let original_auth = json!({"OPENAI_API_KEY":"synthetic-before"});
        crate::codex_config::write_codex_live_atomic(&original_auth, Some("model='before'\n")).unwrap();
        let before_config = fs::read(&config).unwrap();
        let before_auth = fs::read(&auth).unwrap();
        let held = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&auth).unwrap();
        let target_auth = json!({"OPENAI_API_KEY":"synthetic-after"});
        let target_config = "model_provider='custom'\nmodel='synthetic'\n[model_providers.custom]\nname='Synthetic'\nbase_url='https://synthetic.invalid/v1'\nwire_api='responses'\n";
        let result = if with_catalog {
            crate::codex_config::write_codex_provider_live_with_catalog(&json!({}), None, &target_auth, Some(target_config), crate::codex_config::CodexCatalogToolProfile::NativeResponses)
        } else {
            crate::codex_config::write_codex_live_for_provider(None, &target_auth, Some(target_config))
        };
        let error = result.unwrap_err();
        drop(held);
        assert!(error.to_string().contains("rollback: completed"));
        assert_eq!(fs::read(&config).unwrap(), before_config);
        assert_eq!(fs::read(&auth).unwrap(), before_auth);
    }
}
