//! Preview-bound management transitions. A durable stop precedes release work.
use super::*;
use crate::{services::config_guard, store::AppState};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementFile { pub path: String, pub action: String }
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementPlan {
    pub id: String, pub app_id: String, pub enabled: bool, pub revision: String,
    pub files: Vec<ManagementFile>, pub warnings: Vec<String>, pub conflicts: Vec<String>,
}
#[derive(Clone, Serialize)]
pub(crate) struct ReleaseEdit { pub path: PathBuf, pub revision: String, pub content: String }
#[derive(Clone)]
struct Prepared {
    plan: ManagementPlan, edits: Vec<ReleaseEdit>, links: Vec<skills::LinkRelease>,
    blocked: Vec<PathBuf>, fingerprint: String, created: Instant,
    versions: Vec<(PathBuf, String)>, settings_revision: String,
}
fn plans() -> &'static Mutex<HashMap<String, Prepared>> {
    static PLANS: OnceLock<Mutex<HashMap<String, Prepared>>> = OnceLock::new();
    PLANS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn prepare(state: &AppState, app: &AppType, enabled: bool) -> Result<Prepared, AppError> {
    let (edits, release_error) = if enabled { (vec![], None) } else {
        match state.proxy_service.prepare_management_release(app).await {
            Ok(edits) => (edits, None), Err(error) => (vec![], Some(error)),
        }
    };
    let proxy_state = if matches!(app, AppType::Claude | AppType::Codex | AppType::Gemini | AppType::GrokBuild) {
        let config = state.db.get_proxy_config_for_app(app.as_str()).await?;
        let backup = state.db.get_live_backup(app.as_str()).await?;
        serde_json::to_string(&(config, backup.map(|value| value.original_config))).map_err(|source| AppError::JsonSerialize { source })?
    } else { String::new() };
    // Same order as Skills mutations: Skills state, then short native commit lock.
    let _skills = crate::services::skill::skill_state_read_guard();
    let _native = native_mutation_guard()?;
    let policy = read_policy()?;
    let mut conflicts = release_error.into_iter().collect::<Vec<_>>();
    if *app == AppType::Codex && crate::services::codex_repair::repair_is_running() {
        conflicts.push("An already-launched Codex repair is still running. Ordinary writes can stop now, but full release must be retried after that process finishes.".into());
    }
    let (links, skill_conflicts, mut blocked) = if enabled { (vec![], vec![], vec![]) } else { skills::inspect(&state.db, app)? };
    conflicts.extend(skill_conflicts);
    let roots = app_roots(app)?;
    for other in AppType::all().filter(|other| other != app) {
        for root in &roots {
            for other_root in app_roots(&other)? {
                if paths_overlap(root, &other_root) {
                    conflicts.push(format!("{} and {} share a physical configuration directory: {}", app.as_str(), other.as_str(), root.display()));
                    blocked.push(root.clone());
                }
            }
        }
    }
    if enabled && policy.app(app).phase == ManagementPhase::PendingRelease {
        conflicts.push("Complete or resolve the pending release before enabling management".into());
    }
    let mut files = Vec::new();
    let mut versions = Vec::new();
    for path in app_files(app)? {
        let revision = config_guard::file_revision(&path)?;
        if revision != "missing" {
            files.push(ManagementFile { path: path.display().to_string(), action: if enabled { "review_without_writing" } else { "stop_managing" }.into() });
        }
        versions.push((path, revision));
    }
    for edit in &edits {
        if config_guard::file_revision(&edit.path)? != edit.revision { return Err(AppError::Config("Configuration changed while preparing release; preview again".into())); }
        files.push(ManagementFile { path: edit.path.display().to_string(), action: "restore_owned_proxy_fields".into() });
    }
    for link in &links { files.push(ManagementFile { path: link.path.display().to_string(), action: "detach_skill_link_preserving_content".into() }); }
    conflicts.sort(); conflicts.dedup(); blocked.sort(); blocked.dedup();
    let settings = serde_json::to_vec(&crate::settings::get_settings()).map_err(|source| AppError::JsonSerialize { source })?;
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&(&versions, &edits, &links, &blocked, &conflicts, settings, proxy_state)).map_err(|source| AppError::JsonSerialize { source })?));
    Ok(Prepared {
        plan: ManagementPlan { id: uuid::Uuid::new_v4().to_string(), app_id: app.as_str().into(), enabled,
            revision: policy.revision.to_string(), files,
            warnings: vec![if enabled { "Enabling does not apply database configuration. Existing files need review before automatic writes resume." } else { "Database records are retained. Only reviewed ccs-owned proxy fields and Skill links can be released." }.into()], conflicts },
        edits, links, blocked, fingerprint, created: Instant::now(), versions,
        settings_revision: format!("{:x}", Sha256::digest(serde_json::to_vec(&crate::settings::get_settings()).map_err(|source| AppError::JsonSerialize { source })?)),
    })
}

