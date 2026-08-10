#[cfg(not(windows))]
use crate::config::atomic_write;
use crate::error::AppError;
use crate::provider::{CodexAgentRoleOverride, CodexAgentRoleRouting};
use crate::store::AppState;
use crate::AppType;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

pub const MANAGED_MARKER: &str = "# cc-switch-managed: codex-agent-role-v1";
pub const FRONTEND_ROLE_FILE_NAME: &str = "cc-switch-frontend.toml";
pub const BACKEND_ROLE_FILE_NAME: &str = "cc-switch-backend.toml";
pub const FRONTEND_ROLE_DISABLED_FILE_NAME: &str = "cc-switch-frontend.toml.disabled";
pub const BACKEND_ROLE_DISABLED_FILE_NAME: &str = "cc-switch-backend.toml.disabled";
pub const ROLE_ROUTE_HEADER: &str = "x-cc-switch-role-route";
pub const ROLE_OWNER_HEADER: &str = "x-cc-switch-role-owner";
pub const ROLE_TOKEN_HEADER: &str = "x-cc-switch-role-token";
pub const FRONTEND_ROLE_ROUTE_VALUE: &str = "frontend";
const FRONTEND_MODEL_PROVIDER: &str = "cc-switch-frontend-local";
const FRONTEND_DEVELOPER_INSTRUCTIONS: &str = "You are the CC Switch frontend specialist. Focus on user-facing interface work, including components, styling, interaction state, accessibility, and frontend tests. Preserve existing project conventions, keep changes scoped to the delegated task, and report verification evidence.";
const BACKEND_DEVELOPER_INSTRUCTIONS: &str = "You are the CC Switch backend specialist. Focus on services, APIs, persistence, proxy routing, concurrency, and backend tests. Preserve existing project conventions, keep changes scoped to the delegated task, and report verification evidence.";

