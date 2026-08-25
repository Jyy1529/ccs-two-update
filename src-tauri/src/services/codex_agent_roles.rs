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
const MANAGED_DISABLED_ISOLATION_SEPARATOR: &str = ".cc-switch-disabled-";
const MANAGED_DISABLE_QUARANTINE_SEPARATOR: &str = ".cc-switch-disable-";
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleMutationFault {
    DisabledCopyAfterWrite,
    MoveAfterRenameBeforeRead,
    MoveAfterRemove,
}

#[cfg(test)]
thread_local! {
    static ROLE_MUTATION_FAULT: std::cell::Cell<Option<RoleMutationFault>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
struct RoleMutationFaultGuard;

#[cfg(test)]
impl Drop for RoleMutationFaultGuard {
    fn drop(&mut self) {
        ROLE_MUTATION_FAULT.with(|slot| slot.set(None));
    }
}

#[cfg(test)]
fn inject_role_mutation_fault(fault: RoleMutationFault) -> RoleMutationFaultGuard {
    ROLE_MUTATION_FAULT.with(|slot| {
        assert!(
            slot.replace(Some(fault)).is_none(),
            "mutation fault already set"
        );
    });
    RoleMutationFaultGuard
}

#[cfg(test)]
fn take_role_mutation_fault(fault: RoleMutationFault) -> bool {
    ROLE_MUTATION_FAULT.with(|slot| {
        if slot.get() == Some(fault) {
            slot.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
fn injected_role_mutation_error(stage: &str) -> AppError {
    AppError::Message(format!(
        "injected Codex Agent Role mutation failure: {stage}"
    ))
}

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
    post_image: TransactionPostImage,
}

#[derive(Debug)]
enum TransactionPostImage {
    Unchanged,
    Missing,
    Content(Vec<u8>),
}

impl TransactionPostImage {
    fn content(&self) -> Option<Option<&[u8]>> {
        match self {
            Self::Unchanged => None,
            Self::Missing => Some(None),
            Self::Content(content) => Some(Some(content.as_slice())),
        }
    }
}

pub fn current_codex_role_route_requires_proxy(state: &AppState) -> Result<bool, AppError> {
    // 功能范围关闭时角色路由整体停用，不再要求代理接管
    if !crate::settings::agent_role_routing_allowed() {
        return Ok(false);
    }
    Ok(current_codex_role_owner(state)?
        .as_ref()
        .is_some_and(|(_, routing)| routing.is_enabled()))
}

/// 检查本地代理监听地址是否为回环地址
///
/// Codex 角色路由要求本地代理监听回环地址以避免安全风险
fn is_loopback_address(addr: &str) -> bool {
    let addr = addr.trim();
    if addr.eq_ignore_ascii_case("localhost") {
        return true;
    }

    let ip_literal = addr
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(addr);
    ip_literal
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// 验证 Codex 角色路由的前置条件
///
/// 检查：
/// 1. 角色路由已启用
/// 2. 本地代理监听回环地址
pub async fn validate_codex_role_routing_requirements(
    state: &AppState,
    provider_id: &str,
) -> Result<(), AppError> {
    let provider = state
        .db
        .get_provider_by_id(provider_id, "codex")?
        .ok_or_else(|| AppError::Message(format!("Provider not found: {}", provider_id)))?;

    let _routing = provider
        .meta
        .as_ref()
        .and_then(|m| m.codex_agent_role_routing.as_ref())
        .filter(|r| r.is_enabled())
        .ok_or_else(|| AppError::Message("Codex role routing is not enabled".into()))?;

    // 检查本地代理监听地址
    let proxy_config = state.db.get_proxy_config().await?;
    let listen_address = format!(
        "{}:{}",
        proxy_config.listen_address, proxy_config.listen_port
    );

    if !is_loopback_address(&proxy_config.listen_address) {
        log::error!(
            "[CodexRoleRoute] Validation failed: listen address {} is not loopback",
            listen_address
        );
        return Err(AppError::localized(
            "codex_role_routing_requires_loopback",
            format!(
                "Codex 角色路由要求本地代理监听回环地址（127.0.0.1 或 ::1），当前: {}",
                listen_address
            ),
            format!(
                "Codex role routing requires local proxy to listen on loopback address (127.0.0.1 or ::1), current: {}",
                listen_address
            ),
        ));
    }

    log::info!(
        "[CodexRoleRoute] Validation passed for provider {}: listen address {}",
        provider_id,
        listen_address
    );

    Ok(())
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
    // 功能范围关闭时视为禁用：清除已生成的角色配置
    if !crate::settings::agent_role_routing_allowed() {
        return disable_codex_agent_roles_unlocked();
    }

    let Some((owner_provider_id, routing)) = current_codex_role_owner(state)? else {
        return disable_codex_agent_roles_unlocked();
    };
    if !routing.is_enabled() {
        return disable_codex_agent_roles_unlocked();
    }

    // 验证前置条件：本地代理监听回环地址
    if let Err(error) = validate_codex_role_routing_requirements(state, &owner_provider_id).await {
        log::warn!(
            "[CodexRoleRoute] Validation failed, disabling roles: {}",
            error
        );
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

#[allow(dead_code)]
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

    let mut snapshots = snapshot_files(paths)?;
    let discovery_changed =
        !path_entry_exists(&paths.frontend)? || !path_entry_exists(&paths.backend)?;
    let frontend = render_frontend_role(owner_provider_id, local_proxy_base_url, routing);
    let backend = render_backend_role(routing.backend.as_ref());

    let result = (|| {
        let write_result = writer(&paths.frontend, &frontend);
        let observation_result = observe_transaction_path(
            &mut snapshots,
            &paths.frontend,
            &[Some(frontend.as_bytes())],
        );
        combine_transaction_results(write_result, observation_result)?;

        let write_result = writer(&paths.backend, &backend);
        let observation_result =
            observe_transaction_path(&mut snapshots, &paths.backend, &[Some(backend.as_bytes())]);
        combine_transaction_results(write_result, observation_result)?;
        if remove_if_managed(&paths.frontend_disabled)? {
            record_post_image(&mut snapshots, &paths.frontend_disabled, None)?;
        }
        if remove_if_managed(&paths.backend_disabled)? {
            record_post_image(&mut snapshots, &paths.backend_disabled, None)?;
        }
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
    let mut before_copy = |_: &Path| Ok(());
    let mut before_move = |_: &Path| Ok(());
    disable_codex_agent_roles_at_paths_with_hooks_locked(paths, &mut before_copy, &mut before_move)
}

#[cfg(test)]
fn disable_codex_agent_roles_at_with_before_move<F>(
    agents_dir: impl AsRef<Path>,
    mut before_move: F,
) -> Result<CodexAgentRoleReconcileResult, AppError>
where
    F: FnMut(&Path) -> Result<(), AppError>,
{
    let paths = CodexAgentRolePaths::from_agents_dir(agents_dir);
    let _guard = lock_role_projection()?;
    let mut before_copy = |_: &Path| Ok(());
    disable_codex_agent_roles_at_paths_with_hooks_locked(&paths, &mut before_copy, &mut before_move)
}

#[cfg(test)]
fn disable_codex_agent_roles_at_with_before_copy<F>(
    agents_dir: impl AsRef<Path>,
    mut before_copy: F,
) -> Result<CodexAgentRoleReconcileResult, AppError>
where
    F: FnMut(&Path) -> Result<(), AppError>,
{
    let paths = CodexAgentRolePaths::from_agents_dir(agents_dir);
    let _guard = lock_role_projection()?;
    let mut before_move = |_: &Path| Ok(());
    disable_codex_agent_roles_at_paths_with_hooks_locked(&paths, &mut before_copy, &mut before_move)
}

fn disable_codex_agent_roles_at_paths_with_hooks_locked<F, G>(
    paths: &CodexAgentRolePaths,
    before_copy: &mut F,
    before_move: &mut G,
) -> Result<CodexAgentRoleReconcileResult, AppError>
where
    F: FnMut(&Path) -> Result<(), AppError>,
    G: FnMut(&Path) -> Result<(), AppError>,
{
    let frontend = managed_content(&paths.frontend)?;
    let backend = managed_content(&paths.backend)?;
    let frontend_disabled = frontend
        .as_deref()
        .map(|content| {
            disabled_role_destination(&paths.frontend, &paths.frontend_disabled, content)
        })
        .transpose()?;
    let backend_disabled = backend
        .as_deref()
        .map(|content| disabled_role_destination(&paths.backend, &paths.backend_disabled, content))
        .transpose()?;
    let frontend_quarantine = frontend
        .as_ref()
        .map(|_| allocate_role_quarantine(&paths.frontend))
        .transpose()?;
    let backend_quarantine = backend
        .as_ref()
        .map(|_| allocate_role_quarantine(&paths.backend))
        .transpose()?;

    let discovery_changed = frontend.is_some() || backend.is_some();
    if !discovery_changed {
        return Ok(reconcile_result(paths, false, false));
    }

    let mut transaction_paths = vec![paths.frontend.as_path(), paths.backend.as_path()];
    if let Some(path) = frontend_disabled.as_deref() {
        transaction_paths.push(path);
    }
    if let Some(path) = backend_disabled.as_deref() {
        transaction_paths.push(path);
    }
    if let Some(path) = frontend_quarantine.as_deref() {
        transaction_paths.push(path);
    }
    if let Some(path) = backend_quarantine.as_deref() {
        transaction_paths.push(path);
    }
    let mut snapshots = snapshot_paths(transaction_paths)?;
    let result = (|| {
        if let (Some(content), Some(destination), Some(quarantine)) = (
            frontend.as_deref(),
            frontend_disabled.as_deref(),
            frontend_quarantine.as_deref(),
        ) {
            before_copy(destination)?;
            let copy_result = ensure_disabled_role_copy(destination, content);
            let observation_result =
                observe_transaction_path(&mut snapshots, destination, &[Some(content)]);
            combine_transaction_results(copy_result, observation_result)?;

            before_move(&paths.frontend)?;
            let move_result =
                move_captured_role_out_of_discovery(&paths.frontend, quarantine, content);
            let active_observation =
                observe_transaction_path(&mut snapshots, &paths.frontend, &[None]);
            let quarantine_observation =
                observe_transaction_path(&mut snapshots, quarantine, &[None, Some(content)]);
            let observation_result =
                combine_transaction_results(active_observation, quarantine_observation);
            combine_transaction_results(move_result, observation_result)?;
        }
        if let (Some(content), Some(destination), Some(quarantine)) = (
            backend.as_deref(),
            backend_disabled.as_deref(),
            backend_quarantine.as_deref(),
        ) {
            before_copy(destination)?;
            let copy_result = ensure_disabled_role_copy(destination, content);
            let observation_result =
                observe_transaction_path(&mut snapshots, destination, &[Some(content)]);
            combine_transaction_results(copy_result, observation_result)?;

            before_move(&paths.backend)?;
            let move_result =
                move_captured_role_out_of_discovery(&paths.backend, quarantine, content);
            let active_observation =
                observe_transaction_path(&mut snapshots, &paths.backend, &[None]);
            let quarantine_observation =
                observe_transaction_path(&mut snapshots, quarantine, &[None, Some(content)]);
            let observation_result =
                combine_transaction_results(active_observation, quarantine_observation);
            combine_transaction_results(move_result, observation_result)?;
        }
        Ok(())
    })();

    finish_transaction(result, &snapshots)?;
    Ok(reconcile_result(paths, false, true))
}

fn allocate_role_quarantine(active: &Path) -> Result<PathBuf, AppError> {
    let parent = active
        .parent()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role path".to_string()))?;
    let file_name = active
        .file_name()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role file name".to_string()))?
        .to_string_lossy();
    let prefix = format!("{file_name}{MANAGED_DISABLE_QUARANTINE_SEPARATOR}");

    for _ in 0..32 {
        let candidate = parent.join(format!("{prefix}{}", Uuid::new_v4()));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }

    Err(AppError::Message(format!(
        "Failed to allocate a Codex Agent Role quarantine path for {}",
        active.display()
    )))
}

fn move_captured_role_out_of_discovery(
    active: &Path,
    quarantine: &Path,
    expected: &[u8],
) -> Result<(), AppError> {
    match fs::rename(active, quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::IoContext {
                context: format!(
                    "Codex Agent Role atomic quarantine move failed: {} -> {}",
                    active.display(),
                    quarantine.display()
                ),
                source: error,
            });
        }
    }

    #[cfg(test)]
    if take_role_mutation_fault(RoleMutationFault::MoveAfterRenameBeforeRead) {
        return Err(injected_role_mutation_error("after rename before read"));
    }

    let moved = regular_file_content(quarantine)?.ok_or_else(|| {
        AppError::Message(format!(
            "Codex Agent Role quarantine disappeared after move: {}",
            quarantine.display()
        ))
    })?;
    if moved == expected && is_managed(&moved) {
        remove_file_if_exists(quarantine)?;
        #[cfg(test)]
        if take_role_mutation_fault(RoleMutationFault::MoveAfterRemove) {
            return Err(injected_role_mutation_error("after quarantine remove"));
        }
        return Ok(());
    }

    let recovery_error = preserve_unexpected_quarantine(active, quarantine).err();
    let recovery_suffix = recovery_error
        .map(|error| format!("; restoring the user file path failed: {error}"))
        .unwrap_or_default();
    Err(AppError::localized(
        "codex_agent_role_file_conflict",
        format!(
            "Codex Agent Role 在禁用期间被外部修改；用户文件已保留在 {}{}",
            quarantine.display(),
            recovery_suffix
        ),
        format!(
            "Codex Agent Role changed externally while it was being disabled; the user file was preserved at {}{}",
            quarantine.display(),
            recovery_suffix
        ),
    ))
}

fn preserve_unexpected_quarantine(active: &Path, quarantine: &Path) -> Result<(), AppError> {
    if path_entry_exists(active)? {
        return Ok(());
    }

    match fs::hard_link(quarantine, active) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(AppError::IoContext {
            context: format!(
                "Codex Agent Role user-file recovery failed: {} -> {}",
                quarantine.display(),
                active.display()
            ),
            source: error,
        }),
    }
}

fn disabled_role_destination(
    active: &Path,
    fixed: &Path,
    expected: &[u8],
) -> Result<PathBuf, AppError> {
    match regular_file_content(fixed)? {
        None => return Ok(fixed.to_path_buf()),
        Some(content) if content == expected && is_managed(&content) => {
            return Ok(fixed.to_path_buf());
        }
        Some(_) => {}
    }

    let parent = active
        .parent()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role path".to_string()))?;
    let file_name = active
        .file_name()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role file name".to_string()))?
        .to_string_lossy();
    let isolation_prefix = format!("{file_name}{MANAGED_DISABLED_ISOLATION_SEPARATOR}");

    let mut existing_managed = Vec::new();
    for entry in fs::read_dir(parent).map_err(|error| AppError::io(parent, error))? {
        let entry = entry.map_err(|error| AppError::io(parent, error))?;
        let path = entry.path();
        let matches_prefix = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&isolation_prefix));
        if matches_prefix {
            if let Some(content) = regular_file_content(&path)? {
                if content == expected && is_managed(&content) {
                    existing_managed.push(path);
                }
            }
        }
    }
    existing_managed.sort();
    if let Some(path) = existing_managed.into_iter().next() {
        return Ok(path);
    }

    for _ in 0..32 {
        let candidate = parent.join(format!("{isolation_prefix}{}", Uuid::new_v4()));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }

    Err(AppError::Message(format!(
        "Failed to allocate an isolated Codex Agent Role path for {}",
        active.display()
    )))
}

fn ensure_disabled_role_copy(path: &Path, expected: &[u8]) -> Result<(), AppError> {
    match regular_file_content(path)? {
        Some(content) if content == expected && is_managed(&content) => return Ok(()),
        Some(_) => return Err(disabled_role_copy_conflict(path)),
        None => {}
    }

    match create_new_role_file(path, expected) {
        Ok(()) => {
            #[cfg(test)]
            if take_role_mutation_fault(RoleMutationFault::DisabledCopyAfterWrite) {
                return Err(injected_role_mutation_error("after disabled copy write"));
            }
            Ok(())
        }
        Err(AppError::IoContext { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            match regular_file_content(path)? {
                Some(content) if content == expected && is_managed(&content) => Ok(()),
                _ => Err(disabled_role_copy_conflict(path)),
            }
        }
        Err(error) => Err(error),
    }
}

fn disabled_role_copy_conflict(path: &Path) -> AppError {
    AppError::localized(
        "codex_agent_role_file_conflict",
        format!(
            "Codex Agent Role 禁用目标在操作期间发生变化，CC Switch 不会覆盖: {}",
            path.display()
        ),
        format!(
            "Codex Agent Role disabled destination changed during the operation and will not be overwritten: {}",
            path.display()
        ),
    )
}

fn create_new_role_file(path: &Path, data: &[u8]) -> Result<(), AppError> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role path".to_string()))?;
    fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| AppError::io(path, error))?;
    file.write_all(data)
        .map_err(|error| AppError::io(path, error))?;
    file.sync_all().map_err(|error| AppError::io(path, error))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| AppError::io(path, error))?;
    }

    Ok(())
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

    log::info!(
        "[CodexRoleRoute] Writing role config to: {}",
        path.display()
    );

    // 确保父目录存在
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            log::info!(
                "[CodexRoleRoute] Creating parent directory: {}",
                parent.display()
            );
            fs::create_dir_all(parent).map_err(|error| {
                log::error!(
                    "[CodexRoleRoute] Failed to create parent directory {}: {}",
                    parent.display(),
                    error
                );
                AppError::io(parent, error)
            })?;
        }
    }

    ensure_managed_or_absent(path)?;
    atomic_write(path, data)?;

    let result = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    if let Err(ref error) = result {
        log::error!(
            "[CodexRoleRoute] Failed to set permissions for {}: {}",
            path.display(),
            error
        );
    } else {
        log::info!(
            "[CodexRoleRoute] Successfully wrote role config: {}",
            path.display()
        );
    }
    result.map_err(|error| AppError::io(path, error))
}

