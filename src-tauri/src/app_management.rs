//! Device-local native-write authority, independent of visibility and cloud data.
mod paths;
mod transition;
mod skills;
#[cfg(test)]
pub(crate) mod tests;
pub use transition::{apply_change, preview_change, ManagementPlan};
pub(crate) use transition::ReleaseEdit;
pub(crate) use paths::{app_files, app_roots, owners_of_path, paths_overlap};

use crate::{app_config::AppType, error::AppError};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementPhase { Managed, Unmanaged, PendingRelease, PendingReview }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedAppState {
    pub app_id: String,
    pub enabled: bool,
    pub phase: ManagementPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppManagementState { pub revision: String, pub apps: Vec<ManagedAppState> }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalManagement {
    #[serde(default)] pub revision: u64,
    #[serde(default)] pub managed_apps: BTreeMap<String, bool>,
    #[serde(default)] pub phases: BTreeMap<String, ManagementPhase>,
    #[serde(default)] pub messages: BTreeMap<String, String>,
    /// Shared targets that still affect an application's incomplete release.
    #[serde(default)] pub blocked_paths: BTreeMap<String, Vec<PathBuf>>,
}

impl Default for LocalManagement {
    fn default() -> Self {
        Self { revision: 0,
            managed_apps: AppType::all().map(|app| (app.as_str().into(), true)).collect(),
            phases: AppType::all().map(|app| (app.as_str().into(), ManagementPhase::Managed)).collect(),
            messages: BTreeMap::new(), blocked_paths: BTreeMap::new() }
    }
}

impl LocalManagement {
    pub fn app(&self, app: &AppType) -> ManagedAppState {
        let id = app.as_str();
        let enabled = self.managed_apps.get(id).copied().unwrap_or(true);
        ManagedAppState {
            app_id: id.to_string(), enabled,
            phase: self.phases.get(id).copied().unwrap_or(if enabled { ManagementPhase::Managed } else { ManagementPhase::Unmanaged }),
            message: self.messages.get(id).cloned(),
        }
    }
    pub fn state(&self) -> AppManagementState {
        AppManagementState { revision: self.revision.to_string(), apps: AppType::all().map(|app| self.app(&app)).collect() }
    }
}

/// Keep device authority separate even when the user switches the portable/
/// synced ccs database directory. A different DB must not re-enable native IO.
pub(crate) fn device_state_dir() -> PathBuf { crate::config::get_home_dir().join(".cc-switch/local-state") }
fn policy_path() -> PathBuf { device_state_dir().join("app-management.json") }

pub(crate) fn read_policy() -> Result<LocalManagement, AppError> {
    let path = policy_path();
    match fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() > 1024 * 1024 { return Err(AppError::Config("Application management state is too large; native writes are blocked".into())); }
            let state: LocalManagement = serde_json::from_slice(&bytes).map_err(|_| AppError::Config("Application management state is invalid; native writes are blocked. Restore the local app-management.json backup.".into()))?;
            if AppType::all().any(|app| {
                match (state.managed_apps.get(app.as_str()), state.phases.get(app.as_str())) {
                    (Some(enabled), Some(phase)) => *enabled != matches!(phase, ManagementPhase::Managed | ManagementPhase::PendingReview),
                    _ => true,
                }
            }) { return Err(AppError::Config("Application management state is incomplete or inconsistent; native writes are blocked".into())); }
            Ok(state)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LocalManagement::default()),
        Err(error) => Err(AppError::io(path, error)),
    }
}

/// Caller holds native_mutation_guard. Ordinary settings saves cannot call this.
pub(crate) fn save_policy(policy: &LocalManagement) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(policy).map_err(|source| AppError::JsonSerialize { source })?;
    crate::config::atomic_write_private_raw(&policy_path(), &bytes)
}
pub fn get_state() -> Result<AppManagementState, AppError> { read_policy().map(|policy| policy.state()) }
pub fn is_managed(app: &AppType) -> bool {
    read_policy().map(|policy| policy.app(app).phase == ManagementPhase::Managed).unwrap_or(false)
}
pub fn require_managed(app: &AppType) -> Result<(), AppError> {
    if is_reviewed_write(app) { return Ok(()); }
    if read_policy()?.app(app).phase != ManagementPhase::Managed {
        return Err(AppError::localized("app_management_disabled",
            format!("{} 未启用本地管理，或仍有待确认的配置；仅可保存到 ccs 数据库", app.as_str()),
            format!("{} is unmanaged or awaiting configuration review; only database edits are allowed", app.as_str())));
    }
    Ok(())
}