static ROLE_PROJECTION_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static ROLE_COORDINATION_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));
static ROLE_ROUTE_SECRET: Lazy<[u8; 32]> = Lazy::new(|| {
    let mut secret = [0_u8; 32];
    secret[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    secret[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    secret
});

type HmacSha256 = Hmac<Sha256>;

pub fn create_codex_role_route_token(
    owner_provider_id: &str,
    route: &str,
    routing: &CodexAgentRoleRouting,
) -> String {
    let mac = codex_role_route_mac(owner_provider_id, route, routing);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn verify_codex_role_route_token(
    owner_provider_id: &str,
    route: &str,
    routing: &CodexAgentRoleRouting,
    encoded_token: &str,
) -> bool {
    let Ok(token) = URL_SAFE_NO_PAD.decode(encoded_token) else {
        return false;
    };
    codex_role_route_mac(owner_provider_id, route, routing)
        .verify_slice(&token)
        .is_ok()
}

fn codex_role_route_mac(
    owner_provider_id: &str,
    route: &str,
    routing: &CodexAgentRoleRouting,
) -> HmacSha256 {
    let mut mac =
        HmacSha256::new_from_slice(&*ROLE_ROUTE_SECRET).expect("HMAC-SHA256 accepts a 32-byte key");
    mac.update(b"cc-switch/codex-role-route/v1");
    update_mac_field(&mut mac, owner_provider_id.as_bytes());
    update_mac_field(&mut mac, route.as_bytes());
    update_mac_field(&mut mac, &codex_role_routing_version(routing));
    mac
}

fn update_mac_field(mac: &mut HmacSha256, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

fn codex_role_routing_version(routing: &CodexAgentRoleRouting) -> [u8; 32] {
    let serialized = serde_json::to_vec(routing)
        .expect("CodexAgentRoleRouting serialization contains no fallible values");
    Sha256::digest(serialized).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAgentRolePaths {
    pub frontend: PathBuf,
    pub backend: PathBuf,
    pub frontend_disabled: PathBuf,
    pub backend_disabled: PathBuf,
}

impl CodexAgentRolePaths {
    pub fn from_agents_dir(agents_dir: impl AsRef<Path>) -> Self {
        let agents_dir = agents_dir.as_ref();
        Self {
            frontend: agents_dir.join(FRONTEND_ROLE_FILE_NAME),
            backend: agents_dir.join(BACKEND_ROLE_FILE_NAME),
            frontend_disabled: agents_dir.join(FRONTEND_ROLE_DISABLED_FILE_NAME),
            backend_disabled: agents_dir.join(BACKEND_ROLE_DISABLED_FILE_NAME),
        }
    }

    pub fn default_codex_home() -> Self {
        Self::from_agents_dir(crate::codex_config::get_codex_config_dir().join("agents"))
    }

    fn all(&self) -> [&Path; 4] {
        [
            &self.frontend,
            &self.backend,
            &self.frontend_disabled,
            &self.backend_disabled,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAgentRoleReconcileResult {
    pub enabled: bool,
    pub discovery_changed: bool,
    pub frontend_path: PathBuf,
    pub backend_path: PathBuf,
}

#[derive(Debug)]
struct FileSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

pub fn current_codex_role_route_requires_proxy(state: &AppState) -> Result<bool, AppError> {
    Ok(current_codex_role_owner(state)?
        .as_ref()
        .is_some_and(|(_, routing)| routing.is_enabled()))
}

pub async fn reconcile_current_codex_agent_roles(
    state: &AppState,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let _proxy_transaction = state.proxy_service.lock_transaction().await;
    reconcile_current_codex_agent_roles_under_proxy_transaction(state).await
}

pub(crate) async fn reconcile_current_codex_agent_roles_under_proxy_transaction(
    state: &AppState,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let _guard = ROLE_COORDINATION_LOCK.lock().await;
    reconcile_current_codex_agent_roles_locked(state).await
}

async fn reconcile_current_codex_agent_roles_locked(
    state: &AppState,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let Some((owner_provider_id, routing)) = current_codex_role_owner(state)? else {
        return disable_codex_agent_roles_unlocked();
    };
    if !routing.is_enabled() {
        return disable_codex_agent_roles_unlocked();
    }

    let mut proxy_snapshot = state
        .proxy_service
        .snapshot_transaction_state()
        .await
        .map_err(|error| AppError::Message(format!("Failed to snapshot proxy state: {error}")))?;

    if let Err(error) = state
        .proxy_service
        .set_takeover_for_app_inner(AppType::Codex.as_str(), true)
        .await
    {
        let rollback_errors = restore_proxy_transaction(state, &mut proxy_snapshot).await;
        return Err(with_rollback_context(
            AppError::Message(format!("Failed to enable Codex proxy takeover: {error}")),
            rollback_errors,
        ));
    }

    let local_proxy_base_url = match state.proxy_service.codex_proxy_base_url().await {
        Ok(base_url) => base_url,
        Err(error) => {
            let rollback_errors = restore_proxy_transaction(state, &mut proxy_snapshot).await;
            return Err(with_rollback_context(
                AppError::Message(format!("Failed to resolve Codex proxy URL: {error}")),
                rollback_errors,
            ));
        }
    };

    match reconcile_codex_agent_roles(&owner_provider_id, &local_proxy_base_url, Some(&routing)) {
        Ok(result) => Ok(result),
        Err(error) => {
            let rollback_errors = restore_proxy_transaction(state, &mut proxy_snapshot).await;
            Err(with_rollback_context(error, rollback_errors))
        }
    }
}

async fn restore_proxy_transaction(
    state: &AppState,
    snapshot: &mut crate::services::proxy::ProxyTransactionSnapshot,
) -> Vec<String> {
    let mut errors = snapshot.capture_rollback_file_guards();
    errors.extend(
        state
            .proxy_service
            .restore_transaction_state(snapshot)
            .await,
    );
    errors
}

fn current_codex_role_owner(
    state: &AppState,
) -> Result<Option<(String, CodexAgentRoleRouting)>, AppError> {
    let Some(current_id) =
        crate::settings::get_effective_current_provider(&state.db, &AppType::Codex)?
    else {
        return Ok(None);
    };
    let providers = state.db.get_all_providers(AppType::Codex.as_str())?;
    let Some(provider) = providers.get(&current_id) else {
        return Ok(None);
    };
    let routing = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.codex_agent_role_routing.clone())
        .unwrap_or_default();
    Ok(Some((provider.id.clone(), routing)))
}

fn with_rollback_context(error: AppError, rollback_errors: Vec<String>) -> AppError {
    if rollback_errors.is_empty() {
        error
    } else {
        AppError::Message(format!(
            "{error}; proxy rollback encountered: {}",
            rollback_errors.join("; ")
        ))
    }
}

#[allow(dead_code)]
pub fn reconcile_codex_agent_roles(
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: Option<&CodexAgentRoleRouting>,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let paths = CodexAgentRolePaths::default_codex_home();
    reconcile_codex_agent_roles_at_paths(&paths, owner_provider_id, local_proxy_base_url, routing)
}

#[allow(dead_code)]
pub fn reconcile_codex_agent_roles_at(
    agents_dir: impl AsRef<Path>,
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: Option<&CodexAgentRoleRouting>,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let paths = CodexAgentRolePaths::from_agents_dir(agents_dir);
    reconcile_codex_agent_roles_at_paths(&paths, owner_provider_id, local_proxy_base_url, routing)
}

pub async fn disable_codex_agent_roles() -> Result<CodexAgentRoleReconcileResult, AppError> {
    let _guard = ROLE_COORDINATION_LOCK.lock().await;
    disable_codex_agent_roles_unlocked()
}

pub(crate) async fn disable_codex_agent_roles_under_proxy_transaction(
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let _guard = ROLE_COORDINATION_LOCK.lock().await;
    disable_codex_agent_roles_unlocked()
}

fn disable_codex_agent_roles_unlocked() -> Result<CodexAgentRoleReconcileResult, AppError> {
    let paths = CodexAgentRolePaths::default_codex_home();
    disable_codex_agent_roles_at_paths(&paths)
}

pub fn disable_codex_agent_roles_at(
    agents_dir: impl AsRef<Path>,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let paths = CodexAgentRolePaths::from_agents_dir(agents_dir);
    disable_codex_agent_roles_at_paths(&paths)
}

fn reconcile_codex_agent_roles_at_paths(
    paths: &CodexAgentRolePaths,
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: Option<&CodexAgentRoleRouting>,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let _guard = lock_role_projection()?;
    reconcile_codex_agent_roles_at_paths_locked(
        paths,
        owner_provider_id,
        local_proxy_base_url,
        routing,
    )
}

fn reconcile_codex_agent_roles_at_paths_locked(
    paths: &CodexAgentRolePaths,
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: Option<&CodexAgentRoleRouting>,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let Some(routing) = routing.filter(|routing| routing.is_enabled()) else {
        return disable_codex_agent_roles_at_paths_locked(paths);
    };

    let mut writer = |path: &Path, content: &str| atomic_write_role_file(path, content.as_bytes());
    reconcile_codex_agent_roles_at_with_writer_locked(
        paths,
        owner_provider_id,
        local_proxy_base_url,
        routing,
        &mut writer,
    )
}

#[cfg(test)]
fn reconcile_codex_agent_roles_at_with_writer<F>(
    paths: &CodexAgentRolePaths,
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: &CodexAgentRoleRouting,
    writer: &mut F,
) -> Result<CodexAgentRoleReconcileResult, AppError>
where
    F: FnMut(&Path, &str) -> Result<(), AppError>,
{
    let _guard = lock_role_projection()?;
    reconcile_codex_agent_roles_at_with_writer_locked(
        paths,
        owner_provider_id,
        local_proxy_base_url,
        routing,
        writer,
    )
}

fn reconcile_codex_agent_roles_at_with_writer_locked<F>(
    paths: &CodexAgentRolePaths,
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: &CodexAgentRoleRouting,
    writer: &mut F,
) -> Result<CodexAgentRoleReconcileResult, AppError>
where
    F: FnMut(&Path, &str) -> Result<(), AppError>,
{
    let owner_provider_id = require_non_empty(owner_provider_id, "owner Provider ID")?;
    let local_proxy_base_url = validate_local_proxy_base_url(require_non_empty(
        local_proxy_base_url,
        "local proxy base URL",
    )?)?;
    ensure_managed_or_absent(&paths.frontend)?;
    ensure_managed_or_absent(&paths.backend)?;

    let snapshots = snapshot_files(paths)?;
    let discovery_changed =
        !path_entry_exists(&paths.frontend)? || !path_entry_exists(&paths.backend)?;
    let frontend = render_frontend_role(owner_provider_id, local_proxy_base_url, routing);
    let backend = render_backend_role(routing.backend.as_ref());

    let result = (|| {
        writer(&paths.frontend, &frontend)?;
        writer(&paths.backend, &backend)?;
        remove_if_managed(&paths.frontend_disabled)?;
        remove_if_managed(&paths.backend_disabled)?;
        Ok(())
    })();

    finish_transaction(result, &snapshots)?;
    Ok(reconcile_result(paths, true, discovery_changed))
}

fn disable_codex_agent_roles_at_paths(
    paths: &CodexAgentRolePaths,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let _guard = lock_role_projection()?;
    disable_codex_agent_roles_at_paths_locked(paths)
}

fn disable_codex_agent_roles_at_paths_locked(
    paths: &CodexAgentRolePaths,
) -> Result<CodexAgentRoleReconcileResult, AppError> {
    let frontend = managed_content(&paths.frontend)?;
    let backend = managed_content(&paths.backend)?;
    if frontend.is_some() {
        ensure_managed_or_absent(&paths.frontend_disabled)?;
    }
    if backend.is_some() {
        ensure_managed_or_absent(&paths.backend_disabled)?;
    }

    let discovery_changed = frontend.is_some() || backend.is_some();
    if !discovery_changed {
        return Ok(reconcile_result(paths, false, false));
    }

    let snapshots = snapshot_files(paths)?;
    let result = (|| {
        if let Some(content) = frontend.as_deref() {
            atomic_write_role_file(&paths.frontend_disabled, content)?;
            remove_file_if_exists(&paths.frontend)?;
        }
        if let Some(content) = backend.as_deref() {
            atomic_write_role_file(&paths.backend_disabled, content)?;
            remove_file_if_exists(&paths.backend)?;
        }
        Ok(())
    })();

    finish_transaction(result, &snapshots)?;
    Ok(reconcile_result(paths, false, true))
}

fn render_frontend_role(
    owner_provider_id: &str,
    local_proxy_base_url: &str,
    routing: &CodexAgentRoleRouting,
) -> String {
    let role = routing.frontend.as_ref();
    let role_token =
        create_codex_role_route_token(owner_provider_id, FRONTEND_ROLE_ROUTE_VALUE, routing);
    let mut text = format!(
        "{MANAGED_MARKER}\nname = \"cc-switch-frontend\"\ndescription = \"CC Switch frontend specialist\"\ndeveloper_instructions = {}\nmodel_provider = \"{FRONTEND_MODEL_PROVIDER}\"\n",
        toml_string(FRONTEND_DEVELOPER_INSTRUCTIONS)
    );
    append_role_overrides(
        &mut text,
        role.and_then(|role| non_empty(role.model.as_deref())),
        role.and_then(|role| role.reasoning_effort),
    );
    text.push_str(&format!(
        "\n[model_providers.{FRONTEND_MODEL_PROVIDER}]\nname = \"CC Switch Frontend Route\"\nbase_url = {}\nwire_api = \"responses\"\nrequires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nstream_max_retries = 0\n\n[model_providers.{FRONTEND_MODEL_PROVIDER}.http_headers]\n{ROLE_ROUTE_HEADER} = \"{FRONTEND_ROLE_ROUTE_VALUE}\"\n{ROLE_OWNER_HEADER} = {}\n{ROLE_TOKEN_HEADER} = {}\n",
        toml_string(local_proxy_base_url),
        toml_string(owner_provider_id),
        toml_string(&role_token),
    ));
    text
}

fn render_backend_role(role: Option<&CodexAgentRoleOverride>) -> String {
    let mut text = format!(
        "{MANAGED_MARKER}\nname = \"cc-switch-backend\"\ndescription = \"CC Switch backend specialist\"\ndeveloper_instructions = {}\n",
        toml_string(BACKEND_DEVELOPER_INSTRUCTIONS)
    );
    append_role_overrides(
        &mut text,
        role.and_then(|role| non_empty(role.model.as_deref())),
        role.and_then(|role| role.reasoning_effort),
    );
    text
}

fn append_role_overrides(
    text: &mut String,
    model: Option<&str>,
    reasoning_effort: Option<crate::provider::CodexAgentReasoningEffort>,
) {
    if let Some(model) = model {
        text.push_str(&format!("model = {}\n", toml_string(model)));
    }
    if let Some(reasoning_effort) = reasoning_effort {
        text.push_str(&format!(
            "model_reasoning_effort = {}\n",
            toml_string(reasoning_effort.as_str())
        ));
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn require_non_empty<'a>(value: &'a str, label: &str) -> Result<&'a str, AppError> {
    non_empty(Some(value)).ok_or_else(|| AppError::InvalidInput(format!("{label} is required")))
}

fn validate_local_proxy_base_url(value: &str) -> Result<&str, AppError> {
    let url = url::Url::parse(value).map_err(|error| {
        AppError::InvalidInput(format!("invalid local proxy base URL: {error}"))
    })?;
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => std::net::IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => std::net::IpAddr::V6(ip),
        Some(url::Host::Domain(_)) => {
            return Err(AppError::InvalidInput(
                "local proxy base URL host must be a loopback IP".to_string(),
            ));
        }
        None => {
            return Err(AppError::InvalidInput(
                "local proxy base URL requires a host".to_string(),
            ));
        }
    };
    if url.scheme() != "http"
        || !ip.is_loopback()
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "/v1" | "/v1/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::InvalidInput(
            "local proxy base URL must be an HTTP loopback IP endpoint ending in /v1".to_string(),
        ));
    }
    Ok(value)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn reconcile_result(
    paths: &CodexAgentRolePaths,
    enabled: bool,
    discovery_changed: bool,
) -> CodexAgentRoleReconcileResult {
    CodexAgentRoleReconcileResult {
        enabled,
        discovery_changed,
        frontend_path: paths.frontend.clone(),
        backend_path: paths.backend.clone(),
    }
}

fn lock_role_projection() -> Result<MutexGuard<'static, ()>, AppError> {
    ROLE_PROJECTION_LOCK
        .lock()
        .map_err(|_| AppError::Message("Codex Agent Role projection lock is poisoned".to_string()))
}

#[cfg(not(windows))]
fn atomic_write_role_file(path: &Path, data: &[u8]) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

    ensure_managed_or_absent(path)?;
    atomic_write(path, data)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::io(path, error))
}

#[cfg(windows)]
fn atomic_write_role_file(path: &Path, data: &[u8]) -> Result<(), AppError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};

    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role path".to_string()))?;
    fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    ensure_managed_or_absent(path)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role file name".to_string()))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = parent.join(format!(
        ".{file_name}.cc-switch.{}.{}.tmp",
        std::process::id(),
        nonce
    ));

    let result = (|| -> Result<(), AppError> {
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| AppError::io(&temp_path, error))?;
        temp.write_all(data)
            .map_err(|error| AppError::io(&temp_path, error))?;
        temp.sync_all()
            .map_err(|error| AppError::io(&temp_path, error))?;
        drop(temp);

        ensure_managed_or_absent(path)?;

        if !path_entry_exists(path)? {
            return fs::rename(&temp_path, path).map_err(|error| AppError::IoContext {
                context: format!(
                    "Codex Agent Role atomic create failed: {} -> {}",
                    temp_path.display(),
                    path.display()
                ),
                source: error,
            });
        }

        let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let replacement: Vec<u16> = temp_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let replaced = unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                replacement.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if replaced == 0 {
            return Err(AppError::IoContext {
                context: format!(
                    "Codex Agent Role atomic replace failed: {} -> {}",
                    temp_path.display(),
                    path.display()
                ),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn ensure_managed_or_absent(path: &Path) -> Result<(), AppError> {
    let Some(content) = regular_file_content(path)? else {
        return Ok(());
    };
    if is_managed(&content) {
        return Ok(());
    }
    Err(AppError::localized(
        "codex_agent_role_file_conflict",
        format!(
            "Codex Agent Role 文件由用户管理，CC Switch 不会覆盖: {}",
            path.display()
        ),
        format!(
            "Codex Agent Role file is user-managed and will not be overwritten: {}",
            path.display()
        ),
    ))
}

fn managed_content(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    Ok(regular_file_content(path)?.filter(|content| is_managed(content)))
}

fn is_managed(content: &[u8]) -> bool {
    std::str::from_utf8(content)
        .ok()
        .and_then(|content| content.lines().next())
        == Some(MANAGED_MARKER)
}

fn remove_if_managed(path: &Path) -> Result<(), AppError> {
    if managed_content(path)?.is_some() {
        remove_file_if_exists(path)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), AppError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::io(path, error)),
    }
}

fn snapshot_files(paths: &CodexAgentRolePaths) -> Result<Vec<FileSnapshot>, AppError> {
    paths
        .all()
        .into_iter()
        .map(|path| {
            let content = regular_file_content(path)?;
            Ok(FileSnapshot {
                path: path.to_path_buf(),
                content,
            })
        })
        .collect()
}

fn path_entry_exists(path: &Path) -> Result<bool, AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::io(path, error)),
    }
}