#[cfg(windows)]
fn atomic_write_role_file(path: &Path, data: &[u8]) -> Result<(), AppError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};

    log::info!(
        "[CodexRoleRoute] Writing role config to: {}",
        path.display()
    );

    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("invalid Codex Agent Role path".to_string()))?;

    // 确保父目录存在
    if !parent.exists() {
        log::info!(
            "[CodexRoleRoute] Creating parent directory: {}",
            parent.display()
        );
    }
    fs::create_dir_all(parent).map_err(|error| {
        log::error!(
            "[CodexRoleRoute] Failed to create parent directory {}: {}",
            parent.display(),
            error
        );
        AppError::io(parent, error)
    })?;

    // 检查目录权限（仅记录日志）
    if let Ok(metadata) = fs::metadata(parent) {
        log::debug!(
            "[CodexRoleRoute] Parent directory readonly: {}",
            metadata.permissions().readonly()
        );
    }

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
            let rename_result = fs::rename(&temp_path, path);
            if let Err(ref error) = rename_result {
                log::error!(
                    "[CodexRoleRoute] Failed to rename {} -> {}: {}",
                    temp_path.display(),
                    path.display(),
                    error
                );
            }
            return rename_result.map_err(|error| AppError::IoContext {
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
            let os_error = std::io::Error::last_os_error();
            log::error!(
                "[CodexRoleRoute] ReplaceFileW failed: {} -> {}, error: {}",
                temp_path.display(),
                path.display(),
                os_error
            );
            return Err(AppError::IoContext {
                context: format!(
                    "Codex Agent Role atomic replace failed: {} -> {}",
                    temp_path.display(),
                    path.display()
                ),
                source: os_error,
            });
        }
        log::info!(
            "[CodexRoleRoute] Successfully wrote role config: {}",
            path.display()
        );
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

fn remove_if_managed(path: &Path) -> Result<bool, AppError> {
    if managed_content(path)?.is_some() {
        remove_file_if_exists(path)?;
        return Ok(true);
    }
    Ok(false)
}

fn remove_file_if_exists(path: &Path) -> Result<(), AppError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::io(path, error)),
    }
}

