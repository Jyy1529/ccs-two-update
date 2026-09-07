//! Local-only, redacted change audit and version-bound backup recovery.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardBackup {
    pub id: String, pub file_id: String, pub path: String, pub created_at: String,
    pub source: String, pub fields: Vec<String>, pub before_revision: String,
    pub after_revision: String, pub group_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardAudit {
    pub id: String, pub app_id: String, pub created_at: String, pub source: String,
    pub result: String, pub paths: Vec<String>, pub fields: Vec<String>,
}

pub(super) fn source() -> String {
    if SOURCE_PROVIDER.with(|source| source.borrow().is_some()) { "provider_switch" } else { "native_configuration" }.into()
}

pub(super) fn record(store: &mut GuardStore, app: &AppType, result: &str, paths: Vec<PathBuf>, mut fields: Vec<String>) {
    fields.sort(); fields.dedup();
    store.history.push(GuardAudit {
        id: uuid::Uuid::new_v4().to_string(), app_id: app.as_str().into(), created_at: chrono::Utc::now().to_rfc3339(),
        source: source(), result: result.into(), paths: paths.iter().map(|path| path.display().to_string()).collect(), fields,
    });
    // Bound metadata only. Never delete backups or user files automatically.
    if store.history.len() > 512 { store.history.drain(..store.history.len() - 512); }
}

pub(super) fn backups(store: &GuardStore, app: &AppType) -> Vec<GuardBackup> {
    store.backups.iter().rev().filter_map(|backup| {
        let file = store.files.get(&backup.file_id).filter(|file| file.app_id == app.as_str())?;
        Some(GuardBackup {
            id: backup.id.clone(), file_id: backup.file_id.clone(), path: file.path.display().to_string(),
            created_at: backup.created_at.clone(), source: backup.source.clone(), fields: backup.fields.clone(),
            before_revision: backup.before_revision.clone(), after_revision: backup.after_revision.clone(), group_id: backup.group_id.clone(),
        })
    }).collect()
}

pub(super) fn validate_restore_group(app: &AppType, group_id: &str, store: &GuardStore) -> Result<(), AppError> {
    let group: Vec<_> = store.backups.iter().filter(|backup|
        !group_id.is_empty() && (backup.group_id == group_id || backup.id == group_id)
    ).collect();
    if group.is_empty() { return Err(AppError::Config("Backup preview expired; select the backup again".into())); }
    for backup in group {
        let path = checked_file(app, &backup.file_id, store)?;
        if file_revision(&path)? != backup.after_revision {
            return Err(AppError::Config("Configuration changed after the backup; restoration cancelled".into()));
        }
        for (path, expected) in &backup.related_versions {
            if file_revision(path)? != *expected {
                return Err(AppError::Config("A related configuration changed after this backup was created. External edits were preserved; review the current configuration instead of restoring stale files.".into()));
            }
        }
    }
    Ok(())
}

pub fn preview_restore(app: &AppType, backup_id: &str) -> Result<GuardPreview, AppError> {
    let _guard = app_management::native_mutation_guard()?;
    app_management::require_managed(app)?;
    let mut store = read_store()?;
    let chosen = store.backups.iter().find(|backup| backup.id == backup_id).cloned()
        .ok_or_else(|| AppError::InvalidInput("Unknown local backup".into()))?;
    checked_file(app, &chosen.file_id, &store)?;
    let group: Vec<_> = store.backups.iter().filter(|backup| backup.id == chosen.id || (!chosen.group_id.is_empty() && backup.group_id == chosen.group_id)).cloned().collect();
    let restore_group = if chosen.group_id.is_empty() { chosen.id.clone() } else { chosen.group_id.clone() };
    validate_restore_group(app, &restore_group, &store)?;
    let group_id = uuid::Uuid::new_v4().to_string();
    let stamp = authority(&store)?;
    let versions = related_versions(app)?;
    let mut selected = None;
    for backup in group {
        let path = checked_file(app, &backup.file_id, &store)?;
        if file_revision(&path)? != backup.after_revision {
            return Err(AppError::Config("Configuration changed after the backup; restoration cancelled".into()));
        }
        // IDs are looked up in local metadata, never used as frontend paths.
        if uuid::Uuid::parse_str(&backup.id).is_err() { return Err(AppError::Config("Invalid local backup identifier".into())); }
        let candidate = if backup.before_revision == "missing" {
            None
        } else {
            let backup_path = app_management::device_state_dir().join("backups/config-guard").join(format!("{}.txt", backup.id));
            Some(read_content(&backup_path)?.ok_or_else(|| AppError::Config("Backup file is missing".into()))?)
        };
        if content_revision(candidate.as_deref()) != backup.before_revision { return Err(AppError::Config("Backup integrity check failed".into())); }
        let (_, _, preview) = evaluate_change(app, &path, candidate.as_deref(), &store, false)?;
        if backup.id == chosen.id { selected = Some(preview.clone()); }
        store.pending.retain(|_, item| item.file_id != backup.file_id);
        store.pending.insert(preview.id.clone(), Pending {
            preview, file_id: backup.file_id, delete_file: candidate.is_none(), candidate: candidate.unwrap_or_default(), group_id: Some(group_id.clone()), source_provider: None,
            authority: stamp.clone(), related_versions: versions.clone(), restore_group: Some(restore_group.clone()),
        });
    }
    save_store(&store)?;
    selected.ok_or_else(|| AppError::Config("Backup preview could not be prepared".into()))
}