fn regular_file_content(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io(path, error)),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(path_type_conflict(path));
    }
    fs::read(path)
        .map(Some)
        .map_err(|error| AppError::io(path, error))
}

fn path_type_conflict(path: &Path) -> AppError {
    AppError::localized(
        "codex_agent_role_path_conflict",
        format!(
            "Codex Agent Role 路径必须是普通文件，CC Switch 不会覆盖目录或符号链接: {}",
            path.display()
        ),
        format!(
            "Codex Agent Role path must be a regular file; directories and symbolic links will not be overwritten: {}",
            path.display()
        ),
    )
}

fn restore_snapshots(snapshots: &[FileSnapshot]) -> Result<(), AppError> {
    let mut errors = Vec::new();
    for snapshot in snapshots {
        if let Err(error) = restore_snapshot(snapshot) {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Message(errors.join("; ")))
    }
}

fn restore_snapshot(snapshot: &FileSnapshot) -> Result<(), AppError> {
    let current = regular_file_content(&snapshot.path)?;
    if current == snapshot.content {
        return Ok(());
    }

    match (snapshot.content.as_deref(), current.as_deref()) {
        (Some(original), Some(current)) if is_managed(original) && is_managed(current) => {
            atomic_write_role_file(&snapshot.path, original)
        }
        (Some(original), None) if is_managed(original) => {
            atomic_write_role_file(&snapshot.path, original)
        }
        (None, Some(current)) if is_managed(current) => remove_file_if_exists(&snapshot.path),
        (None, None) => Ok(()),
        _ => Err(AppError::localized(
            "codex_agent_role_file_conflict",
            format!(
                "Codex Agent Role 回滚检测到外部文件变更，保留当前文件: {}",
                snapshot.path.display()
            ),
            format!(
                "Codex Agent Role rollback detected an external file change and preserved it: {}",
                snapshot.path.display()
            ),
        )),
    }
}

