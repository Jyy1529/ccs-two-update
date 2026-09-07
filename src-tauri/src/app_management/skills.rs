//! Bounded, content-preserving detachment of locally deployed Skills links.
use super::*;
use crate::{database::Database, services::skill::{SkillService, SkillStorageLocation}};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct LinkRelease { pub path: PathBuf, pub source: PathBuf, pub revision: String, pub recovery_id: Option<String> }

pub(super) fn ssot_path() -> PathBuf {
    match crate::settings::get_skill_storage_location() {
        SkillStorageLocation::CcSwitch => crate::config::get_app_config_dir().join("skills"),
        SkillStorageLocation::Unified => crate::config::get_home_dir().join(".agents/skills"),
    }
}

fn regular_metadata(path: &Path) -> Result<fs::Metadata, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|e| AppError::io(path, e))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 { return Err(AppError::Config("Nested reparse points cannot be safely detached".into())); }
    }
    if metadata.file_type().is_symlink() { return Err(AppError::Config("Nested symlinks cannot be safely detached".into())); }
    Ok(metadata)
}

fn visit(dir: &Path, root: &Path, files: &mut Vec<PathBuf>, total: &mut u64, depth: usize) -> Result<(), AppError> {
    if depth > 32 || files.len() > 4096 { return Err(AppError::Config("Skill detachment exceeds its directory/file limit".into())); }
    for entry in fs::read_dir(dir).map_err(|e| AppError::io(dir, e))? {
        let path = entry.map_err(|e| AppError::io(dir, e))?.path();
        let metadata = regular_metadata(&path)?;
        if metadata.is_dir() {
            files.push(path.strip_prefix(root).map_err(|_| AppError::Config("Invalid Skill path".into()))?.to_path_buf());
            visit(&path, root, files, total, depth + 1)?;
        }
        else if metadata.is_file() {
            *total = total.saturating_add(metadata.len());
            if *total > 64 * 1024 * 1024 { return Err(AppError::Config("Skill detachment exceeds 64 MiB".into())); }
            files.push(path.strip_prefix(root).map_err(|_| AppError::Config("Invalid Skill path".into()))?.to_path_buf());
        } else { return Err(AppError::Config("Skill includes a non-regular resource".into())); }
    }
    Ok(())
}

fn tree_files(root: &Path) -> Result<Vec<PathBuf>, AppError> {
    if !regular_metadata(root)?.is_dir() { return Err(AppError::Config("Skill source is not a directory".into())); }
    let mut files = Vec::new();
    visit(root, root, &mut files, &mut 0, 0)?;
    files.sort();
    Ok(files)
}