fn snapshot_files(paths: &CodexAgentRolePaths) -> Result<Vec<FileSnapshot>, AppError> {
    snapshot_paths(paths.all())
}

fn snapshot_paths<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<Vec<FileSnapshot>, AppError> {
    paths
        .into_iter()
        .map(|path| {
            let content = regular_file_content(path)?;
            Ok(FileSnapshot {
                path: path.to_path_buf(),
                content,
                post_image: TransactionPostImage::Unchanged,
            })
        })
        .collect()
}

fn record_post_image(
    snapshots: &mut [FileSnapshot],
    path: &Path,
    content: Option<&[u8]>,
) -> Result<(), AppError> {
    let snapshot = snapshots
        .iter_mut()
        .find(|snapshot| snapshot.path == path)
        .ok_or_else(|| {
            AppError::Config(format!(
                "Codex Agent Role transaction path was not snapshotted: {}",
                path.display()
            ))
        })?;
    snapshot.post_image = match content {
        Some(content) => TransactionPostImage::Content(content.to_vec()),
        None => TransactionPostImage::Missing,
    };
    Ok(())
}

fn observe_transaction_path(
    snapshots: &mut [FileSnapshot],
    path: &Path,
    allowed_post_images: &[Option<&[u8]>],
) -> Result<(), AppError> {
    let current = regular_file_content(path)?;
    let snapshot = snapshots
        .iter_mut()
        .find(|snapshot| snapshot.path == path)
        .ok_or_else(|| {
            AppError::Config(format!(
                "Codex Agent Role transaction path was not snapshotted: {}",
                path.display()
            ))
        })?;

    if current == snapshot.content {
        return Ok(());
    }

    if allowed_post_images.contains(&current.as_deref()) {
        snapshot.post_image = match current {
            Some(content) => TransactionPostImage::Content(content),
            None => TransactionPostImage::Missing,
        };
        return Ok(());
    }

    Err(AppError::localized(
        "codex_agent_role_file_conflict",
        format!(
            "Codex Agent Role 在事务执行期间被外部修改；当前文件已保留: {}",
            path.display()
        ),
        format!(
            "Codex Agent Role changed externally during the transaction and the current file was preserved: {}",
            path.display()
        ),
    ))
}