fn finish_transaction(
    result: Result<(), AppError>,
    snapshots: &[FileSnapshot],
) -> Result<(), AppError> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => match restore_snapshots(snapshots) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(AppError::Message(format!(
                "{error}; Codex Agent Role rollback failed: {rollback_error}"
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        create_codex_role_route_token, disable_codex_agent_roles_at,
        reconcile_codex_agent_roles_at, reconcile_codex_agent_roles_at_with_writer,
        reconcile_current_codex_agent_roles, restore_snapshots, verify_codex_role_route_token,
        CodexAgentRolePaths, FileSnapshot, FRONTEND_ROLE_ROUTE_VALUE, MANAGED_MARKER,
        ROLE_COORDINATION_LOCK, ROLE_TOKEN_HEADER,
    };
    use crate::config::atomic_write;
    use crate::database::Database;
    use crate::error::AppError;
    use crate::provider::{
        CodexAgentReasoningEffort, CodexAgentRoleOverride, CodexAgentRoleRouting,
        CodexFrontendAgentRoleOverride, Provider, ProviderMeta,
    };
    use crate::store::AppState;
    use crate::AppType;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};
    use tokio::sync::{oneshot, Notify};

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();
            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            for (name, value) in [
                ("HOME", &self.original_home),
                ("USERPROFILE", &self.original_userprofile),
                ("CC_SWITCH_TEST_HOME", &self.original_test_home),
            ] {
                match value {
                    Some(value) => env::set_var(name, value),
                    None => env::remove_var(name),
                }
            }
        }
    }

    fn routing() -> CodexAgentRoleRouting {
        CodexAgentRoleRouting {
            enabled: Some(true),
            frontend: Some(CodexFrontendAgentRoleOverride {
                provider_id: Some("provider-b".to_string()),
                upstream_model: Some("frontend-upstream".to_string()),
                model: None,
                reasoning_effort: None,
            }),
            backend: Some(CodexAgentRoleOverride::default()),
        }
    }

    #[test]
    fn projection_omits_inherited_model_fields_and_configures_frontend_provider() {
        let temp = tempdir().expect("tempdir");
        let result = reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");

        assert!(result.enabled);
        assert!(result.discovery_changed);
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend = fs::read_to_string(&paths.frontend).expect("frontend role");
        let backend = fs::read_to_string(&paths.backend).expect("backend role");
        let _: toml::Value = toml::from_str(&frontend).expect("valid frontend TOML");
        let _: toml::Value = toml::from_str(&backend).expect("valid backend TOML");

        let frontend_doc: toml::Value =
            toml::from_str(&frontend).expect("valid frontend role TOML");
        let backend_doc: toml::Value = toml::from_str(&backend).expect("valid backend role TOML");
        let frontend_instructions = frontend_doc
            .get("developer_instructions")
            .and_then(toml::Value::as_str)
            .expect("frontend developer instructions");
        let backend_instructions = backend_doc
            .get("developer_instructions")
            .and_then(toml::Value::as_str)
            .expect("backend developer instructions");
        assert!(!frontend_instructions.trim().is_empty());
        assert!(frontend_instructions.contains("frontend"));
        assert!(!backend_instructions.trim().is_empty());
        assert!(backend_instructions.contains("backend"));

        assert!(frontend.starts_with(MANAGED_MARKER));
        assert!(frontend.contains("model_provider = \"cc-switch-frontend-local\""));
        assert!(frontend.contains("base_url = \"http://127.0.0.1:15777/v1\""));
        assert!(frontend.contains("wire_api = \"responses\""));
        assert!(frontend.contains("requires_openai_auth = false"));
        assert!(frontend.contains("supports_websockets = false"));
        assert!(frontend.contains("request_max_retries = 0"));
        assert!(frontend.contains("stream_max_retries = 0"));
        assert!(frontend.contains("x-cc-switch-role-route = \"frontend\""));
        assert!(frontend.contains("x-cc-switch-role-owner = \"provider-a\""));
        let token = frontend_doc
            .get("model_providers")
            .and_then(|providers| providers.get("cc-switch-frontend-local"))
            .and_then(|provider| provider.get("http_headers"))
            .and_then(|headers| headers.get(ROLE_TOKEN_HEADER))
            .and_then(toml::Value::as_str)
            .expect("frontend route token");
        assert!(verify_codex_role_route_token(
            "provider-a",
            FRONTEND_ROLE_ROUTE_VALUE,
            &routing(),
            token,
        ));
        assert!(!frontend.contains("\nmodel = "));
        assert!(!frontend.contains("\nmodel_reasoning_effort = "));

        assert!(backend.starts_with(MANAGED_MARKER));
        assert!(!backend.contains("model_provider"));
        assert!(!backend.contains("model_providers."));
        assert!(!backend.contains("\nmodel = "));
        assert!(!backend.contains("\nmodel_reasoning_effort = "));
    }

    #[test]
    fn codex_role_route_token_expires_when_routing_changes() {
        let initial = routing();
        let token =
            create_codex_role_route_token("provider-a", FRONTEND_ROLE_ROUTE_VALUE, &initial);
        assert!(verify_codex_role_route_token(
            "provider-a",
            FRONTEND_ROLE_ROUTE_VALUE,
            &initial,
            &token,
        ));

        let mut changed = initial;
        changed
            .frontend
            .as_mut()
            .expect("frontend routing")
            .upstream_model = Some("new-upstream".to_string());
        assert!(!verify_codex_role_route_token(
            "provider-a",
            FRONTEND_ROLE_ROUTE_VALUE,
            &changed,
            &token,
        ));
    }

    #[test]
    fn projection_rejects_non_loopback_or_non_http_proxy_urls_before_writing_tokens() {
        for base_url in [
            "https://127.0.0.1:15777/v1",
            "http://localhost:15777/v1",
            "http://192.0.2.10:15777/v1",
            "http://127.0.0.1:15777/other",
            "http://127.0.0.1:15777/v1?token=leak",
        ] {
            let temp = tempdir().expect("tempdir");
            let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
            let error = reconcile_codex_agent_roles_at(
                temp.path(),
                "provider-a",
                base_url,
                Some(&routing()),
            )
            .expect_err("unsafe role proxy URL must be rejected");

            assert!(error.to_string().contains("local proxy base URL"));
            assert!(!paths.frontend.exists());
            assert!(!paths.backend.exists());
        }
    }

    #[test]
    fn projection_accepts_an_ipv6_loopback_proxy_url() {
        let temp = tempdir().expect("tempdir");
        let result = reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://[::1]:15777/v1",
            Some(&routing()),
        )
        .expect("IPv6 loopback proxy URL must be accepted");

        assert!(result.enabled);
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        assert!(fs::read_to_string(paths.frontend)
            .expect("frontend role")
            .contains("base_url = \"http://[::1]:15777/v1\""));
    }

    #[test]
    fn projection_writes_explicit_model_and_reasoning_overrides() {
        let temp = tempdir().expect("tempdir");
        let mut config = routing();
        config.frontend.as_mut().unwrap().model = Some("gpt-5.6-sol".to_string());
        config.frontend.as_mut().unwrap().reasoning_effort = Some(CodexAgentReasoningEffort::Ultra);
        config.backend = Some(CodexAgentRoleOverride {
            model: Some("gpt-5.6-terra".to_string()),
            reasoning_effort: Some(CodexAgentReasoningEffort::Medium),
        });

        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&config),
        )
        .expect("project roles");

        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend = fs::read_to_string(paths.frontend).expect("frontend role");
        let backend = fs::read_to_string(paths.backend).expect("backend role");
        assert!(frontend.contains("\nmodel = \"gpt-5.6-sol\""));
        assert!(frontend.contains("\nmodel_reasoning_effort = \"ultra\""));
        assert!(backend.contains("\nmodel = \"gpt-5.6-terra\""));
        assert!(backend.contains("\nmodel_reasoning_effort = \"medium\""));
    }

    #[test]
    fn projection_replaces_existing_managed_role_files() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("initial projection");

        let mut config = routing();
        config.frontend.as_mut().unwrap().model = Some("gpt-5.6-sol".to_string());
        config.backend = Some(CodexAgentRoleOverride {
            model: Some("gpt-5.6-terra".to_string()),
            reasoning_effort: None,
        });
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15778/v1",
            Some(&config),
        )
        .expect("replace managed roles");

        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend = fs::read_to_string(paths.frontend).expect("frontend role");
        let backend = fs::read_to_string(paths.backend).expect("backend role");
        let _: toml::Value = toml::from_str(&frontend).expect("valid frontend TOML");
        let _: toml::Value = toml::from_str(&backend).expect("valid backend TOML");
        assert!(frontend.contains("base_url = \"http://127.0.0.1:15778/v1\""));
        assert!(frontend.contains("requires_openai_auth = false"));
        assert!(frontend.contains("model = \"gpt-5.6-sol\""));
        assert!(backend.contains("model = \"gpt-5.6-terra\""));
    }

    #[test]
    fn projection_refuses_to_overwrite_unmanaged_role_file() {
        let temp = tempdir().expect("tempdir");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        fs::write(&paths.frontend, "name = \"user-frontend\"\n").expect("user role");

        let error = reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect_err("unmanaged file must block projection");

        assert!(error
            .to_string()
            .contains(&paths.frontend.display().to_string()));
        assert_eq!(
            fs::read_to_string(&paths.frontend).expect("user role remains"),
            "name = \"user-frontend\"\n"
        );
        assert!(!paths.backend.exists());
    }

    #[test]
    fn projection_refuses_role_path_that_is_a_directory() {
        let temp = tempdir().expect("tempdir");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        fs::create_dir(&paths.frontend).expect("role path directory");

        let error = reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect_err("directory must block projection");

        assert!(error
            .to_string()
            .contains(&paths.frontend.display().to_string()));
        assert!(paths.frontend.is_dir());
        assert!(!paths.backend.exists());
    }

    #[test]
    fn projection_refuses_role_path_that_is_a_symbolic_link() {
        let temp = tempdir().expect("tempdir");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let target = temp.path().join("user-frontend.toml");
        fs::write(&target, "name = \"user-frontend\"\n").expect("symlink target");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &paths.frontend).expect("role symlink");
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &paths.frontend).is_err() {
            return;
        }

        let error = reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect_err("symlink must block projection");

        assert!(error
            .to_string()
            .contains(&paths.frontend.display().to_string()));
        assert_eq!(
            fs::read_to_string(&target).expect("target remains"),
            "name = \"user-frontend\"\n"
        );
        assert!(!paths.backend.exists());
    }

    #[test]
    fn pair_projection_restores_both_snapshots_when_second_write_fails() {
        let temp = tempdir().expect("tempdir");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let old_frontend = format!("{MANAGED_MARKER}\nname = \"old-frontend\"\n");
        let old_backend = format!("{MANAGED_MARKER}\nname = \"old-backend\"\n");
        fs::write(&paths.frontend, &old_frontend).expect("old frontend");
        fs::write(&paths.backend, &old_backend).expect("old backend");

        let mut writes = 0;
        let error = reconcile_codex_agent_roles_at_with_writer(
            &paths,
            "provider-a",
            "http://127.0.0.1:15777/v1",
            &routing(),
            &mut |path: &Path, content: &str| {
                writes += 1;
                if writes == 2 {
                    return Err(AppError::Message(
                        "injected second write failure".to_string(),
                    ));
                }
                atomic_write(path, content.as_bytes())
            },
        )
        .expect_err("second write should fail");

        assert!(error.to_string().contains("injected second write failure"));
        assert_eq!(fs::read_to_string(&paths.frontend).unwrap(), old_frontend);
        assert_eq!(fs::read_to_string(&paths.backend).unwrap(), old_backend);
    }

    #[test]
    fn rollback_continues_after_a_conflict_and_restores_remaining_role_files() {
        let temp = tempdir().expect("tempdir");
        let conflicted = temp.path().join("conflicted.toml");
        let restorable = temp.path().join("restorable.toml");
        let original_conflicted = format!("{MANAGED_MARKER}\nname = \"old-conflicted\"\n");
        let original_restorable = format!("{MANAGED_MARKER}\nname = \"old-restorable\"\n");
        let external_content = "name = \"external-owner\"\n";
        fs::write(&conflicted, external_content).expect("external replacement");
        fs::write(
            &restorable,
            format!("{MANAGED_MARKER}\nname = \"transaction-value\"\n"),
        )
        .expect("transaction value");

        let error = restore_snapshots(&[
            FileSnapshot {
                path: conflicted.clone(),
                content: Some(original_conflicted.into_bytes()),
            },
            FileSnapshot {
                path: restorable.clone(),
                content: Some(original_restorable.as_bytes().to_vec()),
            },
        ])
        .expect_err("external conflict must be reported");

        assert!(error.to_string().contains("preserved"));
        assert_eq!(fs::read_to_string(conflicted).unwrap(), external_content);
        assert_eq!(fs::read_to_string(restorable).unwrap(), original_restorable);
    }

    #[test]
    fn rollback_preserves_an_external_file_created_after_a_missing_snapshot() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("externally-created.toml");
        let external_content = "name = \"external-owner\"\n";
        fs::write(&path, external_content).expect("external file");

        restore_snapshots(&[FileSnapshot {
            path: path.clone(),
            content: None,
        }])
        .expect_err("external file must block rollback deletion");

        assert_eq!(fs::read_to_string(path).unwrap(), external_content);
    }

    #[tokio::test]
    #[serial]
    async fn role_reconcile_restores_full_proxy_state_when_codex_takeover_commit_fails() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload isolated settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let mut proxy_config = db.get_proxy_config().await.expect("proxy config");
        proxy_config.listen_port = 0;
        proxy_config.enable_logging = false;
        db.update_proxy_config(proxy_config.clone())
            .await
            .expect("seed proxy config");

        let state = AppState::new(db.clone());
        let mut owner = Provider::with_id(
            "provider-a".to_string(),
            "Provider A".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "provider-key" },
                "config": "model_provider = \"provider-a\"\n[model_providers.provider-a]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\n"
            }),
            None,
        );
        owner.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(routing()),
            ..Default::default()
        });
        db.save_provider(AppType::Codex.as_str(), &owner)
            .expect("save owner provider");
        db.set_current_provider(AppType::Codex.as_str(), &owner.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&owner.id))
            .expect("set local current provider");

        let original_auth = json!({ "OPENAI_API_KEY": "live-key" });
        let original_config = "model = \"gpt-5.4\"\n";
        crate::codex_config::write_codex_live_atomic(&original_auth, Some(original_config))
            .expect("seed Codex live files");
        let auth_path = crate::codex_config::get_codex_auth_path();
        let config_path = crate::codex_config::get_codex_config_path();
        let original_auth_bytes = fs::read(&auth_path).expect("snapshot auth bytes");
        let original_config_bytes = fs::read(&config_path).expect("snapshot config bytes");
        let original_global = db
            .get_global_proxy_config()
            .await
            .expect("snapshot global config");
        let original_app = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("snapshot app config");
        db.update_provider_health_with_threshold(
            &owner.id,
            AppType::Codex.as_str(),
            false,
            Some("seed failure".to_string()),
            1,
        )
        .await
        .expect("seed health");
        db.conn
            .lock()
            .expect("lock db")
            .execute_batch(
                "CREATE TRIGGER fail_codex_takeover_enable_commit
                 BEFORE UPDATE OF enabled ON proxy_config
                 WHEN OLD.app_type = 'codex' AND OLD.enabled = 0 AND NEW.enabled = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'injected Codex enabled commit failure');
                 END;",
            )
            .expect("install failure trigger");

        let error = reconcile_current_codex_agent_roles(&state)
            .await
            .expect_err("Codex takeover commit must fail");

        assert!(error
            .to_string()
            .contains("injected Codex enabled commit failure"));
        let final_status = state
            .proxy_service
            .get_status()
            .await
            .expect("proxy status");
        assert!(!final_status.running);
        assert!(final_status.active_targets.is_empty());
        assert_eq!(
            serde_json::to_value(db.get_proxy_config().await.unwrap()).unwrap(),
            serde_json::to_value(proxy_config).unwrap()
        );
        assert_eq!(
            serde_json::to_value(db.get_global_proxy_config().await.unwrap()).unwrap(),
            serde_json::to_value(original_global).unwrap()
        );
        assert_eq!(
            serde_json::to_value(
                db.get_proxy_config_for_app(AppType::Codex.as_str())
                    .await
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(original_app).unwrap()
        );
        assert!(db
            .get_live_backup(AppType::Codex.as_str())
            .await
            .expect("read backup")
            .is_none());
        assert_eq!(fs::read(auth_path).unwrap(), original_auth_bytes);
        assert_eq!(fs::read(config_path).unwrap(), original_config_bytes);
        let health = db
            .get_provider_health(&owner.id, AppType::Codex.as_str())
            .await
            .expect("read health");
        assert_eq!(health.consecutive_failures, 1);
        assert!(!health.is_healthy);
        let role_paths = CodexAgentRolePaths::default_codex_home();
        assert!(role_paths.all().into_iter().all(|path| !path.exists()));
    }

    #[test]
    fn projection_and_disable_do_not_interleave_role_file_transactions() {
        let temp = tempdir().expect("tempdir");
        let agents_dir = temp.path().to_path_buf();
        let projection_paths = CodexAgentRolePaths::from_agents_dir(&agents_dir);
        let (writer_entered_tx, writer_entered_rx) = mpsc::channel();
        let (release_writer_tx, release_writer_rx) = mpsc::channel();

        let projection = thread::spawn(move || {
            let mut writes = 0;
            reconcile_codex_agent_roles_at_with_writer(
                &projection_paths,
                "provider-a",
                "http://127.0.0.1:15777/v1",
                &routing(),
                &mut |path: &Path, content: &str| {
                    atomic_write(path, content.as_bytes())?;
                    writes += 1;
                    if writes == 1 {
                        writer_entered_tx.send(()).expect("signal first write");
                        release_writer_rx.recv().expect("release first write");
                    }
                    Ok(())
                },
            )
        });

        writer_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("projection reached first write");

        let disable_dir = agents_dir.clone();
        let (disable_done_tx, disable_done_rx) = mpsc::channel();
        let disable = thread::spawn(move || {
            let result = disable_codex_agent_roles_at(disable_dir);
            disable_done_tx.send(()).expect("signal disable completion");
            result
        });

        assert!(matches!(
            disable_done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_writer_tx.send(()).expect("release projection");

        projection
            .join()
            .expect("projection thread")
            .expect("projection result");
        disable
            .join()
            .expect("disable thread")
            .expect("disable result");
        disable_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("disable completed after projection");

        let paths = CodexAgentRolePaths::from_agents_dir(agents_dir);
        assert!(!paths.frontend.exists());
        assert!(!paths.backend.exists());
        assert!(paths.frontend_disabled.exists());
        assert!(paths.backend_disabled.exists());
    }

    #[test]
    fn disabling_moves_managed_roles_out_of_toml_discovery() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend = fs::read(&paths.frontend).expect("frontend snapshot");
        let backend = fs::read(&paths.backend).expect("backend snapshot");

        let result = disable_codex_agent_roles_at(temp.path()).expect("disable roles");

        assert!(!result.enabled);
        assert!(result.discovery_changed);
        assert!(!paths.frontend.exists());
        assert!(!paths.backend.exists());
        assert_eq!(fs::read(&paths.frontend_disabled).unwrap(), frontend);
        assert_eq!(fs::read(&paths.backend_disabled).unwrap(), backend);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coordination_lock_prevents_an_older_owner_from_winning_last() {
        let temp = tempdir().expect("tempdir");
        let agents_dir = temp.path().to_path_buf();
        let (first_locked_tx, first_locked_rx) = oneshot::channel();
        let release_first = Arc::new(Notify::new());

        let first_dir = agents_dir.clone();
        let first_release = Arc::clone(&release_first);
        let first = tokio::spawn(async move {
            let _guard = ROLE_COORDINATION_LOCK.lock().await;
            first_locked_tx.send(()).expect("signal first coordinator");
            first_release.notified().await;
            reconcile_codex_agent_roles_at(
                first_dir,
                "provider-a",
                "http://127.0.0.1:15777/v1",
                Some(&routing()),
            )
        });

        first_locked_rx
            .await
            .expect("first coordinator acquired lock");

        let second_dir = agents_dir.clone();
        let (second_started_tx, second_started_rx) = oneshot::channel();
        let second = tokio::spawn(async move {
            second_started_tx
                .send(())
                .expect("signal second coordinator");
            let _guard = ROLE_COORDINATION_LOCK.lock().await;
            reconcile_codex_agent_roles_at(
                second_dir,
                "provider-c",
                "http://127.0.0.1:15778/v1",
                Some(&routing()),
            )
        });

        second_started_rx.await.expect("second coordinator started");
        tokio::task::yield_now().await;
        release_first.notify_one();
        first.await.expect("first task").expect("first projection");
        second
            .await
            .expect("second task")
            .expect("second projection");

        let paths = CodexAgentRolePaths::from_agents_dir(agents_dir);
        let frontend = fs::read_to_string(paths.frontend).expect("final frontend role");
        assert!(frontend.contains("x-cc-switch-role-owner = \"provider-c\""));
        assert!(frontend.contains("base_url = \"http://127.0.0.1:15778/v1\""));
    }
}