fn tree_revision(root: &Path) -> Result<String, AppError> {
    let mut hash = Sha256::new();
    for relative in tree_files(root)? {
        let path = root.join(&relative);
        hash.update(relative.to_string_lossy().as_bytes());
        if regular_metadata(&path)?.is_dir() { hash.update(b"directory\0"); continue; }
        let bytes = fs::read(&path).map_err(|e| AppError::io(&path, e))?;
        hash.update(b"file\0");
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn inspect(db: &Database, app: &AppType) -> Result<(Vec<LinkRelease>, Vec<String>, Vec<PathBuf>), AppError> {
    let root = SkillService::get_app_skills_dir(app).map_err(|e| AppError::Config(e.to_string()))?;
    let ssot = ssot_path();
    let mut conflicts = Vec::new();
    let mut blocked = Vec::new();
    let mut links = Vec::new();
    if crate::settings::get_skill_storage_location() == SkillStorageLocation::Unified {
        conflicts.push("Unified Skills are shared across applications. Ordinary writes will stop; shared library writes remain blocked until storage is separated.".into());
        blocked.push(ssot.clone());
    }
    if !root.exists() { return Ok((links, conflicts, blocked)); }
    if fs::symlink_metadata(&root).map_err(|e| AppError::io(&root, e))?.file_type().is_symlink() {
        conflicts.push(format!("Skills root is a directory alias: {}", root.display()));
        blocked.push(root.canonicalize().map_err(|e| AppError::io(&root, e))?);
        return Ok((links, conflicts, blocked));
    }
    let installed = db.get_all_installed_skills()?;
    let known: BTreeSet<_> = installed.values().filter(|skill| skill.apps.is_enabled_for(app)).map(|skill| skill.directory.as_str()).collect();
    let entries = fs::read_dir(&root).map_err(|e| AppError::io(&root, e))?.map(|entry| entry.map(|entry| entry.path()).map_err(|e| AppError::io(&root, e))).collect::<Result<Vec<_>, _>>()?;
    let mut recovered = BTreeSet::new();
    let mut recovery_paths = BTreeSet::new();
    for path in &entries {
        let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
        let Some(id) = name.strip_prefix(".ccs-detach-").and_then(|name| name.strip_suffix(".json")) else { continue; };
        let record = (|| -> Result<LinkRelease, AppError> {
            uuid::Uuid::parse_str(id).map_err(|_| AppError::Config("Invalid Skill recovery identifier".into()))?;
            if regular_metadata(path)?.len() > 16384 { return Err(AppError::Config("Oversized Skill recovery record".into())); }
            let mut link: LinkRelease = serde_json::from_slice(&fs::read(path).map_err(|e| AppError::io(path, e))?).map_err(|_| AppError::Config("Invalid Skill recovery record".into()))?;
            if link.path.parent() != Some(root.as_path()) || link.recovery_id.as_deref() != Some(id) {
                return Err(AppError::Config("Skill recovery path changed".into()));
            }
            let skill_name = link.path.file_name().and_then(|v| v.to_str()).ok_or_else(|| AppError::Config("Invalid Skill recovery name".into()))?;
            if !known.contains(skill_name) || ssot.join(skill_name).canonicalize().ok().as_ref() != Some(&link.source) {
                return Err(AppError::Config("Skill recovery ownership cannot be established".into()));
            }
            if tree_revision(&link.source)? != link.revision { return Err(AppError::Config("Skill source changed during interrupted detachment".into())); }
            link.recovery_id = Some(id.into());
            Ok(link)
        })();
        match record {
            Ok(link) => {
                recovered.insert(id.to_string());
                recovery_paths.insert(link.path.clone());
                blocked.push(link.source.clone());
                links.push(link);
            }
            Err(error) => { conflicts.push(error.to_string()); blocked.push(ssot.clone()); }
        }
    }
    for path in entries {
        let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
        if name.starts_with(".ccs-detach-") {
            if recovered.iter().any(|id| ["json", "copy", "link"].iter().any(|extension| name == format!(".ccs-detach-{id}.{extension}"))) { continue; }
            conflicts.push(format!("Interrupted detachment resource retained: {}", path.display()));
            blocked.push(ssot.clone());
            continue;
        }
        if recovery_paths.contains(&path) { continue; }
        let Ok(target) = fs::read_link(&path) else { continue; };
        let source = if target.is_absolute() { target } else { root.join(target) };
        let canonical = source.canonicalize().map_err(|e| AppError::io(&source, e))?;
        let expected = ssot.join(name).canonicalize().ok();
        if expected.as_ref() != Some(&canonical) {
            if paths_overlap(&ssot, &canonical) { conflicts.push(format!("Unrecognized shared Skill link: {}", path.display())); blocked.push(canonical); }
            continue;
        }
        if !known.contains(name) || !canonical.join("SKILL.md").is_file() {
            conflicts.push(format!("Skill link ownership is not established: {}", path.display()));
            blocked.push(canonical);
            continue;
        }
        match tree_revision(&canonical) {
            Ok(revision) => links.push(LinkRelease { path, source: canonical, revision, recovery_id: None }),
            Err(error) => { conflicts.push(error.to_string()); blocked.push(canonical); }
        }
    }
    Ok((links, conflicts, blocked))
}

pub(super) fn detach(link: &LinkRelease) -> Result<(), AppError> {
    if tree_revision(&link.source)? != link.revision {
        return Err(AppError::Config("Skill changed since preview; detach was not applied".into()));
    }
    let _permission = permit_path(&link.path)?;
    let parent = link.path.parent().ok_or_else(|| AppError::Config("Invalid Skill path".into()))?;
    let nonce = match &link.recovery_id {
        Some(id) => uuid::Uuid::parse_str(id).map_err(|_| AppError::Config("Invalid Skill recovery identifier".into()))?,
        None => uuid::Uuid::new_v4(),
    };
    let stage = parent.join(format!(".ccs-detach-{nonce}.copy"));
    let saved = parent.join(format!(".ccs-detach-{nonce}.link"));
    let journal = parent.join(format!(".ccs-detach-{nonce}.json"));
    if link.recovery_id.is_some() && regular_metadata(&link.path).is_ok_and(|meta| meta.is_dir()) {
        if tree_revision(&link.path)? != link.revision { return Err(AppError::Config("Detached copy was externally changed; recovery paused".into())); }
        remove_saved_link(&saved, &link.source)?;
        fs::remove_file(&journal).map_err(|e| AppError::io(&journal, e))?;
        return Ok(());
    }
    let original_in_place = link.path.canonicalize().ok().as_ref() == Some(&link.source);
    if !original_in_place && !(link.recovery_id.is_some() && !link.path.exists() && saved.canonicalize().ok().as_ref() == Some(&link.source)) {
        return Err(AppError::Config("Skill link changed since preview".into()));
    }
    if link.recovery_id.is_none() {
        let mut record = link.clone(); record.recovery_id = Some(nonce.to_string());
        crate::config::atomic_write_private_raw(&journal, &serde_json::to_vec(&record).map_err(|source| AppError::JsonSerialize { source })?)?;
    }
    if !stage.exists() { fs::create_dir(&stage).map_err(|e| AppError::io(&stage, e))?; }
    regular_metadata(&stage)?;
    for relative in tree_files(&link.source)? {
        let from = link.source.join(&relative);
        let to = stage.join(&relative);
        if regular_metadata(&from)?.is_dir() { fs::create_dir_all(&to).map_err(|e| AppError::io(&to, e))?; regular_metadata(&to)?; continue; }
        if let Some(parent) = to.parent() { fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?; }
        if to.exists() {
            regular_metadata(&to)?;
            if fs::read(&from).map_err(|e| AppError::io(&from, e))? != fs::read(&to).map_err(|e| AppError::io(&to, e))? {
                return Err(AppError::Config("Recovery copy differs from its recorded source; both were preserved".into()));
            }
        } else { fs::copy(&from, &to).map_err(|e| AppError::io(&to, e))?; }
    }
    if tree_revision(&stage)? != link.revision || tree_revision(&link.source)? != link.revision || (original_in_place && link.path.canonicalize().ok().as_ref() != Some(&link.source)) {
        return Err(AppError::Config(format!("Skill changed while copying; original link and diagnostic copy retained: {}", stage.display())));
    }
    if original_in_place { fs::rename(&link.path, &saved).map_err(|e| AppError::io(&link.path, e))?; }
    if let Err(error) = fs::rename(&stage, &link.path) {
        let _ = fs::rename(&saved, &link.path);
        return Err(AppError::io(&link.path, error));
    }
    remove_saved_link(&saved, &link.source)?;
    fs::remove_file(&journal).map_err(|e| AppError::io(&journal, e))?;
    Ok(())
}

fn remove_saved_link(saved: &Path, source: &Path) -> Result<(), AppError> {
    if fs::symlink_metadata(saved).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) { return Ok(()); }
    if fs::read_link(saved).is_err() || saved.canonicalize().ok().as_deref() != Some(source) {
        return Err(AppError::Config("Recovery refused to remove a changed Skill link".into()));
    }
    // Only remove the exact symlink, never its target or an independent folder.
    #[cfg(windows)]
    fs::remove_dir(saved).map_err(|e| AppError::io(saved, e))?;
    #[cfg(not(windows))]
    fs::remove_file(saved).map_err(|e| AppError::io(saved, e))?;
    Ok(())
}