fn combine_transaction_results(
    primary: Result<(), AppError>,
    secondary: Result<(), AppError>,
) -> Result<(), AppError> {
    match (primary, secondary) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(secondary)) => Err(AppError::Message(format!(
            "{primary}; Codex Agent Role transaction observation failed: {secondary}"
        ))),
    }
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
    let Some(post_image) = snapshot.post_image.content() else {
        return Ok(());
    };
    let current = regular_file_content(&snapshot.path)?;
    if current == snapshot.content {
        return Ok(());
    }

    if current.as_deref() != post_image {
        return Err(AppError::localized(
            "codex_agent_role_file_conflict",
            format!(
                "Codex Agent Role 回滚检测到外部文件变更，保留当前文件: {}",
                snapshot.path.display()
            ),
            format!(
                "Codex Agent Role rollback detected an external file change and preserved it: {}",
                snapshot.path.display()
            ),
        ));
    }

    match snapshot.content.as_deref() {
        Some(original) => atomic_write_role_file(&snapshot.path, original),
        None => remove_file_if_exists(&snapshot.path),
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
        disable_codex_agent_roles_at_with_before_copy,
        disable_codex_agent_roles_at_with_before_move, ensure_disabled_role_copy,
        inject_role_mutation_fault, is_loopback_address, reconcile_codex_agent_roles_at,
        reconcile_codex_agent_roles_at_with_writer, reconcile_current_codex_agent_roles,
        restore_snapshots, verify_codex_role_route_token, CodexAgentRolePaths, FileSnapshot,
        RoleMutationFault, TransactionPostImage, FRONTEND_ROLE_ROUTE_VALUE,
        MANAGED_DISABLE_QUARANTINE_SEPARATOR, MANAGED_MARKER, ROLE_COORDINATION_LOCK,
        ROLE_TOKEN_HEADER,
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
    fn loopback_address_validation_parses_ip_literals_strictly() {
        for address in ["127.0.0.1", "127.42.0.9", "::1", "[::1]", "localhost"] {
            assert!(is_loopback_address(address), "expected loopback: {address}");
        }

        for address in [
            "127.evil",
            "127.0.0.1:15777",
            "localhost:15777",
            "0.0.0.0",
            "::",
            "192.0.2.10",
        ] {
            assert!(
                !is_loopback_address(address),
                "expected non-loopback: {address}"
            );
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
    fn pair_projection_rolls_back_writer_that_errors_after_atomic_write() {
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
                atomic_write(path, content.as_bytes())?;
                if writes == 2 {
                    return Err(AppError::Message(
                        "injected writer failure after atomic write".to_string(),
                    ));
                }
                Ok(())
            },
        )
        .expect_err("writer error after atomic write should fail projection");

        assert!(error
            .to_string()
            .contains("injected writer failure after atomic write"));
        assert!(!error.to_string().contains("rollback failed"));
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
                post_image: TransactionPostImage::Content(
                    format!("{MANAGED_MARKER}\nname = \"transaction-conflicted\"\n").into_bytes(),
                ),
            },
            FileSnapshot {
                path: restorable.clone(),
                content: Some(original_restorable.as_bytes().to_vec()),
                post_image: TransactionPostImage::Content(
                    format!("{MANAGED_MARKER}\nname = \"transaction-value\"\n").into_bytes(),
                ),
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
            post_image: TransactionPostImage::Content(
                format!("{MANAGED_MARKER}\nname = \"transaction-value\"\n").into_bytes(),
            ),
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

    #[test]
    fn disabling_preserves_user_owned_fixed_disabled_files_and_isolates_managed_roles() {
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
        let user_frontend = b"name = \"user-disabled-frontend\"\n";
        let user_backend = b"name = \"user-disabled-backend\"\n";
        fs::write(&paths.frontend_disabled, user_frontend).expect("user frontend disabled file");
        fs::write(&paths.backend_disabled, user_backend).expect("user backend disabled file");

        let result = disable_codex_agent_roles_at(temp.path()).expect("disable roles");

        assert!(!result.enabled);
        assert!(result.discovery_changed);
        assert!(!paths.frontend.exists());
        assert!(!paths.backend.exists());
        assert_eq!(fs::read(&paths.frontend_disabled).unwrap(), user_frontend);
        assert_eq!(fs::read(&paths.backend_disabled).unwrap(), user_backend);

        let isolated: Vec<_> = fs::read_dir(temp.path())
            .expect("read agents directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".cc-switch-disabled-"))
            })
            .collect();
        assert_eq!(isolated.len(), 2);
        assert!(isolated
            .iter()
            .all(|path| path.extension().and_then(|ext| ext.to_str()) != Some("toml")));
        assert!(isolated
            .iter()
            .any(|path| fs::read(path).unwrap() == frontend));
        assert!(isolated
            .iter()
            .any(|path| fs::read(path).unwrap() == backend));
    }

    #[test]
    fn disabling_preserves_marker_bearing_active_user_file_replaced_after_capture() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend_before = fs::read(&paths.frontend).expect("frontend snapshot");
        let backend_before = fs::read(&paths.backend).expect("backend snapshot");
        let user_frontend =
            format!("{MANAGED_MARKER}\nname = \"user-frontend-after-capture\"\n# external edit\n")
                .into_bytes();
        let frontend_path = paths.frontend.clone();
        let mut replaced = false;

        let error = disable_codex_agent_roles_at_with_before_move(temp.path(), |path| {
            if !replaced && path == frontend_path {
                atomic_write(path, &user_frontend)?;
                replaced = true;
            }
            Ok(())
        })
        .expect_err("external active-role replacement must abort disable");

        assert!(error
            .to_string()
            .contains("Codex Agent Role changed externally"));
        assert_eq!(fs::read(&paths.backend).unwrap(), backend_before);
        assert!(!paths.frontend_disabled.exists());
        assert!(!paths.backend_disabled.exists());

        let recovered_user_files: Vec<_> = fs::read_dir(temp.path())
            .expect("read agents directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(MANAGED_DISABLE_QUARANTINE_SEPARATOR))
                    && fs::read(path).is_ok_and(|content| content == user_frontend)
            })
            .collect();
        let active_content = fs::read(&paths.frontend).ok();
        assert_ne!(active_content.as_deref(), Some(frontend_before.as_slice()));
        assert!(
            active_content.as_deref() == Some(user_frontend.as_slice())
                || !recovered_user_files.is_empty(),
            "the external replacement must survive at the active or quarantine path"
        );
        assert!(recovered_user_files
            .iter()
            .all(|path| path.extension().and_then(|ext| ext.to_str()) != Some("toml")));
    }

    #[test]
    fn disabling_preserves_marker_bearing_destination_created_after_snapshot() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend_before = fs::read(&paths.frontend).expect("frontend snapshot");
        let backend_before = fs::read(&paths.backend).expect("backend snapshot");
        let external_destination = format!(
            "{MANAGED_MARKER}\nname = \"user-disabled-destination-after-snapshot\"\n# external edit\n"
        )
        .into_bytes();
        let frontend_disabled = paths.frontend_disabled.clone();
        let mut created = false;

        let error = disable_codex_agent_roles_at_with_before_copy(temp.path(), |path| {
            if !created && path == frontend_disabled {
                fs::write(path, &external_destination)
                    .map_err(|error| AppError::io(path, error))?;
                created = true;
            }
            Ok(())
        })
        .expect_err("external destination creation must abort disable");

        assert!(error.to_string().contains("disabled destination changed"));
        assert_eq!(
            fs::read(&paths.frontend_disabled).unwrap(),
            external_destination
        );
        assert_eq!(fs::read(&paths.frontend).unwrap(), frontend_before);
        assert_eq!(fs::read(&paths.backend).unwrap(), backend_before);
        assert!(!paths.backend_disabled.exists());
    }

    #[test]
    fn disabling_rolls_back_destination_when_copy_errors_after_write() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend_before = fs::read(&paths.frontend).expect("frontend snapshot");
        let backend_before = fs::read(&paths.backend).expect("backend snapshot");
        let _fault = inject_role_mutation_fault(RoleMutationFault::DisabledCopyAfterWrite);

        let error = disable_codex_agent_roles_at(temp.path())
            .expect_err("post-write disabled copy failure must abort disable");

        assert!(error.to_string().contains("after disabled copy write"));
        assert!(!error.to_string().contains("rollback failed"));
        assert_eq!(fs::read(&paths.frontend).unwrap(), frontend_before);
        assert_eq!(fs::read(&paths.backend).unwrap(), backend_before);
        assert!(!paths.frontend_disabled.exists());
        assert!(!paths.backend_disabled.exists());
    }

    #[test]
    fn disabling_rolls_back_rename_when_quarantine_read_errors() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend_before = fs::read(&paths.frontend).expect("frontend snapshot");
        let backend_before = fs::read(&paths.backend).expect("backend snapshot");
        let _fault = inject_role_mutation_fault(RoleMutationFault::MoveAfterRenameBeforeRead);

        let error = disable_codex_agent_roles_at(temp.path())
            .expect_err("post-rename read failure must abort disable");

        assert!(error.to_string().contains("after rename before read"));
        assert!(!error.to_string().contains("rollback failed"));
        assert_eq!(fs::read(&paths.frontend).unwrap(), frontend_before);
        assert_eq!(fs::read(&paths.backend).unwrap(), backend_before);
        assert!(!paths.frontend_disabled.exists());
        assert!(!paths.backend_disabled.exists());
        assert!(fs::read_dir(temp.path())
            .expect("read agents directory")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .contains(MANAGED_DISABLE_QUARANTINE_SEPARATOR)));
    }

    #[test]
    fn disabling_rolls_back_rename_when_quarantine_remove_errors_after_remove() {
        let temp = tempdir().expect("tempdir");
        reconcile_codex_agent_roles_at(
            temp.path(),
            "provider-a",
            "http://127.0.0.1:15777/v1",
            Some(&routing()),
        )
        .expect("project roles");
        let paths = CodexAgentRolePaths::from_agents_dir(temp.path());
        let frontend_before = fs::read(&paths.frontend).expect("frontend snapshot");
        let backend_before = fs::read(&paths.backend).expect("backend snapshot");
        let _fault = inject_role_mutation_fault(RoleMutationFault::MoveAfterRemove);

        let error = disable_codex_agent_roles_at(temp.path())
            .expect_err("post-remove failure must abort disable");

        assert!(error.to_string().contains("after quarantine remove"));
        assert!(!error.to_string().contains("rollback failed"));
        assert_eq!(fs::read(&paths.frontend).unwrap(), frontend_before);
        assert_eq!(fs::read(&paths.backend).unwrap(), backend_before);
        assert!(!paths.frontend_disabled.exists());
        assert!(!paths.backend_disabled.exists());
        assert!(fs::read_dir(temp.path())
            .expect("read agents directory")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .contains(MANAGED_DISABLE_QUARANTINE_SEPARATOR)));
    }

    #[test]
    fn disabled_destination_race_preserves_external_file() {
        let temp = tempdir().expect("tempdir");
        let destination = temp.path().join("cc-switch-frontend.toml.disabled");
        let user_content = b"name = \"user-disabled-destination\"\n";
        let managed_content = format!("{MANAGED_MARKER}\nname = \"managed-frontend\"\n");
        fs::write(&destination, user_content).expect("write external destination");

        let error = ensure_disabled_role_copy(&destination, managed_content.as_bytes())
            .expect_err("external disabled destination must not be overwritten");

        assert!(error.to_string().contains("disabled destination changed"));
        assert_eq!(fs::read(&destination).unwrap(), user_content);
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