pub async fn preview_change(state: Arc<AppState>, app: AppType, enabled: bool) -> Result<ManagementPlan, AppError> {
    let _lifecycle = if app == AppType::Codex { Some(state.lock_codex_provider_lifecycle().await) } else { None };
    let _transaction = state.proxy_service.lock_transaction().await;
    let _switch = state.proxy_service.lock_switch_for_app(app.as_str()).await;
    let prepared = prepare(&state, &app, enabled).await?;
    let plan = prepared.plan.clone();
    let mut saved = plans().lock()?;
    saved.retain(|_, plan| plan.created.elapsed() < Duration::from_secs(600));
    if saved.len() >= 128 { return Err(AppError::Config("Too many outstanding management previews".into())); }
    saved.insert(plan.id.clone(), prepared);
    Ok(plan)
}

pub async fn apply_change(state: Arc<AppState>, id: String) -> Result<AppManagementState, AppError> {
    // Supervise completion even when a webview drops its invocation future.
    tokio::spawn(async move { apply_inner(state, id).await }).await
        .map_err(|error| AppError::Message(format!("Management transition interrupted: {error}")))?
}

async fn apply_inner(state: Arc<AppState>, id: String) -> Result<AppManagementState, AppError> {
    let prepared = plans().lock()?.get(&id).cloned().ok_or_else(|| AppError::Config("Management preview expired; preview again".into()))?;
    if prepared.created.elapsed() >= Duration::from_secs(600) { return Err(AppError::Config("Management preview expired; preview again".into())); }
    let app = <AppType as std::str::FromStr>::from_str(&prepared.plan.app_id)?;
    let _lifecycle = if app == AppType::Codex { Some(state.lock_codex_provider_lifecycle().await) } else { None };
    let _transaction = state.proxy_service.lock_transaction().await;
    let _switch = state.proxy_service.lock_switch_for_app(app.as_str()).await;
    let fresh = prepare(&state, &app, prepared.plan.enabled).await?;
    if fresh.plan.revision != prepared.plan.revision || fresh.fingerprint != prepared.fingerprint {
        return Err(AppError::Config("Settings or target files changed since preview; preview again".into()));
    }
    plans().lock()?.remove(&id);
    let mut release_error = None;
    {
        let _skills = crate::services::skill::skill_state_write_guard();
        let _native = native_mutation_guard()?;
        let mut policy = read_policy()?;
        if policy.revision.to_string() != prepared.plan.revision { return Err(AppError::Config("Management state changed; preview again".into())); }
        if prepared.settings_revision != format!("{:x}", Sha256::digest(serde_json::to_vec(&crate::settings::get_settings()).map_err(|source| AppError::JsonSerialize { source })?)) {
            return Err(AppError::Config("Settings changed before commit; preview again".into()));
        }
        for (path, revision) in &prepared.versions {
            if config_guard::file_revision(path)? != *revision { return Err(AppError::Config("Configuration changed before commit; preview again".into())); }
        }
        if prepared.plan.enabled {
            if !prepared.plan.conflicts.is_empty() { return Err(AppError::Config(prepared.plan.conflicts.join("; "))); }
            policy.managed_apps.insert(app.as_str().into(), true);
            policy.phases.insert(app.as_str().into(), ManagementPhase::PendingReview);
            policy.messages.insert(app.as_str().into(), "Review existing configuration before automatic writes resume".into());
            policy.revision += 1;
            save_policy(&policy)?;
            config_guard::begin_review(&app)?;
            return get_state();
        }
        // Durable fail-closed barrier, including process exit during later steps.
        policy.managed_apps.insert(app.as_str().into(), false);
        policy.phases.insert(app.as_str().into(), ManagementPhase::PendingRelease);
        policy.messages.insert(app.as_str().into(), "Ordinary writes stopped; release pending".into());
        let mut protected = prepared.blocked.clone();
        protected.extend(prepared.links.iter().map(|link| link.source.clone()));
        policy.blocked_paths.insert(app.as_str().into(), protected);
        policy.revision += 1;
        save_policy(&policy)?;
        if prepared.plan.conflicts.is_empty() {
            let release = with_reviewed_write(&app, || {
                // Check every member before modifying any connection field.
                for edit in &prepared.edits {
                    if config_guard::file_revision(&edit.path)? != edit.revision { return Err(AppError::Config("Configuration changed before release; preview again".into())); }
                }
                if !prepared.edits.is_empty() {
                    config_guard::commit_files(&app, &prepared.edits.iter().map(|edit| (edit.path.clone(), edit.content.as_bytes().to_vec())).collect::<Vec<_>>())?;
                }
                for link in &prepared.links { skills::detach(link)?; }
                Ok(())
            });
            if let Err(error) = release { release_error = Some(error.to_string()); }
        } else { release_error = Some(prepared.plan.conflicts.join("; ")); }
    }
    // No synchronous guard is held across async database/runtime operations.
    if release_error.is_none() {
        if let Err(error) = state.proxy_service.finish_management_release(&app).await { release_error = Some(error); }
    }
    {
        let _native = native_mutation_guard()?;
        let mut policy = read_policy()?;
        if let Some(message) = release_error {
            policy.messages.insert(app.as_str().into(), format!("Ordinary writes stopped; release pending: {message}"));
        } else {
            policy.phases.insert(app.as_str().into(), ManagementPhase::Unmanaged);
            policy.messages.remove(app.as_str());
            policy.blocked_paths.remove(app.as_str());
        }
        policy.revision += 1;
        save_policy(&policy)?;
    }
    get_state()
}