thread_local! {
    static MUTATION_DEPTH: Cell<usize> = const { Cell::new(0) };
    static REVIEWED_APP: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Reentrant short filesystem-commit lock. Never hold across .await or acquire
/// an upstream application/skill lock while holding this guard.
pub(crate) struct NativeMutationGuard { _lock: Option<MutexGuard<'static, ()>> }
impl Drop for NativeMutationGuard {
    fn drop(&mut self) { MUTATION_DEPTH.with(|depth| depth.set(depth.get() - 1)); }
}
pub(crate) fn native_mutation_guard() -> Result<NativeMutationGuard, AppError> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let nested = MUTATION_DEPTH.with(|depth| depth.get() > 0);
    let lock = if nested { None } else { Some(LOCK.get_or_init(|| Mutex::new(())).lock()?) };
    MUTATION_DEPTH.with(|depth| depth.set(depth.get() + 1));
    Ok(NativeMutationGuard { _lock: lock })
}

/// Scoped to a previously previewed synchronous release/review; never an HTTP
/// header, frontend flag, or imported provider option.
pub(crate) fn with_reviewed_write<T>(app: &AppType, operation: impl FnOnce() -> Result<T, AppError>) -> Result<T, AppError> {
    struct Reset(Option<String>);
    impl Drop for Reset {
        fn drop(&mut self) { REVIEWED_APP.with(|value| *value.borrow_mut() = self.0.take()); }
    }
    let previous = REVIEWED_APP.with(|value| value.replace(Some(app.as_str().to_string())));
    let _reset = Reset(previous);
    operation()
}
pub(crate) fn is_reviewed_write(app: &AppType) -> bool {
    REVIEWED_APP.with(|value| value.borrow().as_deref() == Some(app.as_str()))
}

pub(crate) fn permit_path(path: &Path) -> Result<NativeMutationGuard, AppError> {
    let guard = native_mutation_guard()?;
    // Avoid recursively reading settings while their write lock is held.
    if path == crate::config::get_home_dir().join(".cc-switch/settings.json") { return Ok(guard); }
    for app in owners_of_path(path)? { require_managed(&app)?; }
    let policy = read_policy()?;
    for (app, targets) in &policy.blocked_paths {
        if REVIEWED_APP.with(|value| value.borrow().as_deref() == Some(app)) { continue; }
        if targets.iter().any(|target| paths_overlap(target, path)) {
            return Err(AppError::Config(format!("Shared path is protected by {app}'s incomplete release: {}", path.display())));
        }
    }
    if crate::settings::get_skill_storage_location() == crate::services::skill::SkillStorageLocation::Unified
        && paths_overlap(&crate::config::get_home_dir().join(".agents/skills"), path)
        && AppType::all().any(|app| !policy.app(&app).enabled)
    {
        return Err(AppError::Config("Unified Skills are shared with an unmanaged application; separate storage before changing them".into()));
    }
    Ok(guard)
}

#[derive(Clone)]
pub(crate) struct WriteTicket { app: AppType, revision: u64 }
impl WriteTicket {
    pub fn capture(app: &AppType) -> Result<Self, AppError> {
        require_managed(app)?;
        Ok(Self { app: app.clone(), revision: read_policy()?.revision })
    }
    pub fn check(&self) -> Result<(), AppError> {
        require_managed(&self.app)?;
        if read_policy()?.revision != self.revision {
            return Err(AppError::Config("Application management changed while this operation was queued; please retry".into()));
        }
        Ok(())
    }
}
