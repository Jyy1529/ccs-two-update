//! Local, field-level configuration ownership. No polling or automatic restore.
mod document;
mod ownership;
mod records;
#[cfg(test)]
mod tests;
pub use records::{GuardAudit, GuardBackup};

use crate::{app_config::AppType, app_management, error::AppError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FileState {
    app_id: String, path: PathBuf,
    baseline: Option<String>,
    #[serde(default)] owned: BTreeSet<String>,
    #[serde(default)] protected_paths: Vec<String>,
    #[serde(default)] protect_file: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pending {
    preview: GuardPreview, file_id: String, candidate: String,
    #[serde(default)] delete_file: bool,
    #[serde(default)] group_id: Option<String>,
    #[serde(default)] source_provider: Option<ProviderOrigin>,
    #[serde(default)] authority: String,
    #[serde(default)] related_versions: BTreeMap<PathBuf, String>,
    #[serde(default)] restore_group: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct GuardStore {
    files: BTreeMap<String, FileState>,
    pending: BTreeMap<String, Pending>,
    #[serde(default)] backups: Vec<BackupRecord>,
    #[serde(default)] rules_revision: u64,
    #[serde(default)] history: Vec<GuardAudit>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupRecord {
    id: String, file_id: String, before_revision: String, after_revision: String,
    #[serde(default)] created_at: String,
    #[serde(default)] source: String,
    #[serde(default)] fields: Vec<String>,
    #[serde(default)] group_id: String,
    #[serde(default)] related_versions: BTreeMap<PathBuf, String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ProviderOrigin { id: String, revision: String }
thread_local! {
    static SOURCE_PROVIDER: RefCell<Option<ProviderOrigin>> = const { RefCell::new(None) };
    static REVIEW_TICKET: RefCell<Option<ReviewTicket>> = const { RefCell::new(None) };
}
struct ReviewTicket { authority: Option<String>, versions: BTreeMap<PathBuf, String> }
pub(crate) struct WriteSource(Option<ProviderOrigin>);
impl Drop for WriteSource { fn drop(&mut self) { SOURCE_PROVIDER.with(|source| *source.borrow_mut() = self.0.take()); } }
pub(crate) fn provider_switch_source(provider: &crate::provider::Provider) -> Result<WriteSource, AppError> {
    let revision = provider_revision(provider)?;
    Ok(WriteSource(SOURCE_PROVIDER.with(|source| source.replace(Some(ProviderOrigin { id: provider.id.clone(), revision })))))
}
fn provider_revision(provider: &crate::provider::Provider) -> Result<String, AppError> {
    let value = serde_json::to_value(provider).map_err(|source| AppError::JsonSerialize { source })?;
    let canonical = crate::config::sort_json_keys(&value);
    Ok(digest(&serde_json::to_vec(&canonical).map_err(|source| AppError::JsonSerialize { source })?))
}
fn related_versions(app: &AppType) -> Result<BTreeMap<PathBuf, String>, AppError> {
    app_management::app_files(app)?.into_iter().map(|path| file_revision(&path).map(|revision| (path, revision))).collect()
}
fn with_review_ticket<T>(authority: String, versions: BTreeMap<PathBuf, String>, operation: impl FnOnce() -> Result<T, AppError>) -> Result<T, AppError> {
    struct Reset(Option<ReviewTicket>);
    impl Drop for Reset { fn drop(&mut self) { REVIEW_TICKET.with(|ticket| *ticket.borrow_mut() = self.0.take()); } }
    let _reset = Reset(REVIEW_TICKET.with(|ticket| ticket.replace(Some(ReviewTicket { authority: Some(authority), versions }))));
    operation()
}
fn authority(store: &GuardStore) -> Result<String, AppError> {
    let settings = serde_json::to_vec(&crate::settings::get_settings()).map_err(|source| AppError::JsonSerialize { source })?;
    Ok(format!("{}:{}:{}", app_management::read_policy()?.revision, store.rules_revision, digest(&settings)))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardFile {
    pub id: String, pub path: String, pub format: String,
    pub protected_paths: Vec<String>, pub protect_file: bool,
    pub has_baseline: bool, pub revision: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardChange {
    pub path: String, pub kind: String,
    pub before: Option<String>, pub after: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardPreview {
    pub id: String, pub app_id: String, pub path: String,
    pub conflicts: Vec<String>, pub changes: Vec<GuardChange>, pub revision: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardState {
    pub app_id: String, pub files: Vec<GuardFile>, pub pending_changes: Vec<GuardPreview>,
    pub backups: Vec<GuardBackup>, pub history: Vec<GuardAudit>,
}

fn store_path() -> PathBuf { app_management::device_state_dir().join("config-guard.json") }
fn read_store() -> Result<GuardStore, AppError> {
    if fs::metadata(store_path()).is_ok_and(|metadata| metadata.len() > 128 * 1024 * 1024) {
        return Err(AppError::Config("Local configuration protection state exceeds the safety limit; writes are blocked".into()));
    }
    match fs::read(store_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| AppError::Config("Local configuration protection state is invalid; writes are blocked".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GuardStore::default()),
        Err(error) => Err(AppError::io(store_path(), error)),
    }
}
fn save_store(store: &GuardStore) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(store).map_err(|source| AppError::JsonSerialize { source })?;
    crate::config::atomic_write_private_raw(&store_path(), &bytes)
}
pub(crate) fn file_revision(path: &Path) -> Result<String, AppError> {
    Ok(content_revision(read_content(path)?.as_deref()))
}
fn content_revision(content: Option<&str>) -> String {
    content.map(|content| digest(content.as_bytes())).unwrap_or_else(|| "missing".into())
}
fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn file_id(app: &AppType, path: &Path) -> String { digest(format!("{}:{}", app.as_str(), path.to_string_lossy()).as_bytes()) }
fn protection_revision(path: &Path, store: &GuardStore) -> Result<String, AppError> {
    Ok(format!("{}:{}", file_revision(path)?, store.rules_revision))
}
fn read_content(path: &Path) -> Result<Option<String>, AppError> {
    match fs::metadata(path) {
        Ok(meta) if !meta.is_file() || meta.len() > MAX_FILE_BYTES => return Err(AppError::Config(format!("Configuration is not a bounded regular file: {}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io(path, error)),
        _ => {}
    }
    fs::read_to_string(path).map(Some).map_err(|error| AppError::io(path, error))
}

fn registered_app(path: &Path) -> Result<Option<AppType>, AppError> {
    // Only registered config resources get field merges. Management still gates
    // every file under an application's roots, including generated role files.
    for app in AppType::all() {
        if app_management::app_files(&app)?.iter().any(|file| file == path) { return Ok(Some(app)); }
    }
    if document::format(path) != "text" {
        let owners = app_management::owners_of_path(path)?;
        if owners.len() == 1 { return Ok(owners.into_iter().next()); }
    }
    Ok(None)
}

fn checked_file(app: &AppType, id: &str, store: &GuardStore) -> Result<PathBuf, AppError> {
    let file = app_management::app_files(app)?.into_iter().find(|path| file_id(app, path) == id)
        .or_else(|| store.files.get(id).filter(|file| file.app_id == app.as_str()).map(|file| file.path.clone()))
        .ok_or_else(|| AppError::InvalidInput("Unknown registered configuration file".into()))?;
    if registered_app(&file)?.as_ref() != Some(app) { return Err(AppError::InvalidInput("Configuration directory changed; refresh the file list".into())); }
    Ok(file)
}

fn external_catalog_protection(app: &AppType, path: &Path, local: &Value) -> Vec<String> {
    if *app == AppType::Codex && path.file_name().is_some_and(|name| name == "config.toml") {
        if let Some(reference) = local.get("model_catalog_json").and_then(Value::as_str) {
            let own_path = crate::codex_config::get_codex_config_dir().join(crate::codex_config::CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME);
            if reference != crate::codex_config::CC_SWITCH_CODEX_MODEL_CATALOG_FILENAME && Path::new(reference) != own_path {
                return vec!["/model_catalog_json".into()];
            }
        }
    }
    vec![]
}

fn describe(value: Option<&Value>) -> Option<String> {
    // Diff summaries deliberately never contain credential or arbitrary string
    // contents. Users see field paths, value types, and the operation instead.
    value.map(|value| match value {
        Value::Null => "null".into(), Value::Bool(value) => value.to_string(), Value::Number(value) => value.to_string(),
        Value::String(value) => format!("[string, {} chars]", value.chars().count()),
        Value::Array(value) => format!("[array, {} items]", value.len()),
        Value::Object(value) => format!("[object, {} fields]", value.len()),
    })
}

fn evaluate(app: &AppType, path: &Path, candidate: &str, store: &GuardStore, approved: bool) -> Result<(String, FileState, GuardPreview), AppError> {
    let current = read_content(path)?;
    let kind = document::format(path);
    let id = file_id(app, path);
    let mut state = store.files.get(&id).cloned().unwrap_or_else(|| FileState { app_id: app.as_str().into(), path: path.to_path_buf(), ..Default::default() });
    state.owned.extend(ownership::defaults(app, path));
    let local = document::parse(kind, current.as_deref().unwrap_or(""))?;
    let desired = document::parse(kind, candidate)?;
    let base = state.baseline.as_deref().map(|text| document::parse(kind, text)).transpose()?;
    let mut protection = state.protected_paths.clone();
    protection.extend(external_catalog_protection(app, path, &local));
    let owned = state.owned.clone();
    let mut merge = document::Merge { owned: &owned, protected: &protection, conflicts: vec![], changes: BTreeSet::new(), approved };
    let merged = merge.value(base.as_ref(), current.as_ref().map(|_| &local), Some(&desired), "").unwrap_or(Value::Null);
    if !approved && current.is_some() {
        // Never report success with a proposed connection field silently left
        // at a different user-owned value (including custom credential names).
        let changing_connection = SOURCE_PROVIDER.with(|source| source.borrow().is_some())
            || merge.changes.iter().any(|field| document::is_connection_field(field));
        merge.conflicts.extend(document::changed_paths(&merged, &desired).into_iter().filter(|field| {
            let proposed_connection = (document::is_connection_field(field) || path.file_name().is_some_and(|name| name == ".credentials.yaml"))
                && document::at_path(&desired, field).is_some();
            let retained_credential = changing_connection && field.rsplit('/').next()
                .is_some_and(crate::services::ProviderService::is_sensitive_config_key);
            proposed_connection || retained_credential
        }));
    }
    if current.is_none() && state.baseline.is_some() && !approved { merge.conflicts.push("/".into()); }
    if state.protect_file && current.as_deref() != Some(candidate) { merge.conflicts.push("/".into()); }
    merge.conflicts.sort(); merge.conflicts.dedup();
    let mut changed = merge.changes.clone();
    changed.extend(merge.conflicts.iter().cloned());
    if !approved && !merge.conflicts.is_empty() {
        let mut reviewed = document::Merge { owned: &owned, protected: &protection, conflicts: vec![], changes: BTreeSet::new(), approved: true };
        let reviewed_value = reviewed.value(base.as_ref(), current.as_ref().map(|_| &local), Some(&desired), "").unwrap_or(Value::Null);
        changed.extend(document::changed_paths(&local, &reviewed_value));
    }
    let preview = GuardPreview {
        id: uuid::Uuid::new_v4().to_string(), app_id: app.as_str().into(), path: path.display().to_string(),
        conflicts: merge.conflicts, revision: current.as_ref().map(|text| digest(text.as_bytes())).unwrap_or_else(|| "missing".into()),
        changes: changed.iter().map(|path| GuardChange {
            path: path.clone(), kind: if document::at_path(&desired, path).is_none() { "remove" } else { "set" }.into(),
            before: describe(document::at_path(&local, path)), after: describe(document::at_path(&desired, path)),
        }).collect(),
    };
    let rendered = document::render(kind, current.as_deref().unwrap_or(""), candidate, &local, &desired, &merged)?;
    if approved || (current.is_none() && kind == "markdown") { document::leaves(&desired, "", &mut state.owned); }
    state.baseline = Some(rendered.clone());
    Ok((rendered, state, preview))
}

fn evaluate_change(app: &AppType, path: &Path, candidate: Option<&str>, store: &GuardStore, approved: bool) -> Result<(Option<String>, FileState, GuardPreview), AppError> {
    if let Some(candidate) = candidate {
        return evaluate(app, path, candidate, store, approved).map(|(content, next, preview)| (Some(content), next, preview));
    }
    let current = read_content(path)?;
    let mut next = store.files.get(&file_id(app, path)).cloned().unwrap_or_else(|| FileState {
        app_id: app.as_str().into(), path: path.into(), ..Default::default()
    });
    next.owned.extend(ownership::defaults(app, path));
    let local = document::parse(document::format(path), current.as_deref().unwrap_or_default())?;
    let mut fields = BTreeSet::new();
    let mut conflicts = Vec::new();
    if current.is_some() {
        document::leaves(&local, "", &mut fields);
        let unowned = fields.iter().any(|field| !next.owned.iter().any(|owned| document::matches_path(owned, field)));
        if next.protect_file || !next.protected_paths.is_empty()
            || (!approved && (next.baseline != current || unowned)) {
            conflicts.push("/".into());
        }
        if fields.is_empty() { fields.insert("/".into()); }
    }
    let preview = GuardPreview {
        id: uuid::Uuid::new_v4().to_string(), app_id: app.as_str().into(), path: path.display().to_string(),
        conflicts, revision: content_revision(current.as_deref()),
        changes: fields.into_iter().map(|path| GuardChange {
            before: describe(document::at_path(&local, &path)), after: None, path, kind: "remove".into(),
        }).collect(),
    };
    next.baseline = None;
    Ok((None, next, preview))
}

fn record_pending(store: &mut GuardStore, app: &AppType, path: &Path, candidate: &str, preview: GuardPreview) -> Result<(), AppError> {
    let id = file_id(app, path);
    if !preview.conflicts.is_empty() { records::record(store, app, "conflict", vec![path.to_path_buf()], preview.conflicts.clone()); }
    store.pending.retain(|_, pending| pending.file_id != id);
    store.pending.insert(preview.id.clone(), Pending { preview, file_id: id, candidate: candidate.into(), delete_file: false, group_id: None,
        source_provider: SOURCE_PROVIDER.with(|source| source.borrow().clone()), authority: authority(store)?, related_versions: related_versions(app)?, restore_group: None });
    save_store(store)
}

/// Authentication, endpoint and model routing are one logical connection even
/// when stored in separate files. A proposal changing any of them must not
/// silently preserve an independently changed auth mode/key from another file.
fn connection_conflicts(app: &AppType, path: &Path, candidate: &str, store: &GuardStore) -> Result<Vec<String>, AppError> {
    if app_management::is_reviewed_write(app) { return Ok(vec![]); }
    let Some(baseline) = store.files.get(&file_id(app, path)).and_then(|file| file.baseline.as_deref()) else { return Ok(vec![]); };
    let kind = document::format(path);
    let base = document::parse(kind, baseline)?;
    let local = document::parse(kind, &read_content(path)?.unwrap_or_default())?;
    let desired = document::parse(kind, candidate)?;
    Ok(document::changed_paths(&base, &local).into_iter().filter(|field|
        document::is_connection_field(field) && document::at_path(&local, field) != document::at_path(&desired, field)
    ).collect())
}

pub(crate) fn preflight_files(app: &AppType, files: &[(PathBuf, Vec<u8>)]) -> Result<(), AppError> {
    let changes = files.iter().map(|(path, bytes)| (path.clone(), Some(bytes.clone()))).collect::<Vec<_>>();
    preflight_file_changes(app, &changes)
}

pub(crate) fn preflight_file_changes(app: &AppType, files: &[(PathBuf, Option<Vec<u8>>)]) -> Result<(), AppError> {
    let _guard = app_management::native_mutation_guard()?;
    app_management::require_managed(app)?;
    let mut store = read_store()?;
    let mut pending = Vec::new();
    for (path, bytes) in files {
        let _permission = app_management::permit_path(path)?;
        if registered_app(path)?.as_ref() != Some(app) { return Err(AppError::InvalidInput("Unregistered configuration bundle target".into())); }
        let candidate = bytes.as_deref().map(std::str::from_utf8).transpose().map_err(|_| AppError::Config("Configuration is not UTF-8".into()))?;
        let (_, _, preview) = evaluate_change(app, path, candidate, &store, app_management::is_reviewed_write(app))?;
        pending.push((file_id(app, path), candidate.map(str::to_string), preview));
    }
    if pending.iter().any(|(_, _, preview)| preview.changes.iter().any(|change| document::is_connection_field(&change.path))) {
        for ((path, _), (_, candidate, preview)) in files.iter().zip(pending.iter_mut()) {
            preview.conflicts.extend(connection_conflicts(app, path, candidate.as_deref().unwrap_or_default(), &store)?);
        }
    }
    if !pending.iter().any(|(_, _, preview)| !preview.conflicts.is_empty()) { return Ok(()); }
    records::record(&mut store, app, "conflict", files.iter().map(|(path, _)| path.clone()).collect(), pending.iter().flat_map(|(_, _, preview)| preview.conflicts.clone()).collect());
    let group_id = uuid::Uuid::new_v4().to_string();
    let stamp = authority(&store)?;
    let versions = related_versions(app)?;
    for (id, candidate, preview) in pending {
        store.pending.retain(|_, pending| pending.file_id != id);
        store.pending.insert(preview.id.clone(), Pending { preview, file_id: id, delete_file: candidate.is_none(), candidate: candidate.unwrap_or_default(), group_id: Some(group_id.clone()),
            source_provider: SOURCE_PROVIDER.with(|source| source.borrow().clone()), authority: stamp.clone(), related_versions: versions.clone(), restore_group: None });
    }
    save_store(&store)?;
    Err(AppError::Config("Configuration conflict: review all related connection files before applying".into()))
}

pub(crate) fn guarded_write(path: &Path, bytes: &[u8], write: impl FnOnce(&[u8]) -> Result<(), AppError>) -> Result<(), AppError> {
    let Some(app) = registered_app(path)? else { return write(bytes); };
    commit_files(&app, &[(path.to_path_buf(), bytes.to_vec())])
}

/// Preflight all related connection files, then commit under one native-write
/// lock. A failed replacement rolls back only bytes this transaction installed.
pub(crate) fn commit_files(app: &AppType, files: &[(PathBuf, Vec<u8>)]) -> Result<(), AppError> {
    let changes = files.iter().map(|(path, bytes)| (path.clone(), Some(bytes.clone()))).collect::<Vec<_>>();
    commit_file_changes(app, &changes)
}

pub(crate) fn commit_file_changes(app: &AppType, files: &[(PathBuf, Option<Vec<u8>>)]) -> Result<(), AppError> {
    let _guard = app_management::native_mutation_guard()?;
    app_management::require_managed(app)?;
    let mut store = read_store()?;
    REVIEW_TICKET.with(|ticket| -> Result<(), AppError> {
        if let Some(ticket) = ticket.borrow().as_ref() {
            if ticket.authority.as_ref().is_some_and(|expected| authority(&store).as_ref().ok() != Some(expected)) {
                return Err(AppError::Config("Management or settings changed while the approved write was queued".into()));
            }
            for (path, expected) in &ticket.versions {
                if file_revision(path)? != *expected {
                    return Err(AppError::Config("A related file changed after preview; approved write cancelled".into()));
                }
            }
            for (path, _) in files {
                let current = file_revision(path)?;
                if ticket.versions.get(path).is_some_and(|expected| *expected != current)
                    || (!ticket.versions.contains_key(path) && current != "missing") {
                    return Err(AppError::Config("A related file changed after preview; approved write cancelled".into()));
                }
            }
        }
        Ok(())
    })?;
    let mut prepared = Vec::new();
    for (path, bytes) in files {
        let _permission = app_management::permit_path(path)?;
        if registered_app(path)?.as_ref() != Some(app) { return Err(AppError::InvalidInput("Unregistered configuration bundle target".into())); }
        let candidate = bytes.as_deref().map(std::str::from_utf8).transpose().map_err(|_| AppError::Config("Configuration is not UTF-8".into()))?;
        let (content, next, preview) = evaluate_change(app, path, candidate, &store, app_management::is_reviewed_write(app))?;
        prepared.push((path.clone(), candidate.map(str::to_string), content, next, preview));
    }
    if prepared.iter().any(|(_, _, _, _, preview)| preview.changes.iter().any(|change| document::is_connection_field(&change.path))) {
        for (path, candidate, _, _, preview) in &mut prepared {
            preview.conflicts.extend(connection_conflicts(app, path, candidate.as_deref().unwrap_or_default(), &store)?);
        }
    }
    if prepared.iter().any(|(_, _, _, _, preview)| !preview.conflicts.is_empty()) {
        records::record(&mut store, app, "conflict", files.iter().map(|(path, _)| path.clone()).collect(), prepared.iter().flat_map(|(_, _, _, _, preview)| preview.conflicts.clone()).collect());
        let group_id = uuid::Uuid::new_v4().to_string();
        let stamp = authority(&store)?;
        let versions = related_versions(app)?;
        for (path, candidate, _, _, preview) in prepared {
            let id = file_id(app, &path);
            store.pending.retain(|_, pending| pending.file_id != id);
            store.pending.insert(preview.id.clone(), Pending { preview, file_id: id, delete_file: candidate.is_none(), candidate: candidate.unwrap_or_default(),
                group_id: Some(group_id.clone()), source_provider: SOURCE_PROVIDER.with(|source| source.borrow().clone()), authority: stamp.clone(), related_versions: versions.clone(), restore_group: None });
        }
        save_store(&store)?;
        return Err(AppError::Config("Configuration has external/user-owned changes. Review the complete connection change in Configuration protection.".into()));
    }
    let mut previous = Vec::new();
    let group_id = uuid::Uuid::new_v4().to_string();
    let mut after_versions = related_versions(app)?;
    for (path, _, content, _, _) in &prepared { after_versions.insert(path.clone(), content_revision(content.as_deref())); }
    for (path, _, content, _, preview) in &prepared {
        if file_revision(path)? != preview.revision { return Err(AppError::Config("Configuration changed before commit; preview again".into())); }
        let before = read_content(path)?;
        if before != *content {
            let id = uuid::Uuid::new_v4().to_string();
            if let Some(before) = &before {
                let backup_path = app_management::device_state_dir().join("backups/config-guard").join(format!("{id}.txt"));
                crate::config::atomic_write_private_raw(&backup_path, before.as_bytes())?;
            }
            store.files.entry(file_id(app, path)).or_insert_with(|| FileState { app_id: app.as_str().into(), path: path.clone(), ..Default::default() });
            store.backups.push(BackupRecord { id, file_id: file_id(app, path), before_revision: preview.revision.clone(), after_revision: content_revision(content.as_deref()),
                created_at: chrono::Utc::now().to_rfc3339(), source: records::source(), fields: preview.changes.iter().map(|change| change.path.clone()).collect(),
                group_id: group_id.clone(), related_versions: after_versions.clone() });
        }
        previous.push(before);
    }
    // Durable backup index is saved before replacing the first client file.
    save_store(&store)?;
    let checkpoint = store.clone();
    let mut written: Vec<usize> = Vec::new();
    let result = (|| -> Result<(), AppError> {
        for (index, (path, _, content, _, preview)) in prepared.iter().enumerate() {
            #[cfg(test)]
            tests::before_replace(path);
            if file_revision(path)? != preview.revision {
                return Err(AppError::Config("Configuration changed during commit; rollback required".into()));
            }
            if content_revision(content.as_deref()) != preview.revision {
                replace_content(path, content.as_deref())?;
                written.push(index);
            }
        }
        let fields = prepared.iter().flat_map(|(_, _, _, _, preview)| preview.changes.iter().map(|change| change.path.clone())).collect();
        records::record(&mut store, app, "applied", files.iter().map(|(path, _)| path.clone()).collect(), fields);
        for (path, _, _, next, _) in &prepared {
            let id = file_id(app, path);
            store.files.insert(id.clone(), next.clone());
            store.pending.retain(|_, pending| pending.file_id != id);
        }
        save_store(&store)
    })();
    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        for written_index in written.into_iter().rev() {
            let (written_path, _, installed, _, _) = &prepared[written_index];
            if file_revision(written_path).ok() != Some(content_revision(installed.as_deref())) {
                rollback_errors.push(format!("External edit preserved: {}", written_path.display()));
                continue;
            }
            if let Err(error) = replace_content(written_path, previous[written_index].as_deref()) {
                rollback_errors.push(error.to_string());
            }
        }
        store = checkpoint;
        records::record(&mut store, app, "failed", files.iter().map(|(path, _)| path.clone()).collect(), vec![]);
        if let Err(error) = save_store(&store) { rollback_errors.push(error.to_string()); }
        return Err(AppError::Config(format!("{error}; rollback: {}", if rollback_errors.is_empty() { "completed".into() } else { rollback_errors.join("; ") })));
    }
    for (path, _, content, _, _) in prepared {
        REVIEW_TICKET.with(|ticket| {
            if let Some(ticket) = ticket.borrow_mut().as_mut() { ticket.authority = None; ticket.versions.insert(path, content_revision(content.as_deref())); }
        });
    }
    Ok(())
}

fn replace_content(path: &Path, content: Option<&str>) -> Result<(), AppError> {
    match content {
        Some(content) => crate::config::atomic_write_private_raw(path, content.as_bytes()),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::io(path, error)),
        },
    }
}

pub(crate) fn require_delete(path: &Path) -> Result<(), AppError> {
    let Some(app) = registered_app(path)? else { return Ok(()); };
    let current = read_content(path)?;
    if current.is_none() { return Ok(()); }
    let store = read_store()?;
    let state = store.files.get(&file_id(&app, path));
    if state.is_none_or(|state| state.protect_file || !state.protected_paths.is_empty() || state.baseline != current) {
        return Err(AppError::Config("Deletion would remove unreviewed or protected configuration; review it first".into()));
    }
    Ok(())
}

pub fn get_state(app: &AppType) -> Result<GuardState, AppError> {
    let _guard = app_management::native_mutation_guard()?;
    let mut store = read_store()?;
    scan_external_changes(app, &mut store)?;
    let mut files = Vec::new();
    let mut resources = app_management::app_files(app)?;
    resources.extend(store.files.values().filter(|file| file.app_id == app.as_str()).map(|file| file.path.clone()));
    resources.sort(); resources.dedup();
    for path in resources {
        let id = file_id(app, &path);
        let state = store.files.get(&id);
        files.push(GuardFile { id, path: path.display().to_string(), format: document::format(&path).into(),
            protected_paths: state.map(|state| state.protected_paths.clone()).unwrap_or_default(),
            protect_file: state.is_some_and(|state| state.protect_file), has_baseline: state.is_some_and(|state| state.baseline.is_some()), revision: protection_revision(&path, &store)?,
        });
    }
    Ok(GuardState { app_id: app.as_str().into(), files, pending_changes: store.pending.values().filter(|pending| pending.preview.app_id == app.as_str()).map(|pending| pending.preview.clone()).collect(),
        backups: records::backups(&store, app), history: store.history.iter().rev().filter(|entry| entry.app_id == app.as_str()).cloned().collect() })
}

fn scan_external_changes(app: &AppType, store: &mut GuardStore) -> Result<(), AppError> {
    let candidates: Vec<_> = store.files.iter().filter(|(id, file)| file.app_id == app.as_str()
        && file.baseline.is_some() && !store.pending.values().any(|pending| &pending.file_id == *id))
        .map(|(_, file)| file.clone()).collect();
    for file in candidates {
        if registered_app(&file.path)?.as_ref() != Some(app) { continue; }
        let baseline = file.baseline.as_deref().unwrap();
        let current = read_content(&file.path)?;
        if current.as_deref() == Some(baseline) { continue; }
        let kind = document::format(&file.path);
        let before = document::parse(kind, baseline)?;
        let local = document::parse(kind, current.as_deref().unwrap_or(""))?;
        let fields = document::changed_paths(&before, &local);
        if fields.is_empty() && current.is_some() { continue; } // formatting-only edits remain untouched
        let (_, _, mut preview) = evaluate(app, &file.path, baseline, store, false)?;
        preview.conflicts = fields.iter().cloned().collect();
        if preview.conflicts.is_empty() { preview.conflicts.push("/".into()); }
        preview.changes = fields.into_iter().map(|path| GuardChange {
            kind: if document::at_path(&before, &path).is_none() { "remove" } else { "set" }.into(),
            before: describe(document::at_path(&local, &path)), after: describe(document::at_path(&before, &path)), path,
        }).collect();
        record_pending(store, app, &file.path, baseline, preview)?;
    }
    Ok(())
}

pub fn set_protection(app: &AppType, id: &str, paths: Vec<String>, protect_file: bool, revision: &str) -> Result<GuardState, AppError> {
    let _guard = app_management::native_mutation_guard()?;
    if paths.len() > 128 || paths.iter().any(|path| path.len() > 1024 || path.contains('\0')) { return Err(AppError::InvalidInput("Too many or invalid protected field paths".into())); }
    let mut store = read_store()?;
    let path = checked_file(app, id, &store)?;
    if protection_revision(&path, &store)? != revision { return Err(AppError::Config("Configuration or protection rules changed; refresh before editing protection".into())); }
    let state = store.files.entry(id.into()).or_insert_with(|| FileState { app_id: app.as_str().into(), path, ..Default::default() });
    state.protect_file = protect_file;
    state.protected_paths = paths.into_iter().map(|path| if path.starts_with('/') || path == "*" { path } else { format!("/{}", path.replace('.', "/")) }).collect();
    store.rules_revision += 1;
    save_store(&store)?;
    get_state(app)
}

pub fn preview(app: &AppType, id: &str) -> Result<GuardPreview, AppError> {
    let _guard = app_management::native_mutation_guard()?;
    let mut store = read_store()?;
    let path = checked_file(app, id, &store)?;
    let existing = store.pending.values().find(|pending| pending.file_id == id).cloned();
    if let Some(pending) = existing.as_ref().filter(|pending| pending.group_id.is_some()) {
        if let Some(group) = &pending.restore_group { records::validate_restore_group(app, group, &store)?; }
        let group: Vec<_> = store.pending.values().filter(|item| item.group_id == pending.group_id).cloned().collect();
        let stamp = authority(&store)?;
        let versions = related_versions(app)?;
        let mut selected = None;
        for mut item in group {
            let item_path = checked_file(app, &item.file_id, &store)?;
            let candidate = if item.delete_file { None } else { Some(item.candidate.as_str()) };
            let (_, _, next) = evaluate_change(app, &item_path, candidate, &store, false)?;
            store.pending.remove(&item.preview.id);
            item.preview = next;
            item.authority = stamp.clone();
            item.related_versions = versions.clone();
            if item.file_id == id { selected = Some(item.preview.clone()); }
            store.pending.insert(item.preview.id.clone(), item);
        }
        save_store(&store)?;
        return selected.ok_or_else(|| AppError::Config("Configuration preview could not be refreshed".into()));
    }
    let candidate = existing.as_ref().map(|pending| pending.candidate.clone())
        .unwrap_or(read_content(&path)?.unwrap_or_default());
    let (_, _, preview) = evaluate(app, &path, &candidate, &store, false)?;
    record_pending(&mut store, app, &path, &candidate, preview.clone())?;
    if let Some(existing) = existing {
        if let Some(pending) = store.pending.get_mut(&preview.id) { pending.source_provider = existing.source_provider; }
        save_store(&store)?;
    }
    Ok(preview)
}

pub fn apply(preview_id: &str, resolution: &str, app_state: Option<&crate::store::AppState>) -> Result<GuardState, AppError> {
    let guard = app_management::native_mutation_guard()?;
    let mut store = read_store()?;
    let pending = store.pending.get(preview_id).cloned().ok_or_else(|| AppError::InvalidInput("Configuration preview expired; preview again".into()))?;
    let app = AppType::from_str(&pending.preview.app_id)?;
    if pending.authority != authority(&store)? { return Err(AppError::Config("Settings or protection rules changed since preview; preview again".into())); }
    if let Some(group) = &pending.restore_group { records::validate_restore_group(&app, group, &store)?; }
    for (path, revision) in &pending.related_versions {
        if file_revision(path)? != *revision { return Err(AppError::Config("A related configuration file changed since preview; preview again".into())); }
    }
    let group: Vec<_> = if let Some(group_id) = &pending.group_id {
        store.pending.values().filter(|item| item.group_id.as_ref() == Some(group_id)).cloned().collect()
    } else { vec![pending.clone()] };
    let mut files = Vec::new();
    for item in &group {
        let path = checked_file(&app, &item.file_id, &store)?;
        if item.authority != pending.authority || file_revision(&path)? != item.preview.revision { return Err(AppError::Config("Configuration changed since preview; no changes were applied".into())); }
        files.push((path, (!item.delete_file).then(|| item.candidate.as_bytes().to_vec())));
    }
    let management = app_management::read_policy()?.app(&app);
    if !management.enabled { return Err(AppError::Config("Enable application management before applying configuration changes".into())); }
    match resolution {
        "keep_local" => {
            for (item, (path, _)) in group.iter().zip(files.iter()) {
                let current = read_content(path)?;
                let state = store.files.entry(item.file_id.clone()).or_insert_with(|| FileState { app_id: app.as_str().into(), path: path.clone(), ..Default::default() });
                state.baseline = current;
                for conflict in &item.preview.conflicts { if !state.protected_paths.contains(conflict) { state.protected_paths.push(conflict.clone()); } }
                store.pending.remove(&item.preview.id);
            }
            store.rules_revision += 1;
            records::record(&mut store, &app, "kept_local", files.iter().map(|(path, _)| path.clone()).collect(), group.iter().flat_map(|item| item.preview.conflicts.clone()).collect());
            save_store(&store)?;
        }
        "apply_ccs" => {
            if let (Some(origin), Some(state)) = (&pending.source_provider, app_state) {
                let provider = state.db.get_provider_by_id(&origin.id, app.as_str())?.ok_or_else(|| AppError::Config("Preview provider no longer exists".into()))?;
                if provider_revision(&provider)? != origin.revision {
                    return Err(AppError::Config("Provider changed since preview; retry the provider operation".into()));
                }
                // Resume the real Provider transaction, including current-provider
                // state, rather than applying its files behind that transaction.
                drop(guard);
                with_review_ticket(pending.authority.clone(), pending.related_versions.clone(), ||
                    app_management::with_reviewed_write(&app, || crate::services::ProviderService::switch(state, app.clone(), &origin.id).map(|_| ())))?;
                let _guard = app_management::native_mutation_guard()?;
                finish_review(&app)?;
                return get_state(&app);
            }
            with_review_ticket(pending.authority.clone(), pending.related_versions.clone(), ||
                app_management::with_reviewed_write(&app, || commit_file_changes(&app, &files)))?;
        }
        _ => return Err(AppError::InvalidInput("Unknown conflict resolution".into())),
    }
    finish_review(&app)?;
    get_state(&app)
}

fn finish_review(app: &AppType) -> Result<(), AppError> {
    let state = get_state(app)?;
    if state.pending_changes.is_empty() && state.files.iter().all(|file| file.revision.starts_with("missing:") || file.has_baseline) {
        let mut policy = app_management::read_policy()?;
        if policy.app(app).phase == app_management::ManagementPhase::PendingReview {
            policy.phases.insert(app.as_str().into(), app_management::ManagementPhase::Managed);
            policy.messages.remove(app.as_str());
            policy.revision += 1;
            app_management::save_policy(&policy)?;
        }
    }
    Ok(())
}

pub use records::preview_restore;

pub(crate) fn begin_review(app: &AppType) -> Result<(), AppError> {
    let _guard = app_management::native_mutation_guard()?;
    let mut store = read_store()?;
    // Each enable starts a fresh confirmation, even if the old baseline matched.
    for state in store.files.values_mut().filter(|state| state.app_id == app.as_str()) { state.baseline = None; }
    save_store(&store)?;
    for path in app_management::app_files(app)? {
        if path.is_file() { preview(app, &file_id(app, &path))?; }
    }
    finish_review(app)
}

/// Restore only fields whose current value is still the value ccs installed.
/// The backup is a source of individual old fields, never a whole-file overwrite.
pub(crate) fn release_edit(path: PathBuf, before: &str, installed: &str) -> Result<Option<app_management::ReleaseEdit>, AppError> {
    let Some(current) = read_content(&path)? else { return Ok(None); };
    let kind = document::format(&path);
    let old = document::parse(kind, before)?;
    let expected = document::parse(kind, installed)?;
    let local = document::parse(kind, &current)?;
    let mut owned = BTreeSet::new();
    document::leaves(&expected, "", &mut owned);
    let mut protected = Vec::new();
    if let Some(app) = registered_app(&path)? {
        let store = read_store()?;
        if let Some(state) = store.files.get(&file_id(&app, &path)) {
            protected = state.protected_paths.clone();
            if state.protect_file { protected.push("/".into()); }
        }
    }
    let mut merge = document::Merge { owned: &owned, protected: &protected, conflicts: vec![], changes: BTreeSet::new(), approved: false };
    let restored = merge.value(Some(&expected), Some(&local), Some(&old), "").unwrap_or(Value::Null);
    if !merge.conflicts.is_empty() {
        return Err(AppError::Config(format!("{} has externally changed takeover fields: {}", path.display(), merge.conflicts.join(", "))));
    }
    let content = document::render(kind, &current, before, &local, &old, &restored)?;
    Ok((content != current).then(|| app_management::ReleaseEdit { path, revision: digest(current.as_bytes()), content }))
}
