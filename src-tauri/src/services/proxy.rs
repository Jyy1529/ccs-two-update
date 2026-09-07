//! 代理服务业务逻辑层
//!
//! 提供代理服务器的启动、停止和配置管理
mod management;

use crate::app_config::AppType;
use crate::config::{get_claude_settings_path, read_json_file, write_json_file};
use crate::database::Database;
use crate::provider::Provider;
use crate::proxy::providers::codex_oauth_auth::CodexOAuthManager;
use crate::proxy::server::{ProxyServer, ProxyServerReuseState};
use crate::proxy::switch_lock::SwitchLockManager;
use crate::proxy::types::*;
use crate::services::provider::{
    build_effective_provider_for_live_with_codex_oauth_manager,
    build_effective_settings_with_common_config,
    write_live_with_common_config_for_codex_oauth_manager,
};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::{Mutex, RwLock};

/// 用于接管 Live 配置时的占位符（避免客户端提示缺少 key，同时不泄露真实 Token）
const PROXY_TOKEN_PLACEHOLDER: &str = "PROXY_MANAGED";
const CODEX_AGENT_ROLE_LOOPBACK_REQUIRED: &str = "codex_agent_role_loopback_required";
const TAKEOVER_APP_LOCK_ORDER: [&str; 4] = ["claude", "codex", "gemini", "grokbuild"];

/// 代理接管模式下需要从 Claude Live 配置中移除的"模型覆盖"字段。
///
/// 原因：接管模式下 `*_MODEL` 必须由 CC Switch 写成稳定的 Claude 角色别名，
/// 再由本地代理映射到当前供应商真实模型；`*_MODEL_NAME` 也需要同步接管，
/// 否则 Claude Code 模型菜单会残留上一个供应商的显示名称。
const CLAUDE_MODEL_OVERRIDE_ENV_KEYS: [&str; 12] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_REASONING_MODEL", // legacy: 已废弃，但旧配置可能残留
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_SMALL_FAST_MODEL", // Legacy key (已废弃)：历史版本使用该字段区分 small/fast 模型
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

const CLAUDE_TAKEOVER_HAIKU_MODEL: &str = "claude-haiku-4-5";
const CLAUDE_TAKEOVER_SONNET_MODEL: &str = "claude-sonnet-4-6";
const CLAUDE_TAKEOVER_OPUS_MODEL: &str = "claude-opus-4-8";
const CLAUDE_TAKEOVER_FABLE_MODEL: &str = "claude-fable-5";
// 写给 Claude Code 时沿用文档示例的大写形式；解析侧大小写不敏感。
const CLAUDE_ONE_M_MARKER_FOR_CLIENT: &str = "[1M]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeTakeoverAuthPolicy {
    PreserveExistingOrAuthToken,
    ManagedAccount { keep_auth_token: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexAuthFileSnapshot {
    contents: Option<Vec<u8>>,
}

impl CodexAuthFileSnapshot {
    fn capture() -> Result<Self, String> {
        let path = crate::codex_config::get_codex_auth_path();
        match std::fs::read(&path) {
            Ok(contents) => Ok(Self {
                contents: Some(contents),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self { contents: None })
            }
            Err(error) => Err(format!(
                "读取 Codex auth 失败 ({}): {error}",
                path.display()
            )),
        }
    }

    fn value(&self) -> Result<Option<Value>, String> {
        self.contents
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|error| format!("读取 Codex auth 失败: {error}"))
    }
}

/// Owns the exact auth.json generation observed at restore start.
///
/// A plain compare-then-write is unsafe because Codex can replace auth.json
/// between those two operations. We instead atomically move the current path
/// aside, compare the moved bytes, and only install a replacement if the path
/// is still vacant. A newer Codex login therefore always wins.
struct CodexAuthFileTransaction {
    path: std::path::PathBuf,
    quarantined: Option<std::path::PathBuf>,
    installed: Option<Vec<u8>>,
    finished: bool,
}

impl CodexAuthFileTransaction {
    fn begin(expected: &CodexAuthFileSnapshot) -> Result<Self, String> {
        let path = crate::codex_config::get_codex_auth_path();
        let _permission = crate::app_management::permit_path(&path).map_err(|e| e.to_string())?;
        let mut transaction = Self {
            path: path.clone(),
            quarantined: None,
            installed: None,
            finished: false,
        };

        let Some(expected_contents) = expected.contents.as_deref() else {
            return match std::fs::read(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(transaction),
                Ok(_) => Err(Self::changed_error()),
                Err(error) => Err(format!(
                    "读取 Codex auth 失败 ({}): {error}",
                    path.display()
                )),
            };
        };

        // The no-clobber install/rollback protocol below requires hard links.
        // Probe before moving the live credentials so unsupported custom Codex
        // directories fail closed with auth.json still in place.
        let probe = Self::unique_sibling_path(&path, "restore-probe")?;
        match std::fs::hard_link(&path, &probe) {
            Ok(()) => {
                std::fs::remove_file(&probe).map_err(|error| {
                    format!(
                        "清理 Codex auth 事务能力探针失败 ({}): {error}",
                        probe.display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Self::changed_error());
            }
            Err(error) => {
                return Err(format!(
                    "Codex auth 所在文件系统不支持安全恢复，原凭据未修改 ({}): {error}",
                    path.display()
                ));
            }
        }

        let quarantine = Self::unique_sibling_path(&path, "restore-backup")?;
        match std::fs::rename(&path, &quarantine) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Self::changed_error());
            }
            Err(error) => {
                return Err(format!(
                    "认领 Codex auth 失败 ({}): {error}",
                    path.display()
                ));
            }
        }
        transaction.quarantined = Some(quarantine.clone());

        let actual = std::fs::read(&quarantine).map_err(|error| {
            format!(
                "读取已认领的 Codex auth 失败 ({}): {error}",
                quarantine.display()
            )
        })?;
        if actual != expected_contents {
            let restore_result = transaction.restore_quarantined_if_vacant();
            transaction.finished = true;
            return match restore_result {
                Ok(()) => Err(Self::changed_error()),
                Err(restore_error) => Err(format!(
                    "{}; 恢复较新的 Codex auth 失败: {restore_error}",
                    Self::changed_error()
                )),
            };
        }

        Ok(transaction)
    }

    fn install(&mut self, replacement: Option<Vec<u8>>) -> Result<(), String> {
        let _permission = crate::app_management::permit_path(&self.path).map_err(|e| e.to_string())?;
        let Some(contents) = replacement else {
            return Ok(());
        };

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("创建 Codex auth 目录失败 ({}): {error}", parent.display())
            })?;
        }
        let temporary = Self::unique_sibling_path(&self.path, "restore-new")?;
        let write_result = (|| -> Result<(), String> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary).map_err(|error| {
                format!(
                    "创建 Codex auth 临时文件失败 ({}): {error}",
                    temporary.display()
                )
            })?;
            use std::io::Write;
            file.write_all(&contents)
                .and_then(|_| file.flush())
                .map_err(|error| {
                    format!(
                        "写入 Codex auth 临时文件失败 ({}): {error}",
                        temporary.display()
                    )
                })?;
            drop(file);

            match std::fs::hard_link(&temporary, &self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(Self::changed_error())
                }
                Err(error) => Err(format!(
                    "安装 Codex auth 失败 ({}): {error}",
                    self.path.display()
                )),
            }
        })();
        let _ = std::fs::remove_file(&temporary);

        if let Err(error) = write_result {
            // If Codex created a newer path while the expected generation was
            // quarantined, never put the older generation back over it.
            if self.path.exists() {
                self.discard_quarantined();
                self.finished = true;
            }
            return Err(error);
        }

        self.installed = Some(contents);
        Ok(())
    }

    fn commit(mut self) -> Result<(), String> {
        self.discard_quarantined();
        self.finished = true;
        Ok(())
    }

    fn rollback(mut self) -> Result<(), String> {
        let result = self.rollback_inner();
        self.finished = true;
        result
    }

    fn rollback_inner(&mut self) -> Result<(), String> {
        let _permission = crate::app_management::permit_path(&self.path).map_err(|e| e.to_string())?;
        if let Some(installed) = self.installed.take() {
            let replacement_quarantine = Self::unique_sibling_path(&self.path, "restore-rollback")?;
            match std::fs::rename(&self.path, &replacement_quarantine) {
                Ok(()) => {
                    let current = std::fs::read(&replacement_quarantine).map_err(|error| {
                        format!(
                            "读取待回滚 Codex auth 失败 ({}): {error}",
                            replacement_quarantine.display()
                        )
                    })?;
                    if current == installed {
                        std::fs::remove_file(&replacement_quarantine).map_err(|error| {
                            format!(
                                "删除待回滚 Codex auth 失败 ({}): {error}",
                                replacement_quarantine.display()
                            )
                        })?;
                    } else {
                        // Codex replaced our installed generation. Restore that
                        // newer file if the path is still vacant and discard the
                        // old expected generation.
                        Self::restore_file_if_vacant(&replacement_quarantine, &self.path)?;
                        self.discard_quarantined();
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // A concurrent logout removed our generation. Missing auth
                    // is newer state too; do not resurrect the old credentials.
                    self.discard_quarantined();
                    return Ok(());
                }
                Err(error) => {
                    return Err(format!(
                        "回滚 Codex auth 失败 ({}): {error}",
                        self.path.display()
                    ));
                }
            }
        }

        self.restore_quarantined_if_vacant()
    }

    fn restore_quarantined_if_vacant(&mut self) -> Result<(), String> {
        let Some(quarantined) = self.quarantined.take() else {
            return Ok(());
        };
        Self::restore_file_if_vacant(&quarantined, &self.path)
    }

    fn restore_file_if_vacant(
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> Result<(), String> {
        let _permission = crate::app_management::permit_path(destination).map_err(|e| e.to_string())?;
        match std::fs::hard_link(source, destination) {
            Ok(()) => {
                std::fs::remove_file(source).map_err(|error| {
                    format!(
                        "清理 Codex auth 事务文件失败 ({}): {error}",
                        source.display()
                    )
                })?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // The destination was recreated by Codex and is newer than the
                // quarantined generation.
                std::fs::remove_file(source).map_err(|remove_error| {
                    format!(
                        "清理旧 Codex auth 事务文件失败 ({}): {remove_error}",
                        source.display()
                    )
                })?;
                Ok(())
            }
            Err(error) => Err(format!(
                "恢复 Codex auth 事务文件失败 ({} -> {}): {error}",
                source.display(),
                destination.display()
            )),
        }
    }

    fn discard_quarantined(&mut self) {
        if let Some(path) = self.quarantined.take() {
            let Ok(_permission) = crate::app_management::permit_path(&path) else {
                self.quarantined = Some(path);
                return;
            };
            if let Err(error) = std::fs::remove_file(&path) {
                log::warn!(
                    "清理旧 Codex auth 事务文件失败 ({}): {error}",
                    path.display()
                );
            }
        }
    }

    fn unique_sibling_path(
        path: &std::path::Path,
        label: &str,
    ) -> Result<std::path::PathBuf, String> {
        let parent = path
            .parent()
            .ok_or_else(|| format!("无效的 Codex auth 路径: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("auth.json");
        Ok(parent.join(format!(
            ".{file_name}.cc-switch-{label}-{}",
            uuid::Uuid::new_v4()
        )))
    }

    fn changed_error() -> String {
        "Codex auth 在恢复期间发生变化；为避免覆盖新凭据，本次恢复已取消，请重试".to_string()
    }
}

impl Drop for CodexAuthFileTransaction {
    fn drop(&mut self) {
        if !self.finished {
            if let Err(error) = self.rollback_inner() {
                log::error!("Codex auth 事务自动回滚失败: {error}");
            }
        }
    }
}

#[derive(Clone)]
pub struct ProxyService {
    db: Arc<Database>,
    codex_oauth_manager: Arc<CodexOAuthManager>,
    /// Serializes compound proxy mutations without replacing the v3.19 lifecycle locks.
    transaction_lock: Arc<Mutex<()>>,
    lifecycle: Arc<Mutex<()>>,
    server: Arc<RwLock<Option<ProxyServer>>>,
    /// AppHandle，用于传递给 ProxyServer 以支持故障转移时的 UI 更新
    app_handle: Arc<RwLock<Option<tauri::AppHandle>>>,
    switch_locks: SwitchLockManager,
    #[cfg(test)]
    start_barrier: Arc<RwLock<Option<Arc<tokio::sync::Barrier>>>>,
    #[cfg(test)]
    stop_pause: Arc<RwLock<Option<StopPause>>>,
    #[cfg(test)]
    start_install_pause: Arc<RwLock<Option<StopPause>>>,
    #[cfg(test)]
    restore_cleanup_pause: Arc<RwLock<Option<StopPause>>>,
    #[cfg(test)]
    plain_stop_takeover_check_pause: Arc<RwLock<Option<StopPause>>>,
    #[cfg(test)]
    takeover_write_failure: Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(test)]
#[derive(Clone)]
struct StopPause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HotSwitchOutcome {
    pub logical_target_changed: bool,
}

#[derive(Debug, Clone)]
struct ProxyRuntimeSnapshot {
    address: String,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProxyFileState {
    Missing,
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
struct ProxyFileSnapshot {
    path: PathBuf,
    state: ProxyFileState,
    rollback_guard: Option<ProxyFileState>,
}

impl ProxyFileSnapshot {
    fn capture(path: PathBuf) -> Result<Self, String> {
        let state = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!("事务快照拒绝符号链接: {}", path.display()));
                }
                if !metadata.is_file() {
                    return Err(format!("事务快照目标不是普通文件: {}", path.display()));
                }
                ProxyFileState::Bytes(std::fs::read(&path).map_err(|error| {
                    format!("读取事务快照文件 {} 失败: {error}", path.display())
                })?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProxyFileState::Missing,
            Err(error) => {
                return Err(format!("检查事务快照文件 {} 失败: {error}", path.display()));
            }
        };
        Ok(Self {
            path,
            state,
            rollback_guard: None,
        })
    }

    fn capture_rollback_guard(&mut self) -> Result<(), String> {
        self.rollback_guard = Some(self.current_state("记录事务文件产物")?);
        Ok(())
    }

    fn restore_if_changed(&self) -> Result<(), String> {
        let _permission = crate::app_management::permit_path(&self.path).map_err(|e| e.to_string())?;
        let current = self.current_state("读取事务回滚文件")?;

        if current == self.state {
            return Ok(());
        }
        let rollback_guard = self.rollback_guard.as_ref().ok_or_else(|| {
            format!(
                "事务回滚文件缺少产物保护状态，保留当前文件: {}",
                self.path.display()
            )
        })?;
        if &current != rollback_guard {
            return Err(format!(
                "事务回滚检测到外部修改，保留当前文件: {}",
                self.path.display()
            ));
        }

        match &self.state {
            ProxyFileState::Missing => std::fs::remove_file(&self.path).map_err(|error| {
                format!("删除事务中新建文件 {} 失败: {error}", self.path.display())
            }),
            ProxyFileState::Bytes(bytes) => crate::config::atomic_write(&self.path, bytes)
                .map_err(|error| format!("恢复事务文件 {} 失败: {error}", self.path.display())),
        }
    }

    fn current_state(&self, operation: &str) -> Result<ProxyFileState, String> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!("事务回滚拒绝符号链接: {}", self.path.display()));
                }
                if !metadata.is_file() {
                    return Err(format!("事务回滚目标不是普通文件: {}", self.path.display()));
                }
                Ok(ProxyFileState::Bytes(std::fs::read(&self.path).map_err(
                    |error| format!("{operation} {} 失败: {error}", self.path.display()),
                )?))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProxyFileState::Missing)
            }
            Err(error) => Err(format!(
                "检查事务回滚文件 {} 失败: {error}",
                self.path.display()
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProxyTransactionSnapshot {
    proxy_config: ProxyConfig,
    global_config: GlobalProxyConfig,
    app_configs: Vec<AppProxyConfig>,
    live_backups: Vec<(String, Option<LiveBackup>)>,
    live_files: Vec<ProxyFileSnapshot>,
    runtime: Option<ProxyRuntimeSnapshot>,
    active_targets: Vec<ActiveTarget>,
}

impl ProxyTransactionSnapshot {
    pub(crate) fn capture_rollback_file_guards(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        for file in &mut self.live_files {
            if let Err(error) = file.capture_rollback_guard() {
                errors.push(error);
            }
        }
        errors
    }
}

fn codex_agent_role_connect_host(listen_address: &str) -> Result<String, String> {
    let address = listen_address
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let ip = address.parse::<std::net::IpAddr>().map_err(|error| {
        format!(
            "{CODEX_AGENT_ROLE_LOOPBACK_REQUIRED}: 无法解析代理监听地址 {listen_address}: {error}"
        )
    })?;
    let connect_ip = match ip {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        ip if ip.is_loopback() => ip,
        _ => {
            return Err(format!(
                "{CODEX_AGENT_ROLE_LOOPBACK_REQUIRED}: Codex Agent Role 路由要求代理监听地址为 loopback 或 wildcard，当前地址为 {listen_address}"
            ));
        }
    };

    Ok(match connect_ip {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    })
}

impl ProxyService {
    pub fn new(db: Arc<Database>) -> Self {
        let codex_oauth_manager =
            Arc::new(CodexOAuthManager::new(crate::config::get_app_config_dir()));

        Self::new_with_codex_oauth_manager(db, codex_oauth_manager)
    }

    pub fn new_with_codex_oauth_manager(
        db: Arc<Database>,
        codex_oauth_manager: Arc<CodexOAuthManager>,
    ) -> Self {
        Self {
            db,
            codex_oauth_manager,
            transaction_lock: Arc::new(Mutex::new(())),
            lifecycle: Arc::new(Mutex::new(())),
            server: Arc::new(RwLock::new(None)),
            app_handle: Arc::new(RwLock::new(None)),
            switch_locks: SwitchLockManager::new(),
            #[cfg(test)]
            start_barrier: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            stop_pause: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            start_install_pause: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            restore_cleanup_pause: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            plain_stop_takeover_check_pause: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            takeover_write_failure: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    async fn set_start_barrier_for_test(&self, barrier: Arc<tokio::sync::Barrier>) {
        *self.start_barrier.write().await = Some(barrier);
    }

    #[cfg(test)]
    async fn wait_at_start_barrier_for_test(&self) {
        let barrier = self.start_barrier.read().await.clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }

    #[cfg(test)]
    async fn set_stop_pause_for_test(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.stop_pause.write().await = Some(StopPause { entered, release });
    }

    #[cfg(test)]
    async fn wait_at_stop_pause_for_test(&self) {
        let pause = self.stop_pause.write().await.take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }

    #[cfg(test)]
    async fn set_start_install_pause_for_test(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.start_install_pause.write().await = Some(StopPause { entered, release });
    }

    #[cfg(test)]
    async fn wait_at_start_install_pause_for_test(&self) {
        let pause = self.start_install_pause.write().await.take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }

    #[cfg(test)]
    async fn set_restore_cleanup_pause_for_test(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.restore_cleanup_pause.write().await = Some(StopPause { entered, release });
    }

    #[cfg(test)]
    async fn wait_at_restore_cleanup_pause_for_test(&self) {
        let pause = self.restore_cleanup_pause.write().await.take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }

    #[cfg(test)]
    async fn set_plain_stop_takeover_check_pause_for_test(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.plain_stop_takeover_check_pause.write().await = Some(StopPause { entered, release });
    }

    #[cfg(test)]
    async fn wait_at_plain_stop_takeover_check_pause_for_test(&self) {
        let pause = self.plain_stop_takeover_check_pause.write().await.take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }

    #[cfg(test)]
    fn fail_next_takeover_write_for_test(&self, app_type: &str) {
        *self
            .takeover_write_failure
            .lock()
            .expect("lock takeover write failure") = Some(app_type.to_string());
    }

    #[cfg(test)]
    fn maybe_fail_takeover_write_for_test(&self, app_type: &AppType) -> Result<(), String> {
        let mut failure = self
            .takeover_write_failure
            .lock()
            .expect("lock takeover write failure");
        if failure.as_deref() == Some(app_type.as_str()) {
            failure.take();
            return Err(format!(
                "injected {} takeover Live write failure",
                app_type.as_str()
            ));
        }
        Ok(())
    }

    async fn lock_all_takeover_apps(&self) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut guards = Vec::with_capacity(TAKEOVER_APP_LOCK_ORDER.len());
        for app_type in TAKEOVER_APP_LOCK_ORDER {
            guards.push(self.switch_locks.lock_for_app(app_type).await);
        }
        guards
    }

    #[cfg(test)]
    fn apply_claude_takeover_fields(config: &mut Value, proxy_url: &str) {
        Self::apply_claude_takeover_fields_with_policy(
            config,
            proxy_url,
            ClaudeTakeoverAuthPolicy::PreserveExistingOrAuthToken,
        );
    }

    fn apply_claude_takeover_fields_for_provider(
        config: &mut Value,
        proxy_url: &str,
        provider: &Provider,
    ) {
        let auth_policy = if provider.uses_managed_account_auth() {
            // Codex 系（含仅凭 base_url 识别、无 provider_type meta 的）必须保留
            // ANTHROPIC_AUTH_TOKEN 占位符：Claude Code 缺该键会弹登录提示（#3784）。
            // Copilot 默认同样注入 AUTH_TOKEN 占位符：Claude Code（实测 2.1.220）
            // 对 ANTHROPIC_API_KEY 会弹"是否使用该自定义 key"确认框且默认
            // "No (recommended)"，按默认走后占位符被忽略、落入 Not logged in
            // （并非 sk-ant-* 格式校验——headless 下占位符原样出站）；AUTH_TOKEN
            // 作为网关 Bearer 被直接信任，零弹窗。仅当供应商表单显式选择了
            // ANTHROPIC_API_KEY（meta.apiKeyField）时才保留 API_KEY 占位，以规避
            // 与 /login 管理的 key 冲突（#1049）。
            ClaudeTakeoverAuthPolicy::ManagedAccount {
                keep_auth_token: !provider.is_github_copilot()
                    || !provider.claude_uses_api_key_field(),
            }
        } else {
            ClaudeTakeoverAuthPolicy::PreserveExistingOrAuthToken
        };
        // Copilot/Codex 接管时 live config 可能还是旧供应商；显示模型必须跟随目标 provider。
        let takeover_model_fields = if provider.uses_managed_account_auth() {
            Self::build_claude_takeover_model_fields(&provider.settings_config)
        } else {
            Self::build_claude_takeover_model_fields(config)
        };

        Self::apply_claude_takeover_fields_with_policy_and_models(
            config,
            proxy_url,
            auth_policy,
            takeover_model_fields,
        );
    }

    #[cfg(test)]
    fn apply_claude_takeover_fields_with_policy(
        config: &mut Value,
        proxy_url: &str,
        auth_policy: ClaudeTakeoverAuthPolicy,
    ) {
        // 必须在 remove/insert 前 snapshot：避免读到自己刚写入的接管别名。
        let takeover_model_fields = Self::build_claude_takeover_model_fields(config);

        Self::apply_claude_takeover_fields_with_policy_and_models(
            config,
            proxy_url,
            auth_policy,
            takeover_model_fields,
        );
    }

    fn apply_claude_takeover_fields_with_policy_and_models(
        config: &mut Value,
        proxy_url: &str,
        auth_policy: ClaudeTakeoverAuthPolicy,
        takeover_model_fields: Vec<(&'static str, String)>,
    ) {
        if !config.is_object() {
            *config = json!({});
        }

        let root = config
            .as_object_mut()
            .expect("Claude config should be normalized to an object");
        let env = root.entry("env".to_string()).or_insert_with(|| json!({}));
        if !env.is_object() {
            *env = json!({});
        }

        let env = env
            .as_object_mut()
            .expect("Claude env should be normalized to an object");
        env.insert("ANTHROPIC_BASE_URL".to_string(), json!(proxy_url));

        for key in CLAUDE_MODEL_OVERRIDE_ENV_KEYS {
            env.remove(key);
        }

        for (key, value) in takeover_model_fields {
            env.insert(key.to_string(), Value::String(value));
        }

        let token_keys = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ];

        match auth_policy {
            ClaudeTakeoverAuthPolicy::PreserveExistingOrAuthToken => {
                let mut replaced_any = false;
                for key in token_keys {
                    if env.contains_key(key) {
                        env.insert(key.to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
                        replaced_any = true;
                    }
                }

                if !replaced_any {
                    env.insert(
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        json!(PROXY_TOKEN_PLACEHOLDER),
                    );
                }
            }
            ClaudeTakeoverAuthPolicy::ManagedAccount { keep_auth_token } => {
                for key in token_keys {
                    env.remove(key);
                }
                // 只注入一个认证键：两者同时存在会触发 Claude Code 的
                // "Both ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY set" 警告（#4919）。
                // - Codex 系保留 AUTH_TOKEN：缺该键 Claude Code 会弹登录提示（#3784）。
                //   无条件注入而非"已存在才保留"：热切换路径传入的是 provider
                //   settings（预设不含该键），且旧版接管已把存量用户 live 中的键删光。
                // - Copilot 默认 AUTH_TOKEN：API_KEY 占位符会触发 Claude Code 的
                //   自定义 key 确认框（默认 "No (recommended)"），按默认走即
                //   Not logged in；仅当表单显式选择了 ANTHROPIC_API_KEY 时才用
                //   API_KEY 占位以规避 /login key 冲突（#1049）。
                if keep_auth_token {
                    env.insert(
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        json!(PROXY_TOKEN_PLACEHOLDER),
                    );
                } else {
                    env.insert(
                        "ANTHROPIC_API_KEY".to_string(),
                        json!(PROXY_TOKEN_PLACEHOLDER),
                    );
                }
            }
        }
    }

    fn build_claude_takeover_model_fields(config: &Value) -> Vec<(&'static str, String)> {
        let Some(env) = config.get("env").and_then(Value::as_object) else {
            return Vec::new();
        };

        let default_model = Self::claude_env_string(env, "ANTHROPIC_MODEL");
        let small_fast_model = Self::claude_env_string(env, "ANTHROPIC_SMALL_FAST_MODEL");
        let haiku_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_HAIKU_MODEL")
            .or(small_fast_model)
            .or(default_model);
        let sonnet_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_SONNET_MODEL")
            .or(default_model)
            .or(small_fast_model);
        let opus_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_OPUS_MODEL")
            .or(default_model)
            .or(small_fast_model);
        // Fable 未配置时不写稳定别名；映射侧会 fable→opus 降级（与官方一致）。
        let fable_model = Self::claude_env_string(env, "ANTHROPIC_DEFAULT_FABLE_MODEL");

        let subagent_model = Self::claude_env_string(env, "CLAUDE_CODE_SUBAGENT_MODEL");

        let mut fields = Vec::with_capacity(9);
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            CLAUDE_TAKEOVER_HAIKU_MODEL,
            false,
            haiku_model,
        );
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            CLAUDE_TAKEOVER_SONNET_MODEL,
            true,
            sonnet_model,
        );
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            CLAUDE_TAKEOVER_OPUS_MODEL,
            true,
            opus_model,
        );
        Self::push_claude_takeover_role_fields(
            &mut fields,
            env,
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
            CLAUDE_TAKEOVER_FABLE_MODEL,
            true,
            fable_model,
        );
        if let Some(subagent_model) = subagent_model {
            fields.push(("CLAUDE_CODE_SUBAGENT_MODEL", subagent_model.to_string()));
        }
        fields
    }

    fn push_claude_takeover_role_fields(
        fields: &mut Vec<(&'static str, String)>,
        env: &Map<String, Value>,
        model_key: &'static str,
        name_key: &'static str,
        takeover_model: &'static str,
        supports_one_m: bool,
        upstream_model: Option<&str>,
    ) {
        let Some(upstream_model) = upstream_model else {
            return;
        };

        let mut client_model = takeover_model.to_string();
        if supports_one_m && Self::has_claude_one_m_marker(upstream_model) {
            client_model.push_str(CLAUDE_ONE_M_MARKER_FOR_CLIENT);
        }
        fields.push((model_key, client_model));

        let display_name = Self::claude_env_string(env, name_key)
            .map(str::to_string)
            .unwrap_or_else(|| Self::strip_claude_one_m_marker(upstream_model));
        if !display_name.is_empty() {
            fields.push((name_key, display_name));
        }
    }

    fn claude_env_string<'a>(env: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
        env.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn has_claude_one_m_marker(model: &str) -> bool {
        model
            .trim_end()
            .to_ascii_lowercase()
            .ends_with(crate::claude_desktop_config::ONE_M_CONTEXT_MARKER)
    }

    fn strip_claude_one_m_marker(model: &str) -> String {
        crate::proxy::model_mapper::strip_one_m_suffix_for_upstream(model)
            .trim()
            .to_string()
    }

    fn claude_provider_with_effective_settings(
        &self,
        provider: &Provider,
    ) -> Result<Provider, String> {
        let mut effective_provider = provider.clone();
        effective_provider.settings_config = build_effective_settings_with_common_config(
            self.db.as_ref(),
            &AppType::Claude,
            provider,
        )
        .map_err(|e| format!("构建 claude 有效配置失败: {e}"))?;
        Ok(effective_provider)
    }

    pub async fn sync_claude_live_from_provider_while_proxy_active(
        &self,
        provider: &Provider,
    ) -> Result<(), String> {
        let effective_provider = self.claude_provider_with_effective_settings(provider)?;
        let mut effective_settings = effective_provider.settings_config.clone();
        let (proxy_url, _) = self.build_proxy_urls().await?;

        Self::apply_claude_takeover_fields_for_provider(
            &mut effective_settings,
            &proxy_url,
            &effective_provider,
        );
        self.write_claude_live(&effective_settings)?;
        Ok(())
    }

    pub async fn sync_codex_live_from_provider_while_proxy_active(
        &self,
        provider: &Provider,
    ) -> Result<(), String> {
        self.sync_codex_live_from_provider_while_proxy_active_guarded(provider, None, None)
            .await
    }

    pub(crate) async fn sync_codex_live_from_provider_while_proxy_active_guarded(
        &self,
        provider: &Provider,
        outgoing_managed_account_id: Option<&str>,
        expected_outgoing_refresh_token: Option<&str>,
    ) -> Result<(), String> {
        let existing_live = self.read_codex_live().ok();
        let mut effective_settings = build_effective_provider_for_live_with_codex_oauth_manager(
            self.db.as_ref(),
            &AppType::Codex,
            provider,
            &self.codex_oauth_manager,
        )
        .map_err(|e| format!("构建 codex 有效配置失败: {e}"))?
        .settings_config;
        if let Some(existing_live) = existing_live.as_ref() {
            Self::preserve_toml_mcp_servers_from_existing_config(
                &mut effective_settings,
                existing_live,
            )?;
        }
        let (_, proxy_codex_base_url) = self.build_proxy_urls().await?;

        Self::apply_codex_takeover_fields_for_provider(
            &mut effective_settings,
            &proxy_codex_base_url,
            provider,
        )?;

        if let (Some(account_id), Some(expected_refresh_token)) =
            (outgoing_managed_account_id, expected_outgoing_refresh_token)
        {
            crate::codex_config::ensure_codex_live_auth_unchanged_for_managed_account(
                account_id,
                expected_refresh_token,
            )
            .map_err(|error| error.to_string())?;
        }

        self.write_codex_takeover_live_for_provider(&effective_settings, Some(provider))?;
        Ok(())
    }

    pub async fn sync_grok_live_from_provider_while_proxy_active(
        &self,
        provider: &Provider,
    ) -> Result<(), String> {
        let existing_live = self.read_grok_live().ok();
        let mut effective_settings = build_effective_settings_with_common_config(
            self.db.as_ref(),
            &AppType::GrokBuild,
            provider,
        )
        .map_err(|e| format!("构建 Grok Build 有效配置失败: {e}"))?;
        if let Some(existing_live) = existing_live.as_ref() {
            Self::preserve_toml_mcp_servers_from_existing_config(
                &mut effective_settings,
                existing_live,
            )?;
        }
        let (proxy_url, _) = self.build_proxy_urls().await?;
        let proxy_grok_base_url = format!("{}/grokbuild/v1", proxy_url.trim_end_matches('/'));
        Self::apply_grok_takeover_fields(&mut effective_settings, &proxy_grok_base_url)?;
        self.write_grok_live(&effective_settings)
    }

    fn get_current_provider_for_app(&self, app_type: &AppType) -> Result<Option<Provider>, String> {
        let Some(current_id) = crate::settings::get_effective_current_provider(&self.db, app_type)
            .map_err(|e| format!("获取 {app_type:?} 当前供应商失败: {e}"))?
        else {
            return Ok(None);
        };

        self.db
            .get_provider_by_id(&current_id, app_type.as_str())
            .map_err(|e| format!("读取 {app_type:?} 当前供应商失败: {e}"))
    }

    fn should_preserve_current_codex_auth(&self) -> Result<bool, String> {
        // Unknown current state is handled conservatively: preserving the live
        // auth file cannot roll a refresh generation back, while restoring an
        // unclassified legacy backup can. A concrete non-official provider is
        // the only case where its stored auth should replace the live file.
        Ok(self
            .get_current_provider_for_app(&AppType::Codex)?
            .as_ref()
            .is_none_or(crate::proxy::providers::is_codex_official_provider))
    }

    /// Official Codex auth is a live, independently rotating login. A takeover
    /// backup may restore config/catalog, but must never freeze refresh tokens
    /// that Codex CLI can advance while the proxy is active.
    fn strip_current_official_codex_auth_from_backup(
        &self,
        config: &mut Value,
    ) -> Result<(), String> {
        if self.should_preserve_current_codex_auth()? {
            if let Some(root) = config.as_object_mut() {
                root.remove("auth");
            }
        }
        Ok(())
    }

    async fn rollback_failed_takeover_activation(
        &self,
        app_type: &AppType,
        codex_snapshot: Option<&crate::codex_config::CodexLiveStateSnapshot>,
    ) -> Result<(), String> {
        if let Some(snapshot) = codex_snapshot {
            return snapshot
                .restore_preserving_newer_same_account_auth()
                .map_err(|error| error.to_string());
        }
        self.restore_live_config_for_app_inner(app_type).await
    }

    async fn refresh_active_target_from_current_provider(&self, app_type: &AppType) {
        let Ok(Some(provider)) = self.get_current_provider_for_app(app_type) else {
            return;
        };
        if let Some(server) = self.server.read().await.as_ref() {
            server
                .set_active_target(app_type.as_str(), &provider.id, &provider.name)
                .await;
        }
    }

    async fn rollback_hot_switch_preparation(
        &self,
        app_type: &AppType,
        previous_backup: Option<&LiveBackup>,
        previous_provider_id: Option<&str>,
        should_sync_backup: bool,
        live_taken_over: bool,
        previous_codex_live_state: Option<&crate::codex_config::CodexLiveStateSnapshot>,
    ) {
        if !should_sync_backup {
            return;
        }

        let rollback_result = match previous_backup {
            Some(backup) => {
                self.db
                    .save_live_backup(app_type.as_str(), &backup.original_config)
                    .await
            }
            None => self.db.delete_live_backup(app_type.as_str()).await,
        };
        if let Err(error) = rollback_result {
            log::error!("{} 热切换失败后恢复原备份失败: {error}", app_type.as_str());
        }

        if let Some(previous_live) = previous_codex_live_state {
            if let Err(error) = previous_live.restore_preserving_newer_same_account_auth() {
                log::error!(
                    "{} 热切换失败后恢复 Codex Live 状态失败: {error}",
                    app_type.as_str()
                );
            }
            return;
        }

        let Some(previous_provider_id) = previous_provider_id else {
            return;
        };
        let Ok(Some(previous_provider)) = self
            .db
            .get_provider_by_id(previous_provider_id, app_type.as_str())
        else {
            return;
        };

        let live_result = if matches!(app_type, AppType::Claude) {
            self.sync_claude_live_from_provider_while_proxy_active(&previous_provider)
                .await
        } else if live_taken_over && matches!(app_type, AppType::Codex) {
            self.sync_codex_live_from_provider_while_proxy_active(&previous_provider)
                .await
        } else if live_taken_over && matches!(app_type, AppType::GrokBuild) {
            self.sync_grok_live_from_provider_while_proxy_active(&previous_provider)
                .await
        } else {
            Ok(())
        };
        if let Err(error) = live_result {
            log::error!(
                "{} 热切换失败后恢复原 Live 配置失败: {error}",
                app_type.as_str()
            );
        }
    }

    fn require_current_provider_for_app(&self, app_type: &AppType) -> Result<Provider, String> {
        self.get_current_provider_for_app(app_type)?
            .ok_or_else(|| format!("{app_type:?} 当前供应商不存在，无法接管 Live 配置"))
    }

    /// 设置 AppHandle（在应用初始化时调用）
    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        futures::executor::block_on(async {
            *self.app_handle.write().await = Some(handle);
        });
    }

    pub(crate) async fn lock_switch_for_app(
        &self,
        app_type: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        self.switch_locks.lock_for_app(app_type).await
    }

    pub(crate) async fn lock_transaction(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.transaction_lock.clone().lock_owned().await
    }

    pub(crate) async fn snapshot_transaction_state(
        &self,
    ) -> Result<ProxyTransactionSnapshot, String> {
        let live_files = [
            get_claude_settings_path(),
            crate::codex_config::get_codex_config_path(),
            crate::codex_config::get_codex_auth_path(),
            crate::codex_config::get_codex_model_catalog_path(),
            crate::gemini_config::get_gemini_env_path(),
            crate::grok_config::get_grok_config_path(),
        ]
        .into_iter()
        .map(ProxyFileSnapshot::capture)
        .collect::<Result<Vec<_>, _>>()?;

        let proxy_config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|error| format!("读取代理配置快照失败: {error}"))?;
        let global_config = self
            .db
            .get_global_proxy_config()
            .await
            .map_err(|error| format!("读取全局代理配置快照失败: {error}"))?;

        let mut app_configs = Vec::new();
        let mut live_backups = Vec::new();
        for app_type in [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::GrokBuild,
        ] {
            let app_type_str = app_type.as_str();
            app_configs.push(
                self.db
                    .get_proxy_config_for_app(app_type_str)
                    .await
                    .map_err(|error| format!("读取 {app_type_str} 代理配置快照失败: {error}"))?,
            );
            live_backups.push((
                app_type_str.to_string(),
                self.db
                    .get_live_backup(app_type_str)
                    .await
                    .map_err(|error| format!("读取 {app_type_str} Live 备份快照失败: {error}"))?,
            ));
        }

        let status = self.get_status().await?;
        let runtime = status.running.then_some(ProxyRuntimeSnapshot {
            address: status.address.clone(),
            port: status.port,
        });
        Ok(ProxyTransactionSnapshot {
            proxy_config,
            global_config,
            app_configs,
            live_backups,
            live_files,
            runtime,
            active_targets: status.active_targets,
        })
    }

    pub(crate) async fn restore_transaction_state(
        &self,
        snapshot: &ProxyTransactionSnapshot,
    ) -> Vec<String> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        let mut errors = Vec::new();
        let current_status = self.get_status().await.unwrap_or_default();
        let runtime_matches = snapshot.runtime.as_ref().is_some_and(|expected| {
            current_status.running
                && current_status.address == expected.address
                && current_status.port == expected.port
        });

        if current_status.running && !runtime_matches {
            if let Err(error) = self.stop_runtime_only().await {
                errors.push(format!("停止当前代理监听器失败: {error}"));
            }
        }

        if let Err(error) = self
            .db
            .update_proxy_config(snapshot.proxy_config.clone())
            .await
        {
            errors.push(format!("恢复原代理配置失败: {error}"));
        }
        for app_config in &snapshot.app_configs {
            if let Err(error) = self
                .db
                .update_proxy_config_for_app(app_config.clone())
                .await
            {
                errors.push(format!(
                    "恢复 {} 应用代理配置失败: {error}",
                    app_config.app_type
                ));
            }
        }
        for (app_type, backup) in &snapshot.live_backups {
            let result = match backup {
                Some(backup) => {
                    self.db
                        .save_live_backup(app_type, &backup.original_config)
                        .await
                }
                None => self.db.delete_live_backup(app_type).await,
            };
            if let Err(error) = result {
                errors.push(format!("恢复 {app_type} Live 备份失败: {error}"));
            }
        }
        for file in &snapshot.live_files {
            if let Err(error) = file.restore_if_changed() {
                errors.push(error);
            }
        }

        match &snapshot.runtime {
            Some(expected) => {
                let status = self.get_status().await.unwrap_or_default();
                if status.running
                    && status.address == expected.address
                    && status.port == expected.port
                {
                    if let Some(server) = self.server.read().await.as_ref() {
                        server.apply_runtime_config(&snapshot.proxy_config).await;
                    }
                } else if !status.running {
                    if let Err(error) = self
                        .start_runtime_at(&snapshot.proxy_config, &expected.address, expected.port)
                        .await
                    {
                        errors.push(format!(
                            "恢复原代理监听地址 {}:{} 失败: {error}",
                            expected.address, expected.port
                        ));
                    }
                } else {
                    errors.push(format!(
                        "当前代理仍监听于 {}:{}，无法恢复 {}:{}",
                        status.address, status.port, expected.address, expected.port
                    ));
                }
            }
            None => {
                if self.is_running().await {
                    if let Err(error) = self.stop_runtime_only().await {
                        errors.push(format!("停止本次新启动的代理失败: {error}"));
                    }
                }
            }
        }

        if let Some(server) = self.server.read().await.as_ref() {
            server
                .replace_active_targets(&snapshot.active_targets)
                .await;
        }
        if let Err(error) = self
            .db
            .update_global_proxy_config(snapshot.global_config.clone())
            .await
        {
            errors.push(format!("恢复全局代理配置失败: {error}"));
        }
        errors
    }

    async fn stop_runtime_only(&self) -> Result<(), String> {
        let mut server_guard = self.server.write().await;
        let Some(server) = server_guard.as_ref() else {
            return Ok(());
        };
        let stop_result = server
            .stop()
            .await
            .map_err(|error| format!("停止代理服务器失败: {error}"));
        server_guard.take();
        stop_result
    }

    async fn start_runtime_at(
        &self,
        config: &ProxyConfig,
        address: &str,
        port: u16,
    ) -> Result<(), String> {
        let mut server_guard = self.server.write().await;
        if let Some(existing) = server_guard.as_ref() {
            match existing.reuse_state().await {
                ProxyServerReuseState::Reusable => {
                    return Err("代理服务器已在运行".to_string());
                }
                ProxyServerReuseState::NeedsReap => {
                    if let Err(error) = existing.stop().await {
                        log::warn!("恢复代理运行态时回收旧任务收到终止结果: {error}");
                    }
                }
                ProxyServerReuseState::Stopped => {}
            }
            server_guard.take();
        }
        let mut runtime_config = config.clone();
        runtime_config.listen_address = address.to_string();
        runtime_config.listen_port = port;
        let app_handle = self.app_handle.read().await.clone();
        let server = ProxyServer::new(runtime_config, self.db.clone(), app_handle);
        let info = server
            .start()
            .await
            .map_err(|error| format!("启动代理服务器失败: {error}"))?;
        if info.address != address || info.port != port {
            let _ = server.stop().await;
            return Err(format!(
                "代理监听地址恢复不一致，期望 {address}:{port}，实际 {}:{}",
                info.address, info.port
            ));
        }
        *server_guard = Some(server);
        Ok(())
    }

    /// 该应用是否正有切换 / 接管操作在进行中。见 `SwitchLockManager::is_locked_for_app`。
    pub(crate) async fn is_switch_in_progress_for_app(&self, app_type: &str) -> bool {
        self.switch_locks.is_locked_for_app(app_type).await
    }

    /// 启动代理服务器
    pub async fn start(&self) -> Result<ProxyServerInfo, String> {
        #[cfg(test)]
        self.wait_at_start_barrier_for_test().await;

        let _transaction = self.lock_transaction().await;
        self.start_inner().await
    }

    /// Caller holds the proxy transaction guard.
    pub(crate) async fn start_inner(&self) -> Result<ProxyServerInfo, String> {
        // Serializes the full state transition, including persisted enablement.
        let _lifecycle_guard = self.lifecycle.lock().await;

        // 1. 启动时自动设置 proxy_enabled = true
        let mut global_config = self
            .db
            .get_global_proxy_config()
            .await
            .map_err(|e| format!("获取全局代理配置失败: {e}"))?;

        if !global_config.proxy_enabled {
            global_config.proxy_enabled = true;
            self.db
                .update_global_proxy_config(global_config.clone())
                .await
                .map_err(|e| format!("更新代理总开关失败: {e}"))?;
        }

        // 2. 获取配置
        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("获取代理配置失败: {e}"))?;

        // 3. 若已在运行：确保持久化状态（如需要）并返回当前信息
        let mut server_guard = self.server.write().await;
        if let Some(server) = server_guard.as_ref() {
            match server.reuse_state().await {
                ProxyServerReuseState::Reusable => {
                    let status = server.get_status().await;
                    if status.running {
                        return Ok(ProxyServerInfo {
                            address: status.address,
                            port: status.port,
                            // 无法精确取回首次启动时间，返回当前时间用于 UI 展示即可
                            started_at: chrono::Utc::now().to_rfc3339(),
                        });
                    }
                    if let Err(error) = server.stop().await {
                        log::warn!("回收刚结束的代理服务器任务时收到终止结果: {error}");
                    }
                }
                ProxyServerReuseState::NeedsReap => {
                    if let Err(error) = server.stop().await {
                        log::warn!("回收不可复用的代理服务器任务时收到终止结果: {error}");
                    }
                }
                ProxyServerReuseState::Stopped => {}
            }
            server_guard.take();
        }

        // 4. 创建并启动服务器
        let app_handle = self.app_handle.read().await.clone();
        let server = ProxyServer::new(config.clone(), self.db.clone(), app_handle);
        let info = match server.start().await {
            Ok(info) => info,
            Err(error) => {
                server_guard.take();
                let start_error = format!("启动代理服务器失败: {error}");
                self.persist_proxy_disabled_after_failure("代理启动失败")
                    .await;
                return Err(start_error);
            }
        };
        *server_guard = Some(server);

        #[cfg(test)]
        self.wait_at_start_install_pause_for_test().await;

        if let Err(e) = self
            .persist_ephemeral_listen_port_if_needed(&config, info.port)
            .await
        {
            if let Some(server) = server_guard.as_ref() {
                let _ = server.stop().await;
            }
            server_guard.take();
            self.persist_proxy_disabled_after_failure("保存动态代理端口失败")
                .await;
            return Err(e);
        }

        log::info!("代理服务器已启动: {}:{}", info.address, info.port);
        Ok(info)
    }

    async fn persist_ephemeral_listen_port_if_needed(
        &self,
        config: &ProxyConfig,
        actual_port: u16,
    ) -> Result<(), String> {
        if config.listen_port != 0 {
            return Ok(());
        }

        let mut resolved_config = config.clone();
        resolved_config.listen_port = actual_port;
        self.db
            .update_proxy_config(resolved_config)
            .await
            .map_err(|e| format!("保存动态代理端口失败: {e}"))
    }

    async fn start_before_takeover_if_ephemeral_port(&self) -> Result<bool, String> {
        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("获取代理配置失败: {e}"))?;
        if config.listen_port != 0 || self.is_running().await {
            return Ok(false);
        }

        self.start_inner().await?;
        Ok(true)
    }

    /// 启动代理服务器（带 Live 配置接管）
    pub async fn start_with_takeover(&self) -> Result<ProxyServerInfo, String> {
        let _transaction = self.lock_transaction().await;
        self.start_with_takeover_inner().await
    }

    /// Caller holds the proxy transaction guard.
    async fn start_with_takeover_inner(&self) -> Result<ProxyServerInfo, String> {
        // 1. 备份各应用的 Live 配置
        self.backup_live_configs().await?;

        // 2. 同步 Live 配置中的 Token 到数据库（确保代理能读到最新的 Token）
        if let Err(e) = self.sync_live_to_providers().await {
            // 同步失败时尚未写入接管配置，但备份可能包含敏感信息，尽量清理
            if let Err(clean_err) = self.db.delete_all_live_backups().await {
                log::warn!("清理 Live 备份失败: {clean_err}");
            }
            return Err(e);
        }

        // 端口 0 需要先启动代理拿到 OS 分配的真实端口，否则接管 Live 配置会写出 :0。
        let started_proxy_before_takeover =
            match self.start_before_takeover_if_ephemeral_port().await {
                Ok(started) => started,
                Err(e) => {
                    if let Err(clean_err) = self.db.delete_all_live_backups().await {
                        log::warn!("清理 Live 备份失败: {clean_err}");
                    }
                    return Err(e);
                }
            };

        // 3. 在写入接管配置之前先落盘接管标志：
        //    这样即使在接管过程中断电/kill，下次启动也能检测到并自动恢复。
        if let Err(e) = self.db.set_live_takeover_active(true).await {
            if let Err(clean_err) = self.db.delete_all_live_backups().await {
                log::warn!("清理 Live 备份失败: {clean_err}");
            }
            if started_proxy_before_takeover {
                let _ = self.stop_runtime_inner(true).await;
            }
            return Err(format!("设置接管状态失败: {e}"));
        }

        // 4. 接管各应用的 Live 配置（写入代理地址，清空 Token）
        if let Err(e) = self.takeover_live_configs().await {
            // 接管失败（可能是部分写入），尝试恢复原始配置；若恢复失败则保留标志与备份，等待下次启动自动恢复。
            log::error!("接管 Live 配置失败，尝试恢复原始配置: {e}");
            match self.restore_live_configs().await {
                Ok(()) => {
                    let _ = self.db.set_live_takeover_active(false).await;
                    let _ = self.db.delete_all_live_backups().await;
                }
                Err(restore_err) => {
                    log::error!("恢复原始配置失败，将保留备份以便下次启动恢复: {restore_err}");
                }
            }
            if started_proxy_before_takeover {
                let _ = self.stop_runtime_inner(true).await;
            }
            return Err(e);
        }

        // 5. 启动代理服务器
        match self.start_inner().await {
            Ok(info) => Ok(info),
            Err(e) => {
                // 启动失败，恢复原始配置
                log::error!("代理启动失败，尝试恢复原始配置: {e}");
                match self.restore_live_configs().await {
                    Ok(()) => {
                        let _ = self.db.set_live_takeover_active(false).await;
                        let _ = self.db.delete_all_live_backups().await;
                    }
                    Err(restore_err) => {
                        log::error!("恢复原始配置失败，将保留备份以便下次启动恢复: {restore_err}");
                    }
                }
                if started_proxy_before_takeover {
                    let _ = self.stop_runtime_inner(true).await;
                }
                Err(e)
            }
        }
    }

    /// 获取各应用的接管状态（是否改写该应用的 Live 配置指向本地代理）
    pub async fn get_takeover_status(&self) -> Result<ProxyTakeoverStatus, String> {
        // 从 proxy_config.enabled 读取（优先），兼容旧的 live_backup 备份检测
        let claude_enabled = self
            .db
            .get_proxy_config_for_app("claude")
            .await
            .map(|c| c.enabled)
            .unwrap_or(false);
        let codex_enabled = self
            .db
            .get_proxy_config_for_app("codex")
            .await
            .map(|c| c.enabled)
            .unwrap_or(false);
        let gemini_enabled = self
            .db
            .get_proxy_config_for_app("gemini")
            .await
            .map(|c| c.enabled)
            .unwrap_or(false);
        let grokbuild_enabled = self
            .db
            .get_proxy_config_for_app("grokbuild")
            .await
            .map(|c| c.enabled)
            .unwrap_or(false);
        // OpenCode and OpenClaw don't support proxy features, always return false
        let opencode_enabled = false;
        let openclaw_enabled = false;

        Ok(ProxyTakeoverStatus {
            claude: claude_enabled,
            codex: codex_enabled,
            gemini: gemini_enabled,
            grokbuild: grokbuild_enabled,
            opencode: opencode_enabled,
            openclaw: openclaw_enabled,
        })
    }

    /// 为指定应用开启/关闭 Live 接管
    ///
    /// - 开启：自动启动代理服务，仅接管当前 app 的 Live 配置
    /// - 关闭：仅恢复当前 app 的 Live 配置；若无其它接管，则自动停止代理服务
    pub async fn set_takeover_for_app(&self, app_type: &str, enabled: bool) -> Result<(), String> {
        let app = AppType::from_str(app_type).map_err(|e| e.to_string())?;
        let ticket = crate::app_management::WriteTicket::capture(&app).map_err(|e| e.to_string())?;
        let _transaction = self.lock_transaction().await;
        ticket.check().map_err(|e| e.to_string())?;
        self.set_takeover_for_app_inner(app_type, enabled).await
    }

    /// Caller holds the proxy transaction guard.
    pub(crate) async fn set_takeover_for_app_inner(
        &self,
        app_type: &str,
        enabled: bool,
    ) -> Result<(), String> {
        self.set_takeover_for_app_inner_with_health_reset(app_type, enabled, true)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn set_takeover_for_app_inner_preserving_health(
        &self,
        app_type: &str,
        enabled: bool,
    ) -> Result<(), String> {
        self.set_takeover_for_app_inner_with_health_reset(app_type, enabled, false)
            .await
    }

    async fn set_takeover_for_app_inner_with_health_reset(
        &self,
        app_type: &str,
        enabled: bool,
        reset_health_on_disable: bool,
    ) -> Result<(), String> {
        let app = AppType::from_str(app_type).map_err(|e| format!("无效的应用类型: {e}"))?;
        let ticket = crate::app_management::WriteTicket::capture(&app).map_err(|e| e.to_string())?;
        let app_type_str = app.as_str();
        let guard = self.switch_locks.lock_for_app(app_type_str).await;
        ticket.check().map_err(|e| e.to_string())?;

        if enabled {
            // 1) 代理服务未运行则自动启动
            if !self.is_running().await {
                self.start_inner().await?;
            }

            // 2) 已接管则直接返回（幂等）；但如果缺少备份或占位符残留，需要重建接管
            let current_config = self
                .db
                .get_proxy_config_for_app(app_type_str)
                .await
                .map_err(|e| format!("获取 {app_type_str} 配置失败: {e}"))?;

            let mut restore_existing_backup_before_takeover = false;
            if current_config.enabled {
                let has_backup = match self.db.get_live_backup(app_type_str).await {
                    Ok(v) => v.is_some(),
                    Err(e) => {
                        log::warn!("读取 {app_type_str} 备份失败（将继续重建接管）: {e}");
                        false
                    }
                };
                let live_matches_current_proxy =
                    match self.live_takeover_matches_current_proxy(&app).await {
                        Ok(value) => value,
                        Err(e) => {
                            log::warn!("检测 {app_type_str} 接管配置失败（将继续重建接管）: {e}");
                            false
                        }
                    };

                // 必须 backup 存在，且 live 确实指向当前代理地址，才算真接管。
                // 只看占位符会把半接管/旧端口残留误判为可复用，导致开启接管后
                // live 文件仍停留在普通供应商配置。
                if has_backup && live_matches_current_proxy {
                    self.refresh_active_target_from_current_provider(&app).await;
                    return Ok(());
                }
                restore_existing_backup_before_takeover = has_backup;

                log::warn!(
                    "{app_type_str} 标记为已接管，但 backup={has_backup} live_matches_current_proxy={live_matches_current_proxy}，正在重新接管并补齐 Live"
                );
            }

            // 3) 备份 Live 配置（严格：目标 app 不存在则报错）
            if restore_existing_backup_before_takeover {
                self.restore_live_config_for_app_inner(&app).await?;
            } else {
                self.backup_live_config_strict(&app).await?;

                // 4) 同步 Live Token 到数据库（仅当前 app）
                if let Err(e) = self.sync_live_to_provider(&app).await {
                    let _ = self.db.delete_live_backup(app_type_str).await;
                    return Err(e);
                }
            }

            // The persistent Official backup intentionally excludes auth.json
            // because its refresh token keeps rotating during takeover. Keep an
            // exact in-memory pre-write snapshot for activation failures so a
            // partial managed write can still restore the user's prior login.
            let codex_live_before_takeover = if matches!(&app, AppType::Codex) {
                Some(
                    crate::codex_config::CodexLiveStateSnapshot::capture()
                        .map_err(|error| format!("捕获 Codex 接管前状态失败: {error}"))?,
                )
            } else {
                None
            };

            // 5) 写入接管配置（仅当前 app）
            if let Err(e) = self.takeover_live_config_strict(&app).await {
                log::error!("{app_type_str} 接管 Live 配置失败，尝试恢复: {e}");
                match self
                    .rollback_failed_takeover_activation(&app, codex_live_before_takeover.as_ref())
                    .await
                {
                    Ok(()) => {
                        // 恢复成功才清理备份，避免失败场景下丢失唯一可回滚来源
                        let _ = self.db.delete_live_backup(app_type_str).await;
                    }
                    Err(restore_err) => {
                        log::error!(
                            "{app_type_str} 恢复 Live 配置失败，将保留备份以便下次启动恢复: {restore_err}"
                        );
                    }
                }
                return Err(e);
            }

            // 6) 设置 proxy_config.enabled = true
            let enable_result = async {
                let mut updated_config = self
                    .db
                    .get_proxy_config_for_app(app_type_str)
                    .await
                    .map_err(|e| format!("获取 {app_type_str} 配置失败: {e}"))?;
                updated_config.enabled = true;
                self.db
                    .update_proxy_config_for_app(updated_config)
                    .await
                    .map_err(|e| format!("设置 {app_type_str} enabled 状态失败: {e}"))
            }
            .await;
            if let Err(error) = enable_result {
                if matches!(&app, AppType::Codex) {
                    log::error!("{app_type_str} 提交接管状态失败，尝试恢复 Live: {error}");
                    match self
                        .rollback_failed_takeover_activation(
                            &app,
                            codex_live_before_takeover.as_ref(),
                        )
                        .await
                    {
                        Ok(()) => {
                            let _ = self.db.delete_live_backup(app_type_str).await;
                        }
                        Err(restore_error) => {
                            log::error!(
                                "{app_type_str} 恢复 Live 配置失败，将保留备份以便下次恢复: {restore_error}"
                            );
                        }
                    }
                } else {
                    // The Live write already succeeded, but enabled=true did not.
                    // Keep both the backup and the detectable proxy placeholder so
                    // plain stop cannot strand the client; stop_with_restore remains
                    // the authoritative recovery path for this partial activation.
                    log::error!(
                        "{app_type_str} 提交接管状态失败，保留 Live 与备份等待显式恢复: {error}"
                    );
                }
                return Err(error);
            }

            // 7) 兼容旧逻辑：写入 any-of 标志（失败不影响功能）
            let _ = self.db.set_live_takeover_active(true).await;

            self.refresh_active_target_from_current_provider(&app).await;

            // 8) Warn if the current provider is official (risk of account ban via proxy)
            if let Ok(Some(current_id)) =
                crate::settings::get_effective_current_provider(&self.db, &app)
            {
                if let Ok(Some(provider)) = self.db.get_provider_by_id(&current_id, app_type_str) {
                    if provider.category.as_deref() == Some("official")
                        && !crate::services::provider::official_provider_supports_proxy_takeover(
                            &app, &provider,
                        )
                    {
                        if let Some(handle) = self.app_handle.read().await.as_ref() {
                            let _ = handle.emit(
                                "proxy-official-warning",
                                serde_json::json!({
                                    "appType": app_type_str,
                                    "providerName": provider.name,
                                }),
                            );
                        }
                    }
                }
            }

            return Ok(());
        }

        // 关闭接管：检查 enabled 状态
        let current_config = self
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| format!("获取 {app_type_str} 配置失败: {e}"))?;

        if !current_config.enabled {
            return Ok(()); // 未接管，幂等返回
        }

        // 1) 恢复 Live 配置
        //
        // 必须走 with_fallback 版本：备份 → SSOT → 清理占位符 的三层兜底。
        // 简版 restore_live_config_for_app 在备份缺失时会静默 Ok(())，
        // 留下接管时写入的占位符（代理地址/PROXY_MANAGED token），客户端无法工作。
        self.restore_live_config_for_app_with_fallback_inner(&app)
            .await?;

        // 2) 删除该 app 的备份（避免长期存储敏感 Token）
        self.db
            .delete_live_backup(app_type_str)
            .await
            .map_err(|e| format!("删除 {app_type_str} Live 备份失败: {e}"))?;

        // 3) 设置 proxy_config.enabled = false
        let mut updated_config = self
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| format!("获取 {app_type_str} 配置失败: {e}"))?;
        updated_config.enabled = false;
        self.db
            .update_proxy_config_for_app(updated_config)
            .await
            .map_err(|e| format!("清除 {app_type_str} enabled 状态失败: {e}"))?;

        // 4) 清除该应用的健康状态（关闭代理时重置队列状态）
        if reset_health_on_disable {
            self.db
                .clear_provider_health_for_app(app_type_str)
                .await
                .map_err(|e| format!("清除 {app_type_str} 健康状态失败: {e}"))?;
        }

        // 5) 若无其它接管，更新旧标志，并停止代理服务
        // 检查是否还有其它 app 的 enabled = true
        let any_enabled = self
            .db
            .is_live_takeover_active()
            .await
            .map_err(|e| format!("检查接管状态失败: {e}"))?;

        if !any_enabled {
            let _ = self.db.set_live_takeover_active(false).await;

            // Release this app's guard before the authoritative all-app stop
            // path acquires the complete fixed-order guard set.
            drop(guard);

            if self.is_running().await {
                // 此时没有任何 app 处于接管状态，停止服务即可
                let _ = self.stop_inner().await;
            }
        }

        Ok(())
    }

    /// 同步关闭指定应用的 Live 接管（恢复配置并清标志，不停止代理服务）。
    ///
    /// 用于 `ProfileService::apply` 等 sync 路径：调用者所在线程可能没有 Tokio
    /// runtime，无法执行 `set_takeover_for_app(false)` 里的停止服务/等待任务等
    /// Tokio IO。这里只恢复 Live 文件、删除备份、清除 DB 接管标志，让后续
    /// `ProviderService::switch` 能正常写入官方供应商配置。
    ///
    /// 代理服务本身保持运行；当最后一个应用也关闭接管后，下次用户手动关闭
    /// 代理或程序退出时会自然停止。
    pub fn disable_takeover_for_app_sync(&self, app_type: &AppType) -> Result<(), String> {
        crate::app_management::require_managed(app_type).map_err(|e| e.to_string())?;
        futures::executor::block_on(async {
            let _transaction = self.lock_transaction().await;
            let _switch_guard = self.switch_locks.lock_for_app(app_type.as_str()).await;
            self.disable_takeover_for_app_without_stop_inner(app_type)
                .await
        })
    }

    /// Caller holds the proxy transaction and matching app switch guards.
    pub(crate) async fn disable_takeover_for_app_without_stop_inner(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        let app_type_str = app_type.as_str();

        // 1) 恢复原始 Live 配置（备份 → SSOT → 清理占位符 三层兜底）
        self.restore_live_config_for_app_with_fallback_inner(app_type)
            .await
            .map_err(|e| format!("恢复 {app_type_str} Live 配置失败: {e}"))?;

        // 2) 删除该 app 的备份
        self.db
            .delete_live_backup(app_type_str)
            .await
            .map_err(|e| format!("删除 {app_type_str} Live 备份失败: {e}"))?;

        // 3) 设置 proxy_config.enabled = false
        let mut config = self
            .db
            .get_proxy_config_for_app(app_type_str)
            .await
            .map_err(|e| format!("获取 {app_type_str} 配置失败: {e}"))?;
        if config.enabled {
            config.enabled = false;
            self.db
                .update_proxy_config_for_app(config)
                .await
                .map_err(|e| format!("清除 {app_type_str} enabled 状态失败: {e}"))?;
        }

        // 4) 清除该应用的健康状态
        self.db
            .clear_provider_health_for_app(app_type_str)
            .await
            .map_err(|e| format!("清除 {app_type_str} 健康状态失败: {e}"))?;

        // 5) 清旧标志
        let _ = self.db.set_live_takeover_active(false).await;

        Ok(())
    }

    /// 同步 Live 配置中的 Token 到数据库
    ///
    /// 在清空 Live Token 之前调用，确保数据库中的 Provider 配置有最新的 Token。
    /// 这样代理才能从数据库读取到正确的认证信息。
    async fn sync_live_to_provider(&self, app_type: &AppType) -> Result<(), String> {
        let live_config = match app_type {
            AppType::Claude => self.read_claude_live()?,
            AppType::Codex => self.read_codex_live()?,
            AppType::Gemini => self.read_gemini_live()?,
            AppType::GrokBuild => self.read_grok_live()?,
            _ => return Err("该应用不支持代理功能".to_string()),
        };

        self.sync_live_config_to_provider(app_type, &live_config)
            .await
    }

    async fn sync_live_config_to_provider(
        &self,
        app_type: &AppType,
        live_config: &Value,
    ) -> Result<(), String> {
        match app_type {
            AppType::Claude => {
                let provider_id =
                    crate::settings::get_effective_current_provider(&self.db, &AppType::Claude)
                        .map_err(|e| format!("获取 Claude 当前供应商失败: {e}"))?;

                if let Some(provider_id) = provider_id {
                    if let Ok(Some(mut provider)) =
                        self.db.get_provider_by_id(&provider_id, "claude")
                    {
                        if let Some(env) = live_config.get("env").and_then(|v| v.as_object()) {
                            let token_pair = [
                                "ANTHROPIC_AUTH_TOKEN",
                                "ANTHROPIC_API_KEY",
                                "OPENROUTER_API_KEY",
                                "OPENAI_API_KEY",
                            ]
                            .into_iter()
                            .find_map(|key| {
                                env.get(key)
                                    .and_then(|v| v.as_str())
                                    .map(|s| (key, s.trim()))
                            })
                            .filter(|(_, token)| {
                                !token.is_empty() && *token != PROXY_TOKEN_PLACEHOLDER
                            });

                            if let Some((token_key, token)) = token_pair {
                                let env_obj = provider
                                    .settings_config
                                    .get_mut("env")
                                    .and_then(|v| v.as_object_mut());

                                match env_obj {
                                    Some(obj) => {
                                        if token_key == "ANTHROPIC_AUTH_TOKEN"
                                            || token_key == "ANTHROPIC_API_KEY"
                                        {
                                            let mut updated = false;
                                            if obj.contains_key("ANTHROPIC_AUTH_TOKEN") {
                                                obj.insert(
                                                    "ANTHROPIC_AUTH_TOKEN".to_string(),
                                                    json!(token),
                                                );
                                                updated = true;
                                            }
                                            if obj.contains_key("ANTHROPIC_API_KEY") {
                                                obj.insert(
                                                    "ANTHROPIC_API_KEY".to_string(),
                                                    json!(token),
                                                );
                                                updated = true;
                                            }
                                            if !updated {
                                                obj.insert(token_key.to_string(), json!(token));
                                            }
                                        } else {
                                            obj.insert(token_key.to_string(), json!(token));
                                        }
                                    }
                                    None => {
                                        // 至少写入一份可用的 Token
                                        if provider.settings_config.is_null() {
                                            provider.settings_config = json!({});
                                        }

                                        if let Some(root) = provider.settings_config.as_object_mut()
                                        {
                                            root.insert(
                                                "env".to_string(),
                                                json!({ token_key: token }),
                                            );
                                        } else {
                                            log::warn!(
                                                "Claude provider settings_config 格式异常（非对象），跳过写入 Token (provider: {provider_id})"
                                            );
                                        }
                                    }
                                }

                                if let Err(e) = self.db.update_provider_settings_config(
                                    "claude",
                                    &provider_id,
                                    &provider.settings_config,
                                ) {
                                    log::warn!("同步 Claude Token 到数据库失败: {e}");
                                } else {
                                    log::info!(
                                        "已同步 Claude Token 到数据库 (provider: {provider_id})"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            AppType::Codex => {
                let provider_id =
                    crate::settings::get_effective_current_provider(&self.db, &AppType::Codex)
                        .map_err(|e| format!("获取 Codex 当前供应商失败: {e}"))?;

                if let Some(provider_id) = provider_id {
                    if let Ok(Some(mut provider)) =
                        self.db.get_provider_by_id(&provider_id, "codex")
                    {
                        // Official rows are routing/account selectors, not
                        // credential stores. Their auth must remain empty even
                        // when the live Codex login uses OPENAI_API_KEY mode.
                        if crate::proxy::providers::is_codex_official_provider(&provider) {
                            return Ok(());
                        }
                        if let Some(token) = live_config
                            .get("auth")
                            .and_then(|v| v.get("OPENAI_API_KEY"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty() && *s != PROXY_TOKEN_PLACEHOLDER)
                        {
                            if let Some(auth_obj) = provider
                                .settings_config
                                .get_mut("auth")
                                .and_then(|v| v.as_object_mut())
                            {
                                auth_obj.insert("OPENAI_API_KEY".to_string(), json!(token));
                            } else {
                                if provider.settings_config.is_null() {
                                    provider.settings_config = json!({});
                                }

                                if let Some(root) = provider.settings_config.as_object_mut() {
                                    root.insert(
                                        "auth".to_string(),
                                        json!({ "OPENAI_API_KEY": token }),
                                    );
                                } else {
                                    log::warn!(
                                        "Codex provider settings_config 格式异常（非对象），跳过写入 Token (provider: {provider_id})"
                                    );
                                }
                            }

                            if let Err(e) = self.db.update_provider_settings_config(
                                "codex",
                                &provider_id,
                                &provider.settings_config,
                            ) {
                                log::warn!("同步 Codex Token 到数据库失败: {e}");
                            } else {
                                log::info!("已同步 Codex Token 到数据库 (provider: {provider_id})");
                            }
                        }
                    }
                }
            }
            AppType::Gemini => {
                let provider_id =
                    crate::settings::get_effective_current_provider(&self.db, &AppType::Gemini)
                        .map_err(|e| format!("获取 Gemini 当前供应商失败: {e}"))?;

                if let Some(provider_id) = provider_id {
                    if let Ok(Some(mut provider)) =
                        self.db.get_provider_by_id(&provider_id, "gemini")
                    {
                        if let Some(token) = live_config
                            .get("env")
                            .and_then(|v| v.get("GEMINI_API_KEY"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty() && *s != PROXY_TOKEN_PLACEHOLDER)
                        {
                            if let Some(env_obj) = provider
                                .settings_config
                                .get_mut("env")
                                .and_then(|v| v.as_object_mut())
                            {
                                env_obj.insert("GEMINI_API_KEY".to_string(), json!(token));
                            } else {
                                if provider.settings_config.is_null() {
                                    provider.settings_config = json!({});
                                }

                                if let Some(root) = provider.settings_config.as_object_mut() {
                                    root.insert(
                                        "env".to_string(),
                                        json!({ "GEMINI_API_KEY": token }),
                                    );
                                } else {
                                    log::warn!(
                                        "Gemini provider settings_config 格式异常（非对象），跳过写入 Token (provider: {provider_id})"
                                    );
                                }
                            }

                            if let Err(e) = self.db.update_provider_settings_config(
                                "gemini",
                                &provider_id,
                                &provider.settings_config,
                            ) {
                                log::warn!("同步 Gemini Token 到数据库失败: {e}");
                            } else {
                                log::info!(
                                    "已同步 Gemini Token 到数据库 (provider: {provider_id})"
                                );
                            }
                        }
                    }
                }
            }
            AppType::GrokBuild => {
                let provider_id =
                    crate::settings::get_effective_current_provider(&self.db, &AppType::GrokBuild)
                        .map_err(|e| format!("获取 Grok Build 当前供应商失败: {e}"))?;

                if let Some(provider_id) = provider_id {
                    if let Ok(Some(mut provider)) =
                        self.db.get_provider_by_id(&provider_id, "grokbuild")
                    {
                        let live_config_toml = live_config
                            .get("config")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(token) =
                            crate::grok_config::extract_inline_api_key(live_config_toml)
                        {
                            if !token.is_empty() && token != PROXY_TOKEN_PLACEHOLDER {
                                if let Some(provider_config) = provider
                                    .settings_config
                                    .get("config")
                                    .and_then(Value::as_str)
                                {
                                    let updated =
                                        crate::grok_config::update_api_key(provider_config, &token)
                                            .map_err(|e| {
                                                format!("更新 Grok Build API Key 失败: {e}")
                                            })?;
                                    provider.settings_config["config"] = json!(updated);
                                    self.db
                                        .update_provider_settings_config(
                                            "grokbuild",
                                            &provider_id,
                                            &provider.settings_config,
                                        )
                                        .map_err(|e| {
                                            format!("同步 Grok Build Token 到数据库失败: {e}")
                                        })?;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn sync_live_to_providers(&self) -> Result<(), String> {
        if let Ok(live_config) = self.read_claude_live() {
            self.sync_live_config_to_provider(&AppType::Claude, &live_config)
                .await?;
        }

        if let Ok(live_config) = self.read_codex_live() {
            self.sync_live_config_to_provider(&AppType::Codex, &live_config)
                .await?;
        }

        if let Ok(live_config) = self.read_gemini_live() {
            self.sync_live_config_to_provider(&AppType::Gemini, &live_config)
                .await?;
        }

        if let Ok(live_config) = self.read_grok_live() {
            self.sync_live_config_to_provider(&AppType::GrokBuild, &live_config)
                .await?;
        }

        log::info!("Live 配置 Token 同步完成");
        Ok(())
    }

    async fn persist_proxy_disabled(&self) -> Result<(), String> {
        let mut global_config = self
            .db
            .get_global_proxy_config()
            .await
            .map_err(|e| format!("获取全局代理配置失败: {e}"))?;

        if global_config.proxy_enabled {
            global_config.proxy_enabled = false;
            self.db
                .update_global_proxy_config(global_config)
                .await
                .map_err(|e| format!("更新代理总开关失败: {e}"))?;
        }

        Ok(())
    }

    async fn persist_proxy_disabled_after_failure(&self, context: &str) {
        if let Err(error) = self.persist_proxy_disabled().await {
            log::warn!("{context}后持久化 disabled 状态失败: {error}");
        }
    }

    /// Caller holds every supported app switch guard and the lifecycle guard.
    async fn has_takeover_ownership_evidence(&self) -> Result<bool, String> {
        if self
            .db
            .is_live_takeover_active()
            .await
            .map_err(|e| format!("检查接管状态失败: {e}"))?
        {
            return Ok(true);
        }

        if self
            .db
            .has_any_live_backup()
            .await
            .map_err(|e| format!("检查 Live 备份失败: {e}"))?
        {
            return Ok(true);
        }

        Ok(self.detect_takeover_in_live_configs())
    }

    /// 停止代理服务器
    pub async fn stop(&self) -> Result<(), String> {
        let _transaction = self.lock_transaction().await;
        self.stop_inner().await
    }

    /// Caller holds the proxy transaction guard.
    pub(crate) async fn stop_inner(&self) -> Result<(), String> {
        #[cfg(test)]
        self.wait_at_plain_stop_takeover_check_pause_for_test()
            .await;

        let _switch_guards = self.lock_all_takeover_apps().await;

        // Keep ownership until the old task is fully reaped and persisted state
        // reflects the stopped generation.
        let _lifecycle_guard = self.lifecycle.lock().await;

        if self.has_takeover_ownership_evidence().await? {
            return Err(
                "仍有应用处于代理接管状态或检测到接管异常残留，请先在设置中关闭对应应用接管后再停止本地路由；若开关已关闭，请使用“停止并恢复 Live 配置”（stop_with_restore）清理异常残留。"
                    .to_string(),
            );
        }

        self.stop_runtime_locked(false).await
    }

    /// Caller holds the lifecycle guard.
    async fn stop_runtime_locked(&self, not_running_is_ok: bool) -> Result<(), String> {
        let mut server_guard = self.server.write().await;
        if server_guard.is_none() {
            if !not_running_is_ok {
                return Err("代理服务器未运行".to_string());
            }
            drop(server_guard);
            return self.persist_proxy_disabled().await;
        }

        #[cfg(test)]
        self.wait_at_stop_pause_for_test().await;

        let stop_result = match server_guard
            .as_ref()
            .expect("server presence checked")
            .stop()
            .await
        {
            Ok(()) | Err(crate::proxy::ProxyError::NotRunning) if not_running_is_ok => Ok(()),
            Ok(()) => Ok(()),
            Err(error) => Err(format!("停止代理服务器失败: {error}")),
        };
        server_guard.take();
        drop(server_guard);

        let persist_result = self.persist_proxy_disabled().await;

        if let Err(persist_error) = persist_result {
            if let Err(stop_error) = stop_result {
                log::warn!("代理停止后持久化 stopped 状态失败: {persist_error}");
                return Err(stop_error);
            }
            return Err(persist_error);
        }

        stop_result?;
        log::info!("代理服务器已停止");
        Ok(())
    }

    /// Caller holds the proxy transaction guard.
    async fn stop_runtime_inner(&self, not_running_is_ok: bool) -> Result<(), String> {
        let _lifecycle_guard = self.lifecycle.lock().await;
        self.stop_runtime_locked(not_running_is_ok).await
    }

    fn combine_stop_and_cleanup_results(
        stop_result: Result<(), String>,
        cleanup_result: Result<(), String>,
    ) -> Result<(), String> {
        match (stop_result, cleanup_result) {
            (Err(stop_error), Err(cleanup_error)) => {
                log::warn!(
                    "代理停止与 Live 恢复/清理均失败；向调用方保留停止错误。cleanup={cleanup_error}"
                );
                Err(format!("{stop_error}；Live 恢复/清理失败: {cleanup_error}"))
            }
            (Err(stop_error), Ok(())) => Err(stop_error),
            (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Caller holds every supported app switch guard and the lifecycle guard.
    async fn restore_and_cleanup_takeover_state(&self, keep_state: bool) -> Result<(), String> {
        self.restore_live_configs_inner().await?;

        let mut errors = Vec::new();
        if keep_state {
            match self.db.get_proxy_config().await {
                Ok(mut config) => {
                    config.live_takeover_active = false;
                    if let Err(error) = self.db.update_proxy_config(config).await {
                        errors.push(format!("更新接管状态失败: {error}"));
                    }
                }
                Err(error) => errors.push(format!("获取代理配置失败: {error}")),
            }
        } else {
            if let Err(error) = self.db.set_live_takeover_active(false).await {
                errors.push(format!("清除接管状态失败: {error}"));
            }

            for app_type in TAKEOVER_APP_LOCK_ORDER {
                match self.db.get_proxy_config_for_app(app_type).await {
                    Ok(mut config) if config.enabled => {
                        config.enabled = false;
                        if let Err(error) = self.db.update_proxy_config_for_app(config).await {
                            errors.push(format!("清除 {app_type} enabled 状态失败: {error}"));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        errors.push(format!("获取 {app_type} enabled 状态失败: {error}"));
                    }
                }
            }
        }

        if let Err(error) = self.db.delete_all_live_backups().await {
            errors.push(format!("删除备份失败: {error}"));
        }
        if let Err(error) = self.db.clear_all_provider_health().await {
            errors.push(format!("重置健康状态失败: {error}"));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("；"))
        }
    }

    /// 停止代理服务器（恢复 Live 配置，用户手动关闭时使用）
    ///
    /// 会清除 settings 表中的代理状态，下次启动不会自动恢复。
    pub async fn stop_with_restore(&self) -> Result<(), String> {
        let _transaction = self.lock_transaction().await;
        self.stop_with_restore_inner().await
    }

    /// Caller holds the proxy transaction guard.
    pub(crate) async fn stop_with_restore_inner(&self) -> Result<(), String> {
        let _switch_guards = self.lock_all_takeover_apps().await;
        let _lifecycle_guard = self.lifecycle.lock().await;
        let stop_result = self.stop_runtime_locked(true).await;

        #[cfg(test)]
        self.wait_at_restore_cleanup_pause_for_test().await;

        let cleanup_result = self.restore_and_cleanup_takeover_state(false).await;
        let result = Self::combine_stop_and_cleanup_results(stop_result, cleanup_result);
        if result.is_ok() {
            // 注意：不清除故障转移队列和开关状态，保留供下次开启代理时使用
            log::info!("代理已停止，Live 配置已恢复");
        }
        result
    }

    /// 停止代理服务器（恢复 Live 配置，但保留 settings 表中的代理状态）
    ///
    /// 用于程序正常退出时，保留代理状态以便下次启动时自动恢复
    pub async fn stop_with_restore_keep_state(&self) -> Result<(), String> {
        let _transaction = self.lock_transaction().await;
        self.stop_with_restore_keep_state_inner().await
    }

    /// Caller holds the proxy transaction guard.
    async fn stop_with_restore_keep_state_inner(&self) -> Result<(), String> {
        let _switch_guards = self.lock_all_takeover_apps().await;
        let _lifecycle_guard = self.lifecycle.lock().await;
        let stop_result = self.stop_runtime_locked(true).await;

        #[cfg(test)]
        self.wait_at_restore_cleanup_pause_for_test().await;

        let cleanup_result = self.restore_and_cleanup_takeover_state(true).await;
        let result = Self::combine_stop_and_cleanup_results(stop_result, cleanup_result);
        if result.is_ok() {
            log::info!("代理已停止，Live 配置已恢复（保留代理状态，下次启动将自动恢复）");
        }
        result
    }

    /// 备份各应用的 Live 配置
    async fn backup_live_configs(&self) -> Result<(), String> {
        // Claude
        if let Ok(config) = self.read_claude_live() {
            // 跳过已被代理接管的 Live：避免把代理占位符当作"原始 Live"存进备份槽。
            // 否则下次 start_with_takeover 在异常历史状态下（Live 已是占位符）再次
            // 调用本函数，会用代理配置覆盖一个原本正常的备份；之后 stop 恢复时
            // 即便走到备份路径也会把代理占位符再写回 Live，永久卡在 127.0.0.1:15721。
            if Self::live_has_proxy_placeholder_for_app(&AppType::Claude, &config) {
                log::warn!("claude Live 已被代理接管，不备份（避免把代理配置固化进备份槽）；下次 stop 会从 SSOT 重建 Live");
            } else {
                let json_str = serde_json::to_string(&config)
                    .map_err(|e| format!("序列化 Claude 配置失败: {e}"))?;
                self.db
                    .save_live_backup("claude", &json_str)
                    .await
                    .map_err(|e| format!("备份 Claude 配置失败: {e}"))?;
            }
        }

        // Codex
        if let Ok(mut config) = self.read_codex_live() {
            if Self::live_has_proxy_placeholder_for_app(&AppType::Codex, &config) {
                log::warn!("codex Live 已被代理接管，不备份（避免把代理配置固化进备份槽）；下次 stop 会从 SSOT 重建 Live");
            } else {
                self.strip_current_official_codex_auth_from_backup(&mut config)?;
                let json_str = serde_json::to_string(&config)
                    .map_err(|e| format!("序列化 Codex 配置失败: {e}"))?;
                self.db
                    .save_live_backup("codex", &json_str)
                    .await
                    .map_err(|e| format!("备份 Codex 配置失败: {e}"))?;
            }
        }

        // Gemini
        if let Ok(config) = self.read_gemini_live() {
            if Self::live_has_proxy_placeholder_for_app(&AppType::Gemini, &config) {
                log::warn!("gemini Live 已被代理接管，不备份（避免把代理配置固化进备份槽）；下次 stop 会从 SSOT 重建 Live");
            } else {
                let json_str = serde_json::to_string(&config)
                    .map_err(|e| format!("序列化 Gemini 配置失败: {e}"))?;
                self.db
                    .save_live_backup("gemini", &json_str)
                    .await
                    .map_err(|e| format!("备份 Gemini 配置失败: {e}"))?;
            }
        }

        // Grok Build
        if let Ok(config) = self.read_grok_live() {
            if Self::live_has_proxy_placeholder_for_app(&AppType::GrokBuild, &config) {
                log::warn!("grokbuild Live 已被代理接管，不备份；下次 stop 会从 SSOT 重建 Live");
            } else {
                let json_str = serde_json::to_string(&config)
                    .map_err(|e| format!("序列化 Grok Build 配置失败: {e}"))?;
                self.db
                    .save_live_backup("grokbuild", &json_str)
                    .await
                    .map_err(|e| format!("备份 Grok Build 配置失败: {e}"))?;
            }
        }

        log::info!("已备份所有应用的 Live 配置");
        Ok(())
    }

    /// 备份指定应用的 Live 配置（严格模式：目标配置不存在则返回错误）
    async fn backup_live_config_strict(&self, app_type: &AppType) -> Result<(), String> {
        let (app_type_str, mut config) = match app_type {
            AppType::Claude => ("claude", self.read_claude_live()?),
            AppType::Codex => ("codex", self.read_codex_live()?),
            AppType::Gemini => ("gemini", self.read_gemini_live()?),
            AppType::GrokBuild => ("grokbuild", self.read_grok_live()?),
            _ => return Err("该应用不支持代理功能".to_string()),
        };

        // 跳过已被代理接管的 Live：避免把代理占位符当作"原始 Live"存进备份槽
        // （见 backup_live_configs 中的注释）。
        if Self::live_has_proxy_placeholder_for_app(app_type, &config) {
            log::warn!(
                "{app_type_str} Live 已被代理接管，不备份（避免把代理配置固化进备份槽）；下次 stop 会从 SSOT 重建 Live"
            );
            return Ok(());
        }

        if matches!(app_type, AppType::Codex) {
            self.strip_current_official_codex_auth_from_backup(&mut config)?;
        }

        let json_str = serde_json::to_string(&config)
            .map_err(|e| format!("序列化 {app_type_str} 配置失败: {e}"))?;
        self.db
            .save_live_backup(app_type_str, &json_str)
            .await
            .map_err(|e| format!("备份 {app_type_str} 配置失败: {e}"))?;

        Ok(())
    }

    /// 构造写入 Live 的代理地址（处理 0.0.0.0 / IPv6 等特殊情况）
    async fn build_proxy_urls(&self) -> Result<(String, String), String> {
        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("获取代理配置失败: {e}"))?;

        let mut listen_address = config.listen_address;
        let mut listen_port = config.listen_port;
        if let Some(server) = self.server.read().await.as_ref() {
            let status = server.get_status().await;
            if status.running {
                listen_address = status.address;
                listen_port = status.port;
            }
        }

        Self::build_proxy_urls_for_bind(&listen_address, listen_port)
    }

    fn build_proxy_urls_for_bind(
        listen_address: &str,
        listen_port: u16,
    ) -> Result<(String, String), String> {
        // listen_address 可能是 0.0.0.0（用于监听所有网卡），但客户端无法用 0.0.0.0 连接；
        // 因此写回到各应用配置时，优先使用本机回环地址。
        let connect_host = match listen_address {
            "0.0.0.0" => "127.0.0.1".to_string(),
            "::" => "::1".to_string(),
            _ => listen_address.to_string(),
        };
        let connect_host_for_url = if connect_host.contains(':') && !connect_host.starts_with('[') {
            format!("[{connect_host}]")
        } else {
            connect_host
        };

        if listen_port == 0 {
            return Err("代理监听端口为 0，但代理服务器尚未运行，无法生成接管地址".to_string());
        }

        let proxy_origin = format!("http://{}:{}", connect_host_for_url, listen_port);
        let proxy_url = proxy_origin.clone();
        let proxy_codex_base_url = format!("{}/v1", proxy_origin.trim_end_matches('/'));

        Ok((proxy_url, proxy_codex_base_url))
    }

    /// Returns the loopback Responses endpoint used by managed Codex Agent Roles.
    pub(crate) async fn codex_proxy_base_url(&self) -> Result<String, String> {
        let config = self
            .db
            .get_proxy_config()
            .await
            .map_err(|error| format!("获取代理配置失败: {error}"))?;
        let mut listen_address = config.listen_address;
        let mut listen_port = config.listen_port;
        if let Some(server) = self.server.read().await.as_ref() {
            let status = server.get_status().await;
            if status.running {
                listen_address = status.address;
                listen_port = status.port;
            }
        }
        let connect_host = codex_agent_role_connect_host(&listen_address)?;
        if listen_port == 0 {
            return Err(
                "代理监听端口为 0，但代理服务器尚未运行，无法生成 Codex Agent Role 地址"
                    .to_string(),
            );
        }
        Ok(format!("http://{connect_host}:{listen_port}/v1"))
    }

    /// Grok Build live 是否具备可接管的自定义模型表。
    ///
    /// 官方态 live（Grok CLI 自带 OAuth 登录、无 `[model.*]` 表）没有注入
    /// 占位符的落点：Grok CLI 以「config 是否为空」区分官方 OAuth / 自定义
    /// 供应商两种模式，表达不出「官方 OAuth + 自定义 base_url」。官方供应商
    /// 的接管能力门见 `official_provider_supports_proxy_takeover`（按应用逐个
    /// 开，目前仅 Codex），调用方应跳过接管或直接报错。官方态的用量统计由
    /// `session_usage_grokbuild` 从会话日志导入，不依赖代理。
    fn grok_live_config_supports_takeover(config: &Value) -> bool {
        config
            .get("config")
            .and_then(Value::as_str)
            .and_then(crate::grok_config::extract_model_config)
            .is_some()
    }

    fn apply_grok_takeover_fields(config: &mut Value, proxy_base_url: &str) -> Result<(), String> {
        let config_toml = config
            .get("config")
            .and_then(Value::as_str)
            .ok_or_else(|| "Grok Build 配置缺少 config 字段".to_string())?;
        let updated = crate::grok_config::apply_proxy_takeover(
            config_toml,
            proxy_base_url,
            PROXY_TOKEN_PLACEHOLDER,
        )
        .map_err(|e| format!("更新 Grok Build 接管配置失败: {e}"))?;
        config["config"] = json!(updated);
        Ok(())
    }

    /// 接管各应用的 Live 配置（写入代理地址）
    ///
    /// 代理服务器的路由已经根据 API 端点自动区分应用类型：
    /// - `/v1/messages` → Claude
    /// - `/v1/chat/completions`, `/v1/responses` → Codex
    /// - `/v1beta/*` → Gemini
    ///
    /// 因此不需要在 URL 中添加应用前缀。
    async fn takeover_live_configs(&self) -> Result<(), String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;
        let proxy_grok_base_url = format!("{}/grokbuild/v1", proxy_url.trim_end_matches('/'));

        // Claude: 修改 ANTHROPIC_BASE_URL，使用占位符替代真实 Token（代理会注入真实 Token）
        if let Ok(mut live_config) = self.read_claude_live() {
            let claude_provider = self.require_current_provider_for_app(&AppType::Claude)?;
            let claude_provider = self.claude_provider_with_effective_settings(&claude_provider)?;
            Self::apply_claude_takeover_fields_for_provider(
                &mut live_config,
                &proxy_url,
                &claude_provider,
            );
            self.write_claude_live(&live_config)?;
            log::info!("Claude Live 配置已接管，代理地址: {proxy_url}");
        }

        // Codex: project the selected provider through the local Responses endpoint.
        if self.read_codex_live().is_ok() {
            let codex_provider = self.require_current_provider_for_app(&AppType::Codex)?;
            self.sync_codex_live_from_provider_while_proxy_active(&codex_provider)
                .await?;
            log::info!("Codex Live 配置已接管，代理地址: {proxy_codex_base_url}");
        }

        // Gemini: 修改 GOOGLE_GEMINI_BASE_URL，使用占位符替代真实 Token（代理会注入真实 Token）
        if let Ok(mut live_config) = self.read_gemini_live() {
            if let Some(env) = live_config.get_mut("env").and_then(|v| v.as_object_mut()) {
                env.insert("GOOGLE_GEMINI_BASE_URL".to_string(), json!(&proxy_url));
                // 使用占位符，避免显示缺少 key 的警告
                env.insert("GEMINI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
            } else {
                live_config["env"] = json!({
                    "GOOGLE_GEMINI_BASE_URL": &proxy_url,
                    "GEMINI_API_KEY": PROXY_TOKEN_PLACEHOLDER
                });
            }
            self.write_gemini_live(&live_config)?;
            log::info!("Gemini Live 配置已接管，代理地址: {proxy_url}");
        }

        // Grok Build: keep its own provider namespace while reusing Responses forwarding.
        if let Ok(mut live_config) = self.read_grok_live() {
            if Self::grok_live_config_supports_takeover(&live_config) {
                Self::apply_grok_takeover_fields(&mut live_config, &proxy_grok_base_url)?;
                self.write_grok_live(&live_config)?;
                log::info!("Grok Build Live 配置已接管，代理地址: {proxy_grok_base_url}");
            } else {
                log::info!("Grok Build Live 处于官方登录态（无自定义模型表），跳过代理接管");
            }
        }

        Ok(())
    }

    /// 接管指定应用的 Live 配置（严格模式：目标配置不存在则返回错误）
    async fn takeover_live_config_strict(&self, app_type: &AppType) -> Result<(), String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;
        self.takeover_live_config_strict_with_urls(app_type, &proxy_url, &proxy_codex_base_url)
    }

    fn takeover_live_config_strict_with_urls(
        &self,
        app_type: &AppType,
        proxy_url: &str,
        proxy_codex_base_url: &str,
    ) -> Result<(), String> {
        crate::app_management::require_managed(app_type).map_err(|e| e.to_string())?;
        #[cfg(test)]
        self.maybe_fail_takeover_write_for_test(app_type)?;

        let proxy_grok_base_url = format!("{}/grokbuild/v1", proxy_url.trim_end_matches('/'));

        match app_type {
            AppType::Claude => {
                let mut live_config = self.read_claude_live()?;
                let claude_provider = self.require_current_provider_for_app(&AppType::Claude)?;
                let claude_provider =
                    self.claude_provider_with_effective_settings(&claude_provider)?;
                Self::apply_claude_takeover_fields_for_provider(
                    &mut live_config,
                    proxy_url,
                    &claude_provider,
                );
                self.write_claude_live(&live_config)?;
                log::info!("Claude Live 配置已接管，代理地址: {proxy_url}");
            }
            AppType::Codex => {
                let mut live_config = self.read_codex_live()?;
                let codex_provider = self.require_current_provider_for_app(&AppType::Codex)?;
                Self::apply_codex_takeover_fields_for_provider(
                    &mut live_config,
                    proxy_codex_base_url,
                    &codex_provider,
                )?;

                self.write_codex_takeover_live_for_provider(&live_config, Some(&codex_provider))?;
                log::info!("Codex Live 配置已接管，代理地址: {proxy_codex_base_url}");
            }
            AppType::Gemini => {
                let mut live_config = self.read_gemini_live()?;

                if let Some(env) = live_config.get_mut("env").and_then(|v| v.as_object_mut()) {
                    env.insert("GOOGLE_GEMINI_BASE_URL".to_string(), json!(proxy_url));
                    env.insert("GEMINI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
                } else {
                    live_config["env"] = json!({
                        "GOOGLE_GEMINI_BASE_URL": proxy_url,
                        "GEMINI_API_KEY": PROXY_TOKEN_PLACEHOLDER
                    });
                }

                self.write_gemini_live(&live_config)?;
                log::info!("Gemini Live 配置已接管，代理地址: {proxy_url}");
            }
            AppType::GrokBuild => {
                let mut live_config = self.read_grok_live()?;
                if !Self::grok_live_config_supports_takeover(&live_config) {
                    return Err(
                        "Grok Build 当前为官方登录态（无自定义模型表），官方供应商不支持代理接管 \
                         (Grok Build is using the official login without a custom model table; \
                         official providers cannot be taken over by the proxy)"
                            .to_string(),
                    );
                }
                Self::apply_grok_takeover_fields(&mut live_config, &proxy_grok_base_url)?;
                self.write_grok_live(&live_config)?;
                log::info!("Grok Build Live 配置已接管，代理地址: {proxy_grok_base_url}");
            }
            _ => return Err("该应用不支持代理功能".to_string()),
        }

        Ok(())
    }

    async fn restore_live_config_for_app_inner(&self, app_type: &AppType) -> Result<(), String> {
        if !crate::app_management::is_managed(app_type) { log::info!("Skipped proxy recovery for {}: unmanaged", app_type.as_str()); return Ok(()); }
        match app_type {
            AppType::Claude => {
                if let Ok(Some(backup)) = self.db.get_live_backup("claude").await {
                    let config: Value = serde_json::from_str(&backup.original_config)
                        .map_err(|e| format!("解析 Claude 备份失败: {e}"))?;
                    self.write_claude_live(&config)?;
                    log::info!("Claude Live 配置已恢复");
                }
            }
            AppType::Codex => {
                if let Ok(Some(backup)) = self.db.get_live_backup("codex").await {
                    let config: Value = serde_json::from_str(&backup.original_config)
                        .map_err(|e| format!("解析 Codex 备份失败: {e}"))?;
                    self.write_codex_restore_backup(&config)?;
                    log::info!("Codex Live 配置已恢复");
                }
            }
            AppType::Gemini => {
                if let Ok(Some(backup)) = self.db.get_live_backup("gemini").await {
                    let config: Value = serde_json::from_str(&backup.original_config)
                        .map_err(|e| format!("解析 Gemini 备份失败: {e}"))?;
                    self.write_gemini_live(&config)?;
                    log::info!("Gemini Live 配置已恢复");
                }
            }
            AppType::GrokBuild => {
                if let Ok(Some(backup)) = self.db.get_live_backup("grokbuild").await {
                    let config: Value = serde_json::from_str(&backup.original_config)
                        .map_err(|e| format!("解析 Grok Build 备份失败: {e}"))?;
                    self.write_grok_live(&config)?;
                    log::info!("Grok Build Live 配置已恢复");
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// 恢复原始 Live 配置
    async fn restore_live_configs(&self) -> Result<(), String> {
        let _switch_guards = self.lock_all_takeover_apps().await;
        self.restore_live_configs_inner().await
    }

    /// Caller holds every supported app switch guard.
    async fn restore_live_configs_inner(&self) -> Result<(), String> {
        let mut errors = Vec::new();

        for app_type in [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::GrokBuild,
        ] {
            if let Err(e) = self
                .restore_live_config_for_app_with_fallback_inner(&app_type)
                .await
            {
                errors.push(e);
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("；"))
        }
    }

    #[cfg(test)]
    async fn restore_live_config_for_app_with_fallback(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        let _guard = self.switch_locks.lock_for_app(app_type.as_str()).await;
        self.restore_live_config_for_app_with_fallback_inner(app_type)
            .await
    }

    pub(crate) async fn restore_live_config_for_app_with_fallback_inner(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        let app_type_str = app_type.as_str();

        // 1) 优先从 Live 备份恢复（这是"原始 Live"的唯一可靠来源）
        let backup = self
            .db
            .get_live_backup(app_type_str)
            .await
            .map_err(|e| format!("获取 {app_type_str} Live 备份失败: {e}"))?;
        if let Some(backup) = backup {
            let config: Value = serde_json::from_str(&backup.original_config)
                .map_err(|e| format!("解析 {app_type_str} 备份失败: {e}"))?;

            // 备份若是代理占位符（异常历史：上次 stop 失败导致 Live 留在了代理状态，
            // 下次接管时又被错误地备份成"原始 Live"），不能直接用 — 否则 stop 后
            // Live 永远卡在 127.0.0.1:15721。落到下面的 SSOT 兜底重建。
            if Self::live_has_proxy_placeholder_for_app(app_type, &config) {
                log::warn!(
                    "{app_type_str} 备份本身已是代理占位符（异常历史状态），跳过备份，改走 SSOT 重建 Live"
                );
            } else {
                self.write_live_config_for_app(app_type, &config)?;
                log::info!("{app_type_str} Live 配置已从备份恢复");
                return Ok(());
            }
        }

        // 2) 兜底：备份缺失，但 Live 仍包含接管占位符（异常退出/历史 bug 场景）
        if !self.detect_takeover_in_live_config_for_app(app_type) {
            return Ok(());
        }

        // 2.1) 优先从 SSOT（当前供应商）重建 Live（比"清理字段"更可用）
        match self.restore_live_from_ssot_for_app(app_type) {
            Ok(true) => {
                log::info!("{app_type_str} Live 配置已从 SSOT 恢复（无备份兜底）");
                return Ok(());
            }
            Ok(false) => {
                log::warn!(
                    "{app_type_str} Live 备份缺失，且无法从 SSOT 恢复，将尝试清理接管占位符"
                );
            }
            Err(e) => {
                log::error!(
                    "{app_type_str} Live 备份缺失，SSOT 恢复失败，将尝试清理接管占位符: {e}"
                );
            }
        }

        // 2.2) 最后兜底：尽力清理占位符与本地代理地址，避免长期卡在代理占位符状态
        self.cleanup_takeover_placeholders_in_live_for_app(app_type)?;
        log::info!("{app_type_str} Live 接管占位符已清理（无备份兜底）");
        Ok(())
    }

    fn write_live_config_for_app(&self, app_type: &AppType, config: &Value) -> Result<(), String> {
        match app_type {
            AppType::Claude => self.write_claude_live(config),
            AppType::Codex => self.write_codex_restore_backup(config),
            AppType::Gemini => self.write_gemini_live(config),
            AppType::GrokBuild => self.write_grok_live(config),
            _ => Err("该应用不支持代理功能".to_string()),
        }
    }

    pub fn detect_takeover_in_live_config_for_app(&self, app_type: &AppType) -> bool {
        match app_type {
            AppType::Claude => match self.read_claude_live() {
                Ok(config) => Self::is_claude_live_taken_over(&config),
                Err(_) => false,
            },
            AppType::Codex => match self.read_codex_live() {
                Ok(config) => Self::is_codex_live_taken_over(&config),
                Err(_) => false,
            },
            AppType::Gemini => match self.read_gemini_live() {
                Ok(config) => Self::is_gemini_live_taken_over(&config),
                Err(_) => false,
            },
            AppType::GrokBuild => match self.read_grok_live() {
                Ok(config) => Self::is_grok_live_taken_over(&config),
                Err(_) => false,
            },
            _ => false,
        }
    }

    /// 当 Live 备份缺失时，尝试用 SSOT（当前供应商）写回 Live，以解除占位符接管。
    ///
    /// 返回值：
    /// - Ok(true)：已成功写回
    /// - Ok(false)：缺少当前供应商/供应商不存在/供应商本身含占位符，无法写回
    fn restore_live_from_ssot_for_app(&self, app_type: &AppType) -> Result<bool, String> {
        let current_id = crate::settings::get_effective_current_provider(&self.db, app_type)
            .map_err(|e| format!("获取 {app_type:?} 当前供应商失败: {e}"))?;

        let Some(current_id) = current_id else {
            return Ok(false);
        };

        let providers = self
            .db
            .get_all_providers(app_type.as_str())
            .map_err(|e| format!("读取 {app_type:?} 供应商列表失败: {e}"))?;

        let Some(provider) = providers.get(&current_id) else {
            return Ok(false);
        };

        // 供应商配置本身含接管占位符时不可写回（历史异常：接管期间 Live 被
        // 误导入成了供应商）。写回只会把占位符固化进 Live；返回 Ok(false)
        // 让调用方落到"清理占位符"兜底。
        if Self::live_has_proxy_placeholder_for_app(app_type, &provider.settings_config) {
            log::warn!(
                "{app_type:?} 当前供应商配置含代理接管占位符（疑似接管期间被导入的残留），跳过 SSOT 写回，改走占位符清理"
            );
            return Ok(false);
        }

        write_live_with_common_config_for_codex_oauth_manager(
            self.db.as_ref(),
            app_type,
            provider,
            &self.codex_oauth_manager,
        )
        .map_err(|e| format!("写入 {app_type:?} Live 配置失败: {e}"))?;

        Ok(true)
    }

    fn cleanup_takeover_placeholders_in_live_for_app(
        &self,
        app_type: &AppType,
    ) -> Result<(), String> {
        match app_type {
            AppType::Claude => self.cleanup_claude_takeover_placeholders_in_live(),
            AppType::Codex => self.cleanup_codex_takeover_placeholders_in_live(),
            AppType::Gemini => self.cleanup_gemini_takeover_placeholders_in_live(),
            AppType::GrokBuild => self.cleanup_grok_takeover_placeholders_in_live(),
            _ => Ok(()),
        }
    }

    fn is_local_proxy_url(url: &str) -> bool {
        let url = url.trim();
        if !url.starts_with("http://") {
            return false;
        }
        let rest = &url["http://".len()..];
        rest.starts_with("127.0.0.1")
            || rest.starts_with("localhost")
            || rest.starts_with("0.0.0.0")
            || rest.starts_with("[::1]")
            || rest.starts_with("[::]")
            || rest.starts_with("::1")
            || rest.starts_with("::")
    }

    fn proxy_urls_match(actual: &str, expected: &str) -> bool {
        actual.trim().trim_end_matches('/') == expected.trim().trim_end_matches('/')
    }

    fn codex_config_has_base_url_matching(
        config_text: &str,
        predicate: impl Fn(&str) -> bool,
    ) -> bool {
        let Ok(doc) = toml::from_str::<toml::Value>(config_text) else {
            return false;
        };

        let active_provider = doc
            .get("model_provider")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty());

        if let Some(provider_id) = active_provider {
            if doc
                .get("model_providers")
                .and_then(|value| value.get(provider_id))
                .and_then(|value| value.get("base_url"))
                .and_then(|value| value.as_str())
                .is_some_and(&predicate)
            {
                return true;
            }
        }

        doc.get("base_url")
            .and_then(|value| value.as_str())
            .is_some_and(predicate)
    }

    async fn live_takeover_matches_current_proxy(
        &self,
        app_type: &AppType,
    ) -> Result<bool, String> {
        let (proxy_url, proxy_codex_base_url) = self.build_proxy_urls().await?;
        let proxy_grok_base_url = format!("{}/grokbuild/v1", proxy_url.trim_end_matches('/'));

        match app_type {
            AppType::Claude => {
                let config = self.read_claude_live()?;
                let base_url_matches = config
                    .get("env")
                    .and_then(|value| value.get("ANTHROPIC_BASE_URL"))
                    .and_then(|value| value.as_str())
                    .is_some_and(|url| Self::proxy_urls_match(url, &proxy_url));
                Ok(Self::is_claude_live_taken_over(&config) && base_url_matches)
            }
            AppType::Codex => {
                let config = self.read_codex_live()?;
                let base_url_matches = config
                    .get("config")
                    .and_then(|value| value.as_str())
                    .is_some_and(|config_text| {
                        Self::codex_config_has_base_url_matching(config_text, |url| {
                            Self::proxy_urls_match(url, &proxy_codex_base_url)
                        })
                    });
                Ok(Self::is_codex_live_taken_over(&config) && base_url_matches)
            }
            AppType::Gemini => {
                let config = self.read_gemini_live()?;
                let base_url_matches = config
                    .get("env")
                    .and_then(|value| value.get("GOOGLE_GEMINI_BASE_URL"))
                    .and_then(|value| value.as_str())
                    .is_some_and(|url| Self::proxy_urls_match(url, &proxy_url));
                Ok(Self::is_gemini_live_taken_over(&config) && base_url_matches)
            }
            AppType::GrokBuild => {
                let config = self.read_grok_live()?;
                let base_url_matches =
                    config
                        .get("config")
                        .and_then(Value::as_str)
                        .is_some_and(|config_toml| {
                            crate::grok_config::base_url_matches(config_toml, |url| {
                                Self::proxy_urls_match(url, &proxy_grok_base_url)
                            })
                        });
                Ok(Self::is_grok_live_taken_over(&config) && base_url_matches)
            }
            _ => Ok(false),
        }
    }

    fn cleanup_claude_takeover_placeholders_in_live(&self) -> Result<(), String> {
        let mut config = self.read_claude_live()?;

        let Some(env) = config.get_mut("env").and_then(|v| v.as_object_mut()) else {
            return Ok(());
        };

        for key in [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ] {
            if env.get(key).and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER) {
                env.remove(key);
            }
        }

        if env
            .get("ANTHROPIC_BASE_URL")
            .and_then(|v| v.as_str())
            .map(Self::is_local_proxy_url)
            .unwrap_or(false)
        {
            env.remove("ANTHROPIC_BASE_URL");
        }

        self.write_claude_live(&config)?;
        Ok(())
    }

    fn cleanup_codex_takeover_placeholders_in_live(&self) -> Result<(), String> {
        let mut config = self.read_codex_live()?;

        if let Some(auth) = config.get_mut("auth").and_then(|v| v.as_object_mut()) {
            if auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER)
            {
                auth.remove("OPENAI_API_KEY");
            }
        }

        if let Some(cfg_str) = config.get("config").and_then(|v| v.as_str()) {
            let updated = Self::remove_local_toml_base_url(cfg_str);
            let updated =
                crate::codex_config::remove_codex_experimental_bearer_token_if(&updated, |token| {
                    token == PROXY_TOKEN_PLACEHOLDER
                })
                .map_err(|e| format!("清理 Codex 接管占位符失败: {e}"))?;
            let updated = crate::codex_config::remove_codex_official_proxy_route(&updated)
                .map_err(|e| format!("清理 Codex 官方接管路由失败: {e}"))?;
            config["config"] = json!(updated);
        }

        self.write_codex_live(&config)?;
        Ok(())
    }

    /// Remove local proxy base_url from TOML（委托给 codex_config 共享实现）
    fn remove_local_toml_base_url(toml_str: &str) -> String {
        crate::codex_config::remove_codex_toml_base_url_if(toml_str, Self::is_local_proxy_url)
    }

    fn cleanup_gemini_takeover_placeholders_in_live(&self) -> Result<(), String> {
        let mut config = self.read_gemini_live()?;

        let Some(env) = config.get_mut("env").and_then(|v| v.as_object_mut()) else {
            return Ok(());
        };

        if env.get("GEMINI_API_KEY").and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER) {
            env.remove("GEMINI_API_KEY");
        }

        if env
            .get("GOOGLE_GEMINI_BASE_URL")
            .and_then(|v| v.as_str())
            .map(Self::is_local_proxy_url)
            .unwrap_or(false)
        {
            env.remove("GOOGLE_GEMINI_BASE_URL");
        }

        self.write_gemini_live(&config)?;
        Ok(())
    }

    fn cleanup_grok_takeover_placeholders_in_live(&self) -> Result<(), String> {
        let config = self.read_grok_live()?;
        let Some(config_toml) = config.get("config").and_then(Value::as_str) else {
            return Ok(());
        };
        if !crate::grok_config::has_proxy_placeholder(config_toml, PROXY_TOKEN_PLACEHOLDER) {
            return Ok(());
        }

        // A valid provider snapshot should normally restore before this fallback.
        // Clearing the token prevents a stale local route from looking usable.
        let updated = crate::grok_config::update_api_key(config_toml, "")
            .map_err(|e| format!("清理 Grok Build 接管占位符失败: {e}"))?;
        crate::config::write_text_file(&crate::grok_config::get_grok_config_path(), &updated)
            .map_err(|e| format!("写入 Grok Build 配置失败: {e}"))
    }

    /// 检查是否处于 Live 接管模式
    pub async fn is_takeover_active(&self) -> Result<bool, String> {
        let status = self.get_takeover_status().await?;
        Ok(status.claude || status.codex || status.gemini || status.grokbuild)
    }

    /// 从异常退出中恢复（启动时调用）
    ///
    /// 检测到 Live 备份残留时调用此方法。
    /// 会恢复 Live 配置、清除接管标志、删除备份。
    pub async fn recover_from_crash(&self) -> Result<(), String> {
        let _transaction = self.lock_transaction().await;
        self.recover_from_crash_inner().await
    }

    /// Caller holds the proxy transaction guard.
    async fn recover_from_crash_inner(&self) -> Result<(), String> {
        // 1. 恢复 Live 配置
        self.restore_live_configs().await?;

        // 2. 清除接管标志
        self.db
            .set_live_takeover_active(false)
            .await
            .map_err(|e| format!("清除接管状态失败: {e}"))?;

        // 3. 删除备份
        self.db
            .delete_all_live_backups()
            .await
            .map_err(|e| format!("删除备份失败: {e}"))?;

        log::info!("已从异常退出中恢复 Live 配置");
        Ok(())
    }

    /// 检测 Live 配置是否处于"被接管"的残留状态
    ///
    /// 用于兜底处理：当数据库备份缺失但 Live 文件已经写成代理占位符时，
    /// 启动流程可以据此触发恢复逻辑。
    pub fn detect_takeover_in_live_configs(&self) -> bool {
        if let Ok(config) = self.read_claude_live() {
            if Self::is_claude_live_taken_over(&config) {
                return true;
            }
        }

        if let Ok(config) = self.read_codex_live() {
            if Self::is_codex_live_taken_over(&config) {
                return true;
            }
        }

        if let Ok(config) = self.read_gemini_live() {
            if Self::is_gemini_live_taken_over(&config) {
                return true;
            }
        }

        if let Ok(config) = self.read_grok_live() {
            if Self::is_grok_live_taken_over(&config) {
                return true;
            }
        }

        false
    }

    fn is_claude_live_taken_over(config: &Value) -> bool {
        let env = match config.get("env").and_then(|v| v.as_object()) {
            Some(env) => env,
            None => return false,
        };

        for key in [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ] {
            if env.get(key).and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER) {
                return true;
            }
        }

        false
    }

    fn codex_live_has_proxy_placeholder(config: &Value) -> bool {
        if config
            .get("auth")
            .and_then(|v| v.as_object())
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(|v| v.as_str())
            == Some(PROXY_TOKEN_PLACEHOLDER)
        {
            return true;
        }

        config
            .get("config")
            .and_then(|v| v.as_str())
            .and_then(crate::codex_config::extract_codex_experimental_bearer_token)
            .as_deref()
            == Some(PROXY_TOKEN_PLACEHOLDER)
    }

    fn is_codex_live_taken_over(config: &Value) -> bool {
        Self::codex_live_has_proxy_placeholder(config)
            || config
                .get("config")
                .and_then(|v| v.as_str())
                .is_some_and(crate::codex_config::codex_config_has_official_proxy_route)
    }

    fn is_gemini_live_taken_over(config: &Value) -> bool {
        let env = match config.get("env").and_then(|v| v.as_object()) {
            Some(env) => env,
            None => return false,
        };
        env.get("GEMINI_API_KEY").and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER)
    }

    fn is_grok_live_taken_over(config: &Value) -> bool {
        config
            .get("config")
            .and_then(Value::as_str)
            .is_some_and(|config_toml| {
                crate::grok_config::has_proxy_placeholder(config_toml, PROXY_TOKEN_PLACEHOLDER)
            })
    }

    /// 判断给定的 Live/备份配置是否已被代理接管（包含占位符）
    ///
    /// 用途：检测"备份里存的其实是代理配置"这种异常历史状态。
    /// 如果发现，备份不可信，备份路径不能写入（否则会把代理配置固化进备份槽），
    /// 恢复路径不能读取（否则会把代理占位符原样写回 Live，永久卡在代理地址）。
    /// 两种情况下都应该走 SSOT 兜底重建 Live。
    fn live_has_proxy_placeholder_for_app(app_type: &AppType, config: &Value) -> bool {
        match app_type {
            AppType::Claude => Self::is_claude_live_taken_over(config),
            AppType::Codex => Self::is_codex_live_taken_over(config),
            AppType::Gemini => Self::is_gemini_live_taken_over(config),
            AppType::GrokBuild => Self::is_grok_live_taken_over(config),
            _ => false,
        }
    }

    /// 从供应商配置更新 Live 备份（用于代理模式下的热切换）
    ///
    /// 与 backup_live_configs() 不同，此方法从供应商的 settings_config 生成备份，
    /// 而不是从 Live 文件读取（因为 Live 文件已被代理接管）。
    pub async fn update_live_backup_from_provider(
        &self,
        app_type: &str,
        provider: &Provider,
    ) -> Result<(), String> {
        let _transaction = self.lock_transaction().await;
        let _guard = self.switch_locks.lock_for_app(app_type).await;
        self.update_live_backup_from_provider_inner(app_type, provider, None)
            .await
    }

    /// 仅供已持有代理事务锁和 per-app 切换锁的调用方使用。
    pub(crate) async fn update_live_backup_from_provider_inner(
        &self,
        app_type: &str,
        provider: &Provider,
        clear_codex_auth_for_account: Option<&str>,
    ) -> Result<(), String> {
        let app_type_enum =
            AppType::from_str(app_type).map_err(|_| format!("未知的应用类型: {app_type}"))?;
        let mut effective_settings = if matches!(app_type_enum, AppType::Codex) {
            build_effective_provider_for_live_with_codex_oauth_manager(
                self.db.as_ref(),
                &app_type_enum,
                provider,
                &self.codex_oauth_manager,
            )
            .map_err(|e| format!("构建 {app_type} 有效配置失败: {e}"))?
            .settings_config
        } else {
            build_effective_settings_with_common_config(self.db.as_ref(), &app_type_enum, provider)
                .map_err(|e| format!("构建 {app_type} 有效配置失败: {e}"))?
        };

        if matches!(app_type_enum, AppType::Codex) {
            let is_codex_official = crate::proxy::providers::is_codex_official_provider(provider);
            let existing_backup_value = self
                .db
                .get_live_backup(app_type)
                .await
                .map_err(|e| format!("读取 {app_type} 现有备份失败: {e}"))?
                .map(|backup| {
                    serde_json::from_str::<Value>(&backup.original_config)
                        .map_err(|e| format!("解析 {app_type} 现有备份失败: {e}"))
                })
                .transpose()?;
            // A stale takeover marker can survive without its DB backup (for
            // example after a partial cleanup). In that abnormal state the live
            // auth file is still the only copy of the user's login, so use it as
            // the preservation source instead of replacing it with the empty
            // official seed snapshot.
            let existing_backup_value =
                existing_backup_value.or_else(|| self.read_codex_live().ok());

            if let Some(existing_value) = existing_backup_value.as_ref() {
                Self::preserve_toml_mcp_servers_from_existing_config(
                    &mut effective_settings,
                    existing_value,
                )?;
                if let Some(account_id) = clear_codex_auth_for_account {
                    Self::clear_codex_auth_in_backup(
                        &mut effective_settings,
                        existing_value,
                        account_id,
                    )?;
                } else if provider
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
                    .is_none()
                {
                    Self::preserve_codex_auth_in_backup(
                        &mut effective_settings,
                        existing_value,
                        is_codex_official,
                    )?;
                }
            }

            // 统一会话开关：备份是接管释放时恢复 live 的来源，官方配置的
            // 共享 custom 路由注入必须落在备份里，否则恢复后开关失效。
            crate::codex_config::apply_codex_unified_session_bucket_to_settings(
                if is_codex_official {
                    Some("official")
                } else {
                    provider.category.as_deref()
                },
                &mut effective_settings,
            )
            .map_err(|e| format!("注入统一会话路由失败: {e}"))?;

            if is_codex_official {
                // auth.json keeps rotating independently while takeover is
                // active. Persisting it in the DB backup would later roll a
                // valid R1 generation back to the frozen R0 generation.
                if let Some(root) = effective_settings.as_object_mut() {
                    root.remove("auth");
                }
            }
        }

        if matches!(app_type_enum, AppType::GrokBuild) {
            let existing_value = self
                .db
                .get_live_backup(app_type)
                .await
                .map_err(|e| format!("读取 {app_type} 现有备份失败: {e}"))?
                .map(|backup| {
                    serde_json::from_str::<Value>(&backup.original_config)
                        .map_err(|e| format!("解析 {app_type} 现有备份失败: {e}"))
                })
                .transpose()?
                .or_else(|| self.read_grok_live().ok());
            if let Some(existing_value) = existing_value.as_ref() {
                Self::preserve_toml_mcp_servers_from_existing_config(
                    &mut effective_settings,
                    existing_value,
                )?;
            }
        }

        let backup_json = match app_type_enum {
            AppType::Claude => serde_json::to_string(&effective_settings)
                .map_err(|e| format!("序列化 Claude 配置失败: {e}"))?,
            AppType::Codex => serde_json::to_string(&effective_settings)
                .map_err(|e| format!("序列化 Codex 配置失败: {e}"))?,
            AppType::GrokBuild => serde_json::to_string(&effective_settings)
                .map_err(|e| format!("序列化 Grok Build 配置失败: {e}"))?,
            AppType::Gemini => {
                // Gemini takeover 仅修改 .env；settings.json（含 mcpServers）保持原样。
                let env_backup = if let Some(env) = effective_settings.get("env") {
                    json!({ "env": env })
                } else {
                    json!({ "env": {} })
                };
                serde_json::to_string(&env_backup)
                    .map_err(|e| format!("序列化 Gemini 配置失败: {e}"))?
            }
            _ => return Err(format!("未知的应用类型: {app_type}")),
        };

        self.db
            .save_live_backup(app_type, &backup_json)
            .await
            .map_err(|e| format!("更新 {app_type} 备份失败: {e}"))?;

        log::info!("已更新 {app_type} Live 备份（热切换）");
        Ok(())
    }

    pub async fn hot_switch_provider(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<HotSwitchOutcome, String> {
        let _transaction = self.lock_transaction().await;
        let _guard = self.switch_locks.lock_for_app(app_type).await;
        self.hot_switch_provider_inner(app_type, provider_id).await
    }

    pub(crate) async fn hot_switch_provider_inner(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<HotSwitchOutcome, String> {
        let app_type_enum =
            AppType::from_str(app_type).map_err(|_| format!("无效的应用类型: {app_type}"))?;
        let provider = self
            .db
            .get_provider_by_id(provider_id, app_type)
            .map_err(|e| format!("读取供应商失败: {e}"))?
            .ok_or_else(|| format!("供应商不存在: {provider_id}"))?;

        // Defense-in-depth: only Codex official providers support native OpenAI
        // auth passthrough during takeover.
        if provider.category.as_deref() == Some("official")
            && !crate::services::provider::official_provider_supports_proxy_takeover(
                &app_type_enum,
                &provider,
            )
        {
            return Err(
                "代理接管模式下不能切换到官方供应商 (Cannot switch to official provider during proxy takeover)"
                    .to_string(),
            );
        }

        let previous_provider_id =
            crate::settings::get_effective_current_provider(&self.db, &app_type_enum)
                .map_err(|e| format!("读取当前供应商失败: {e}"))?;
        let previous_provider = previous_provider_id
            .as_deref()
            .map(|id| {
                self.db
                    .get_provider_by_id(id, app_type_enum.as_str())
                    .map_err(|error| format!("读取原供应商失败: {error}"))
            })
            .transpose()?
            .flatten();
        let previous_managed_codex_account_id = previous_provider
            .as_ref()
            .and_then(|provider| provider.meta.as_ref())
            .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
            .map(|account_id| account_id.trim().to_string())
            .filter(|account_id| !account_id.is_empty());
        let target_managed_codex_account_id = provider
            .meta
            .as_ref()
            .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
            .map(|account_id| account_id.trim().to_string())
            .filter(|account_id| !account_id.is_empty());
        let outgoing_managed_codex_account_id = previous_managed_codex_account_id
            .as_ref()
            .filter(|account_id| target_managed_codex_account_id.as_ref() != Some(*account_id))
            .cloned();
        let previous_local_provider_id = crate::settings::get_current_provider(&app_type_enum);
        let logical_target_changed = previous_provider_id.as_deref() != Some(provider_id);

        let has_backup = self
            .db
            .get_live_backup(app_type_enum.as_str())
            .await
            .map_err(|e| format!("读取 {app_type} 备份失败: {e}"))?
            .is_some();
        let live_taken_over = self.detect_takeover_in_live_config_for_app(&app_type_enum);
        let should_sync_backup = has_backup || live_taken_over;
        let outgoing_live_refresh_token =
            if should_sync_backup && matches!(app_type_enum, AppType::Codex) {
                match outgoing_managed_codex_account_id.as_deref() {
                    Some(account_id) => self
                        .codex_oauth_manager
                        .prepare_live_auth_for_account_switch_away(account_id)
                        .await
                        .map_err(|error| error.to_string())?,
                    None => None,
                }
            } else {
                None
            };

        // All fallible backup/live writes must finish before committing the logical
        // current provider. Otherwise a failed hot switch leaves the UI pointing at
        // the new provider while the proxy still serves the old one (and the next
        // query may fall back to the first provider).
        let previous_backup = if should_sync_backup {
            self.db
                .get_live_backup(app_type_enum.as_str())
                .await
                .map_err(|e| format!("读取 {app_type} 原备份失败: {e}"))?
        } else {
            None
        };
        let previous_codex_live_state =
            if should_sync_backup && matches!(app_type_enum, AppType::Codex) {
                Some(
                    crate::codex_config::CodexLiveStateSnapshot::capture()
                        .map_err(|error| format!("捕获 Codex 热切换前状态失败: {error}"))?,
                )
            } else {
                None
            };

        let prepare_result: Result<(), String> = async {
            if should_sync_backup {
                self.update_live_backup_from_provider_inner(
                    app_type,
                    &provider,
                    outgoing_managed_codex_account_id.as_deref(),
                )
                .await?;

                if matches!(app_type_enum, AppType::Claude) {
                    self.sync_claude_live_from_provider_while_proxy_active(&provider)
                        .await?;
                } else if live_taken_over && matches!(app_type_enum, AppType::Codex) {
                    self.sync_codex_live_from_provider_while_proxy_active_guarded(
                        &provider,
                        outgoing_managed_codex_account_id.as_deref(),
                        outgoing_live_refresh_token.as_deref(),
                    )
                    .await?;
                } else if live_taken_over && matches!(app_type_enum, AppType::GrokBuild) {
                    self.sync_grok_live_from_provider_while_proxy_active(&provider)
                        .await?;
                }
            }

            if has_backup && !live_taken_over && matches!(app_type_enum, AppType::Codex) {
                let effective_provider =
                    build_effective_provider_for_live_with_codex_oauth_manager(
                        self.db.as_ref(),
                        &AppType::Codex,
                        &provider,
                        &self.codex_oauth_manager,
                    )
                    .map_err(|e| format!("构建 Codex 有效配置失败: {e}"))?;
                let effective_settings = &effective_provider.settings_config;
                let auth = effective_settings
                    .get("auth")
                    .ok_or_else(|| "Codex 供应商缺少 auth 配置".to_string())?;
                let config_str = effective_settings.get("config").and_then(|v| v.as_str());
                let profile = crate::proxy::providers::resolve_codex_catalog_tool_profile(
                    &effective_provider,
                );

                if let (Some(account_id), Some(expected_refresh_token)) = (
                    outgoing_managed_codex_account_id.as_deref(),
                    outgoing_live_refresh_token.as_deref(),
                ) {
                    crate::codex_config::ensure_codex_live_auth_unchanged_for_managed_account(
                        account_id,
                        expected_refresh_token,
                    )
                    .map_err(|error| error.to_string())?;
                }

                crate::codex_config::write_codex_provider_live_with_catalog(
                    effective_settings,
                    effective_provider.category.as_deref(),
                    auth,
                    config_str,
                    profile,
                )
                .map_err(|e| format!("写入 Codex 配置失败: {e}"))?;
                if let Some(account_id) = target_managed_codex_account_id.as_deref() {
                    crate::codex_config::record_codex_managed_oauth_live_auth(auth, account_id)
                        .map_err(|error| format!("记录 Codex 托管认证标记失败: {error}"))?;
                }
            }

            if should_sync_backup && matches!(app_type_enum, AppType::Codex) {
                if let Some(account_id) = outgoing_managed_codex_account_id.as_deref() {
                    if let Some(expected_refresh_token) = outgoing_live_refresh_token.as_deref() {
                        crate::codex_config::clear_codex_live_auth_for_managed_account_if_unchanged(
                            account_id,
                            Some(expected_refresh_token),
                        )
                        .map_err(|error| error.to_string())?;
                    } else {
                        crate::codex_config::clear_codex_live_auth_for_managed_account(account_id)
                            .map_err(|error| error.to_string())?;
                    }
                }
            }

            Ok(())
        }
        .await;

        if let Err(error) = prepare_result {
            self.rollback_hot_switch_preparation(
                &app_type_enum,
                previous_backup.as_ref(),
                previous_provider_id.as_deref(),
                should_sync_backup,
                live_taken_over,
                previous_codex_live_state.as_ref(),
            )
            .await;
            return Err(error);
        }

        if let Err(error) = crate::settings::set_current_provider(&app_type_enum, Some(provider_id))
        {
            self.rollback_hot_switch_preparation(
                &app_type_enum,
                previous_backup.as_ref(),
                previous_provider_id.as_deref(),
                should_sync_backup,
                live_taken_over,
                previous_codex_live_state.as_ref(),
            )
            .await;
            return Err(format!("更新本地当前供应商失败: {error}"));
        }
        if let Err(error) = self
            .db
            .set_current_provider(app_type_enum.as_str(), provider_id)
        {
            if let Err(rollback_error) = crate::settings::set_current_provider(
                &app_type_enum,
                previous_local_provider_id.as_deref(),
            ) {
                log::error!("数据库切换失败后恢复本地当前供应商失败: {rollback_error}");
            }
            self.rollback_hot_switch_preparation(
                &app_type_enum,
                previous_backup.as_ref(),
                previous_provider_id.as_deref(),
                should_sync_backup,
                live_taken_over,
                previous_codex_live_state.as_ref(),
            )
            .await;
            return Err(format!("更新当前供应商失败: {error}"));
        }

        if let Some(server) = self.server.read().await.as_ref() {
            server
                .set_active_target(app_type_enum.as_str(), &provider.id, &provider.name)
                .await;
        }

        Ok(HotSwitchOutcome {
            logical_target_changed,
        })
    }

    #[cfg(test)]
    async fn lock_switch_for_test(&self, app_type: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.switch_locks.lock_for_app(app_type).await
    }

    fn preserve_toml_mcp_servers_from_existing_config(
        target_settings: &mut Value,
        existing_config: &Value,
    ) -> Result<(), String> {
        let target_obj = target_settings
            .as_object_mut()
            .ok_or_else(|| "TOML 应用备份必须是 JSON 对象".to_string())?;

        let target_config = target_obj
            .get("config")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut target_doc = if target_config.trim().is_empty() {
            toml_edit::DocumentMut::new()
        } else {
            target_config
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| format!("解析新的 config.toml 失败: {e}"))?
        };

        let existing_config = existing_config
            .get("config")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if existing_config.trim().is_empty() {
            target_obj.insert("config".to_string(), json!(target_doc.to_string()));
            return Ok(());
        }

        let existing_doc = existing_config
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("解析现有 config.toml 备份失败: {e}"))?;

        if let Some(existing_mcp_servers) = existing_doc.get("mcp_servers") {
            match target_doc.get_mut("mcp_servers") {
                Some(target_mcp_servers) => {
                    if let (Some(target_table), Some(existing_table)) = (
                        target_mcp_servers.as_table_like_mut(),
                        existing_mcp_servers.as_table_like(),
                    ) {
                        for (server_id, server_item) in existing_table.iter() {
                            if target_table.get(server_id).is_none() {
                                target_table.insert(server_id, server_item.clone());
                            }
                        }
                    } else {
                        log::warn!(
                            "config.toml contains a non-table mcp_servers section; skipping MCP merge"
                        );
                    }
                }
                None => {
                    target_doc["mcp_servers"] = existing_mcp_servers.clone();
                }
            }
        }

        target_obj.insert("config".to_string(), json!(target_doc.to_string()));
        Ok(())
    }

    fn clear_codex_auth_in_backup(
        target_settings: &mut Value,
        existing_backup: &Value,
        account_id: &str,
    ) -> Result<(), String> {
        let Some(existing_auth) = existing_backup.get("auth") else {
            return Ok(());
        };
        let Some(target_obj) = target_settings.as_object_mut() else {
            return Err("Codex 备份必须是 JSON 对象".to_string());
        };

        // Access and refresh tokens rotate independently while Codex is running,
        // so backup stripping uses the stable local-account marker plus workspace
        // ID rather than a token fingerprint.
        if crate::codex_config::codex_live_auth_is_managed_chatgpt_login(existing_auth, account_id)
        {
            // Do not copy the outgoing managed bundle over the target. Keep the
            // target provider's own auth material intact (important for a
            // managed -> third-party hot switch); unbound official targets are
            // already empty and official backups are stripped below.
            return Ok(());
        }

        target_obj.insert("auth".to_string(), existing_auth.clone());
        Ok(())
    }

    fn preserve_codex_auth_in_backup(
        target_settings: &mut Value,
        existing_backup: &Value,
        preserve_api_key: bool,
    ) -> Result<(), String> {
        let Some(existing_auth) = existing_backup
            .get("auth")
            .filter(|auth| {
                !Self::codex_auth_has_proxy_placeholder(auth)
                    && (crate::codex_config::codex_auth_has_oauth_login_material(auth)
                        || (preserve_api_key
                            && crate::codex_config::codex_auth_has_login_material(auth)))
            })
            .cloned()
        else {
            return Ok(());
        };

        let Some(target_obj) = target_settings.as_object_mut() else {
            return Ok(());
        };

        let provider_auth = target_obj.get("auth").cloned().unwrap_or_else(|| json!({}));
        if let Some(config_text) = target_obj.get("config").and_then(|value| value.as_str()) {
            let live_config = crate::codex_config::prepare_codex_provider_live_config(
                &provider_auth,
                config_text,
            )
            .map_err(|e| format!("更新 Codex 备份配置失败: {e}"))?;
            target_obj.insert("config".to_string(), json!(live_config));
        }
        target_obj.insert("auth".to_string(), existing_auth);

        Ok(())
    }

    /// 恢复备份到 Codex live 时绝不覆盖官方 ChatGPT 登录。
    ///
    /// 备份是接管开启时的快照，而 live 的 `auth.json` 可能被 Codex
    /// （登录 / token 自刷新）或接管中的托管账号切换更新——所以只要 live
    /// 持有真实登录凭据（非接管占位符），它就比快照新，必须获胜：登录发生在接管期间时备份
    /// 里是第三方 API key（#6277 的循环覆盖链），把它降级进 config 的
    /// `experimental_bearer_token`；备份也是官方形态时它的 tokens 只是旧
    /// 副本，同样不写回。摘掉备份 auth 槽后 `write_codex_live_verbatim`
    /// 只写 config.toml，live 登录零接触。判定用
    /// `codex_auth_has_credential_login_material`——`last_refresh` /
    /// `tokens.account_id` 等元数据残留不算登录，既不让 sk-+元数据形态的
    /// 备份逃过降级，也不把元数据残留的 live 误当官方登录。与
    /// `auth.json` 独立读取，避免损坏的 config.toml 掩盖有效登录。官方
    /// provider 缺失 auth 文件时也必须保留，因为它表示接管期间主动登出；
    /// 当前 provider 无法归类时沿用备份侧的保守策略，不回放未知来源的旧凭据。
    /// 与 `preserve_codex_auth_in_backup` 语义对称（那边保护备份方向），同样
    /// 不受"非接管切换保留官方登录"设置门控（接管子系统的既有不变量是
    /// 无条件不清官方登录）。
    fn preserve_codex_oauth_login_on_restore(
        &self,
        target: &mut Value,
    ) -> Result<CodexAuthFileSnapshot, String> {
        let auth_snapshot = CodexAuthFileSnapshot::capture()?;
        let live_auth = auth_snapshot.value()?;
        let live_has_login = live_auth.as_ref().is_some_and(|auth| {
            !Self::codex_auth_has_proxy_placeholder(auth)
                && crate::codex_config::codex_auth_has_credential_login_material(auth)
        });
        let Some(target_obj) = target.as_object_mut() else {
            return Ok(auth_snapshot);
        };

        if live_has_login {
            if let Some(config_text) = target_obj
                .get("config")
                .and_then(|value| value.as_str())
                .map(str::to_string)
            {
                let provider_auth = target_obj.get("auth").cloned().unwrap_or_else(|| json!({}));
                match crate::codex_config::prepare_codex_provider_live_config(
                    &provider_auth,
                    &config_text,
                ) {
                    Ok(live_config) => {
                        target_obj.insert("config".to_string(), json!(live_config));
                    }
                    Err(e) => {
                        // 降级失败时仍优先保住登录：API key 可从 DB 供应商配置随时
                        // 重新落盘，OAuth 登录被覆盖则只能重新登录。
                        log::warn!(
                            "Codex 恢复：备份 API key 降级进 config 失败，仅跳过 auth 覆盖: {e}"
                        );
                    }
                }
            }
            target_obj.remove("auth");
            log::info!(
                "Codex 恢复：live 持有官方 ChatGPT 登录凭据（恒比备份快照新），保留登录，仅恢复 config"
            );
            return Ok(auth_snapshot);
        }

        let missing_preservable_auth =
            live_auth.is_none() && self.should_preserve_current_codex_auth()?;
        if missing_preservable_auth {
            target_obj.remove("auth");
            log::info!("Codex 恢复：保留当前缺失的 auth 状态，仅恢复 config");
        }
        Ok(auth_snapshot)
    }

    /// 代理模式下切换供应商（热切换，并按需刷新代理安全的 Live 显示字段）
    pub async fn switch_proxy_target(
        &self,
        app_type: &str,
        provider_id: &str,
    ) -> Result<(), String> {
        let app = AppType::from_str(app_type).map_err(|e| format!("无效的应用类型: {e}"))?;
        if !app.supports_local_proxy() {
            return Err(format!("{} 不支持本地路由", app.as_str()));
        }
        let outcome = self.hot_switch_provider(app_type, provider_id).await?;

        if outcome.logical_target_changed {
            log::info!("代理模式：已切换 {app_type} 的目标供应商为 {provider_id}");
        } else {
            log::debug!("代理模式：{app_type} 已对齐到目标供应商 {provider_id}");
        }
        Ok(())
    }

    // ==================== Live 配置读写辅助方法 ====================

    /// 接管 Codex 时，本地客户端必须继续以 Responses wire API 访问代理。
    /// 真实上游是否走 Chat Completions 由 provider 配置决定，并在代理内部转换。
    fn apply_codex_proxy_toml_config_for_provider(
        toml_str: &str,
        proxy_url: &str,
        provider: Option<&Provider>,
    ) -> Result<String, String> {
        if provider.is_some_and(crate::proxy::providers::is_codex_official_provider) {
            return crate::codex_config::apply_codex_official_proxy_route(toml_str, proxy_url)
                .map_err(|e| format!("生成 Codex 官方接管配置失败: {e}"));
        }

        let updated = crate::codex_config::update_codex_toml_field(toml_str, "base_url", proxy_url)
            .map_err(|e| format!("更新 Codex 代理地址失败: {e}"))?;
        let mut updated =
            crate::codex_config::update_codex_toml_field(&updated, "wire_api", "responses")
                .map_err(|e| format!("更新 Codex wire_api 失败: {e}"))?;

        if let Some(upstream_model) =
            provider.and_then(crate::proxy::providers::codex_provider_upstream_model)
        {
            updated =
                crate::codex_config::update_codex_toml_field(&updated, "model", &upstream_model)
                    .map_err(|e| format!("更新 Codex 上游模型失败: {e}"))?;
        }

        Ok(updated)
    }

    fn apply_codex_takeover_auth_placeholder(settings: &mut Value, provider: Option<&Provider>) {
        if provider.is_some_and(crate::proxy::providers::is_codex_official_provider) {
            return;
        }

        if let Some(auth) = settings.get_mut("auth").and_then(|v| v.as_object_mut()) {
            auth.insert("OPENAI_API_KEY".to_string(), json!(PROXY_TOKEN_PLACEHOLDER));
        } else if let Some(root) = settings.as_object_mut() {
            root.insert(
                "auth".to_string(),
                json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
            );
        }
    }

    fn apply_codex_takeover_fields_for_provider(
        settings: &mut Value,
        proxy_base_url: &str,
        provider: &Provider,
    ) -> Result<(), String> {
        Self::apply_codex_takeover_auth_placeholder(settings, Some(provider));
        let config_text = settings
            .get("config")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let projected = Self::apply_codex_proxy_toml_config_for_provider(
            &config_text,
            proxy_base_url,
            Some(provider),
        )?;
        settings["config"] = json!(projected);
        Self::attach_codex_model_catalog_from_provider(settings, Some(provider));
        Ok(())
    }

    fn attach_codex_model_catalog_from_provider(
        live_config: &mut Value,
        provider: Option<&Provider>,
    ) {
        let Some(provider) = provider else {
            return;
        };

        let model_catalog = provider
            .settings_config
            .get("modelCatalog")
            .cloned()
            .unwrap_or_else(|| json!({ "models": [] }));

        if let Some(root) = live_config.as_object_mut() {
            root.insert("modelCatalog".to_string(), model_catalog);
        }
    }

    fn read_claude_live(&self) -> Result<Value, String> {
        let path = get_claude_settings_path();
        if !path.exists() {
            return Err("Claude 配置文件不存在".to_string());
        }

        let mut value: Value =
            read_json_file(&path).map_err(|e| format!("读取 Claude 配置失败: {e}"))?;

        if value.is_null() {
            value = json!({});
        }

        if !value.is_object() {
            let kind = match &value {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
            };
            return Err(format!(
                "Claude 配置文件格式错误：根节点必须是 JSON 对象（当前为 {kind}），路径: {}",
                path.display()
            ));
        }

        Ok(value)
    }

    fn write_claude_live(&self, config: &Value) -> Result<(), String> {
        let path = get_claude_settings_path();
        let settings = crate::services::provider::sanitize_claude_settings_for_live(config);
        write_json_file(&path, &settings).map_err(|e| format!("写入 Claude 配置失败: {e}"))
    }

    fn read_codex_live(&self) -> Result<Value, String> {
        crate::codex_config::read_codex_live_settings()
            .map_err(|e| format!("读取 Codex Live 配置失败: {e}"))
    }

    fn write_codex_live(&self, config: &Value) -> Result<(), String> {
        self.write_codex_live_verbatim(config)
    }

    fn write_codex_restore_backup(&self, config: &Value) -> Result<(), String> {
        let mut config = config.clone();
        let auth_snapshot = self.preserve_codex_oauth_login_on_restore(&mut config)?;
        self.write_codex_live_verbatim_with_auth_guard(&config, Some(&auth_snapshot))
    }

    fn write_codex_live_for_provider(
        &self,
        config: &Value,
        provider: Option<&Provider>,
    ) -> Result<(), String> {
        let Some(provider) = provider else {
            if crate::settings::preserve_codex_official_auth_on_switch() {
                if let (Some(auth), Some(config_str)) = (
                    config.get("auth"),
                    config.get("config").and_then(|v| v.as_str()),
                ) {
                    if auth.get("OPENAI_API_KEY").and_then(|v| v.as_str())
                        == Some(PROXY_TOKEN_PLACEHOLDER)
                    {
                        let live_config = crate::codex_config::prepare_codex_provider_live_config(
                            auth, config_str,
                        )
                        .map_err(|e| format!("写入 Codex 配置失败: {e}"))?;
                        crate::codex_config::write_codex_live_config_atomic(Some(&live_config))
                            .map_err(|e| format!("写入 Codex 配置失败: {e}"))?;
                        return Ok(());
                    }
                }
            }

            return self.write_codex_live_verbatim(config);
        };

        let auth = config
            .get("auth")
            .ok_or_else(|| "Codex 配置缺少 auth 字段".to_string())?;
        let config_str = config.get("config").and_then(|v| v.as_str());
        let profile = crate::proxy::providers::resolve_codex_catalog_tool_profile(provider);

        crate::codex_config::write_codex_provider_live_with_catalog(
            config,
            provider.category.as_deref(),
            auth,
            config_str,
            profile,
        )
        .map_err(|e| format!("写入 Codex 配置失败: {e}"))
    }

    fn codex_auth_has_proxy_placeholder(auth: &Value) -> bool {
        auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(PROXY_TOKEN_PLACEHOLDER)
    }

    fn write_codex_takeover_live_for_provider(
        &self,
        config: &Value,
        provider: Option<&Provider>,
    ) -> Result<(), String> {
        let official_passthrough =
            provider.is_some_and(crate::proxy::providers::is_codex_official_provider);
        let managed_account_id = provider
            .and_then(|provider| provider.meta.as_ref())
            .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
            .filter(|account_id| !account_id.trim().is_empty());
        let managed_official = official_passthrough && managed_account_id.is_some();
        let placeholder_auth = config
            .get("auth")
            .is_some_and(Self::codex_auth_has_proxy_placeholder);

        // Takeover must never overwrite Codex's long-lived ChatGPT login. For
        // third-party providers the placeholder is moved into config.toml; for
        // codex-official no placeholder is needed because requires_openai_auth
        // makes Codex supply its native authorization.
        if official_passthrough || placeholder_auth {
            let config_str = config.get("config").and_then(|v| v.as_str()).unwrap_or("");
            let profile = provider
                .map(crate::proxy::providers::resolve_codex_catalog_tool_profile)
                .unwrap_or(crate::codex_config::CodexCatalogToolProfile::ProxyChat);
            let prepared_config =
                crate::codex_config::prepare_codex_live_config_text_with_optional_catalog(
                    config, config_str, profile,
                )
                .map_err(|e| format!("写入 Codex 配置失败: {e}"))?;
            if managed_official {
                let auth = config
                    .get("auth")
                    .ok_or_else(|| "Codex 托管官方配置缺少 auth 字段".to_string())?;
                // An explicitly managed official account is different from the
                // unbound native-login passthrough: the selected account owns
                // auth.json and must replace any previously active account.
                crate::codex_config::write_codex_live_for_provider(
                    Some("official"),
                    auth,
                    Some(&prepared_config),
                )
                .map_err(|e| format!("写入 Codex 配置失败: {e}"))?;
                crate::codex_config::record_codex_managed_oauth_live_auth(
                    auth,
                    managed_account_id
                        .as_deref()
                        .expect("managed official account checked"),
                )
                .map_err(|e| format!("记录 Codex 托管认证标记失败: {e}"))?;
                return Ok(());
            }
            let live_config = if official_passthrough {
                prepared_config
            } else {
                crate::codex_config::prepare_codex_provider_live_config(
                    config.get("auth").unwrap_or(&Value::Null),
                    &prepared_config,
                )
                .map_err(|e| format!("写入 Codex 配置失败: {e}"))?
            };
            crate::codex_config::write_codex_live_config_atomic(Some(&live_config))
                .map_err(|e| format!("写入 Codex 配置失败: {e}"))?;
            return Ok(());
        }

        self.write_codex_live_for_provider(config, provider)
    }

    fn write_codex_live_verbatim(&self, config: &Value) -> Result<(), String> {
        self.write_codex_live_verbatim_with_auth_guard(config, None)
    }

    fn write_codex_live_verbatim_with_auth_guard(
        &self,
        config: &Value,
        expected_auth: Option<&CodexAuthFileSnapshot>,
    ) -> Result<(), String> {
        use crate::codex_config::{get_codex_auth_path, get_codex_config_path};

        let auth = config.get("auth");
        let config_str = config.get("config").and_then(|v| v.as_str());

        let catalog_snapshot = if expected_auth.is_some()
            && config_str.is_some()
            && config.get("modelCatalog").is_some()
        {
            Some(
                crate::codex_config::CodexModelCatalogFileSnapshot::capture()
                    .map_err(|e| format!("捕获 Codex 模型目录失败: {e}"))?,
            )
        } else {
            None
        };

        // Decide the config.toml text ONCE, before splitting on auth. A stored
        // Codex backup comes in two shapes needing opposite handling:
        //  - snapshot backup (`read_codex_live_settings`): no inline `modelCatalog`;
        //    the config text already carries the live `model_catalog_json` pointer
        //    → keep raw, or projection would strip it.
        //  - provider-rebuilt backup (`update_live_backup_from_provider`): inline
        //    `modelCatalog` (DB SSOT) with a pointer-less config text → project,
        //    or the mapping is lost on restore.
        // The projection decision is orthogonal to auth: a provider-rebuilt backup
        // can pair an inline `modelCatalog` with empty/absent `auth.json` (the key
        // living in the config's `experimental_bearer_token`). Computing it up here
        // keeps every config-writing branch — write-auth, delete-auth, no-auth —
        // consistent instead of letting the empty-auth path skip projection.
        // Verbatim restore has no Provider in hand (we only have the stored
        // backup config), so the catalog tool profile can't be recovered here.
        // Default to ProxyChat: a restored native-direct backup keeps its inline
        // modelCatalog but would not get apply_patch re-stripped until the next
        // provider switch rewrites it via write_live_snapshot. Acceptable known
        // limitation (restore-of-deleted-provider-backup only).
        let prepared_cfg_result = config_str
            .map(|cfg| {
                crate::codex_config::prepare_codex_live_config_text_with_optional_catalog(
                    config,
                    cfg,
                    crate::codex_config::CodexCatalogToolProfile::ProxyChat,
                )
            })
            .transpose()
            .map_err(|e| format!("写入 Codex 配置失败: {e}"));
        let prepared_cfg = match prepared_cfg_result {
            Ok(prepared_cfg) => prepared_cfg,
            Err(error) => {
                if let Some(snapshot) = catalog_snapshot.as_ref() {
                    snapshot.restore().map_err(|rollback_error| {
                        format!("{error}; 回滚 Codex 模型目录失败: {rollback_error}")
                    })?;
                }
                return Err(error);
            }
        };

        let write_result = if let (Some(expected_auth), Some(auth)) = (expected_auth, auth) {
            (|| -> Result<(), String> {
                let replacement = if auth.as_object().is_some_and(Map::is_empty) {
                    None
                } else {
                    Some(
                        serde_json::to_vec_pretty(auth)
                            .map_err(|error| format!("序列化 Codex auth 失败: {error}"))?,
                    )
                };
                let mut transaction = CodexAuthFileTransaction::begin(expected_auth)?;
                if let Err(error) = transaction.install(replacement) {
                    return match transaction.rollback() {
                        Ok(()) => Err(error),
                        Err(rollback_error) => {
                            Err(format!("{error}; 回滚 Codex auth 失败: {rollback_error}"))
                        }
                    };
                }

                let config_result = prepared_cfg.as_deref().map_or(Ok(()), |cfg| {
                    crate::config::write_text_file(&get_codex_config_path(), cfg)
                        .map_err(|error| format!("写入 Codex config 失败: {error}"))
                });
                match config_result {
                    Ok(()) => transaction.commit(),
                    Err(error) => match transaction.rollback() {
                        Ok(()) => Err(error),
                        Err(rollback_error) => {
                            Err(format!("{error}; 回滚 Codex auth 失败: {rollback_error}"))
                        }
                    },
                }
            })()
        } else {
            match (auth, prepared_cfg.as_deref()) {
                (Some(auth), Some(cfg)) => {
                    if auth.as_object().is_some_and(Map::is_empty) {
                        // Unguarded provider writes preserve an existing login;
                        // only restore transactions interpret empty auth as an
                        // exact-generation deletion.
                        crate::config::write_text_file(&get_codex_config_path(), cfg)
                            .map_err(|e| format!("写入 Codex config 失败: {e}"))
                    } else {
                        crate::codex_config::write_codex_live_atomic(auth, Some(cfg))
                            .map_err(|e| format!("写入 Codex 配置失败: {e}"))
                    }
                }
                (Some(auth), None) => {
                    if auth.as_object().is_some_and(Map::is_empty) {
                        Ok(())
                    } else {
                        write_json_file(&get_codex_auth_path(), auth)
                            .map_err(|e| format!("写入 Codex auth 失败: {e}"))
                    }
                }
                (None, Some(cfg)) => crate::config::write_text_file(&get_codex_config_path(), cfg)
                    .map_err(|e| format!("写入 Codex config 失败: {e}")),
                (None, None) => Ok(()),
            }
        };

        if let Err(error) = write_result {
            if let Some(snapshot) = catalog_snapshot.as_ref() {
                snapshot.restore().map_err(|rollback_error| {
                    format!("{error}; 回滚 Codex 模型目录失败: {rollback_error}")
                })?;
            }
            return Err(error);
        }

        Ok(())
    }

    fn read_gemini_live(&self) -> Result<Value, String> {
        use crate::gemini_config::{env_to_json, get_gemini_env_path, read_gemini_env};

        let env_path = get_gemini_env_path();
        if !env_path.exists() {
            return Err("Gemini .env 文件不存在".to_string());
        }

        let env_map = read_gemini_env().map_err(|e| format!("读取 Gemini env 失败: {e}"))?;
        Ok(env_to_json(&env_map))
    }

    fn write_gemini_live(&self, config: &Value) -> Result<(), String> {
        use crate::gemini_config::{json_to_env, write_gemini_env_atomic};

        let env_map = json_to_env(config).map_err(|e| format!("转换 Gemini 配置失败: {e}"))?;
        write_gemini_env_atomic(&env_map).map_err(|e| format!("写入 Gemini env 失败: {e}"))?;
        Ok(())
    }

    fn read_grok_live(&self) -> Result<Value, String> {
        crate::grok_config::read_grok_live_settings()
            .map_err(|e| format!("读取 Grok Build 配置失败: {e}"))
    }

    fn write_grok_live(&self, config: &Value) -> Result<(), String> {
        crate::grok_config::write_grok_live_settings(config)
            .map_err(|e| format!("写入 Grok Build 配置失败: {e}"))
    }

    // ==================== 原有方法 ====================

    /// 获取服务器状态
    pub async fn get_status(&self) -> Result<ProxyStatus, String> {
        if let Some(server) = self.server.read().await.as_ref() {
            Ok(server.get_status().await)
        } else {
            // 服务器未运行时返回默认状态
            Ok(ProxyStatus {
                running: false,
                ..Default::default()
            })
        }
    }

    /// 获取代理配置
    pub async fn get_config(&self) -> Result<ProxyConfig, String> {
        self.db
            .get_proxy_config()
            .await
            .map_err(|e| format!("获取代理配置失败: {e}"))
    }

    /// 更新代理配置
    pub async fn update_config(&self, config: &ProxyConfig) -> Result<(), String> {
        let _transaction = self.lock_transaction().await;
        self.update_config_inner(config).await
    }

    /// Caller holds the proxy transaction guard.
    pub(crate) async fn update_config_inner(&self, config: &ProxyConfig) -> Result<(), String> {
        // Keep the global lock order aligned with set_takeover_for_app:
        // per-app switch guards first, then the lifecycle transition.
        let _switch_guards = self.lock_all_takeover_apps().await;
        let _lifecycle_guard = self.lifecycle.lock().await;

        // 记录旧配置用于判定是否需要重启
        let previous = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| format!("获取代理配置失败: {e}"))?;

        // 保存到数据库（保持 live_takeover_active 状态不变）
        let mut new_config = config.clone();
        new_config.live_takeover_active = previous.live_takeover_active;

        self.db
            .update_proxy_config(new_config.clone())
            .await
            .map_err(|e| format!("保存代理配置失败: {e}"))?;

        // 检查服务器当前状态
        let mut server_guard = self.server.write().await;
        if server_guard.is_none() {
            return Ok(());
        }

        let mut reuse_state = server_guard
            .as_ref()
            .expect("server presence checked")
            .reuse_state()
            .await;
        if reuse_state == ProxyServerReuseState::Stopped {
            server_guard.take();
            return Ok(());
        }

        // Compare the desired bind against the live listener. The persisted
        // previous row may already contain the desired value after a cancelled
        // update, while the listener still owns the older address/port.
        let mut require_restart = reuse_state == ProxyServerReuseState::NeedsReap
            || !server_guard
                .as_ref()
                .expect("server presence checked")
                .bind_config_matches(&new_config)
                .await;

        if !require_restart {
            let server = server_guard.as_ref().expect("server presence checked");
            server.apply_runtime_config(&new_config).await;
            require_restart = !server.is_running().await;
            if require_restart {
                reuse_state = ProxyServerReuseState::NeedsReap;
                log::warn!("代理任务在应用运行时配置期间结束，改为回收并重启");
            } else {
                log::info!("代理配置已实时应用，无需重启代理服务器");
            }
        }

        if require_restart {
            let app_handle = self.app_handle.read().await.clone();
            if let Some(server) = server_guard.as_ref() {
                let stop_result = server
                    .stop()
                    .await
                    .map_err(|e| format!("重启前停止代理服务器失败: {e}"));
                if let Err(stop_error) = stop_result {
                    if reuse_state == ProxyServerReuseState::Reusable {
                        server_guard.take();
                        self.persist_proxy_disabled_after_failure("重启停止失败")
                            .await;
                        return Err(stop_error);
                    }
                    log::warn!("回收取消操作遗留的代理任务时收到终止结果: {stop_error}");
                }
            }
            server_guard.take();

            let new_server = ProxyServer::new(new_config.clone(), self.db.clone(), app_handle);
            let info = match new_server.start().await {
                Ok(info) => info,
                Err(error) => {
                    server_guard.take();
                    let restart_error = format!("重启代理服务器失败: {error}");
                    self.persist_proxy_disabled_after_failure("重启启动失败")
                        .await;
                    return Err(restart_error);
                }
            };
            *server_guard = Some(new_server);

            #[cfg(test)]
            self.wait_at_start_install_pause_for_test().await;

            if let Err(e) = self
                .persist_ephemeral_listen_port_if_needed(&new_config, info.port)
                .await
            {
                if let Some(server) = server_guard.as_ref() {
                    let _ = server.stop().await;
                }
                server_guard.take();
                self.persist_proxy_disabled_after_failure("重启后保存动态代理端口失败")
                    .await;
                return Err(e);
            }

            log::info!("代理配置已更新，服务器已自动重启应用最新配置");
        }

        let status = server_guard
            .as_ref()
            .expect("a successful update keeps a running server installed")
            .get_status()
            .await;
        if !status.running {
            server_guard.take();
            return Ok(());
        }
        let (proxy_url, proxy_codex_base_url) =
            Self::build_proxy_urls_for_bind(&status.address, status.port)?;
        drop(server_guard);

        self.reconcile_enabled_takeovers(&proxy_url, &proxy_codex_base_url)
            .await?;

        Ok(())
    }

    /// Caller holds every supported app switch guard.
    async fn reconcile_enabled_takeovers(
        &self,
        proxy_url: &str,
        proxy_codex_base_url: &str,
    ) -> Result<(), String> {
        let mut updated_any = false;
        for app_type in [
            AppType::Claude,
            AppType::Codex,
            AppType::Gemini,
            AppType::GrokBuild,
        ] {
            let app_type_str = app_type.as_str();
            let enabled = self
                .db
                .get_proxy_config_for_app(app_type_str)
                .await
                .map_err(|e| format!("获取 {app_type_str} 配置失败: {e}"))?
                .enabled;
            if enabled {
                self.takeover_live_config_strict_with_urls(
                    &app_type,
                    proxy_url,
                    proxy_codex_base_url,
                )?;
                updated_any = true;
            }
        }

        if updated_any {
            log::info!("已严格同步更新所有已启用 Live 接管的代理地址");
        }
        Ok(())
    }

    /// 检查服务器是否正在运行
    pub async fn is_running(&self) -> bool {
        if let Some(server) = self.server.read().await.as_ref() {
            server.is_running().await
        } else {
            false
        }
    }

    /// 热更新熔断器配置
    ///
    /// 如果代理服务器正在运行，将新配置应用到所有已创建的熔断器实例
    pub async fn update_circuit_breaker_configs(
        &self,
        config: crate::proxy::CircuitBreakerConfig,
    ) -> Result<(), String> {
        if let Some(server) = self.server.read().await.as_ref() {
            server.update_circuit_breaker_configs(config).await;
            log::info!("已热更新运行中的熔断器配置");
        } else {
            log::debug!("代理服务器未运行，熔断器配置将在下次启动时生效");
        }
        Ok(())
    }

    /// 热更新指定应用的熔断器配置
    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: crate::proxy::CircuitBreakerConfig,
    ) -> Result<(), String> {
        if let Some(server) = self.server.read().await.as_ref() {
            server
                .update_circuit_breaker_config_for_app(app_type, config)
                .await;
            log::info!("已热更新 {app_type} 运行中的熔断器配置");
        } else {
            log::debug!("{app_type} 熔断器配置将在下次代理启动时生效");
        }
        Ok(())
    }

    /// 重置指定 Provider 的熔断器
    pub async fn fill_key_pool_status(
        &self,
        status: &mut crate::provider_groups::ProviderGroupStatus,
    ) {
        if let Some(server) = self.server.read().await.as_ref() {
            server.fill_key_pool_status(status).await;
        }
    }

    pub async fn clear_key_pool_runtime(&self, app_type: &str, group_id: &str) {
        if let Some(server) = self.server.read().await.as_ref() {
            server.clear_key_pool_runtime(app_type, group_id).await;
        }
    }

    /// 重置指定 Provider 的熔断器
    ///
    /// 如果代理服务器正在运行，立即重置内存中的熔断器状态
    pub async fn reset_provider_circuit_breaker(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Result<(), String> {
        if let Some(server) = self.server.read().await.as_ref() {
            server
                .reset_provider_circuit_breaker(provider_id, app_type)
                .await;
            log::info!("已重置 Provider {provider_id} (app: {app_type}) 的熔断器");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
    use serial_test::serial;
    use std::{env, time::Duration};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn proxy_file_rollback_preserves_external_file_created_after_transaction_output() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("auth.json");
        let mut snapshot = ProxyFileSnapshot::capture(path.clone()).expect("capture missing file");
        std::fs::write(&path, b"transaction-output").expect("transaction output");
        snapshot
            .capture_rollback_guard()
            .expect("capture transaction output");
        std::fs::write(&path, b"external-login").expect("external file update");

        let error = snapshot
            .restore_if_changed()
            .expect_err("external file must block rollback deletion");

        assert!(error.contains("外部修改"));
        assert_eq!(std::fs::read(path).unwrap(), b"external-login");
    }

    #[test]
    fn proxy_file_rollback_preserves_external_change_to_existing_file() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("config.toml");
        std::fs::write(&path, b"original").expect("original file");
        let mut snapshot = ProxyFileSnapshot::capture(path.clone()).expect("capture file");
        std::fs::write(&path, b"transaction-output").expect("transaction output");
        snapshot
            .capture_rollback_guard()
            .expect("capture transaction output");
        std::fs::write(&path, b"external-edit").expect("external file update");

        let error = snapshot
            .restore_if_changed()
            .expect_err("external edit must block rollback overwrite");

        assert!(error.contains("外部修改"));
        assert_eq!(std::fs::read(path).unwrap(), b"external-edit");
    }

    #[test]
    fn codex_agent_role_maps_wildcard_and_loopback_listeners_to_loopback_urls() {
        for (listen_address, expected_host) in [
            ("0.0.0.0", "127.0.0.1"),
            ("::", "[::1]"),
            ("127.0.0.1", "127.0.0.1"),
            ("::1", "[::1]"),
        ] {
            assert_eq!(
                codex_agent_role_connect_host(listen_address)
                    .expect("wildcard or loopback listener must be accepted"),
                expected_host
            );
        }
    }

    #[test]
    fn codex_agent_role_rejects_explicit_lan_listeners() {
        for listen_address in ["192.0.2.10", "2001:db8::10"] {
            let error = codex_agent_role_connect_host(listen_address)
                .expect_err("a LAN-only listener is unavailable through loopback");
            assert!(
                error.starts_with(&format!("{CODEX_AGENT_ROLE_LOOPBACK_REQUIRED}:")),
                "LAN rejection must expose the stable marker: {error}"
            );
        }
    }

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
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
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
                None => env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    fn assert_env_str(env: &Map<String, Value>, key: &str, expected: Option<&str>) {
        assert_eq!(env.get(key).and_then(|value| value.as_str()), expected);
    }

    async fn use_ephemeral_proxy_port(db: &Arc<Database>) {
        let mut proxy_config = db.get_proxy_config().await.expect("get test proxy config");
        proxy_config.listen_port = 0;
        db.update_proxy_config(proxy_config)
            .await
            .expect("set test proxy config to an ephemeral port");
    }

    async fn running_codex_base_url(service: &ProxyService) -> String {
        let status = service.get_status().await.expect("get proxy status");
        format!("http://127.0.0.1:{}/v1", status.port)
    }

    async fn proxy_health_responds(port: u16) -> bool {
        let Ok(Ok(mut stream)) = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        else {
            return false;
        };
        if stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .is_err()
        {
            return false;
        }

        let mut response = [0u8; 1024];
        let Ok(Ok(read)) =
            tokio::time::timeout(Duration::from_secs(1), stream.read(&mut response)).await
        else {
            return false;
        };
        String::from_utf8_lossy(&response[..read]).contains("200 OK")
    }

    fn fail_resolved_port_persistence(db: &Database) {
        db.conn
            .lock()
            .expect("lock proxy test database")
            .execute_batch(
                "CREATE TRIGGER fail_resolved_proxy_port
                 BEFORE UPDATE OF listen_port ON proxy_config
                 WHEN OLD.listen_port = 0 AND NEW.listen_port <> 0
                 BEGIN
                   SELECT RAISE(ABORT, 'injected resolved proxy port persistence failure');
                 END;",
            )
            .expect("install resolved port persistence failure trigger");
    }

    fn seed_claude_takeover_provider(db: &Arc<Database>, service: &ProxyService) {
        let provider = Provider::with_id(
            "lifecycle-p1".to_string(),
            "Lifecycle P1".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "provider-key",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }),
            None,
        );
        db.save_provider("claude", &provider)
            .expect("save lifecycle Claude provider");
        db.set_current_provider("claude", "lifecycle-p1")
            .expect("set lifecycle DB current provider");
        crate::settings::set_current_provider(&AppType::Claude, Some("lifecycle-p1"))
            .expect("set lifecycle local current provider");
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_API_KEY": "live-key",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }))
            .expect("seed lifecycle Claude Live config");
    }

    fn seed_gemini_takeover_live(service: &ProxyService) {
        service
            .write_gemini_live(&json!({
                "env": {
                    "GEMINI_API_KEY": "gemini-live-key",
                    "GOOGLE_GEMINI_BASE_URL": "https://generativelanguage.googleapis.com"
                }
            }))
            .expect("seed lifecycle Gemini Live config");
    }

    fn fail_claude_takeover_enable_commit(db: &Database) {
        db.conn
            .lock()
            .expect("lock proxy test database")
            .execute_batch(
                "CREATE TRIGGER fail_claude_takeover_enable_commit
                 BEFORE UPDATE OF enabled ON proxy_config
                 WHEN OLD.app_type = 'claude' AND OLD.enabled = 0 AND NEW.enabled = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'injected Claude enabled commit failure');
                 END;",
            )
            .expect("install Claude enabled commit failure trigger");
    }

    #[tokio::test]
    #[serial]
    async fn role_transaction_resolves_ephemeral_loopback_codex_url() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db);
        let _transaction = service.lock_transaction().await;

        let started = service.start_inner().await.expect("start proxy");
        let base_url = service
            .codex_proxy_base_url()
            .await
            .expect("resolve Codex role proxy URL");
        assert_eq!(base_url, format!("http://127.0.0.1:{}/v1", started.port));
        assert!(!base_url.contains(":0/"));

        service.stop_inner().await.expect("stop proxy");
    }

    #[tokio::test]
    #[serial]
    async fn role_transaction_uses_runtime_address_and_port_as_one_endpoint() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let started = service.start().await.expect("start loopback proxy");

        let mut persisted = db.get_proxy_config().await.expect("persisted config");
        persisted.listen_address = "192.0.2.10".to_string();
        persisted.listen_port = started.port.saturating_add(1);
        db.update_proxy_config(persisted)
            .await
            .expect("simulate cancelled bind update");

        let base_url = service
            .codex_proxy_base_url()
            .await
            .expect("resolve from live listener");
        assert_eq!(base_url, format!("http://127.0.0.1:{}/v1", started.port));

        service.stop().await.expect("stop proxy");
    }

    #[tokio::test]
    #[serial]
    async fn transaction_restore_reaps_a_stopped_server_slot_before_restarting_runtime() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db);
        let transaction = service.lock_transaction().await;
        let started = service.start_inner().await.expect("start proxy");
        let mut snapshot = service
            .snapshot_transaction_state()
            .await
            .expect("snapshot running proxy");

        let server = service
            .server
            .write()
            .await
            .take()
            .expect("running server slot");
        server.stop().await.expect("stop server generation");
        *service.server.write().await = Some(server);
        assert!(!service.get_status().await.expect("stopped status").running);
        assert!(snapshot.capture_rollback_file_guards().is_empty());

        let errors = service.restore_transaction_state(&snapshot).await;
        assert!(errors.is_empty(), "runtime rollback errors: {errors:?}");
        let restored = service.get_status().await.expect("restored status");
        assert!(restored.running);
        assert_eq!(restored.address, started.address);
        assert_eq!(restored.port, started.port);

        service.stop_inner().await.expect("stop restored proxy");
        drop(transaction);
    }

    #[tokio::test]
    #[serial]
    async fn rollback_takeover_disable_preserves_codex_provider_health() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let provider = Provider::with_id(
            "role-owner".to_string(),
            "Role Owner".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "test-key" },
                "config": "model_provider = \"test\"\n[model_providers.test]\nbase_url = \"https://example.invalid/v1\"\nwire_api = \"responses\"\n"
            }),
            None,
        );
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("save provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider.id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local current provider");
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "live-key" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed Codex live files");

        service
            .set_takeover_for_app(AppType::Codex.as_str(), true)
            .await
            .expect("enable Codex takeover");
        db.update_provider_health_with_threshold(
            &provider.id,
            AppType::Codex.as_str(),
            false,
            Some("seed failure".to_string()),
            1,
        )
        .await
        .expect("seed provider health");

        let _transaction = service.lock_transaction().await;
        service
            .set_takeover_for_app_inner_preserving_health(AppType::Codex.as_str(), false)
            .await
            .expect("rollback takeover without clearing health");

        let health = db
            .get_provider_health(&provider.id, AppType::Codex.as_str())
            .await
            .expect("read provider health");
        assert_eq!(health.consecutive_failures, 1);
        assert!(!health.is_healthy);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_concurrent_start_reuses_one_proxy_server() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db);
        service
            .set_start_barrier_for_test(Arc::new(tokio::sync::Barrier::new(2)))
            .await;

        let first_service = service.clone();
        let first = tokio::spawn(async move { first_service.start().await });
        let second_service = service.clone();
        let second = tokio::spawn(async move { second_service.start().await });

        let first_info = first
            .await
            .expect("first start task")
            .expect("first start succeeds");
        let second_info = second
            .await
            .expect("second start task")
            .expect("second start succeeds");
        let running_port = service
            .get_status()
            .await
            .expect("read running status")
            .port;
        service
            .stop()
            .await
            .expect("stop proxy after assertion setup");

        assert_eq!(first_info.port, second_info.port);
        assert_eq!(running_port, first_info.port);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_plain_stop_rechecks_takeover_under_switch_guards() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload lifecycle settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        seed_claude_takeover_provider(&db, &service);
        let started = service.start().await.expect("start plain proxy");
        assert!(
            !db.get_proxy_config_for_app("claude")
                .await
                .expect("read initial Claude proxy config")
                .enabled
        );

        let stop_checked = Arc::new(tokio::sync::Notify::new());
        let stop_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_plain_stop_takeover_check_pause_for_test(
                stop_checked.clone(),
                stop_release.clone(),
            )
            .await;

        let stopping_service = service.clone();
        let stop_task = tokio::spawn(async move { stopping_service.stop().await });
        tokio::time::timeout(Duration::from_secs(2), stop_checked.notified())
            .await
            .expect("plain stop reaches the takeover-check window");

        let enabling_service = service.clone();
        let mut enable_task =
            tokio::spawn(
                async move { enabling_service.set_takeover_for_app("claude", true).await },
            );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut enable_task)
                .await
                .is_err(),
            "takeover enable must wait for the plain-stop transaction"
        );

        stop_release.notify_one();
        let stop_result = stop_task.await.expect("plain stop task join");
        stop_result.expect("plain stop completes before takeover enable");
        enable_task
            .await
            .expect("takeover task join")
            .expect("enable Claude takeover after plain stop");
        let final_status = service.get_status().await.expect("read final proxy status");
        let final_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read final Claude proxy config");
        let final_backup_present = db
            .get_live_backup("claude")
            .await
            .expect("read final Claude backup")
            .is_some();
        let final_live = service.read_claude_live().expect("read final Claude Live");
        let final_live_url = final_live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let final_health = proxy_health_responds(started.port).await;

        service
            .set_takeover_for_app("claude", false)
            .await
            .expect("clean up Claude takeover");

        assert!(final_status.running);
        assert!(
            final_health,
            "the accepted takeover must keep a live listener"
        );
        assert!(final_config.enabled);
        assert!(final_backup_present);
        assert_eq!(
            final_live_url.as_deref(),
            Some(format!("http://127.0.0.1:{}", started.port).as_str())
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_plain_stop_refuses_partial_takeover_ownership_after_commit_failure() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload lifecycle settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        seed_claude_takeover_provider(&db, &service);
        let started = service.start().await.expect("start plain proxy");
        fail_claude_takeover_enable_commit(&db);

        let enable_error = service
            .set_takeover_for_app("claude", true)
            .await
            .expect_err("injected enabled commit must fail takeover activation");
        let partial_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read partially activated Claude config");
        let partial_live = service
            .read_claude_live()
            .expect("read partially taken-over Claude Live");
        let partial_backup_present = db
            .get_live_backup("claude")
            .await
            .expect("read partial Claude backup")
            .is_some();

        let stop_result = service.stop().await;
        let status_after_stop = service
            .get_status()
            .await
            .expect("read proxy status after rejected plain stop");
        let health_after_stop = proxy_health_responds(started.port).await;
        let backup_after_stop_present = db
            .get_live_backup("claude")
            .await
            .expect("read retained Claude backup")
            .is_some();
        let live_after_stop = service
            .read_claude_live()
            .expect("read retained partial Claude Live");

        service
            .stop_with_restore()
            .await
            .expect("restore and stop partial Claude takeover");

        assert!(enable_error.contains("injected Claude enabled commit failure"));
        assert!(!partial_config.enabled);
        assert!(partial_backup_present);
        assert!(ProxyService::is_claude_live_taken_over(&partial_live));
        assert!(
            stop_result.as_ref().is_err_and(|error| {
                error.contains("仍有应用处于代理接管状态")
                    && error.contains("stop_with_restore")
            }),
            "plain stop must preserve partial takeover ownership and direct recovery: {stop_result:?}"
        );
        assert!(status_after_stop.running);
        assert_eq!(status_after_stop.port, started.port);
        assert!(health_after_stop);
        assert!(backup_after_stop_present);
        assert_eq!(live_after_stop, partial_live);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_takeover_waiting_on_plain_stop_starts_healthy_listener_after_stop() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload lifecycle settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        seed_claude_takeover_provider(&db, &service);
        service.start().await.expect("start initial plain proxy");

        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let stop_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_stop_pause_for_test(stop_entered.clone(), stop_release.clone())
            .await;
        let stopping_service = service.clone();
        let stop_task = tokio::spawn(async move { stopping_service.stop().await });
        tokio::time::timeout(Duration::from_secs(2), stop_entered.notified())
            .await
            .expect("plain stop owns all switch locks before stopping the listener");

        use_ephemeral_proxy_port(&db).await;
        let enabling_service = service.clone();
        let mut enable_task =
            tokio::spawn(
                async move { enabling_service.set_takeover_for_app("claude", true).await },
            );
        let enable_while_stop_holds_switches =
            tokio::time::timeout(Duration::from_millis(250), &mut enable_task).await;
        assert!(
            enable_while_stop_holds_switches.is_err(),
            "takeover must wait while plain stop owns all app switch guards"
        );

        stop_release.notify_one();
        stop_task
            .await
            .expect("plain stop task join")
            .expect("plain stop completes before takeover");
        enable_task
            .await
            .expect("takeover task join")
            .expect("waiting takeover starts a replacement listener");

        let final_status = service.get_status().await.expect("read final proxy status");
        let final_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read final Claude config");
        assert!(final_status.running);
        assert!(proxy_health_responds(final_status.port).await);
        assert!(final_config.enabled);
        assert!(db
            .get_live_backup("claude")
            .await
            .expect("read final Claude backup")
            .is_some());
        assert!(service
            .live_takeover_matches_current_proxy(&AppType::Claude)
            .await
            .expect("validate final Claude takeover"));

        service
            .set_takeover_for_app("claude", false)
            .await
            .expect("clean up final Claude takeover");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_last_disable_rechecks_other_app_takeover_before_plain_stop() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload lifecycle settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        seed_claude_takeover_provider(&db, &service);
        seed_gemini_takeover_live(&service);
        service
            .set_takeover_for_app("claude", true)
            .await
            .expect("enable initial Claude takeover");

        let stop_checked = Arc::new(tokio::sync::Notify::new());
        let stop_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_plain_stop_takeover_check_pause_for_test(
                stop_checked.clone(),
                stop_release.clone(),
            )
            .await;
        let disabling_service = service.clone();
        let disable_task = tokio::spawn(async move {
            disabling_service
                .set_takeover_for_app("claude", false)
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), stop_checked.notified())
            .await
            .expect("last-app disable releases its single guard before plain stop recheck");

        let enabling_service = service.clone();
        let mut enable_task =
            tokio::spawn(
                async move { enabling_service.set_takeover_for_app("gemini", true).await },
            );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut enable_task)
                .await
                .is_err(),
            "the proxy transaction keeps takeover enable behind the final disable"
        );
        stop_release.notify_one();
        disable_task
            .await
            .expect("Claude disable task join")
            .expect("Claude disable completes");
        enable_task
            .await
            .expect("Gemini enable task join")
            .expect("Gemini takeover starts after the disable transaction completes");

        let final_status = service.get_status().await.expect("read final proxy status");
        let claude_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read final Claude config");
        let gemini_config = db
            .get_proxy_config_for_app("gemini")
            .await
            .expect("read final Gemini config");
        assert!(final_status.running);
        assert!(proxy_health_responds(final_status.port).await);
        assert!(!claude_config.enabled);
        assert!(db
            .get_live_backup("claude")
            .await
            .expect("read cleaned Claude backup")
            .is_none());
        assert!(gemini_config.enabled);
        assert!(db
            .get_live_backup("gemini")
            .await
            .expect("read final Gemini backup")
            .is_some());
        assert!(service
            .live_takeover_matches_current_proxy(&AppType::Gemini)
            .await
            .expect("validate final Gemini takeover"));

        service
            .set_takeover_for_app("gemini", false)
            .await
            .expect("clean up final Gemini takeover");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_start_waits_until_stop_transition_finishes() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let first_info = service.start().await.expect("start initial proxy");

        // The first ephemeral port is persisted. Reset to zero so the RED
        // implementation can expose a second live instance instead of failing bind.
        use_ephemeral_proxy_port(&db).await;
        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let stop_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_stop_pause_for_test(stop_entered.clone(), stop_release.clone())
            .await;

        let stop_service = service.clone();
        let stop_task = tokio::spawn(async move { stop_service.stop().await });
        tokio::time::timeout(Duration::from_secs(2), stop_entered.notified())
            .await
            .expect("stop reaches the controlled transition window");

        let start_service = service.clone();
        let mut start_task = tokio::spawn(async move { start_service.start().await });
        let completed_during_stop =
            tokio::time::timeout(Duration::from_millis(250), &mut start_task).await;
        let start_finished_early = completed_during_stop.is_ok();

        stop_release.notify_one();
        stop_task
            .await
            .expect("stop task join")
            .expect("stop succeeds");
        let second_info = match completed_during_stop {
            Ok(result) => result.expect("start task join").expect("start succeeds"),
            Err(_) => start_task
                .await
                .expect("start task join")
                .expect("start succeeds"),
        };
        let status = service.get_status().await.expect("read final proxy status");
        let proxy_enabled = db
            .get_global_proxy_config()
            .await
            .expect("read global proxy config")
            .proxy_enabled;
        service.stop().await.expect("stop final proxy");

        assert!(
            !start_finished_early,
            "start must remain blocked while stop owns the lifecycle transition"
        );
        assert_ne!(first_info.port, second_info.port);
        assert!(status.running);
        assert_eq!(status.port, second_info.port);
        assert!(proxy_enabled);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_start_waits_until_update_config_restart_finishes() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let restart_stop_entered = Arc::new(tokio::sync::Notify::new());
        let restart_stop_release = Arc::new(tokio::sync::Notify::new());

        let server = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db.clone(),
            None,
        );
        let entered = restart_stop_entered.clone();
        let release = restart_stop_release.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let old_handle = tokio::spawn(async move {
            let _ = shutdown_rx.await;
            entered.notify_one();
            release.notified().await;
        });
        server
            .install_server_task_for_test(shutdown_tx, old_handle)
            .await;
        *service.server.write().await = Some(server);

        let replacement_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve replacement proxy port");
        let replacement_port = replacement_listener
            .local_addr()
            .expect("read replacement proxy port")
            .port();
        drop(replacement_listener);
        let mut new_config = db.get_proxy_config().await.expect("read proxy config");
        new_config.listen_port = replacement_port;
        let updating_service = service.clone();
        let update_task =
            tokio::spawn(async move { updating_service.update_config(&new_config).await });
        tokio::time::timeout(Duration::from_secs(1), restart_stop_entered.notified())
            .await
            .expect("restart reaches old-server stop");

        let starting_service = service.clone();
        let mut start_task = tokio::spawn(async move { starting_service.start().await });
        let start_during_restart =
            tokio::time::timeout(Duration::from_millis(250), &mut start_task).await;
        let start_finished_early = start_during_restart.is_ok();

        restart_stop_release.notify_one();
        update_task
            .await
            .expect("update task join")
            .expect("restart succeeds");
        let start_info = match start_during_restart {
            Ok(result) => result.expect("start task join").expect("start succeeds"),
            Err(_) => start_task
                .await
                .expect("start task join")
                .expect("start succeeds"),
        };
        let status = service.get_status().await.expect("read restarted status");
        service.stop().await.expect("stop restarted proxy");

        assert!(
            !start_finished_early,
            "start must wait while update_config owns the restart transition"
        );
        assert!(status.running);
        assert_eq!(start_info.port, status.port);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_retrying_cancelled_config_update_restarts_from_live_bind_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve first proxy port");
        let first_port = first_listener
            .local_addr()
            .expect("read first proxy port")
            .port();
        drop(first_listener);
        let mut first_config = db.get_proxy_config().await.expect("read proxy config");
        first_config.listen_port = first_port;
        db.update_proxy_config(first_config)
            .await
            .expect("persist first proxy port");

        let service = ProxyService::new(db.clone());
        service.start().await.expect("start first proxy listener");
        assert!(proxy_health_responds(first_port).await);

        let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve second proxy port");
        let second_port = second_listener
            .local_addr()
            .expect("read second proxy port")
            .port();
        assert_ne!(first_port, second_port);
        let mut second_config = db.get_proxy_config().await.expect("read proxy config");
        second_config.listen_port = second_port;

        // Let update_config persist P2, then block it before it can inspect or
        // replace the P1 server slot.
        let blocked_server_slot = service.server.read().await;
        let updating_service = service.clone();
        let cancelled_config = second_config.clone();
        let update_task =
            tokio::spawn(async move { updating_service.update_config(&cancelled_config).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if db
                    .get_proxy_config()
                    .await
                    .expect("poll persisted proxy config")
                    .listen_port
                    == second_port
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first update persists P2 before blocking on the server slot");
        update_task.abort();
        let update_join = update_task.await;
        drop(blocked_server_slot);
        drop(second_listener);

        assert!(update_join.is_err_and(|error| error.is_cancelled()));
        assert_eq!(
            service
                .get_status()
                .await
                .expect("read stale live status")
                .port,
            first_port,
            "the cancelled first attempt leaves the P1 listener alive"
        );

        service
            .update_config(&second_config)
            .await
            .expect("same P2 retry compares against the live listener and restarts");
        let final_status = service.get_status().await.expect("read P2 proxy status");

        assert_eq!(final_status.port, second_port);
        assert!(proxy_health_responds(second_port).await);
        assert!(
            !proxy_health_responds(first_port).await,
            "the P1 listener must close after the P2 retry"
        );
        service.stop().await.expect("stop P2 proxy");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_stop_timeout_still_persists_disabled_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let mut global_config = db
            .get_global_proxy_config()
            .await
            .expect("read global proxy config");
        global_config.proxy_enabled = true;
        db.update_global_proxy_config(global_config)
            .await
            .expect("enable global proxy config");

        let mut server = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db.clone(),
            None,
        );
        server.set_stop_timeout_for_test(Duration::from_millis(50));
        let pending_handle = tokio::spawn(async {
            futures::future::pending::<()>().await;
        });
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        server
            .install_server_task_for_test(shutdown_tx, pending_handle)
            .await;
        *service.server.write().await = Some(server);

        let stop_result = service.stop().await;
        let proxy_enabled = db
            .get_global_proxy_config()
            .await
            .expect("read stopped global proxy config")
            .proxy_enabled;

        assert!(
            stop_result
                .as_ref()
                .is_err_and(|error| error.contains("停止超时")),
            "the graceful-stop timeout remains visible to callers"
        );
        assert!(!service.is_running().await);
        assert!(
            !proxy_enabled,
            "a reaped timeout generation must still persist the stopped state"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_restart_stop_timeout_still_persists_disabled_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let mut global_config = db
            .get_global_proxy_config()
            .await
            .expect("read global proxy config");
        global_config.proxy_enabled = true;
        db.update_global_proxy_config(global_config)
            .await
            .expect("enable global proxy config");

        let mut server = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db.clone(),
            None,
        );
        server.set_stop_timeout_for_test(Duration::from_millis(50));
        let pending_handle = tokio::spawn(async {
            futures::future::pending::<()>().await;
        });
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        server
            .install_server_task_for_test(shutdown_tx, pending_handle)
            .await;
        *service.server.write().await = Some(server);

        let mut new_config = db.get_proxy_config().await.expect("read proxy config");
        new_config.listen_port = 1;
        let update_result = service.update_config(&new_config).await;
        let proxy_enabled = db
            .get_global_proxy_config()
            .await
            .expect("read stopped global proxy config")
            .proxy_enabled;

        assert!(
            update_result
                .as_ref()
                .is_err_and(|error| error.contains("重启前停止代理服务器失败: 停止超时")),
            "the restart caller still receives the graceful-stop timeout"
        );
        assert!(!service.is_running().await);
        assert!(
            !proxy_enabled,
            "a failed restart with a reaped old task must persist disabled state"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_restart_bind_failure_persists_disabled_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        service.start().await.expect("start initial proxy");

        let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied restart port");
        let occupied_port = occupied_listener
            .local_addr()
            .expect("read occupied port")
            .port();
        let mut new_config = db.get_proxy_config().await.expect("read proxy config");
        new_config.listen_address = "127.0.0.1".to_string();
        new_config.listen_port = occupied_port;

        let update_result = service.update_config(&new_config).await;
        let proxy_enabled = db
            .get_global_proxy_config()
            .await
            .expect("read failed-restart global proxy config")
            .proxy_enabled;

        assert!(update_result
            .as_ref()
            .is_err_and(|error| error.contains("重启代理服务器失败")));
        assert!(!service.is_running().await);
        assert!(
            !proxy_enabled,
            "a failed restart bind must persist disabled state"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_direct_start_invalid_address_persists_disabled_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let mut config = db.get_proxy_config().await.expect("read proxy config");
        config.listen_address = "not-a-socket-address".to_string();
        db.update_proxy_config(config)
            .await
            .expect("persist invalid proxy address");
        let service = ProxyService::new(db.clone());

        let start_result = service.start().await;
        let global = db
            .get_global_proxy_config()
            .await
            .expect("read failed-start global config");

        assert!(
            start_result
                .as_ref()
                .is_err_and(|error| error.contains("无效的地址")),
            "the original address parse failure remains visible"
        );
        assert!(!service.is_running().await);
        assert!(!global.proxy_enabled);
        assert!(service.server.read().await.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_direct_start_bind_failure_persists_disabled_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied direct-start port");
        let occupied_port = occupied_listener
            .local_addr()
            .expect("read occupied direct-start port")
            .port();
        let mut config = db.get_proxy_config().await.expect("read proxy config");
        config.listen_port = occupied_port;
        db.update_proxy_config(config)
            .await
            .expect("persist occupied direct-start port");
        let service = ProxyService::new(db.clone());

        let start_result = service.start().await;
        let global = db
            .get_global_proxy_config()
            .await
            .expect("read failed-start global config");

        assert!(
            start_result
                .as_ref()
                .is_err_and(|error| error.contains("地址绑定失败")),
            "the original bind failure remains visible"
        );
        assert!(!service.is_running().await);
        assert!(!global.proxy_enabled);
        assert!(service.server.read().await.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_direct_start_dynamic_port_persist_failure_disables_and_clears_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        fail_resolved_port_persistence(&db);
        let service = ProxyService::new(db.clone());

        let start_result = service.start().await;
        let global = db
            .get_global_proxy_config()
            .await
            .expect("read dynamic-port failure global config");

        assert!(
            start_result
                .as_ref()
                .is_err_and(|error| error.contains("保存动态代理端口失败")),
            "the original dynamic-port persistence error remains visible"
        );
        assert!(!service.is_running().await);
        assert!(!global.proxy_enabled);
        assert!(service.server.read().await.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_restart_dynamic_port_persist_failure_disables_and_clears_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let initial_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve initial fixed proxy port");
        let initial_port = initial_listener
            .local_addr()
            .expect("read initial fixed proxy port")
            .port();
        drop(initial_listener);
        let mut initial_config = db.get_proxy_config().await.expect("read proxy config");
        initial_config.listen_port = initial_port;
        db.update_proxy_config(initial_config)
            .await
            .expect("persist initial fixed proxy port");
        let service = ProxyService::new(db.clone());
        service.start().await.expect("start initial fixed proxy");
        fail_resolved_port_persistence(&db);

        let mut dynamic_config = db
            .get_proxy_config()
            .await
            .expect("read fixed proxy config");
        dynamic_config.listen_port = 0;
        let update_result = service.update_config(&dynamic_config).await;
        let global = db
            .get_global_proxy_config()
            .await
            .expect("read restart dynamic-port failure global config");

        assert!(
            update_result
                .as_ref()
                .is_err_and(|error| error.contains("保存动态代理端口失败")),
            "the restart returns the original dynamic-port persistence error"
        );
        assert!(!service.is_running().await);
        assert!(!global.proxy_enabled);
        assert!(service.server.read().await.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_same_config_retry_reconciles_enabled_takeover_after_write_failure() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload lifecycle settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        seed_claude_takeover_provider(&db, &service);
        service
            .set_takeover_for_app("claude", true)
            .await
            .expect("enable Claude takeover");
        let first_status = service.get_status().await.expect("read first proxy status");
        let first_live = service.read_claude_live().expect("read first Claude Live");
        let first_live_url = first_live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            .expect("read first Claude takeover URL")
            .to_string();

        let next_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve next takeover port");
        let next_port = next_listener
            .local_addr()
            .expect("read next takeover port")
            .port();
        drop(next_listener);
        assert_ne!(first_status.port, next_port);
        let mut next_config = db.get_proxy_config().await.expect("read proxy config");
        next_config.listen_port = next_port;
        service.fail_next_takeover_write_for_test("claude");

        let first_update = service.update_config(&next_config).await;
        let status_after_failed_reconcile = service
            .get_status()
            .await
            .expect("read restarted proxy status");
        let live_after_failed_reconcile = service
            .read_claude_live()
            .expect("read unreconciled Claude Live");
        let stale_live_url = live_after_failed_reconcile
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            .expect("read stale Claude takeover URL");

        assert!(first_update
            .as_ref()
            .is_err_and(|error| error.contains("injected claude takeover Live write failure")));
        assert_eq!(status_after_failed_reconcile.port, next_port);
        assert_eq!(stale_live_url, first_live_url);

        service
            .update_config(&next_config)
            .await
            .expect("same config retry strictly reconciles enabled takeover");
        let final_status = service.get_status().await.expect("read final proxy status");
        let final_live = service.read_claude_live().expect("read final Claude Live");
        let final_live_url = final_live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            .expect("read final Claude takeover URL");

        assert_eq!(final_status.port, next_port);
        assert_eq!(final_live_url, format!("http://127.0.0.1:{next_port}"));
        assert!(proxy_health_responds(next_port).await);
        service
            .set_takeover_for_app("claude", false)
            .await
            .expect("disable Claude takeover");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_start_after_server_started_keeps_instance_owned() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db);
        let install_entered = Arc::new(tokio::sync::Notify::new());
        let install_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_start_install_pause_for_test(install_entered.clone(), install_release)
            .await;

        let starting_service = service.clone();
        let start_task = tokio::spawn(async move { starting_service.start().await });
        tokio::time::timeout(Duration::from_secs(2), install_entered.notified())
            .await
            .expect("start reaches the post-bind installation window");
        start_task.abort();
        let start_join = start_task.await;

        let owned_after_cancel = service.is_running().await;
        let status_after_cancel = service.get_status().await.expect("read proxy status");
        if owned_after_cancel {
            service.stop().await.expect("stop retained proxy instance");
        }

        assert!(start_join.is_err_and(|error| error.is_cancelled()));
        assert!(
            owned_after_cancel,
            "a successfully started server must be installed before the next cancellation point"
        );
        assert!(status_after_cancel.running);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_stop_keeps_service_server_slot_owned() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db);
        service.start().await.expect("start proxy");

        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let stop_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_stop_pause_for_test(stop_entered.clone(), stop_release)
            .await;
        let stopping_service = service.clone();
        let stop_task = tokio::spawn(async move { stopping_service.stop().await });
        tokio::time::timeout(Duration::from_secs(1), stop_entered.notified())
            .await
            .expect("stop reaches the owned-slot window");
        stop_task.abort();
        let stop_join = stop_task.await;

        let owned_after_cancel = service.is_running().await;
        if owned_after_cancel {
            service.stop().await.expect("retry stop retained instance");
        }

        assert!(stop_join.is_err_and(|error| error.is_cancelled()));
        assert!(
            owned_after_cancel,
            "cancelling service stop must retain the server slot until task reaping completes"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_start_replaces_server_left_stopping_after_cancelled_stop() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let first_info = service.start().await.expect("start initial proxy");

        let next_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve next proxy port");
        let next_port = next_listener
            .local_addr()
            .expect("read next proxy port")
            .port();
        assert_ne!(first_info.port, next_port);
        let mut next_config = db.get_proxy_config().await.expect("read proxy config");
        next_config.listen_port = next_port;
        db.update_proxy_config(next_config)
            .await
            .expect("persist next proxy port");

        let publish_entered = Arc::new(tokio::sync::Notify::new());
        let publish_release = Arc::new(tokio::sync::Notify::new());
        service
            .server
            .read()
            .await
            .as_ref()
            .expect("installed proxy server")
            .set_stop_publish_pause_for_test(publish_entered.clone(), publish_release)
            .await;

        let stopping_service = service.clone();
        let stop_task = tokio::spawn(async move { stopping_service.stop().await });
        tokio::time::timeout(Duration::from_secs(2), publish_entered.notified())
            .await
            .expect("stop reaches the completed-result publication window");
        stop_task.abort();
        let stop_join = stop_task.await;

        assert!(stop_join.is_err_and(|error| error.is_cancelled()));
        assert!(
            !service.is_running().await,
            "a completed listener in Stopping must not be reported as running"
        );

        drop(next_listener);
        let second_info = service
            .start()
            .await
            .expect("start reaps the cancelled stop and creates a new server");
        let new_connection = tokio::net::TcpStream::connect(("127.0.0.1", second_info.port)).await;

        assert_eq!(second_info.port, next_port);
        assert!(
            new_connection.is_ok(),
            "the replacement server must accept TCP connections"
        );
        service.stop().await.expect("stop replacement proxy");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_restart_stop_is_reaped_by_next_start() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let stop_release = Arc::new(tokio::sync::Notify::new());

        let server = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db.clone(),
            None,
        );
        let entered = stop_entered.clone();
        let release = stop_release.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let pending_handle = tokio::spawn(async move {
            let _ = shutdown_rx.await;
            entered.notify_one();
            release.notified().await;
        });
        server
            .install_server_task_for_test(shutdown_tx, pending_handle)
            .await;
        *service.server.write().await = Some(server);

        let replacement_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve cancelled-restart replacement port");
        let replacement_port = replacement_listener
            .local_addr()
            .expect("read cancelled-restart replacement port")
            .port();
        drop(replacement_listener);
        let mut new_config = db.get_proxy_config().await.expect("read proxy config");
        new_config.listen_port = replacement_port;
        let updating_service = service.clone();
        let update_task =
            tokio::spawn(async move { updating_service.update_config(&new_config).await });
        tokio::time::timeout(Duration::from_secs(1), stop_entered.notified())
            .await
            .expect("restart sends shutdown to the old server");
        update_task.abort();
        let update_join = update_task.await;

        let running_after_cancel = service.is_running().await;
        stop_release.notify_one();
        let replacement = service
            .start()
            .await
            .expect("next start reaps the cancelled restart task");

        assert!(update_join.is_err_and(|error| error.is_cancelled()));
        assert!(
            !running_after_cancel,
            "shutdown-sent old tasks are owned for reaping without being reported as live"
        );
        assert_eq!(replacement.port, replacement_port);
        assert!(proxy_health_responds(replacement_port).await);
        service.stop().await.expect("stop replacement proxy");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_restart_after_new_server_started_keeps_instance_owned() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        service.start().await.expect("start initial proxy");

        let install_entered = Arc::new(tokio::sync::Notify::new());
        let install_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_start_install_pause_for_test(install_entered.clone(), install_release)
            .await;
        let replacement_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve restarted proxy port");
        let replacement_port = replacement_listener
            .local_addr()
            .expect("read restarted proxy port")
            .port();
        drop(replacement_listener);
        let mut new_config = db.get_proxy_config().await.expect("read proxy config");
        new_config.listen_port = replacement_port;
        let updating_service = service.clone();
        let update_task =
            tokio::spawn(async move { updating_service.update_config(&new_config).await });
        tokio::time::timeout(Duration::from_secs(2), install_entered.notified())
            .await
            .expect("restart reaches the post-start installation window");
        update_task.abort();
        let update_join = update_task.await;

        let owned_after_cancel = service.is_running().await;
        let status_after_cancel = service.get_status().await.expect("read proxy status");
        if owned_after_cancel {
            service
                .stop()
                .await
                .expect("stop retained restart instance");
        }

        assert!(update_join.is_err_and(|error| error.is_cancelled()));
        assert!(
            owned_after_cancel,
            "a restarted server must be installed before the next cancellation point"
        );
        assert!(status_after_cancel.running);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_stop_with_restore_completes_cleanup_then_returns_stop_timeout() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let original_live = json!({
            "env": {
                "ANTHROPIC_API_KEY": "original-key",
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
            }
        });
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
                }
            }))
            .expect("seed taken-over Claude Live");
        db.save_live_backup(
            "claude",
            &serde_json::to_string(&original_live).expect("serialize original Claude Live"),
        )
        .await
        .expect("save original Claude backup");
        let mut claude_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read Claude proxy config");
        claude_config.enabled = true;
        db.update_proxy_config_for_app(claude_config)
            .await
            .expect("enable Claude takeover state");
        let mut global = db
            .get_global_proxy_config()
            .await
            .expect("read global proxy config");
        global.proxy_enabled = true;
        db.update_global_proxy_config(global)
            .await
            .expect("enable global proxy state");

        let mut server = ProxyServer::new(ProxyConfig::default(), db.clone(), None);
        server.set_stop_timeout_for_test(Duration::from_millis(50));
        let pending_handle = tokio::spawn(async {
            futures::future::pending::<()>().await;
        });
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        server
            .install_server_task_for_test(shutdown_tx, pending_handle)
            .await;
        *service.server.write().await = Some(server);

        let stop_result = service.stop_with_restore().await;
        let restored_live = service
            .read_claude_live()
            .expect("read restored Claude Live");
        let final_claude_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read final Claude proxy config");

        assert!(
            stop_result
                .as_ref()
                .is_err_and(|error| error.contains("停止代理服务器失败: 停止超时")),
            "the real graceful-stop timeout remains visible after cleanup"
        );
        assert_eq!(restored_live, original_live);
        assert!(!final_claude_config.enabled);
        assert!(db
            .get_live_backup("claude")
            .await
            .expect("read cleaned Claude backup")
            .is_none());
        assert!(!service.is_running().await);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_stop_error_has_priority_when_restore_also_fails() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        db.save_live_backup("claude", "{ invalid backup")
            .await
            .expect("save invalid Claude backup");

        let mut server = ProxyServer::new(ProxyConfig::default(), db.clone(), None);
        server.set_stop_timeout_for_test(Duration::from_millis(50));
        let pending_handle = tokio::spawn(async {
            futures::future::pending::<()>().await;
        });
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        server
            .install_server_task_for_test(shutdown_tx, pending_handle)
            .await;
        *service.server.write().await = Some(server);

        let stop_error = service
            .stop_with_restore()
            .await
            .expect_err("stop and restore must both fail");

        assert!(
            stop_error.starts_with("停止代理服务器失败: 停止超时"),
            "the stop timeout remains the primary error: {stop_error}"
        );
        assert!(
            stop_error.contains("Live 恢复/清理失败")
                && stop_error.contains("解析 claude 备份失败"),
            "the returned error must append the restore failure context: {stop_error}"
        );
        assert!(
            db.get_live_backup("claude")
                .await
                .expect("read retained invalid backup")
                .is_some(),
            "failed restore keeps its backup for a later recovery attempt"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_keep_state_cleanup_preserves_stop_failed_and_enabled_state() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let original_live = json!({
            "env": {
                "ANTHROPIC_API_KEY": "keep-state-key",
                "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
            }
        });
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
                }
            }))
            .expect("seed taken-over Claude Live");
        db.save_live_backup(
            "claude",
            &serde_json::to_string(&original_live).expect("serialize keep-state backup"),
        )
        .await
        .expect("save keep-state Claude backup");
        let mut claude_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read Claude proxy config");
        claude_config.enabled = true;
        db.update_proxy_config_for_app(claude_config)
            .await
            .expect("enable Claude takeover state");

        let failed_handle = tokio::spawn(async {
            panic!("injected proxy task failure");
        });
        while !failed_handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let server = ProxyServer::new(ProxyConfig::default(), db.clone(), None);
        server
            .install_server_task_for_test(shutdown_tx, failed_handle)
            .await;
        *service.server.write().await = Some(server);

        let stop_result = service.stop_with_restore_keep_state().await;
        let restored_live = service
            .read_claude_live()
            .expect("read restored Claude Live");
        let final_claude_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read final Claude proxy config");

        assert!(
            stop_result
                .as_ref()
                .is_err_and(|error| error.contains("停止代理服务器失败: 停止失败")),
            "the real JoinError-backed StopFailed remains visible after cleanup"
        );
        assert_eq!(restored_live, original_live);
        assert!(final_claude_config.enabled);
        assert!(db
            .get_live_backup("claude")
            .await
            .expect("read cleaned keep-state backup")
            .is_none());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_restore_wrappers_are_idempotent_without_server_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);

        service
            .stop_with_restore()
            .await
            .expect("manual restore wrapper treats no slot as stopped");
        service
            .stop_with_restore_keep_state()
            .await
            .expect("keep-state restore wrapper treats no slot as stopped");
    }

    #[tokio::test]
    async fn unsupported_apps_are_rejected_before_proxy_side_effects() {
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);

        assert!(service.set_takeover_for_app("pi", true).await.is_err());
        assert!(!service.is_running().await);
        assert!(service.switch_proxy_target("pi", "missing").await.is_err());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_stop_with_restore_serializes_concurrent_takeover_reenable() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload lifecycle settings");
        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        seed_claude_takeover_provider(&db, &service);
        service
            .set_takeover_for_app("claude", true)
            .await
            .expect("enable initial Claude takeover");

        let cleanup_entered = Arc::new(tokio::sync::Notify::new());
        let cleanup_release = Arc::new(tokio::sync::Notify::new());
        service
            .set_restore_cleanup_pause_for_test(cleanup_entered.clone(), cleanup_release.clone())
            .await;
        let restoring_service = service.clone();
        let restore_task = tokio::spawn(async move { restoring_service.stop_with_restore().await });
        tokio::time::timeout(Duration::from_secs(2), cleanup_entered.notified())
            .await
            .expect("restore wrapper stops before its restore/cleanup phase");

        let mut ephemeral_config = db.get_proxy_config().await.expect("read proxy config");
        ephemeral_config.listen_port = 0;
        db.update_proxy_config(ephemeral_config)
            .await
            .expect("request a fresh port for the re-enabled takeover");
        let enabling_service = service.clone();
        let mut enable_task =
            tokio::spawn(
                async move { enabling_service.set_takeover_for_app("claude", true).await },
            );
        let enable_while_restore_paused =
            tokio::time::timeout(Duration::from_millis(250), &mut enable_task).await;

        assert!(
            enable_while_restore_paused.is_err(),
            "takeover re-enable must wait for the wrapper's switch-guarded cleanup"
        );
        cleanup_release.notify_one();
        restore_task
            .await
            .expect("restore task join")
            .expect("restore wrapper succeeds");
        enable_task
            .await
            .expect("enable task join")
            .expect("takeover re-enable succeeds after restore cleanup");

        let final_config = db
            .get_proxy_config_for_app("claude")
            .await
            .expect("read final Claude proxy config");
        assert!(service.is_running().await);
        assert!(final_config.enabled);
        assert!(db
            .get_live_backup("claude")
            .await
            .expect("read final Claude backup")
            .is_some());
        assert!(service
            .live_takeover_matches_current_proxy(&AppType::Claude)
            .await
            .expect("validate final Claude takeover URL"));
        service
            .set_takeover_for_app("claude", false)
            .await
            .expect("disable final Claude takeover");
    }

    fn seed_codex_model_template() {
        let codex_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        std::fs::write(
            codex_dir.join("models_cache.json"),
            serde_json::to_string(&serde_json::json!({
                "models": [{
                    "slug": "gpt-5.5",
                    "display_name": "GPT-5.5",
                    "model_messages": { "instructions_template": "t" },
                    "additional_speed_tiers": [],
                    "context_window": 128000
                }]
            }))
            .expect("serialize models_cache"),
        )
        .expect("write models_cache.json");
    }

    #[test]
    fn managed_account_claude_takeover_uses_auth_token_placeholder() {
        let mut provider = Provider::with_id(
            "copilot".to_string(),
            "GitHub Copilot".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com",
                    "ANTHROPIC_MODEL": "claude-haiku-4.5"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("github_copilot".to_string()),
            ..Default::default()
        });

        let mut live_config = provider.settings_config.clone();
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN")
                .and_then(|value| value.as_str()),
            Some(PROXY_TOKEN_PLACEHOLDER)
        );
        assert!(
            env.get("ANTHROPIC_API_KEY").is_none(),
            "API_KEY placeholders trigger Claude Code's custom-key approval prompt (defaults to No), landing users in Not logged in"
        );
    }

    #[test]
    fn managed_account_claude_takeover_sources_copilot_models_from_provider() {
        let mut provider = Provider::with_id(
            "copilot".to_string(),
            "GitHub Copilot".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com",
                    "ANTHROPIC_MODEL": "claude-sonnet-4.6",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4.5",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4.6",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-sonnet-4.6",
                    "CLAUDE_CODE_SUBAGENT_MODEL": "claude-sonnet-4.6[1M]"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("github_copilot".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://stale.example.com",
                "ANTHROPIC_API_KEY": "stale-key",
                "ANTHROPIC_MODEL": "stale-model",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "stale-haiku",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Stale Haiku",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "stale-sonnet",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Stale Sonnet",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "stale-opus",
                "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "Stale Opus",
                "CLAUDE_CODE_SUBAGENT_MODEL": "stale-subagent"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_env_str(env, "ANTHROPIC_MODEL", None);
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            Some("claude-haiku-4-5"),
        );
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            Some("claude-haiku-4.5"),
        );
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            Some("claude-sonnet-4-6"),
        );
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
            Some("claude-sonnet-4.6"),
        );
        assert_env_str(env, "ANTHROPIC_DEFAULT_OPUS_MODEL", Some("claude-opus-4-8"));
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
            Some("claude-sonnet-4.6"),
        );
        assert_env_str(
            env,
            "CLAUDE_CODE_SUBAGENT_MODEL",
            Some("claude-sonnet-4.6[1M]"),
        );
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
    }

    #[test]
    fn managed_account_claude_takeover_removes_stale_subagent_model_when_provider_omits_it() {
        let mut provider = Provider::with_id(
            "codex".to_string(),
            "Codex".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "provider-sonnet"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://stale.example.com",
                "ANTHROPIC_API_KEY": "stale-key",
                "CLAUDE_CODE_SUBAGENT_MODEL": "stale-subagent"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_env_str(env, "CLAUDE_CODE_SUBAGENT_MODEL", None);
    }

    #[test]
    fn managed_account_claude_takeover_sources_codex_models_from_provider() {
        let mut provider = Provider::with_id(
            "codex".to_string(),
            "Codex".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex",
                    "ANTHROPIC_MODEL": "gpt-5.4",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "gpt-5.4-mini",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "gpt-5.4",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "gpt-5.4"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://stale.example.com",
                "ANTHROPIC_AUTH_TOKEN": "stale-token",
                "ANTHROPIC_MODEL": "stale-model",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "stale-haiku",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Stale Haiku",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "stale-sonnet",
                "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Stale Sonnet",
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "stale-opus",
                "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "Stale Opus"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_env_str(env, "ANTHROPIC_MODEL", None);
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            Some("claude-haiku-4-5"),
        );
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
            Some("gpt-5.4-mini"),
        );
        assert_env_str(
            env,
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            Some("claude-sonnet-4-6"),
        );
        assert_env_str(env, "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME", Some("gpt-5.4"));
        assert_env_str(env, "ANTHROPIC_DEFAULT_OPUS_MODEL", Some("claude-opus-4-8"));
        assert_env_str(env, "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME", Some("gpt-5.4"));
        // Codex 系只保留 AUTH_TOKEN；双键会触发 Claude Code 告警（#4919）
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    }

    #[test]
    fn managed_account_claude_takeover_codex_injects_auth_token_without_preexisting_key() {
        let mut provider = Provider::with_id(
            "codex".to_string(),
            "Codex".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            ..Default::default()
        });

        // 全新安装/热切换形态：传入的 env 没有任何 token 键。
        let mut live_config = provider.settings_config.clone();
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    }

    #[test]
    fn managed_account_claude_takeover_xai_keeps_one_auth_key() {
        let mut provider = Provider::with_id(
            "xai".to_string(),
            "xAI".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.x.ai/v1"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("xai_oauth".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "old-token",
                "ANTHROPIC_API_KEY": "old-key",
                "OPENAI_API_KEY": "old-openai-key"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(Value::as_object)
            .expect("env should exist");
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
        assert_env_str(env, "OPENAI_API_KEY", None);
    }

    #[test]
    fn managed_account_claude_takeover_codex_by_base_url_keeps_auth_token() {
        // 无 provider_type meta、仅凭 base_url 识别为受管 codex 的供应商，
        // 也必须保留 AUTH_TOKEN 占位符（与策略选择共用同一判定族）。
        let provider = Provider::with_id(
            "codex-url-only".to_string(),
            "Codex (URL only)".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex"
                }
            }),
            None,
        );
        assert!(provider.uses_managed_account_auth());
        assert!(!provider.is_codex_oauth());

        let mut live_config = provider.settings_config.clone();
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
    }

    // #4919 复现场景：从第三方 Claude 供应商（live 已有 AUTH_TOKEN）切换到
    // Codex 受管供应商时，只应保留 AUTH_TOKEN 占位符，不得同时写入 API_KEY。
    #[test]
    fn managed_account_claude_takeover_codex_from_third_party_keeps_single_auth_key() {
        let mut provider = Provider::with_id(
            "codex".to_string(),
            "Codex".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://chatgpt.com/backend-api/codex"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("codex_oauth".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
                "ANTHROPIC_AUTH_TOKEN": "sk-third-party"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
    }

    #[test]
    fn managed_account_claude_takeover_copilot_defaults_to_auth_token() {
        let mut provider = Provider::with_id(
            "copilot".to_string(),
            "GitHub Copilot".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("github_copilot".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://stale.example.com",
                "ANTHROPIC_AUTH_TOKEN": "stale-token",
                "ANTHROPIC_API_KEY": "stale-key"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        // Default Copilot takeover injects AUTH_TOKEN: the API_KEY placeholder
        // triggers Claude Code's custom-key approval prompt (defaults to
        // "No (recommended)"), which lands users in "Not logged in".
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", Some(PROXY_TOKEN_PLACEHOLDER));
        assert_env_str(env, "ANTHROPIC_API_KEY", None);
    }

    #[test]
    fn managed_account_claude_takeover_copilot_honors_api_key_field_choice() {
        let mut provider = Provider::with_id(
            "copilot".to_string(),
            "GitHub Copilot".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.githubcopilot.com"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            provider_type: Some("github_copilot".to_string()),
            api_key_field: Some("ANTHROPIC_API_KEY".to_string()),
            ..Default::default()
        });

        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://stale.example.com",
                "ANTHROPIC_AUTH_TOKEN": "stale-token"
            }
        });
        ProxyService::apply_claude_takeover_fields_for_provider(
            &mut live_config,
            "http://127.0.0.1:15721",
            &provider,
        );

        let env = live_config
            .get("env")
            .and_then(|value| value.as_object())
            .expect("env should exist");
        // Explicit API-key-field choice keeps the API_KEY placeholder to avoid
        // conflicting with the /login-managed key (#1049).
        assert_env_str(env, "ANTHROPIC_API_KEY", Some(PROXY_TOKEN_PLACEHOLDER));
        assert_env_str(env, "ANTHROPIC_AUTH_TOKEN", None);
    }

    #[test]
    fn normal_claude_takeover_without_token_keeps_auth_token_fallback() {
        let mut live_config = json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.example.com",
                "ANTHROPIC_MODEL": "claude-haiku-4.5"
            }
        });

        ProxyService::apply_claude_takeover_fields(&mut live_config, "http://127.0.0.1:15721");

        assert_eq!(
            live_config
                .get("env")
                .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
                .and_then(|value| value.as_str()),
            Some(PROXY_TOKEN_PLACEHOLDER)
        );
        assert!(
            live_config
                .get("env")
                .and_then(|env| env.get("ANTHROPIC_API_KEY"))
                .is_none(),
            "non-managed providers should retain the legacy fallback behavior"
        );
    }

    #[tokio::test]
    #[serial]
    async fn start_with_takeover_ephemeral_port_writes_actual_live_url() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());

        let provider = Provider::with_id(
            "p1".to_string(),
            "P1".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "provider-key",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }),
            None,
        );
        db.save_provider("claude", &provider)
            .expect("save provider");
        db.set_current_provider("claude", "p1")
            .expect("set db current provider");
        crate::settings::set_current_provider(&AppType::Claude, Some("p1"))
            .expect("set local current provider");
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_API_KEY": "live-key",
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com"
                }
            }))
            .expect("seed claude live config");

        let info = service
            .start_with_takeover()
            .await
            .expect("start proxy with takeover");
        assert_ne!(info.port, 0, "OS should assign a concrete port");

        let stored_config = db.get_proxy_config().await.expect("read proxy config");
        assert_eq!(
            stored_config.listen_port, info.port,
            "resolved dynamic port should be persisted for DB-only proxy URL paths"
        );

        let live = service.read_claude_live().expect("read taken-over live");
        let base_url = live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(|value| value.as_str())
            .expect("taken-over base url");
        assert_eq!(base_url, format!("http://127.0.0.1:{}", info.port));
        assert!(
            !base_url.contains(":0"),
            "takeover must never write an unresolved :0 port"
        );

        service
            .stop_with_restore()
            .await
            .expect("stop proxy and restore live config");
    }

    #[tokio::test]
    #[serial]
    async fn update_config_reprojection_waits_for_codex_switch_lock_before_rebuilding_live_auth() {
        use tokio::time::{sleep, timeout, Duration};

        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let state = crate::store::AppState::new(db.clone());
        state
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
            .await
            .expect("seed managed account");

        let mut provider = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("save managed official provider");
        db.set_current_provider(AppType::Codex.as_str(), &provider.id)
            .expect("set DB current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local current provider");

        let initial_info = state
            .proxy_service
            .start()
            .await
            .expect("start proxy server");
        db.save_live_backup(
            AppType::Codex.as_str(),
            &serde_json::to_string(&provider.settings_config).expect("serialize backup"),
        )
        .await
        .expect("seed takeover backup");
        let mut codex_proxy_config = db
            .get_proxy_config_for_app(AppType::Codex.as_str())
            .await
            .expect("get Codex proxy config");
        codex_proxy_config.enabled = true;
        db.update_proxy_config_for_app(codex_proxy_config)
            .await
            .expect("enable Codex takeover state");
        state
            .proxy_service
            .sync_codex_live_from_provider_while_proxy_active(&provider)
            .await
            .expect("seed managed Codex takeover Live config");
        assert!(crate::codex_config::get_codex_auth_path().exists());
        assert!(crate::codex_config::codex_managed_oauth_live_auth_marker_exists());

        let port_reservation =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve replacement port");
        let replacement_port = port_reservation
            .local_addr()
            .expect("read replacement port")
            .port();
        assert_ne!(replacement_port, initial_info.port);
        drop(port_reservation);

        let switch_guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Codex.as_str())
            .await;
        let mut new_config = state
            .proxy_service
            .get_config()
            .await
            .expect("get current proxy config");
        new_config.listen_port = replacement_port;
        let service_for_update = state.proxy_service.clone();
        let mut update_task =
            tokio::spawn(async move { service_for_update.update_config(&new_config).await });

        sleep(Duration::from_millis(50)).await;
        assert!(
            !update_task.is_finished(),
            "update_config must wait for the Codex switch lock before restart and Live reprojection"
        );
        assert_eq!(
            state
                .proxy_service
                .get_status()
                .await
                .expect("get still-running proxy status")
                .port,
            initial_info.port,
            "the listener must remain on the old port while the Codex switch lock is held"
        );

        // Perform the credential-removal critical section while owning the same
        // lock. Once released, update_config must rebuild from current manager
        // state instead of a credential bundle prepared before removal.
        state
            .codex_oauth_manager
            .remove_account("acct-managed")
            .await
            .expect("remove managed account while switch lock is held");
        assert!(!crate::codex_config::get_codex_auth_path().exists());
        assert!(!crate::codex_config::codex_managed_oauth_live_auth_marker_exists());
        drop(switch_guard);

        let _update_result = timeout(Duration::from_secs(5), &mut update_task)
            .await
            .expect("update_config should finish after releasing the Codex switch lock")
            .expect("join update_config task");
        assert_eq!(
            state
                .proxy_service
                .get_status()
                .await
                .expect("get restarted proxy status")
                .port,
            replacement_port,
            "the listener must move to the replacement port after the lock is released"
        );
        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "post-removal restart reprojection must not recreate auth.json"
        );
        assert!(
            !crate::codex_config::codex_managed_oauth_live_auth_marker_exists(),
            "post-removal restart reprojection must not recreate the ownership marker"
        );

        state
            .proxy_service
            .stop_runtime_inner(true)
            .await
            .expect("stop proxy runtime after assertions");
    }

    #[test]
    #[serial]
    fn codex_custom_provider_live_write_preserves_oauth_auth_json() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        crate::codex_config::write_codex_live_atomic(
            &oauth_auth,
            Some(
                r#"model_provider = "openai"
model = "gpt-5-codex"
"#,
            ),
        )
        .expect("seed live OAuth auth");

        let mut provider = Provider::with_id(
            "rightcode".to_string(),
            "RightCode".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "rightcode-key"
                },
                "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("custom".to_string());
        let takeover_settings = json!({
            "auth": {
                "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
            },
            "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
"#
        });

        service
            .write_codex_live_for_provider(&takeover_settings, Some(&provider))
            .expect("write provider-driven Codex live config");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "third-party Codex proxy writes must not overwrite ChatGPT OAuth login state"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains("experimental_bearer_token"),
            "proxy placeholder should move into config.toml instead of auth.json"
        );
        assert!(
            live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "live config should carry the proxy placeholder token"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_preserves_oauth_auth_json_when_preserve_enabled() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
            .expect("seed live OAuth auth with DeepSeek config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("cn_official".to_string());
        db.save_provider("codex", &provider)
            .expect("save DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");

        service
            .takeover_live_config_strict(&AppType::Codex)
            .await
            .expect("take over Codex live config");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "Codex takeover should not overwrite ChatGPT OAuth auth when preservation is enabled"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "takeover placeholder should move into config.toml"
        );
        assert!(
            service.detect_takeover_in_live_config_for_app(&AppType::Codex),
            "Codex takeover detection should recognize config.toml placeholders"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_preserves_oauth_auth_json_even_when_provider_category_is_official() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
            .expect("seed live OAuth auth with DeepSeek config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("official".to_string());
        db.save_provider("codex", &provider)
            .expect("save misclassified DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");

        service
            .takeover_live_config_strict(&AppType::Codex)
            .await
            .expect("take over Codex live config");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "Codex takeover must not rewrite auth.json when preservation is enabled, even if provider category is stale or misclassified"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "takeover placeholder should move into config.toml"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_hot_switches_between_builtin_official_and_third_party() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        // Exercise the default setting: takeover itself must now preserve native
        // auth regardless of the legacy compatibility toggle.
        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some("model = \"gpt-5.4\"\n"))
            .expect("seed official live config");

        let mut official = Provider::with_id(
            "codex-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        official.category = Some("official".to_string());
        db.save_provider("codex", &official)
            .expect("save official provider");

        let mut third_party = Provider::with_id(
            "rightcode".to_string(),
            "RightCode".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "rightcode-key" },
                "config": r#"model_provider = "rightcode"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        third_party.category = Some("custom".to_string());
        db.save_provider("codex", &third_party)
            .expect("save third-party provider");
        db.set_current_provider("codex", "codex-official")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("codex-official"))
            .expect("set local current provider");

        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("enable official takeover");

        let read_auth = || -> Value {
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth")
        };
        assert_eq!(read_auth(), oauth_auth);
        let official_live = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read official takeover config");
        assert!(crate::codex_config::codex_config_has_official_proxy_route(
            &official_live
        ));
        assert!(official_live.contains("requires_openai_auth = true"));
        assert!(!official_live.contains(PROXY_TOKEN_PLACEHOLDER));

        service
            .hot_switch_provider("codex", "rightcode")
            .await
            .expect("switch to third-party provider");
        assert_eq!(
            read_auth(),
            oauth_auth,
            "third-party route must preserve OAuth"
        );
        let third_party_live =
            std::fs::read_to_string(crate::codex_config::get_codex_config_path())
                .expect("read third-party takeover config");
        assert!(third_party_live.contains(PROXY_TOKEN_PLACEHOLDER));
        assert!(!crate::codex_config::codex_config_has_official_proxy_route(
            &third_party_live
        ));

        service
            .hot_switch_provider("codex", "codex-official")
            .await
            .expect("switch back to official provider");
        assert_eq!(
            read_auth(),
            oauth_auth,
            "official switch must reuse native OAuth"
        );
        let official_live = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read restored official takeover config");
        assert!(crate::codex_config::codex_config_has_official_proxy_route(
            &official_live
        ));
        assert!(!official_live.contains(PROXY_TOKEN_PLACEHOLDER));

        service
            .set_takeover_for_app("codex", false)
            .await
            .expect("disable takeover");
        assert_eq!(read_auth(), oauth_auth);
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_hot_switch_adopts_and_clears_outgoing_managed_auth() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
            .await
            .expect("seed managed account");

        let mut managed = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        managed.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });
        let mut third_party = Provider::with_id(
            "rightcode".to_string(),
            "RightCode".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "rightcode-key" },
                "config": r#"model_provider = "rightcode"
[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        third_party.category = Some("custom".to_string());
        db.save_provider("codex", &managed)
            .expect("save managed official");
        db.save_provider("codex", &third_party)
            .expect("save third party");
        db.set_current_provider("codex", &managed.id)
            .expect("set managed current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&managed.id))
            .expect("set local current");
        crate::services::provider::write_live_with_common_config_for_codex_oauth_manager(
            db.as_ref(),
            &AppType::Codex,
            &managed,
            &service.codex_oauth_manager,
        )
        .expect("seed managed normal live config");

        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("enable managed takeover");
        let id_token = crate::codex_config::test_codex_id_token("managed-user");
        crate::config::write_json_file(
            &crate::codex_config::get_codex_auth_path(),
            &crate::codex_config::codex_managed_oauth_auth_value(
                "acct-managed",
                "cli-access-r1",
                Some(&id_token),
                "cli-refresh-r1",
                "2099-02-01T00:00:00Z",
            ),
        )
        .expect("simulate CLI refresh during takeover");

        service
            .hot_switch_provider("codex", &third_party.id)
            .await
            .expect("hot switch managed official to third party");

        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "managed auth must not remain live after takeover hot-switch"
        );
        assert!(!crate::codex_config::codex_managed_oauth_live_auth_marker_exists());
        assert_eq!(
            service
                .codex_oauth_manager
                .test_refresh_token_for_account("acct-managed")
                .await
                .as_deref(),
            Some("cli-refresh-r1")
        );
        let backup = db
            .get_live_backup("codex")
            .await
            .expect("read backup")
            .expect("backup exists");
        let backup: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
        assert_eq!(
            backup
                .pointer("/auth/OPENAI_API_KEY")
                .and_then(Value::as_str),
            Some("rightcode-key"),
            "stripping outgoing managed auth must preserve the target provider key"
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_active_takeover_hot_switches_between_managed_accounts() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed-a", "managed-access-a", "user-a")
            .await
            .expect("seed managed account A");
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed-b", "managed-access-b", "user-b")
            .await
            .expect("seed managed account B");

        let mut managed_a = Provider::with_id(
            "managed-a".to_string(),
            "OpenAI Official A".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        managed_a.category = Some("official".to_string());
        managed_a.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed-a".to_string()),
            }),
            ..Default::default()
        });
        let mut managed_b = Provider::with_id(
            "managed-official-b".to_string(),
            "OpenAI Official B".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        managed_b.category = Some("official".to_string());
        managed_b.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed-b".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &managed_a)
            .expect("save managed provider A");
        db.save_provider("codex", &managed_b)
            .expect("save managed provider B");
        db.set_current_provider("codex", &managed_a.id)
            .expect("set managed A current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&managed_a.id))
            .expect("set local managed A current");
        crate::services::provider::write_live_with_common_config_for_codex_oauth_manager(
            db.as_ref(),
            &AppType::Codex,
            &managed_a,
            &service.codex_oauth_manager,
        )
        .expect("seed managed A normal live config");

        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("enable managed A takeover");
        let id_token_a = crate::codex_config::test_codex_id_token("user-a");
        crate::config::write_json_file(
            &crate::codex_config::get_codex_auth_path(),
            &crate::codex_config::codex_managed_oauth_auth_value(
                "acct-managed-a",
                "cli-access-a1",
                Some(&id_token_a),
                "cli-refresh-a1",
                "2099-02-01T00:00:00Z",
            ),
        )
        .expect("simulate account A CLI refresh during takeover");

        service
            .hot_switch_provider("codex", &managed_b.id)
            .await
            .expect("hot switch managed A to managed B");

        assert_eq!(
            service
                .codex_oauth_manager
                .test_refresh_token_for_account("acct-managed-a")
                .await
                .as_deref(),
            Some("cli-refresh-a1"),
            "A's CLI-rotated refresh token must be adopted before B overwrites live auth"
        );
        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read managed B live auth");
        assert_eq!(
            live_auth
                .pointer("/tokens/account_id")
                .and_then(Value::as_str),
            Some("acct-managed-b")
        );
        assert_eq!(
            live_auth
                .pointer("/tokens/access_token")
                .and_then(Value::as_str),
            Some("managed-access-b")
        );
        assert!(
            crate::codex_config::codex_auth_matches_recorded_managed_oauth(
                &live_auth,
                "acct-managed-b"
            )
            .expect("check managed B marker"),
            "the live ownership marker must move to account B"
        );

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("read managed B backup")
            .expect("managed B backup exists");
        let backup_value: Value =
            serde_json::from_str(&backup.original_config).expect("parse managed B backup");
        assert!(
            backup_value.get("auth").is_none(),
            "managed official auth must remain live-only instead of freezing account A in backup"
        );
        assert!(
            !backup.original_config.contains("cli-access-a1")
                && !backup.original_config.contains("cli-refresh-a1")
                && !backup.original_config.contains("cli-id-a1"),
            "the restore backup must not contain account A's rotated auth generation"
        );
        assert_eq!(
            db.get_current_provider("codex")
                .expect("read DB current provider")
                .as_deref(),
            Some(managed_b.id.as_str())
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_half_takeover_hot_switch_to_managed_records_marker() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
            .await
            .expect("seed managed account");

        let previous = Provider::with_id(
            "previous".to_string(),
            "Previous".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "previous-key" },
                "config": "model_provider = \"previous\"\n"
            }),
            None,
        );
        let mut managed = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        managed.category = Some("official".to_string());
        managed.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &previous)
            .expect("save previous provider");
        db.save_provider("codex", &managed)
            .expect("save managed provider");
        db.set_current_provider("codex", &previous.id)
            .expect("set previous current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&previous.id))
            .expect("set local current");
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&previous.settings_config).expect("serialize backup"),
        )
        .await
        .expect("seed backup without live takeover marker");

        service
            .hot_switch_provider("codex", &managed.id)
            .await
            .expect("hot switch half-takeover to managed official");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read managed auth");
        assert!(
            crate::codex_config::codex_auth_matches_recorded_managed_oauth(
                &live_auth,
                "acct-managed"
            )
            .expect("check managed marker"),
            "semi-takeover managed write must record the ownership marker"
        );
    }

    #[test]
    fn codex_takeover_backup_preserves_api_key_login_material() {
        let mut target = json!({ "auth": {}, "config": "" });
        let existing = json!({
            "auth": { "OPENAI_API_KEY": "sk-real" },
            "config": "model = \"gpt-5.4\"\n"
        });

        ProxyService::preserve_codex_auth_in_backup(&mut target, &existing, true)
            .expect("preserve API-key auth");
        assert_eq!(target["auth"]["OPENAI_API_KEY"], "sk-real");
    }

    #[tokio::test]
    #[serial]
    async fn codex_official_backup_rebuild_keeps_live_auth_out_of_backup() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "sk-real" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed live auth");
        let mut official = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        official.category = Some("official".to_string());

        service
            .update_live_backup_from_provider_inner("codex", &official, None)
            .await
            .expect("rebuild backup");
        let backup = db
            .get_live_backup("codex")
            .await
            .expect("read backup")
            .expect("backup exists");
        let value: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
        assert!(
            value.get("auth").is_none(),
            "official login material must remain live-only"
        );
    }

    #[test]
    #[serial]
    fn codex_empty_restore_snapshot_does_not_delete_existing_auth_json() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let auth = json!({ "OPENAI_API_KEY": "sk-real" });
        crate::codex_config::write_codex_live_atomic(&auth, Some("model = \"gpt-5.4\"\n"))
            .expect("seed auth");

        service
            .write_codex_live_verbatim(&json!({
                "auth": {},
                "config": "model = \"gpt-5.4-mini\"\n"
            }))
            .expect("restore empty snapshot");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read auth");
        assert_eq!(live_auth, auth);
    }

    #[tokio::test]
    #[serial]
    async fn codex_sync_live_does_not_store_credentials_in_builtin_official_row() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let mut official = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        official.category = Some("official".to_string());
        db.save_provider("codex", &official).expect("save official");
        db.set_current_provider("codex", crate::database::CODEX_OFFICIAL_PROVIDER_ID)
            .expect("set current");
        crate::settings::set_current_provider(
            &AppType::Codex,
            Some(crate::database::CODEX_OFFICIAL_PROVIDER_ID),
        )
        .expect("set local current");
        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": "sk-real" }),
            Some("model = \"gpt-5.4\"\n"),
        )
        .expect("seed live auth");

        service
            .sync_live_to_provider(&AppType::Codex)
            .await
            .expect("sync live");

        let stored = db
            .get_provider_by_id(crate::database::CODEX_OFFICIAL_PROVIDER_ID, "codex")
            .expect("read official")
            .expect("official exists");
        assert_eq!(stored.settings_config["auth"], json!({}));
    }

    #[tokio::test]
    #[serial]
    async fn codex_set_takeover_for_app_preserves_oauth_auth_json_when_preserve_enabled() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
            .expect("seed live OAuth auth with DeepSeek config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("official".to_string());
        db.save_provider("codex", &provider)
            .expect("save misclassified DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");

        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("enable Codex takeover");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "the public takeover command path must not rewrite auth.json when preservation is enabled"
        );

        service
            .set_takeover_for_app("codex", false)
            .await
            .expect("disable Codex takeover");
        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_enabled_commit_failure_restores_native_live_bundle() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
            .await
            .expect("seed managed account");

        let native_id_token = crate::codex_config::test_codex_id_token("native-user");
        let native_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": native_id_token,
                "access_token": "native-access",
                "refresh_token": "native-refresh",
                "account_id": "acct-native"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        let native_config = "model = \"gpt-5.4\"\n";
        crate::codex_config::write_codex_live_atomic(&native_auth, Some(native_config))
            .expect("seed native live bundle");

        let mut provider = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": native_config }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &provider)
            .expect("save managed official");
        db.set_current_provider("codex", &provider.id)
            .expect("set DB current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local current");

        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER reject_codex_takeover_enabled_commit
                 BEFORE UPDATE OF enabled ON proxy_config
                 WHEN NEW.app_type = 'codex' AND NEW.enabled = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'forced Codex takeover enabled failure');
                 END;",
            )
            .expect("install enabled failure trigger");
        }

        let error = service
            .set_takeover_for_app("codex", true)
            .await
            .expect_err("enabled DB failure must abort takeover");
        service.stop().await.expect("stop test proxy");

        assert!(
            error.contains("forced Codex takeover enabled failure"),
            "surface DB failure: {error}"
        );
        let restored = service
            .read_codex_live()
            .expect("read restored live bundle");
        assert_eq!(restored.get("auth"), Some(&native_auth));
        assert_eq!(
            restored.get("config").and_then(Value::as_str),
            Some(native_config)
        );
        assert!(
            db.get_live_backup("codex")
                .await
                .expect("read backup")
                .is_none(),
            "successful in-memory rollback should clean the takeover backup"
        );
        assert!(
            !db.get_proxy_config_for_app("codex")
                .await
                .expect("read Codex proxy config")
                .enabled
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_sync_current_to_live_during_takeover_preserves_oauth_auth_json() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let state = crate::store::AppState::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
            .expect("seed live OAuth auth with DeepSeek config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("official".to_string());
        db.save_provider("codex", &provider)
            .expect("save misclassified DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");

        state
            .proxy_service
            .set_takeover_for_app("codex", true)
            .await
            .expect("enable Codex takeover");

        crate::services::provider::ProviderService::sync_current_to_live(&state)
            .expect("sync current providers while Codex is taken over");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "post-change provider sync must not rewrite Codex auth.json during takeover"
        );

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let backup_value: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup");
        assert_eq!(
            backup_value.get("auth"),
            Some(&oauth_auth),
            "provider-derived takeover backup should preserve official OAuth auth"
        );
        assert!(
            backup_value
                .get("config")
                .and_then(|value| value.as_str())
                .is_some_and(|config| config.contains("deepseek-key")),
            "provider token should be carried by config.toml in the restore backup"
        );

        state
            .proxy_service
            .set_takeover_for_app("codex", false)
            .await
            .expect("disable Codex takeover");
        let restored_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read restored auth");
        assert_eq!(
            restored_auth, oauth_auth,
            "turning takeover off should restore the preserved official OAuth auth"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_sync_current_to_live_during_takeover_activation_keeps_proxy_live_config() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let state = crate::store::AppState::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
            .expect("seed live OAuth auth with DeepSeek config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("official".to_string());
        db.save_provider("codex", &provider)
            .expect("save misclassified DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");

        state
            .proxy_service
            .backup_live_config_strict(&AppType::Codex)
            .await
            .expect("backup Codex live config");
        state
            .proxy_service
            .takeover_live_config_strict(&AppType::Codex)
            .await
            .expect("take over Codex live config");
        assert!(
            !db.get_proxy_config_for_app("codex")
                .await
                .expect("get Codex proxy config")
                .enabled,
            "this reproduces the activation window before set_takeover_for_app marks enabled=true"
        );

        crate::services::provider::ProviderService::sync_current_to_live(&state)
            .expect("sync current providers during takeover activation");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "activation-time provider sync must not rewrite Codex OAuth auth.json"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "activation-time provider sync must keep the proxy bearer placeholder"
        );
        assert!(
            live_config.contains("http://127.0.0.1"),
            "activation-time provider sync must keep the local proxy base_url"
        );
        assert!(
            state
                .proxy_service
                .detect_takeover_in_live_config_for_app(&AppType::Codex),
            "Codex live config should still be detected as taken over"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_set_takeover_rebuilds_stale_enabled_state_without_overwriting_backup() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let original_deepseek_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "deepseek-key"
"#;
        let stale_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(stale_live_config))
            .expect("seed stale Codex live config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("official".to_string());
        db.save_provider("codex", &provider)
            .expect("save misclassified DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": oauth_auth,
                "config": original_deepseek_config
            }))
            .expect("serialize original backup"),
        )
        .await
        .expect("seed original live backup");
        let mut proxy_config = db
            .get_proxy_config_for_app("codex")
            .await
            .expect("get Codex proxy config");
        proxy_config.enabled = true;
        db.update_proxy_config_for_app(proxy_config)
            .await
            .expect("mark Codex takeover enabled");

        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("rebuild Codex takeover");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "repairing stale takeover must restore the preserved OAuth auth from backup"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        let expected_base_url = running_codex_base_url(&service).await;
        assert!(
            live_config.contains(&expected_base_url),
            "stale enabled takeover must be rebuilt to the current proxy base_url"
        );
        assert!(
            live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "rebuilt takeover should keep the proxy bearer placeholder"
        );
        assert!(
            service
                .live_takeover_matches_current_proxy(&AppType::Codex)
                .await
                .expect("detect rebuilt Codex takeover"),
            "rebuilt Codex live config should match the active proxy address"
        );

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get Codex live backup")
            .expect("backup exists");
        let backup_value: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup");
        assert_eq!(
            backup_value.get("auth"),
            Some(&oauth_auth),
            "rebuilding stale takeover must not overwrite the original OAuth backup"
        );
        assert!(
            backup_value
                .get("config")
                .and_then(|value| value.as_str())
                .is_some_and(|config| config.contains("deepseek-key")
                    && !config.contains("http://127.0.0.1")),
            "backup should remain the restorable DeepSeek config, not the proxy config"
        );

        service
            .set_takeover_for_app("codex", false)
            .await
            .expect("disable Codex takeover");
        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_preserves_native_auth_even_when_legacy_toggle_is_disabled() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: false,
            ..Default::default()
        })
        .expect("disable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        let deepseek_live_config = r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#;
        crate::codex_config::write_codex_live_atomic(&oauth_auth, Some(deepseek_live_config))
            .expect("seed live OAuth auth with DeepSeek config");

        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("cn_official".to_string());
        db.save_provider("codex", &provider)
            .expect("save DeepSeek provider");
        db.set_current_provider("codex", "deepseek")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("deepseek"))
            .expect("set local current provider");

        service
            .takeover_live_config_strict(&AppType::Codex)
            .await
            .expect("take over Codex live config");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "takeover must preserve native OAuth independently of the legacy toggle"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "third-party takeover should carry its local placeholder in config.toml"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[test]
    #[serial]
    fn codex_takeover_cleanup_removes_config_placeholder_without_touching_oauth_auth() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        crate::codex_config::write_codex_live_atomic(
            &oauth_auth,
            Some(
                r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
experimental_bearer_token = "PROXY_MANAGED"
"#,
            ),
        )
        .expect("seed taken-over Codex live config");

        assert!(
            service.detect_takeover_in_live_config_for_app(&AppType::Codex),
            "config.toml placeholder should be detected before cleanup"
        );

        service
            .cleanup_codex_takeover_placeholders_in_live()
            .expect("cleanup Codex takeover placeholders");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read live auth");
        assert_eq!(
            live_auth, oauth_auth,
            "cleanup should preserve ChatGPT OAuth auth"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            !live_config.contains(PROXY_TOKEN_PLACEHOLDER),
            "cleanup should remove config.toml proxy bearer placeholder"
        );
        assert!(
            !live_config.contains("http://127.0.0.1:15721"),
            "cleanup should remove local proxy base_url"
        );
    }

    #[test]
    #[serial]
    fn codex_custom_provider_live_write_removes_auth_when_preserve_disabled() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: false,
            ..Default::default()
        })
        .expect("disable Codex official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let oauth_auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": "oauth-id",
                "access_token": "oauth-access"
            }
        });
        crate::codex_config::write_codex_live_atomic(
            &oauth_auth,
            Some(
                r#"model_provider = "openai"
model = "gpt-5-codex"
"#,
            ),
        )
        .expect("seed live OAuth auth");

        let mut provider = Provider::with_id(
            "rightcode".to_string(),
            "RightCode".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "rightcode-key"
                },
                "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.category = Some("custom".to_string());
        let takeover_auth = json!({
            "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
        });
        let takeover_settings = json!({
            "auth": takeover_auth,
            "config": r#"model_provider = "rightcode"
model = "gpt-5-codex"

[model_providers.rightcode]
name = "RightCode"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
"#
        });

        service
            .write_codex_live_for_provider(&takeover_settings, Some(&provider))
            .expect("write provider-driven Codex live config");

        // Disabled preservation historically overwrote the OAuth login with
        // the placeholder; config-only switching removes auth.json instead —
        // the login is equally gone, and the placeholder now travels as the
        // provider-scoped bearer token that Codex >= 0.149 actually sends.
        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "disabled preservation removes auth.json on a third-party takeover write"
        );

        let live_config = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read live config");
        assert!(
            live_config.contains(&format!(
                "experimental_bearer_token = \"{PROXY_TOKEN_PLACEHOLDER}\""
            )),
            "the placeholder must ride in config.toml as the provider token; got:\n{live_config}"
        );

        crate::settings::update_settings(crate::settings::AppSettings::default())
            .expect("reset settings");
    }

    #[test]
    fn update_toml_base_url_updates_active_model_provider_base_url() {
        let input = r#"
model_provider = "any"
model = "gpt-5.1-codex"
disable_response_storage = true

[model_providers.any]
name = "any"
base_url = "https://anyrouter.top/v1"
wire_api = "responses"
requires_openai_auth = true
"#;

        let new_url = "http://127.0.0.1:5000/v1";
        let output = crate::codex_config::update_codex_toml_field(input, "base_url", new_url)
            .expect("update base_url");

        let parsed: toml::Value =
            toml::from_str(&output).expect("updated config should be valid TOML");

        let base_url = parsed
            .get("model_providers")
            .and_then(|v| v.get("any"))
            .and_then(|v| v.get("base_url"))
            .and_then(|v| v.as_str())
            .expect("model_providers.any.base_url should exist");

        assert_eq!(base_url, new_url);
        assert!(
            parsed.get("base_url").is_none(),
            "should not write top-level base_url"
        );

        let wire_api = parsed
            .get("model_providers")
            .and_then(|v| v.get("any"))
            .and_then(|v| v.get("wire_api"))
            .and_then(|v| v.as_str())
            .expect("model_providers.any.wire_api should exist");
        assert_eq!(wire_api, "responses");
    }

    #[test]
    fn apply_codex_proxy_toml_config_forces_local_responses_wire_api() {
        let input = r#"
model_provider = "chat_only"
model = "gpt-5.1-codex"

[model_providers.chat_only]
name = "Chat Only"
base_url = "https://chat-only.example/v1"
wire_api = "chat"
"#;

        let proxy_url = "http://127.0.0.1:5000/v1";
        let output =
            ProxyService::apply_codex_proxy_toml_config_for_provider(input, proxy_url, None)
                .expect("apply proxy config");
        let parsed: toml::Value =
            toml::from_str(&output).expect("updated config should be valid TOML");

        let provider = parsed
            .get("model_providers")
            .and_then(|v| v.get("chat_only"))
            .expect("model_providers.chat_only should exist");

        assert_eq!(
            provider.get("base_url").and_then(|v| v.as_str()),
            Some(proxy_url)
        );
        assert_eq!(
            provider.get("wire_api").and_then(|v| v.as_str()),
            Some("responses")
        );
    }

    #[test]
    fn apply_codex_proxy_toml_config_routes_builtin_official_with_native_auth() {
        let mut provider = Provider::with_id(
            "codex-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        let proxy_url = "http://127.0.0.1:5000/v1";

        let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
            "experimental_bearer_token = \"PROXY_MANAGED\"\n",
            proxy_url,
            Some(&provider),
        )
        .expect("apply official proxy config");
        let parsed: toml::Value = toml::from_str(&output).expect("valid official route");
        let route_id = crate::codex_config::CC_SWITCH_CODEX_OFFICIAL_PROXY_PROVIDER_ID;
        let route = &parsed["model_providers"][route_id];

        assert_eq!(parsed["model_provider"].as_str(), Some(route_id));
        assert_eq!(route["base_url"].as_str(), Some(proxy_url));
        assert_eq!(route["requires_openai_auth"].as_bool(), Some(true));
        assert!(parsed.get("experimental_bearer_token").is_none());
    }

    #[test]
    fn apply_codex_proxy_toml_config_fails_closed_for_invalid_official_config() {
        let mut provider = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());

        let result = ProxyService::apply_codex_proxy_toml_config_for_provider(
            "model_providers = 3\n",
            "http://127.0.0.1:5000/v1",
            Some(&provider),
        );
        assert!(result.is_err());
    }

    #[test]
    fn apply_codex_proxy_toml_config_keeps_upstream_model_for_chat_provider() {
        let input = r#"
model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
"#;
        let mut provider = Provider::with_id(
            "deepseek".to_string(),
            "DeepSeek".to_string(),
            json!({
                "config": input
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            api_format: Some("openai_chat".to_string()),
            ..Default::default()
        });

        let proxy_url = "http://127.0.0.1:5000/v1";
        let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
            input,
            proxy_url,
            Some(&provider),
        )
        .expect("apply chat proxy config");
        let parsed: toml::Value =
            toml::from_str(&output).expect("updated config should be valid TOML");

        assert_eq!(
            parsed.get("model").and_then(|v| v.as_str()),
            Some("deepseek-v4-flash")
        );
        assert_eq!(
            parsed
                .get("model_providers")
                .and_then(|v| v.get("deepseek"))
                .and_then(|v| v.get("base_url"))
                .and_then(|v| v.as_str()),
            Some(proxy_url)
        );
    }

    #[test]
    fn apply_codex_proxy_toml_config_preserves_model_for_responses_provider() {
        let input = r#"
model_provider = "responses"
model = "upstream-responses-model"

[model_providers.responses]
name = "Responses"
base_url = "https://responses.example/v1"
wire_api = "responses"
"#;
        let mut provider = Provider::with_id(
            "responses".to_string(),
            "Responses".to_string(),
            json!({
                "config": input
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            api_format: Some("openai_responses".to_string()),
            ..Default::default()
        });

        let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
            input,
            "http://127.0.0.1:5000/v1",
            Some(&provider),
        )
        .expect("apply responses proxy config");
        let parsed: toml::Value =
            toml::from_str(&output).expect("updated config should be valid TOML");

        assert_eq!(
            parsed.get("model").and_then(|v| v.as_str()),
            Some("upstream-responses-model")
        );
    }

    #[test]
    fn apply_codex_proxy_toml_config_restores_upstream_model_for_responses_provider() {
        let input = r#"
model_provider = "responses"
model = "gpt-5.4"

[model_providers.responses]
name = "Responses"
base_url = "http://127.0.0.1:5000/v1"
wire_api = "responses"
"#;
        let mut provider = Provider::with_id(
            "responses".to_string(),
            "Responses".to_string(),
            json!({
                "config": r#"model_provider = "responses"
model = "upstream-responses-model"

[model_providers.responses]
name = "Responses"
base_url = "https://responses.example/v1"
wire_api = "responses"
"#
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            api_format: Some("openai_responses".to_string()),
            ..Default::default()
        });

        let output = ProxyService::apply_codex_proxy_toml_config_for_provider(
            input,
            "http://127.0.0.1:5000/v1",
            Some(&provider),
        )
        .expect("restore responses model");
        let parsed: toml::Value =
            toml::from_str(&output).expect("updated config should be valid TOML");

        assert_eq!(
            parsed.get("model").and_then(|v| v.as_str()),
            Some("upstream-responses-model")
        );
    }

    #[test]
    fn update_toml_base_url_falls_back_to_top_level_base_url() {
        let input = r#"
model = "gpt-5.1-codex"
"#;

        let new_url = "http://127.0.0.1:5000/v1";
        let output = crate::codex_config::update_codex_toml_field(input, "base_url", new_url)
            .expect("update top-level base_url");

        let parsed: toml::Value =
            toml::from_str(&output).expect("updated config should be valid TOML");

        let base_url = parsed
            .get("base_url")
            .and_then(|v| v.as_str())
            .expect("base_url should exist");

        assert_eq!(base_url, new_url);
    }

    #[tokio::test]
    #[serial]
    async fn sync_claude_token_does_not_add_anthropic_api_key() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider = Provider::with_id(
            "p1".to_string(),
            "P1".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                    "ANTHROPIC_AUTH_TOKEN": "stale"
                }
            }),
            None,
        );
        db.save_provider("claude", &provider)
            .expect("save provider");
        db.set_current_provider("claude", "p1")
            .expect("set current provider");

        let live_config = json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "fresh"
            }
        });

        service
            .sync_live_config_to_provider(&AppType::Claude, &live_config)
            .await
            .expect("sync");

        let updated = db
            .get_provider_by_id("p1", "claude")
            .expect("get provider")
            .expect("provider exists");
        let env = updated
            .settings_config
            .get("env")
            .and_then(|v| v.as_object())
            .expect("env object");

        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN").and_then(|v| v.as_str()),
            Some("fresh")
        );
        assert!(
            !env.contains_key("ANTHROPIC_API_KEY"),
            "should not add ANTHROPIC_API_KEY when absent"
        );
    }

    #[tokio::test]
    #[serial]
    async fn sync_claude_token_respects_existing_api_key_field() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider = Provider::with_id(
            "p1".to_string(),
            "P1".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                    "ANTHROPIC_API_KEY": "stale"
                }
            }),
            None,
        );
        db.save_provider("claude", &provider)
            .expect("save provider");
        db.set_current_provider("claude", "p1")
            .expect("set current provider");

        let live_config = json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "fresh"
            }
        });

        service
            .sync_live_config_to_provider(&AppType::Claude, &live_config)
            .await
            .expect("sync");

        let updated = db
            .get_provider_by_id("p1", "claude")
            .expect("get provider")
            .expect("provider exists");
        let env = updated
            .settings_config
            .get("env")
            .and_then(|v| v.as_object())
            .expect("env object");

        assert_eq!(
            env.get("ANTHROPIC_API_KEY").and_then(|v| v.as_str()),
            Some("fresh")
        );
        assert!(
            !env.contains_key("ANTHROPIC_AUTH_TOKEN"),
            "should not add ANTHROPIC_AUTH_TOKEN when absent"
        );
    }

    #[tokio::test]
    #[serial]
    async fn switch_proxy_target_updates_live_backup_when_taken_over() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "A".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "a-key"
                }
            }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "B".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "b-key"
                }
            }),
            None,
        );
        db.save_provider("claude", &provider_a)
            .expect("save provider a");
        db.save_provider("claude", &provider_b)
            .expect("save provider b");
        db.set_current_provider("claude", "a")
            .expect("set current provider");

        // 模拟"已接管"状态：存在 Live 备份（内容不重要，会被热切换更新）
        db.save_live_backup("claude", "{\"env\":{}}")
            .await
            .expect("seed live backup");

        service
            .switch_proxy_target("claude", "b")
            .await
            .expect("switch proxy target");

        // 断言：本地 settings 的 current provider 已同步
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Claude).as_deref(),
            Some("b")
        );

        // 断言：Live 备份已更新为目标供应商配置（用于 stop_with_restore 恢复）
        let backup = db
            .get_live_backup("claude")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let expected = serde_json::to_string(&provider_b.settings_config).expect("serialize");
        assert_eq!(backup.original_config, expected);
    }

    #[tokio::test]
    #[serial]
    async fn hot_switch_provider_updates_claude_live_while_preserving_takeover_fields() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "A".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "a-key",
                    "ANTHROPIC_BASE_URL": "https://api.a.example",
                    "ANTHROPIC_MODEL": "claude-old"
                },
                "permissions": { "allow": ["Bash"] }
            }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "B".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_API_KEY": "b-key",
                    "ANTHROPIC_BASE_URL": "https://api.b.example",
                    "ANTHROPIC_MODEL": "claude-new",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "deepseek-v4-flash",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "DeepSeek V4 Flash",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "deepseek-v4-pro[1M]",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "DeepSeek V4 Pro",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "deepseek-v4-ultra [1m]",
                    "CLAUDE_CODE_SUBAGENT_MODEL": "deepseek-v4-pro[1M]"
                },
                "permissions": { "allow": ["Read"] }
            }),
            None,
        );

        db.save_provider("claude", &provider_a)
            .expect("save provider a");
        db.save_provider("claude", &provider_b)
            .expect("save provider b");
        db.set_current_provider("claude", "a")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Claude, Some("a"))
            .expect("set local current provider");
        db.save_live_backup(
            "claude",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
        )
        .await
        .expect("seed live backup");
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721",
                    "ANTHROPIC_API_KEY": PROXY_TOKEN_PLACEHOLDER,
                    "ANTHROPIC_MODEL": "stale-model",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Stale Sonnet",
                    "CLAUDE_CODE_SUBAGENT_MODEL": "stale-subagent"
                },
                "permissions": { "allow": ["Bash"] }
            }))
            .expect("seed taken-over live file");

        service
            .hot_switch_provider("claude", "b")
            .await
            .expect("hot switch provider");

        let live = service.read_claude_live().expect("read live config");
        assert_eq!(
            live.get("permissions"),
            provider_b.settings_config.get("permissions"),
            "provider-derived live settings should be refreshed"
        );
        assert_eq!(
            live.get("env")
                .and_then(|env| env.get("ANTHROPIC_API_KEY"))
                .and_then(|v| v.as_str()),
            Some(PROXY_TOKEN_PLACEHOLDER),
            "takeover token placeholder should be preserved"
        );
        assert_eq!(
            live.get("env")
                .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                .and_then(|v| v.as_str()),
            Some("http://127.0.0.1:15721"),
            "takeover proxy URL should remain active"
        );
        assert!(
            live.get("env")
                .and_then(|env| env.get("ANTHROPIC_MODEL"))
                .is_none(),
            "fallback model override should be removed in takeover mode"
        );
        let live_env = live
            .get("env")
            .and_then(|env| env.as_object())
            .expect("live env");
        assert_eq!(
            live_env
                .get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
                .and_then(|v| v.as_str()),
            Some("claude-haiku-4-5"),
            "takeover mode should expose a stable Haiku role model"
        );
        assert_eq!(
            live_env
                .get("ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME")
                .and_then(|v| v.as_str()),
            Some("DeepSeek V4 Flash"),
            "model menu should show the current provider Haiku display name"
        );
        assert_eq!(
            live_env
                .get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                .and_then(|v| v.as_str()),
            Some("claude-sonnet-4-6[1M]"),
            "Sonnet role should carry the local 1M declaration for Claude Code"
        );
        assert_eq!(
            live_env
                .get("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME")
                .and_then(|v| v.as_str()),
            Some("DeepSeek V4 Pro"),
            "stale model display names should be replaced during hot switch"
        );
        assert_eq!(
            live_env
                .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
                .and_then(|v| v.as_str()),
            Some("claude-opus-4-8[1M]"),
            "Opus role should preserve the current provider 1M capability marker"
        );
        assert_eq!(
            live_env
                .get("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME")
                .and_then(|v| v.as_str()),
            Some("deepseek-v4-ultra"),
            "implicit display names should strip the local 1M marker"
        );
        assert_eq!(
            live_env
                .get("CLAUDE_CODE_SUBAGENT_MODEL")
                .and_then(|v| v.as_str()),
            Some("deepseek-v4-pro[1M]"),
            "subagent model should follow the target provider during hot switch"
        );

        let backup = db
            .get_live_backup("claude")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let expected = serde_json::to_string(&provider_b.settings_config).expect("serialize");
        assert_eq!(backup.original_config, expected);
    }

    #[tokio::test]
    #[serial]
    async fn hot_switch_provider_serializes_same_app_switches() {
        use tokio::time::{sleep, Duration};

        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "A".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "a-key" } }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "B".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "b-key" } }),
            None,
        );
        let provider_c = Provider::with_id(
            "c".to_string(),
            "C".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "c-key" } }),
            None,
        );

        db.save_provider("claude", &provider_a)
            .expect("save provider a");
        db.save_provider("claude", &provider_b)
            .expect("save provider b");
        db.save_provider("claude", &provider_c)
            .expect("save provider c");
        db.set_current_provider("claude", "a")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Claude, Some("a"))
            .expect("set local current provider");
        db.save_live_backup("claude", "{\"env\":{}}")
            .await
            .expect("seed live backup");

        let guard = service.lock_switch_for_test("claude").await;
        let service_for_b = service.clone();
        let service_for_c = service.clone();

        let switch_b = tokio::spawn(async move {
            service_for_b
                .hot_switch_provider("claude", "b")
                .await
                .expect("switch to b")
        });
        sleep(Duration::from_millis(20)).await;
        let switch_c = tokio::spawn(async move {
            service_for_c
                .hot_switch_provider("claude", "c")
                .await
                .expect("switch to c")
        });

        sleep(Duration::from_millis(20)).await;
        drop(guard);

        let outcome_b = switch_b.await.expect("join switch b");
        let outcome_c = switch_c.await.expect("join switch c");
        assert!(outcome_b.logical_target_changed);
        assert!(outcome_c.logical_target_changed);

        assert_eq!(
            crate::settings::get_effective_current_provider(&db, &AppType::Claude)
                .expect("effective current"),
            Some("c".to_string())
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Claude).as_deref(),
            Some("c")
        );
        assert_eq!(
            db.get_current_provider("claude").expect("db current"),
            Some("c".to_string())
        );

        let backup = db
            .get_live_backup("claude")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let expected = serde_json::to_string(&provider_c.settings_config).expect("serialize");
        assert_eq!(backup.original_config, expected);
    }

    #[tokio::test]
    #[serial]
    async fn hot_switch_provider_waits_for_proxy_transaction() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let provider = Provider::with_id(
            "target".to_string(),
            "Target".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "target-key" } }),
            None,
        );
        db.save_provider("claude", &provider)
            .expect("save target provider");
        let transaction = service.lock_transaction().await;
        let switching_service = service.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let result = switching_service
                .hot_switch_provider("claude", "target")
                .await;
            let _ = done_tx.send(());
            result
        });

        assert!(tokio::time::timeout(Duration::from_millis(50), done_rx)
            .await
            .is_err());
        drop(transaction);
        task.await
            .expect("join hot switch")
            .expect("hot switch after transaction release");
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_waits_for_proxy_transaction() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let provider = Provider::with_id(
            "target".to_string(),
            "Target".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "target-key" } }),
            None,
        );
        let transaction = service.lock_transaction().await;
        let updating_service = service.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let result = updating_service
                .update_live_backup_from_provider("claude", &provider)
                .await;
            let _ = done_tx.send(());
            result
        });

        assert!(tokio::time::timeout(Duration::from_millis(50), done_rx)
            .await
            .is_err());
        drop(transaction);
        task.await
            .expect("join backup update")
            .expect("backup update after transaction release");
    }

    #[tokio::test]
    #[serial]
    async fn crash_recovery_waits_for_proxy_transaction() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let transaction = service.lock_transaction().await;
        let recovering_service = service.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let result = recovering_service.recover_from_crash().await;
            let _ = done_tx.send(());
            result
        });

        assert!(tokio::time::timeout(Duration::from_millis(50), done_rx)
            .await
            .is_err());
        drop(transaction);
        task.await
            .expect("join crash recovery")
            .expect("crash recovery after transaction release");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn synchronous_takeover_disable_waits_for_proxy_transaction() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let transaction = service.lock_transaction().await;
        let disabling_service = service.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let result = disabling_service.disable_takeover_for_app_sync(&AppType::Claude);
            let _ = done_tx.send(());
            result
        });

        assert!(tokio::time::timeout(Duration::from_millis(50), done_rx)
            .await
            .is_err());
        drop(transaction);
        task.await
            .expect("join synchronous disable")
            .expect("disable after transaction release");
    }

    #[tokio::test]
    #[serial]
    async fn restore_waits_for_hot_switch_and_restores_latest_backup() {
        use tokio::time::{sleep, Duration};

        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "A".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "a-key" } }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "B".to_string(),
            json!({ "env": { "ANTHROPIC_API_KEY": "b-key" } }),
            None,
        );

        db.save_provider("claude", &provider_a)
            .expect("save provider a");
        db.save_provider("claude", &provider_b)
            .expect("save provider b");
        db.set_current_provider("claude", "a")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Claude, Some("a"))
            .expect("set local current provider");
        db.save_live_backup(
            "claude",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
        )
        .await
        .expect("seed live backup");
        service
            .write_claude_live(&json!({ "env": { "ANTHROPIC_API_KEY": "stale" } }))
            .expect("seed live file");

        let guard = service.lock_switch_for_test("claude").await;
        let service_for_switch = service.clone();
        let service_for_restore = service.clone();

        let switch_to_b = tokio::spawn(async move {
            service_for_switch
                .hot_switch_provider("claude", "b")
                .await
                .expect("switch to b")
        });
        sleep(Duration::from_millis(20)).await;
        let restore = tokio::spawn(async move {
            service_for_restore
                .restore_live_config_for_app_with_fallback(&AppType::Claude)
                .await
                .expect("restore claude live")
        });

        sleep(Duration::from_millis(20)).await;
        drop(guard);

        let outcome = switch_to_b.await.expect("join switch");
        restore.await.expect("join restore");
        assert!(outcome.logical_target_changed);

        assert_eq!(
            crate::settings::get_effective_current_provider(&db, &AppType::Claude)
                .expect("effective current"),
            Some("b".to_string())
        );

        let backup = db
            .get_live_backup("claude")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let expected = serde_json::to_string(&provider_b.settings_config).expect("serialize");
        assert_eq!(backup.original_config, expected);
        assert_eq!(
            service.read_claude_live().expect("read live"),
            provider_b.settings_config
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_from_provider_applies_claude_common_config() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        db.set_config_snippet(
            "claude",
            Some(
                serde_json::json!({
                    "includeCoAuthoredBy": false
                })
                .to_string(),
            ),
        )
        .expect("set common config snippet");

        let service = ProxyService::new(db.clone());

        let mut provider = Provider::with_id(
            "p1".to_string(),
            "P1".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": "token",
                    "ANTHROPIC_BASE_URL": "https://claude.example"
                }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            common_config_enabled: Some(true),
            ..Default::default()
        });

        service
            .update_live_backup_from_provider("claude", &provider)
            .await
            .expect("update live backup");

        let backup = db
            .get_live_backup("claude")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");

        assert_eq!(
            stored.get("includeCoAuthoredBy").and_then(|v| v.as_bool()),
            Some(false),
            "common config should be applied into Claude restore backup"
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_from_provider_applies_codex_common_config() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        db.set_config_snippet(
            "codex",
            Some("disable_response_storage = true\n".to_string()),
        )
        .expect("set common config snippet");

        let service = ProxyService::new(db.clone());

        let mut provider = Provider::with_id(
            "p1".to_string(),
            "P1".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "token"
                },
                "config": r#"model_provider = "any"
model = "gpt-5"

[model_providers.any]
base_url = "https://codex.example/v1"
"#
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            common_config_enabled: Some(true),
            ..Default::default()
        });

        service
            .update_live_backup_from_provider("codex", &provider)
            .await
            .expect("update live backup");

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");
        let config = stored
            .get("config")
            .and_then(|v| v.as_str())
            .expect("config string");

        assert!(
            config.contains("disable_response_storage = true"),
            "common config should be applied into Codex restore backup"
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_from_managed_official_does_not_freeze_oauth_tokens() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed", "managed-token", "managed-user")
            .await
            .expect("seed managed Codex OAuth account");

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "auth_mode": "chatgpt",
                    "OPENAI_API_KEY": null,
                    "tokens": {
                        "access_token": "old-native-token",
                        "account_id": "acct-native"
                    }
                },
                "config": ""
            }))
            .expect("serialize seed backup"),
        )
        .await
        .expect("seed live backup");

        let mut provider = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({
                "auth": {},
                "config": ""
            }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });

        service
            .update_live_backup_from_provider("codex", &provider)
            .await
            .expect("update live backup");

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");

        assert!(
            stored.get("auth").is_none(),
            "official OAuth generations must remain live-only so restore cannot roll them back"
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_switch_to_managed_official_replaces_native_account() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        db.update_proxy_config(ProxyConfig {
            listen_port: 15_721,
            ..Default::default()
        })
        .await
        .expect("set proxy port");
        let service = ProxyService::new(db);
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed", "managed-access", "managed-user")
            .await
            .expect("seed managed account");

        let native_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "native-id",
                "access_token": "native-access",
                "refresh_token": "native-refresh",
                "account_id": "acct-native"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&native_auth, Some("model = \"gpt-5.4\"\n"))
            .expect("seed native auth");

        let mut provider = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });

        service
            .sync_codex_live_from_provider_while_proxy_active(&provider)
            .await
            .expect("sync managed official takeover");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read managed live auth");
        assert_eq!(
            live_auth
                .pointer("/tokens/account_id")
                .and_then(Value::as_str),
            Some("acct-managed")
        );
        assert_eq!(
            live_auth
                .pointer("/tokens/access_token")
                .and_then(Value::as_str),
            Some("managed-access")
        );
        assert!(
            crate::codex_config::codex_auth_matches_recorded_managed_oauth(
                &live_auth,
                "acct-managed"
            )
            .expect("read managed marker"),
            "takeover write must record ownership of the managed auth"
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_takeover_unbound_official_preserves_native_account() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        db.update_proxy_config(ProxyConfig {
            listen_port: 15_721,
            ..Default::default()
        })
        .await
        .expect("set proxy port");
        let service = ProxyService::new(db);
        let native_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "native-id",
                "access_token": "native-access",
                "refresh_token": "native-refresh",
                "account_id": "acct-native"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&native_auth, Some("model = \"gpt-5.4\"\n"))
            .expect("seed native auth");

        let mut provider = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        provider.category = Some("official".to_string());

        service
            .sync_codex_live_from_provider_while_proxy_active(&provider)
            .await
            .expect("sync unbound official takeover");

        let live_auth: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read native live auth");
        assert_eq!(live_auth, native_auth);
    }

    #[tokio::test]
    #[serial]
    async fn restoring_official_codex_takeover_preserves_rotated_live_auth() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let mut provider = Provider::with_id(
            "managed-official".to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "" }),
            None,
        );
        provider.category = Some("official".to_string());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &provider)
            .expect("save official provider");
        db.set_current_provider("codex", &provider.id)
            .expect("set DB current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&provider.id))
            .expect("set local current");

        let stale_backup_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id-r0",
                "access_token": "access-r0",
                "refresh_token": "refresh-r0",
                "account_id": "acct-managed"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": stale_backup_auth,
                "config": "model = \"gpt-5.4\"\n"
            }))
            .expect("serialize stale backup"),
        )
        .await
        .expect("save stale backup");

        let rotated_live_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id-r1",
                "access_token": "access-r1",
                "refresh_token": "refresh-r1",
                "account_id": "acct-managed"
            },
            "last_refresh": "2026-01-02T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(
            &rotated_live_auth,
            Some(
                r#"model_provider = "cc-switch"
[model_providers.cc-switch]
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
"#,
            ),
        )
        .expect("seed rotated takeover live");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore official Codex config");

        let restored = service.read_codex_live().expect("read restored Codex live");
        assert_eq!(restored.get("auth"), Some(&rotated_live_auth));
        assert_eq!(
            restored.get("config").and_then(Value::as_str),
            Some("model = \"gpt-5.4\"\n")
        );
    }

    #[test]
    #[serial]
    fn codex_snapshot_rollback_preserves_newer_native_login_without_marker() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let id_token = crate::codex_config::test_codex_id_token("native-user");

        let auth_r0 = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token,
                "access_token": "access-r0",
                "refresh_token": "refresh-r0",
                "account_id": "acct-a"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&auth_r0, Some("model = \"before\"\n"))
            .expect("seed R0 live");
        let snapshot = crate::codex_config::CodexLiveStateSnapshot::capture()
            .expect("capture transaction snapshot");

        let auth_r1 = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": crate::codex_config::test_codex_id_token("native-user"),
                "access_token": "access-r1",
                "refresh_token": "refresh-r1",
                "account_id": "acct-a"
            },
            "last_refresh": "2026-01-02T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&auth_r1, Some("model = \"after\"\n"))
            .expect("write concurrent R1 live");

        snapshot
            .restore_preserving_newer_same_account_auth()
            .expect("selective rollback");

        let restored: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read restored auth");
        assert_eq!(restored, auth_r1, "newer same-account auth must survive");
        assert!(!crate::codex_config::codex_managed_oauth_live_auth_marker_exists());
        assert_eq!(
            std::fs::read_to_string(crate::codex_config::get_codex_config_path())
                .expect("read rolled-back config"),
            "model = \"before\"\n"
        );
    }

    #[test]
    #[serial]
    fn codex_snapshot_rollback_restores_previous_local_account_in_same_workspace() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        let id_token_a = crate::codex_config::test_codex_id_token("user-a");

        let auth_a = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token_a,
                "access_token": "access-a",
                "refresh_token": "refresh-a",
                "account_id": "workspace-shared"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&auth_a, Some("model = \"before\"\n"))
            .expect("seed account A live");
        crate::codex_config::record_codex_managed_oauth_live_auth(&auth_a, "local-a")
            .expect("record A marker");
        let snapshot = crate::codex_config::CodexLiveStateSnapshot::capture()
            .expect("capture account A snapshot");

        let auth_b = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": crate::codex_config::test_codex_id_token("user-b"),
                "access_token": "access-b",
                "refresh_token": "refresh-b",
                "account_id": "workspace-shared"
            },
            "last_refresh": "2026-01-02T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&auth_b, Some("model = \"after\"\n"))
            .expect("write account B live");
        crate::codex_config::record_codex_managed_oauth_live_auth(&auth_b, "local-b")
            .expect("record B marker");

        snapshot
            .restore_preserving_newer_same_account_auth()
            .expect("cross-account rollback");

        let restored: Value =
            crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
                .expect("read restored auth");
        assert_eq!(restored, auth_a, "failed A to B change must restore A");
        assert!(
            crate::codex_config::codex_auth_matches_recorded_managed_oauth(&restored, "local-a")
                .expect("check restored A marker")
        );
        assert_eq!(
            std::fs::read_to_string(crate::codex_config::get_codex_config_path())
                .expect("read rolled-back config"),
            "model = \"before\"\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_clearing_managed_codex_auth_keeps_official_auth_live_only() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        crate::settings::update_settings(crate::settings::AppSettings {
            preserve_codex_official_auth_on_switch: true,
            ..Default::default()
        })
        .expect("enable official auth preservation");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let native_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": "native-token",
                "account_id": "acct-managed"
            }
        });

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": native_auth,
                "config": ""
            }))
            .expect("serialize seed backup"),
        )
        .await
        .expect("seed live backup");

        let mut provider = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({
                "auth": {},
                "config": ""
            }),
            None,
        );
        provider.category = Some("official".to_string());

        service
            .update_live_backup_from_provider_inner("codex", &provider, Some("acct-managed"))
            .await
            .expect("update live backup");

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");

        assert!(
            stored.get("auth").is_none(),
            "official native auth must not be frozen into the restore backup"
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_from_provider_preserves_codex_mcp_servers() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "OPENAI_API_KEY": "old-token"
                },
                "config": r#"model_provider = "any"
model = "gpt-4"

[model_providers.any]
base_url = "https://old.example/v1"

[mcp_servers.echo]
command = "npx"
args = ["echo-server"]
"#
            }))
            .expect("serialize seed backup"),
        )
        .await
        .expect("seed live backup");

        let provider = Provider::with_id(
            "p2".to_string(),
            "P2".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "new-token"
                },
                "config": r#"model_provider = "any"
model = "gpt-5"

[model_providers.any]
base_url = "https://new.example/v1"
"#
            }),
            None,
        );

        service
            .update_live_backup_from_provider("codex", &provider)
            .await
            .expect("update live backup");

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");
        let config = stored
            .get("config")
            .and_then(|v| v.as_str())
            .expect("config string");

        assert!(
            config.contains("[mcp_servers.echo]"),
            "existing Codex MCP section should survive proxy hot-switch backup update"
        );
        assert!(
            config.contains("https://new.example/v1"),
            "provider-specific base_url should still update to the new provider"
        );
    }

    #[tokio::test]
    #[serial]
    async fn hot_switch_codex_provider_preserves_provider_model_provider_in_backup_and_restore() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "RightCode".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "rightcode-key"
                },
                "config": r#"model_provider = "rightcode"
model = "gpt-5.4"

[model_providers.rightcode]
name = "RightCode"
base_url = "https://rightcode.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
            }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "AiHubMix".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "aihubmix-key"
                },
                "config": r#"model_provider = "aihubmix"
model = "gpt-5.4"

[model_providers.aihubmix]
name = "AiHubMix"
base_url = "https://aihubmix.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
            }),
            None,
        );

        db.save_provider("codex", &provider_a)
            .expect("save provider a");
        db.save_provider("codex", &provider_b)
            .expect("save provider b");
        db.set_current_provider("codex", "a")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("a"))
            .expect("set local current provider");
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
        )
        .await
        .expect("seed live backup");
        service
            .write_codex_live(&json!({
                "auth": {
                    "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
                },
                "config": r#"model_provider = "rightcode"
model = "gpt-5.4"

[model_providers.rightcode]
name = "RightCode"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
requires_openai_auth = true
"#
            }))
            .expect("seed taken-over Codex live config");

        service
            .hot_switch_provider("codex", "b")
            .await
            .expect("hot switch Codex provider");

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");
        let backup_config = stored
            .get("config")
            .and_then(|v| v.as_str())
            .expect("backup config string");
        let parsed_backup: toml::Value =
            toml::from_str(backup_config).expect("parse backup config");
        assert_eq!(
            parsed_backup.get("model_provider").and_then(|v| v.as_str()),
            Some("aihubmix"),
            "provider-derived restore backup should preserve the provider's model_provider"
        );
        let backup_model_providers = parsed_backup
            .get("model_providers")
            .and_then(|v| v.as_table())
            .expect("backup model_providers");
        assert!(backup_model_providers.get("custom").is_none());
        assert_eq!(
            backup_model_providers
                .get("aihubmix")
                .and_then(|v| v.get("base_url"))
                .and_then(|v| v.as_str()),
            Some("https://aihubmix.example/v1"),
            "provider id should point at the hot-switched provider endpoint"
        );

        let live = service.read_codex_live().expect("read Codex live config");
        let live_config = live
            .get("config")
            .and_then(|v| v.as_str())
            .expect("live config string");
        let parsed_live: toml::Value = toml::from_str(live_config).expect("parse live config");
        assert_eq!(
            parsed_live.get("model_provider").and_then(|v| v.as_str()),
            Some("aihubmix"),
            "hot-switched Codex live config should expose the selected provider"
        );
        assert_eq!(
            parsed_live
                .get("model_providers")
                .and_then(|v| v.get("aihubmix"))
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str()),
            Some("AiHubMix"),
            "Codex app provider label should follow the selected provider"
        );
        assert_eq!(
            parsed_live
                .get("model_providers")
                .and_then(|v| v.get("aihubmix"))
                .and_then(|v| v.get("base_url"))
                .and_then(|v| v.as_str()),
            Some("http://127.0.0.1:15721/v1"),
            "taken-over live config should stay pointed at the local proxy"
        );

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore Codex live config");

        let live = service.read_codex_live().expect("read Codex live config");
        let live_config = live
            .get("config")
            .and_then(|v| v.as_str())
            .expect("live config string");
        let parsed_live: toml::Value = toml::from_str(live_config).expect("parse live config");
        assert_eq!(
            parsed_live.get("model_provider").and_then(|v| v.as_str()),
            Some("aihubmix"),
            "restored Codex live config should preserve the provider's model_provider"
        );
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str()),
            Some("aihubmix-key"),
            "restore should still use the hot-switched provider auth"
        );
    }

    #[tokio::test]
    #[serial]
    async fn hot_switch_codex_chat_provider_updates_live_provider_display() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "Responses".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "responses-key"
                },
                "config": r#"model_provider = "stable"
model = "responses-model"

[model_providers.stable]
name = "Stable"
base_url = "https://responses.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#
            }),
            None,
        );
        let mut provider_b = Provider::with_id(
            "b".to_string(),
            "DeepSeek".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "deepseek-key"
                },
                "config": r#"model_provider = "deepseek"
model = "deepseek-v4-flash"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
wire_api = "responses"
requires_openai_auth = true
"#
            }),
            None,
        );
        provider_b.meta = Some(ProviderMeta {
            api_format: Some("openai_chat".to_string()),
            ..Default::default()
        });

        db.save_provider("codex", &provider_a)
            .expect("save provider a");
        db.save_provider("codex", &provider_b)
            .expect("save provider b");
        db.set_current_provider("codex", "a")
            .expect("set current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some("a"))
            .expect("set local current provider");
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize provider a"),
        )
        .await
        .expect("seed live backup");
        service
            .write_codex_live(&json!({
                "auth": {
                    "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER
                },
                "config": r#"model_provider = "stable"
model = "responses-model"

[model_providers.stable]
name = "Stable"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "responses"
requires_openai_auth = true
"#
            }))
            .expect("seed taken-over Codex live config");

        service
            .hot_switch_provider("codex", "b")
            .await
            .expect("hot switch Codex provider");

        let live = service.read_codex_live().expect("read Codex live config");
        let live_config = live
            .get("config")
            .and_then(|v| v.as_str())
            .expect("live config string");
        let parsed_live: toml::Value = toml::from_str(live_config).expect("parse live config");

        assert_eq!(
            parsed_live.get("model_provider").and_then(|v| v.as_str()),
            Some("deepseek")
        );
        assert_eq!(
            parsed_live
                .get("model_providers")
                .and_then(|v| v.get("deepseek"))
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str()),
            Some("DeepSeek")
        );
        assert_eq!(
            parsed_live
                .get("model_providers")
                .and_then(|v| v.get("deepseek"))
                .and_then(|v| v.get("base_url"))
                .and_then(|v| v.as_str()),
            Some("http://127.0.0.1:15721/v1")
        );
        assert_eq!(
            parsed_live.get("model").and_then(|v| v.as_str()),
            Some("deepseek-v4-flash")
        );
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str()),
            Some(PROXY_TOKEN_PLACEHOLDER)
        );
    }

    #[tokio::test]
    #[serial]
    async fn update_live_backup_from_provider_keeps_new_codex_mcp_entries_on_conflict() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "OPENAI_API_KEY": "old-token"
                },
                "config": r#"[mcp_servers.shared]
command = "old-command"

[mcp_servers.legacy]
command = "legacy-command"
"#
            }))
            .expect("serialize seed backup"),
        )
        .await
        .expect("seed live backup");

        let provider = Provider::with_id(
            "p2".to_string(),
            "P2".to_string(),
            json!({
                "auth": {
                    "OPENAI_API_KEY": "new-token"
                },
                "config": r#"[mcp_servers.shared]
command = "new-command"

[mcp_servers.latest]
command = "latest-command"
"#
            }),
            None,
        );

        service
            .update_live_backup_from_provider("codex", &provider)
            .await
            .expect("update live backup");

        let backup = db
            .get_live_backup("codex")
            .await
            .expect("get live backup")
            .expect("backup exists");
        let stored: Value =
            serde_json::from_str(&backup.original_config).expect("parse backup json");
        let config = stored
            .get("config")
            .and_then(|v| v.as_str())
            .expect("config string");
        let parsed: toml::Value = toml::from_str(config).expect("parse merged codex config");

        let mcp_servers = parsed
            .get("mcp_servers")
            .expect("mcp_servers should be present");
        assert_eq!(
            mcp_servers
                .get("shared")
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str()),
            Some("new-command"),
            "new provider/common-config MCP definition should win on conflict"
        );
        assert_eq!(
            mcp_servers
                .get("legacy")
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str()),
            Some("legacy-command"),
            "backup-only MCP entries should still be preserved"
        );
        assert_eq!(
            mcp_servers
                .get("latest")
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str()),
            Some("latest-command"),
            "new MCP entries should remain in the restore backup"
        );
    }

    #[tokio::test]
    #[serial]
    async fn provider_switch_with_restored_codex_backup_refreshes_catalog_and_common_config() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        seed_codex_model_template();

        let db = Arc::new(Database::memory().expect("init db"));
        let state = crate::store::AppState::new(db.clone());

        db.set_config_snippet(
            "codex",
            Some(
                r#"[mcp_servers.shared]
command = "shared-command"
"#
                .to_string(),
            ),
        )
        .expect("set common config snippet");

        let proxy_config = ProxyConfig {
            listen_port: 0,
            ..Default::default()
        };
        db.update_proxy_config(proxy_config)
            .await
            .expect("set test proxy config");
        state
            .proxy_service
            .start()
            .await
            .expect("start proxy server");

        let config_a = r#"model_provider = "provider-a"
model = "model-a"

[model_providers.provider-a]
name = "ProviderA"
base_url = "https://provider-a.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        let config_b = r#"model_provider = "provider-b"
model = "model-b"

[model_providers.provider-b]
name = "ProviderB"
base_url = "https://provider-b.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;

        let provider_a = Provider::with_id(
            "a".to_string(),
            "ProviderA".to_string(),
            serde_json::json!({
                "auth": { "OPENAI_API_KEY": "key-a" },
                "config": config_a,
                "modelCatalog": { "models": [{ "model": "model-a" }] }
            }),
            None,
        );
        let mut provider_b = Provider::with_id(
            "b".to_string(),
            "ProviderB".to_string(),
            serde_json::json!({
                "auth": { "OPENAI_API_KEY": "key-b" },
                "config": config_b,
                "modelCatalog": { "models": [{ "model": "model-b" }] }
            }),
            None,
        );
        provider_b.meta = Some(ProviderMeta {
            common_config_enabled: Some(true),
            ..Default::default()
        });

        db.save_provider("codex", &provider_a)
            .expect("save provider a");
        db.save_provider("codex", &provider_b)
            .expect("save provider b");
        db.set_current_provider("codex", "a")
            .expect("set current provider a");
        crate::settings::set_current_provider(&AppType::Codex, Some("a"))
            .expect("set local current provider a");

        state
            .proxy_service
            .write_codex_live_for_provider(&provider_a.settings_config, Some(&provider_a))
            .expect("seed live codex config");
        assert!(
            !state
                .proxy_service
                .detect_takeover_in_live_config_for_app(&AppType::Codex),
            "seeded live config should not be proxy-taken-over"
        );

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
        )
        .await
        .expect("seed restored backup");

        crate::services::provider::ProviderService::switch(&state, AppType::Codex, "b")
            .expect("provider switch to provider b");
        state
            .proxy_service
            .stop_runtime_inner(true)
            .await
            .expect("stop proxy runtime after assertions");

        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        assert!(
            catalog_path.exists(),
            "cc-switch-model-catalog.json must be created on provider switch"
        );
        let catalog_text = std::fs::read_to_string(&catalog_path).expect("read catalog json");
        let catalog: serde_json::Value =
            serde_json::from_str(&catalog_text).expect("parse catalog json");
        let slugs: Vec<&str> = catalog
            .get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("slug").and_then(|s| s.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            slugs.contains(&"model-b"),
            "catalog must contain provider B's model after switch; got: {slugs:?}"
        );
        assert!(
            !slugs.contains(&"model-a"),
            "catalog must not contain stale provider A model after switch; got: {slugs:?}"
        );

        let config_path = crate::codex_config::get_codex_config_path();
        let config_text = std::fs::read_to_string(&config_path).expect("read config.toml");
        assert!(
            config_text.contains("model_catalog_json"),
            "config.toml must reference model_catalog_json after switch"
        );
        assert!(
            config_text.contains("[mcp_servers.shared]"),
            "config.toml must keep common config after switch"
        );
        assert!(
            config_text.contains(r#"command = "shared-command""#),
            "config.toml must include common config content after switch"
        );
    }

    #[tokio::test]
    #[serial]
    async fn provider_switch_with_restored_codex_backup_propagates_catalog_write_errors() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        seed_codex_model_template();

        let db = Arc::new(Database::memory().expect("init db"));
        let state = crate::store::AppState::new(db.clone());

        let proxy_config = ProxyConfig {
            listen_port: 0,
            ..Default::default()
        };
        db.update_proxy_config(proxy_config)
            .await
            .expect("set test proxy config");
        state
            .proxy_service
            .start()
            .await
            .expect("start proxy server");

        let config_a = r#"model_provider = "provider-a"
model = "model-a"

[model_providers.provider-a]
name = "ProviderA"
base_url = "https://provider-a.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;
        let config_b = r#"model_provider = "provider-b"
model = "model-b"

[model_providers.provider-b]
name = "ProviderB"
base_url = "https://provider-b.example/v1"
wire_api = "responses"
requires_openai_auth = true
"#;

        let provider_a = Provider::with_id(
            "a".to_string(),
            "ProviderA".to_string(),
            serde_json::json!({
                "auth": { "OPENAI_API_KEY": "key-a" },
                "config": config_a,
                "modelCatalog": { "models": [{ "model": "model-a" }] }
            }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "ProviderB".to_string(),
            serde_json::json!({
                "auth": { "OPENAI_API_KEY": "key-b" },
                "config": config_b,
                "modelCatalog": { "models": [{ "model": "model-b" }] }
            }),
            None,
        );

        db.save_provider("codex", &provider_a)
            .expect("save provider a");
        db.save_provider("codex", &provider_b)
            .expect("save provider b");
        db.set_current_provider("codex", "a")
            .expect("set current provider a");
        crate::settings::set_current_provider(&AppType::Codex, Some("a"))
            .expect("set local current provider a");

        state
            .proxy_service
            .write_codex_live_for_provider(&provider_a.settings_config, Some(&provider_a))
            .expect("seed live codex config");
        assert!(
            !state
                .proxy_service
                .detect_takeover_in_live_config_for_app(&AppType::Codex),
            "seeded live config should not be proxy-taken-over"
        );

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
        )
        .await
        .expect("seed restored backup");

        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        if catalog_path.exists() {
            std::fs::remove_file(&catalog_path).expect("remove catalog file");
        }
        std::fs::create_dir_all(&catalog_path).expect("turn catalog path into directory");

        let err = crate::services::provider::ProviderService::switch(&state, AppType::Codex, "b")
            .expect_err("provider switch should fail when catalog cannot be written");
        state
            .proxy_service
            .stop_runtime_inner(true)
            .await
            .expect("stop proxy runtime after assertions");

        let message = err.to_string();
        assert!(
            message.contains("写入 Codex 配置失败")
                || message.contains("原子替换失败")
                || (message.contains("捕获 Codex 热切换前状态失败")
                    && message.contains("cc-switch-model-catalog.json")),
            "switch should surface catalog write failure, got: {message}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_direct_live_write_rolls_back_when_provider_commit_fails() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let state = crate::store::AppState::new(db.clone());
        db.update_proxy_config(ProxyConfig {
            listen_port: 0,
            ..Default::default()
        })
        .await
        .expect("set test proxy config");
        state
            .proxy_service
            .start()
            .await
            .expect("start proxy server");

        let provider_a = Provider::with_id(
            "a".to_string(),
            "Provider A".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "key-a" },
                "config": "model_provider = \"provider-a\"\nmodel = \"model-a\"\n\n[model_providers.provider-a]\nname = \"Provider A\"\nbase_url = \"https://a.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
            }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "Provider B".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "key-b" },
                "config": "model_provider = \"provider-b\"\nmodel = \"model-b\"\n\n[model_providers.provider-b]\nname = \"Provider B\"\nbase_url = \"https://b.example/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
            }),
            None,
        );
        db.save_provider("codex", &provider_a)
            .expect("save provider a");
        db.save_provider("codex", &provider_b)
            .expect("save provider b");
        db.set_current_provider("codex", "a")
            .expect("set current provider a");
        crate::settings::set_current_provider(&AppType::Codex, Some("a"))
            .expect("set local current provider a");
        state
            .proxy_service
            .write_codex_live_for_provider(&provider_a.settings_config, Some(&provider_a))
            .expect("seed direct live config");
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&provider_a.settings_config).expect("serialize backup"),
        )
        .await
        .expect("seed restored backup");
        let original_live = state
            .proxy_service
            .read_codex_live()
            .expect("read original live config");

        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER reject_codex_current_update
                 BEFORE UPDATE OF is_current ON providers
                 WHEN NEW.app_type = 'codex'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced current-provider commit failure');
                 END;",
            )
            .expect("install failure trigger");
        }

        let error = crate::services::provider::ProviderService::switch(&state, AppType::Codex, "b")
            .expect_err("database commit should fail");
        state
            .proxy_service
            .stop_runtime_inner(true)
            .await
            .expect("stop proxy runtime after assertions");

        assert!(error
            .to_string()
            .contains("forced current-provider commit failure"));
        assert_eq!(
            state
                .proxy_service
                .read_codex_live()
                .expect("read rolled-back live config"),
            original_live,
            "commit failure must restore the exact direct Live snapshot"
        );
        assert_eq!(
            db.get_current_provider("codex")
                .expect("read current provider")
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some("a")
        );
    }

    #[tokio::test]
    #[serial]
    async fn codex_active_takeover_hot_switch_failure_restores_native_official_auth() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        use_ephemeral_proxy_port(&db).await;
        let service = ProxyService::new(db.clone());
        service
            .codex_oauth_manager
            .add_test_account_with_user_identity("acct-managed-b", "managed-access-b", "user-b")
            .await
            .expect("seed managed account B");

        let native_auth = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "native-id",
                "access_token": "native-access",
                "refresh_token": "native-refresh",
                "account_id": "acct-native"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        crate::codex_config::write_codex_live_atomic(&native_auth, Some("model = \"gpt-5.4\"\n"))
            .expect("seed native official live");

        let mut official = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        official.category = Some("official".to_string());
        db.save_provider("codex", &official)
            .expect("save unbound official");
        db.set_current_provider("codex", &official.id)
            .expect("set DB current");
        crate::settings::set_current_provider(&AppType::Codex, Some(&official.id))
            .expect("set local current");

        service
            .set_takeover_for_app("codex", true)
            .await
            .expect("enable unbound official takeover");
        let backup_before = db
            .get_live_backup("codex")
            .await
            .expect("read original backup")
            .expect("backup exists");
        let live_before = crate::codex_config::CodexLiveStateSnapshot::capture()
            .expect("capture native takeover live");

        official.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".to_string()),
                account_id: Some("acct-managed-b".to_string()),
            }),
            ..Default::default()
        });
        db.save_provider("codex", &official)
            .expect("persist managed target before hot-switch projection");
        {
            let conn = db.conn.lock().expect("lock database");
            conn.execute_batch(
                "CREATE TRIGGER reject_managed_official_current_commit
                 BEFORE UPDATE OF is_current ON providers
                 WHEN NEW.app_type = 'codex'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced managed official current failure');
                 END;",
            )
            .expect("install current failure trigger");
        }

        let error = service
            .hot_switch_provider("codex", &official.id)
            .await
            .expect_err("current commit failure should abort hot-switch");
        service
            .stop_runtime_inner(true)
            .await
            .expect("stop proxy runtime after assertions");

        assert!(error.contains("forced managed official current failure"));
        assert_eq!(
            db.get_live_backup("codex")
                .await
                .expect("read rolled-back backup")
                .expect("backup remains")
                .original_config,
            backup_before.original_config
        );
        assert_eq!(
            crate::codex_config::CodexLiveStateSnapshot::capture()
                .expect("capture rolled-back takeover live"),
            live_before,
            "failed projection must restore native auth/config/catalog/marker exactly"
        );
        assert_eq!(
            db.get_current_provider("codex")
                .expect("read DB current")
                .as_deref(),
            Some(official.id.as_str())
        );
    }

    /// Regression: turning proxy takeover off restores Live from the backup. The
    /// backup snapshot is `read_codex_live_settings()` output (`{auth, config}`,
    /// never an inline `modelCatalog`). The restore must NOT route the config
    /// through catalog projection, which would see no specs and strip the
    /// `model_catalog_json` pointer — silently dropping the user's Codex model
    /// mapping from Live even though the DB SSOT still holds it.
    #[tokio::test]
    #[serial]
    async fn codex_restore_from_backup_preserves_model_catalog_pointer() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        // Pre-takeover Live state: config.toml points at the cc-switch generated
        // catalog file, and that file exists on disk (takeover never touches it).
        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        if let Some(parent) = catalog_path.parent() {
            std::fs::create_dir_all(parent).expect("create codex dir");
        }
        std::fs::write(
            &catalog_path,
            r#"{"models":[{"slug":"deepseek-v4-flash"}]}"#,
        )
        .expect("seed generated catalog file");

        let pointer = catalog_path.to_string_lossy().replace('\\', "/");
        let backup_config = format!(
            "model_provider = \"custom\"\n\
             model = \"deepseek-v4-flash\"\n\
             model_catalog_json = \"{pointer}\"\n\n\
             [model_providers.custom]\n\
             name = \"DeepSeek\"\n\
             base_url = \"https://api.deepseek.example/v1\"\n\
             wire_api = \"responses\"\n"
        );
        let backup_json = serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "deepseek-key" },
            "config": backup_config,
        }))
        .expect("serialize backup");
        db.save_live_backup("codex", &backup_json)
            .await
            .expect("seed live backup");

        // Turning takeover off restores Live from this backup.
        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex live from backup");

        let restored = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read restored config.toml");
        assert!(
            restored.contains("model_catalog_json"),
            "restore must preserve the model_catalog_json pointer, got:\n{restored}"
        );
        assert!(
            restored.contains(pointer.as_str()),
            "restored pointer must still reference the cc-switch generated catalog file"
        );
    }

    /// Regression: a hot-switch during takeover rebuilds the backup from the DB
    /// provider (`update_live_backup_from_provider`), so the backup carries an
    /// inline `modelCatalog` (DB SSOT) but a `config.toml` text WITHOUT a
    /// `model_catalog_json` pointer. Restoring that backup must project the
    /// inline catalog — (re)generating both the catalog file and the pointer —
    /// or the Codex model mapping vanishes from Live after takeover-off.
    #[tokio::test]
    #[serial]
    async fn codex_restore_from_backup_projects_inline_model_catalog() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        // Catalog projection needs a model template; seed `models_cache.json`
        // with the template slug so we don't depend on the `codex` CLI.
        let codex_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        std::fs::write(
            codex_dir.join("models_cache.json"),
            r#"{"models":[{"slug":"gpt-5.5"}]}"#,
        )
        .expect("seed models_cache template");

        // Provider-rebuilt backup shape: inline modelCatalog, pointer-less config.
        let backup_json = serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "deepseek-key" },
            "config": "model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\n\n[model_providers.custom]\nname = \"DeepSeek\"\nbase_url = \"https://api.deepseek.example/v1\"\nwire_api = \"responses\"\n",
            "modelCatalog": {
                "models": [
                    { "model": "deepseek-v4-flash", "displayName": "DeepSeek V4 Flash", "contextWindow": 1_000_000 }
                ]
            }
        }))
        .expect("serialize backup");
        db.save_live_backup("codex", &backup_json)
            .await
            .expect("seed live backup");
        write_json_file(
            &crate::codex_config::get_codex_auth_path(),
            &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
        )
        .expect("seed takeover placeholder auth");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex live from backup");

        let restored = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read restored config.toml");
        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        assert!(
            restored.contains("model_catalog_json"),
            "restore must (re)generate the model_catalog_json pointer from inline catalog, got:\n{restored}"
        );
        assert!(
            catalog_path.exists(),
            "restore must generate the cc-switch catalog file on disk"
        );
        let catalog: Value = serde_json::from_str(
            &std::fs::read_to_string(&catalog_path).expect("read generated catalog"),
        )
        .expect("parse generated catalog");
        let slugs: Vec<&str> = catalog
            .get("models")
            .and_then(|m| m.as_array())
            .expect("catalog models")
            .iter()
            .filter_map(|m| m.get("slug").and_then(|s| s.as_str()))
            .collect();
        assert!(
            slugs.contains(&"deepseek-v4-flash"),
            "generated catalog must contain the inline model, got slugs: {slugs:?}"
        );
    }

    /// Regression: a provider-rebuilt backup can pair an inline `modelCatalog`
    /// with EMPTY `auth.json` (`{}`) — the bearer-token / Mobile-compat shape
    /// where the API key lives in the config's `experimental_bearer_token`. The
    /// empty-auth restore branch deletes `auth.json` and writes config raw; it
    /// must still project the inline catalog (decision is orthogonal to auth), or
    /// the model mapping vanishes on takeover-off for this provider shape.
    #[tokio::test]
    #[serial]
    async fn codex_restore_empty_auth_backup_still_projects_inline_catalog() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        let codex_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        std::fs::write(
            codex_dir.join("models_cache.json"),
            r#"{"models":[{"slug":"gpt-5.5"}]}"#,
        )
        .expect("seed models_cache template");

        // Empty auth.json + key carried in config.toml's experimental_bearer_token,
        // plus the inline modelCatalog (DB SSOT).
        let backup_json = serde_json::to_string(&json!({
            "auth": {},
            "config": "model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\n\n[model_providers.custom]\nname = \"DeepSeek\"\nbase_url = \"https://api.deepseek.example/v1\"\nwire_api = \"responses\"\nexperimental_bearer_token = \"sk-deepseek\"\n",
            "modelCatalog": {
                "models": [ { "model": "deepseek-v4-flash", "displayName": "DeepSeek V4 Flash" } ]
            }
        }))
        .expect("serialize backup");
        db.save_live_backup("codex", &backup_json)
            .await
            .expect("seed live backup");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex live from backup");

        let restored = std::fs::read_to_string(crate::codex_config::get_codex_config_path())
            .expect("read restored config.toml");
        assert!(
            restored.contains("model_catalog_json"),
            "empty-auth restore must still project the inline catalog pointer, got:\n{restored}"
        );
        assert!(
            crate::codex_config::get_codex_model_catalog_path().exists(),
            "empty-auth restore must generate the cc-switch catalog file"
        );
        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "empty-auth restore must delete auth.json rather than write an empty one"
        );
    }

    /// Regression: when the backup row itself contains the proxy placeholder
    /// (a corrupted state where previous start/stop cycles saved the proxy
    /// config as the "original Live"), restore must NOT write it back to Live.
    /// It should fall through to the SSOT (current provider) path and rebuild
    /// Live from the provider DB instead.
    #[tokio::test]
    #[serial]
    async fn restore_falls_through_to_ssot_when_backup_is_proxy_placeholder() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        // Seed DB with a current provider that has a real API key
        let provider = Provider::with_id(
            "p1".to_string(),
            "P1".to_string(),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.minimaxi.com/anthropic",
                    "ANTHROPIC_API_KEY": "real-key-from-db"
                }
            }),
            None,
        );
        db.save_provider("claude", &provider)
            .expect("save provider");
        db.set_current_provider("claude", "p1")
            .expect("set current provider");

        // Seed backup with proxy placeholder (the corrupted state)
        let corrupted_backup = serde_json::to_string(&json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
            }
        }))
        .expect("serialize corrupted backup");
        db.save_live_backup("claude", &corrupted_backup)
            .await
            .expect("seed corrupted backup");

        // Seed Live with the same proxy placeholder (matches the corrupted state)
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
                }
            }))
            .expect("seed taken-over live file");

        // Restore: must NOT use the corrupted backup
        service
            .restore_live_config_for_app_with_fallback(&AppType::Claude)
            .await
            .expect("restore should succeed via SSOT");

        // The backup should still be the corrupted one (we didn't touch it on this path)
        let backup_after = db
            .get_live_backup("claude")
            .await
            .expect("get backup")
            .expect("backup still exists");
        assert_eq!(
            backup_after.original_config, corrupted_backup,
            "restore must NOT overwrite the corrupted backup"
        );

        // Live should now reflect the SSOT (provider DB), NOT the proxy URL
        let restored_live = service.read_claude_live().expect("read live");
        let restored_url = restored_live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(|v| v.as_str());
        assert_eq!(
            restored_url,
            Some("https://api.minimaxi.com/anthropic"),
            "Live must be rebuilt from SSOT, not from the corrupted backup"
        );
        let restored_key = restored_live
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_API_KEY"))
            .and_then(|v| v.as_str());
        assert_eq!(
            restored_key,
            Some("real-key-from-db"),
            "Live must carry the real API key from the provider DB"
        );
        assert_ne!(
            restored_live
                .get("env")
                .and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN"))
                .and_then(|v| v.as_str()),
            Some(PROXY_TOKEN_PLACEHOLDER),
            "Live must not still carry the proxy placeholder"
        );
    }

    /// Regression for #6277: the restore backup is a snapshot taken when
    /// takeover started. If the user logs into official ChatGPT DURING
    /// takeover, live auth.json holds OAuth tokens the backup never saw;
    /// restoring the backup verbatim on exit/crash-recovery would wipe the
    /// login every restart. Restore must keep the live OAuth login and demote
    /// the backup's API key into config.toml instead.
    #[tokio::test]
    #[serial]
    async fn restore_keeps_codex_oauth_login_when_backup_has_api_key_only() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        // Backup snapshot: third-party API-key shape (pre-login state)
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": { "OPENAI_API_KEY": "sk-third-party" },
                "config": r#"model_provider = "any"
model = "gpt-5"

[model_providers.any]
base_url = "https://third.example/v1"
"#
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        // Live: user logged into official ChatGPT during takeover
        crate::codex_config::write_codex_live_atomic(
            &json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "idt",
                    "access_token": "at",
                    "refresh_token": "rt",
                    "account_id": "acc"
                },
                "last_refresh": "2026-08-10T00:00:00Z"
            }),
            Some("model_provider = \"any\"\n"),
        )
        .expect("seed live codex files");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        let live = service.read_codex_live().expect("read live");
        let auth = live.get("auth").expect("auth present");
        assert_eq!(
            auth.get("tokens")
                .and_then(|t| t.get("refresh_token"))
                .and_then(|v| v.as_str()),
            Some("rt"),
            "official ChatGPT login must survive restore"
        );
        assert!(
            crate::codex_config::codex_auth_has_oauth_login_material(auth),
            "restored auth.json must keep OAuth login material"
        );

        let config_text = live
            .get("config")
            .and_then(|v| v.as_str())
            .expect("config text");
        assert!(
            config_text.contains("https://third.example/v1"),
            "backup config.toml must still be restored"
        );
        assert!(
            config_text.contains("sk-third-party"),
            "backup API key must be demoted into config.toml as experimental_bearer_token"
        );
    }

    /// The normal restore path (no login happened during takeover): live auth
    /// only carries the takeover placeholder, so the backup must be restored
    /// verbatim including its auth.json.
    #[tokio::test]
    #[serial]
    async fn restore_writes_codex_backup_auth_verbatim_when_live_has_no_oauth_login() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": { "OPENAI_API_KEY": "sk-third-party" },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        crate::codex_config::write_codex_live_atomic(
            &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
            Some("model_provider = \"any\"\n"),
        )
        .expect("seed live codex files");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        let live = service.read_codex_live().expect("read live");
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str()),
            Some("sk-third-party"),
            "without a live OAuth login the backup auth must be restored verbatim"
        );
    }

    #[tokio::test]
    #[serial]
    async fn restore_preserves_logged_out_official_codex_state() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let mut official = Provider::with_id(
            crate::database::CODEX_OFFICIAL_PROVIDER_ID.to_string(),
            "OpenAI Official".to_string(),
            json!({ "auth": {}, "config": "model = \"gpt-5.4\"\n" }),
            None,
        );
        official.category = Some("official".to_string());
        db.save_provider("codex", &official)
            .expect("save official provider");
        db.set_current_provider("codex", &official.id)
            .expect("set DB current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(&official.id))
            .expect("set local current provider");

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "auth_mode": "chatgpt",
                    "tokens": { "refresh_token": "stale-rt" }
                },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");
        crate::codex_config::write_codex_live_config_atomic(Some("model_provider = \"any\"\n"))
            .expect("seed logged-out live config");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "restore must not replay stale backup credentials after logout"
        );

        let auth_path = crate::codex_config::get_codex_auth_path();
        std::fs::write(&auth_path, b"{").expect("seed malformed live auth");
        let error = service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect_err("malformed live auth must stop restore");
        assert!(error.contains("读取 Codex auth 失败"));
        assert_eq!(
            std::fs::read(&auth_path).expect("read malformed auth"),
            b"{"
        );
    }

    #[tokio::test]
    #[serial]
    async fn restore_preserves_missing_codex_auth_when_current_provider_is_unknown() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "auth_mode": "chatgpt",
                    "tokens": { "refresh_token": "stale-rt" }
                },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");
        crate::codex_config::write_codex_live_config_atomic(Some("model = \"gpt-5.4\"\n"))
            .expect("seed config without auth");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "an unclassified backup must not replay credentials over missing live auth"
        );
    }

    #[test]
    #[serial]
    fn guarded_codex_restore_rejects_a_changed_auth_file() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let auth_path = crate::codex_config::get_codex_auth_path();
        write_json_file(
            &auth_path,
            &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
        )
        .expect("seed observed auth");
        let observed = CodexAuthFileSnapshot::capture().expect("capture auth");
        write_json_file(
            &auth_path,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": { "refresh_token": "new-rt" }
            }),
        )
        .expect("simulate concurrent login");

        let error = service
            .write_codex_live_verbatim_with_auth_guard(
                &json!({
                    "auth": { "OPENAI_API_KEY": "stale-key" },
                    "config": "model_provider = \"any\"\n"
                }),
                Some(&observed),
            )
            .expect_err("changed auth must cancel restore");

        assert!(error.contains("发生变化"));
        let current: Value = read_json_file(&auth_path).expect("read current auth");
        assert_eq!(
            current
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("new-rt")
        );
    }

    #[tokio::test]
    #[serial]
    async fn restore_empty_auth_without_config_keeps_auth_file_absent() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let provider = Provider::with_id(
            "third-party".to_string(),
            "Third Party".to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": "sk-third-party" },
                "config": "model_provider = \"third-party\"\n"
            }),
            None,
        );
        db.save_provider("codex", &provider)
            .expect("save current provider");
        db.set_current_provider("codex", &provider.id)
            .expect("set current provider");
        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {},
                "config": null
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore empty auth backup");

        assert!(
            !crate::codex_config::get_codex_auth_path().exists(),
            "an empty backup must preserve the missing-file logout state"
        );
    }

    #[test]
    #[serial]
    fn changed_auth_after_catalog_prepare_rolls_catalog_back() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        seed_codex_model_template();

        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        write_json_file(&catalog_path, &json!({ "models": ["old"] })).expect("seed catalog");
        let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture missing auth");

        let auth_path = crate::codex_config::get_codex_auth_path();
        write_json_file(
            &auth_path,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": { "refresh_token": "new-rt" }
            }),
        )
        .expect("simulate concurrent login");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let error = service
            .write_codex_live_verbatim_with_auth_guard(
                &json!({
                    "auth": { "OPENAI_API_KEY": "stale-key" },
                    "config": "model = \"model-a\"\n",
                    "modelCatalog": { "models": [{ "model": "model-a" }] }
                }),
                Some(&auth_snapshot),
            )
            .expect_err("changed auth must cancel the prepared restore");

        assert!(error.contains("发生变化"));
        let catalog: Value = read_json_file(&catalog_path).expect("read rolled-back catalog");
        assert_eq!(catalog, json!({ "models": ["old"] }));
        let auth: Value = read_json_file(&auth_path).expect("read concurrent auth");
        assert_eq!(
            auth.pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("new-rt")
        );
    }

    #[test]
    #[serial]
    fn guarded_restore_delete_preserves_login_created_after_auth_claim() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let auth_path = crate::codex_config::get_codex_auth_path();
        write_json_file(
            &auth_path,
            &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
        )
        .expect("seed takeover auth");
        let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture takeover auth");
        let mut transaction =
            CodexAuthFileTransaction::begin(&auth_snapshot).expect("claim takeover auth");

        write_json_file(
            &auth_path,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": { "refresh_token": "new-rt" }
            }),
        )
        .expect("simulate login after claim");
        transaction.install(None).expect("stage auth deletion");
        transaction.commit().expect("commit auth deletion");

        let current: Value = read_json_file(&auth_path).expect("read newer auth");
        assert_eq!(
            current
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("new-rt")
        );
    }

    #[test]
    #[serial]
    fn guarded_restore_write_rejects_login_created_after_auth_claim() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let auth_path = crate::codex_config::get_codex_auth_path();
        write_json_file(
            &auth_path,
            &json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER }),
        )
        .expect("seed takeover auth");
        let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture takeover auth");
        let mut transaction =
            CodexAuthFileTransaction::begin(&auth_snapshot).expect("claim takeover auth");

        write_json_file(
            &auth_path,
            &json!({
                "auth_mode": "chatgpt",
                "tokens": { "refresh_token": "new-rt" }
            }),
        )
        .expect("simulate login after claim");
        transaction
            .install(Some(br#"{"OPENAI_API_KEY":"stale-key"}"#.to_vec()))
            .expect_err("newer login must win the no-clobber install");

        let current: Value = read_json_file(&auth_path).expect("read newer auth");
        assert_eq!(
            current
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("new-rt")
        );
    }

    #[test]
    #[serial]
    fn guarded_restore_final_config_failure_rolls_back_auth_and_catalog() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");
        seed_codex_model_template();

        let auth_path = crate::codex_config::get_codex_auth_path();
        let original_auth = json!({ "OPENAI_API_KEY": PROXY_TOKEN_PLACEHOLDER });
        write_json_file(&auth_path, &original_auth).expect("seed takeover auth");
        let auth_snapshot = CodexAuthFileSnapshot::capture().expect("capture takeover auth");

        let catalog_path = crate::codex_config::get_codex_model_catalog_path();
        let original_catalog = json!({ "models": ["old"] });
        write_json_file(&catalog_path, &original_catalog).expect("seed catalog");

        let config_path = crate::codex_config::get_codex_config_path();
        std::fs::create_dir(&config_path).expect("make config target unwritable as a file");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db);
        let error = service
            .write_codex_live_verbatim_with_auth_guard(
                &json!({
                    "auth": { "OPENAI_API_KEY": "restored-key" },
                    "config": "model = \"model-a\"\n",
                    "modelCatalog": { "models": [{ "model": "model-a" }] }
                }),
                Some(&auth_snapshot),
            )
            .expect_err("config write must fail");

        assert!(error.contains("config"));
        let auth: Value = read_json_file(&auth_path).expect("read rolled-back auth");
        assert_eq!(auth, original_auth);
        let catalog: Value = read_json_file(&catalog_path).expect("read rolled-back catalog");
        assert_eq!(catalog, original_catalog);
    }

    /// Live auth.json can advance through Codex login/token refresh or a managed
    /// account switch. That state is newer than the takeover-start snapshot, so
    /// even an official-shape backup must not roll live tokens back. The auth
    /// check must not depend on config.toml parsing.
    #[tokio::test]
    #[serial]
    async fn restore_prefers_live_codex_oauth_tokens_over_backup_snapshot() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "auth_mode": "chatgpt",
                    "OPENAI_API_KEY": null,
                    "tokens": { "refresh_token": "backup-rt" }
                },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        crate::codex_config::write_codex_live_atomic(
            &json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": { "refresh_token": "live-rt" }
            }),
            Some("model_provider = \"any\"\n"),
        )
        .expect("seed live codex files");
        crate::config::write_text_file(
            &crate::codex_config::get_codex_config_path(),
            "model_provider = [",
        )
        .expect("corrupt live config after writing valid auth");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        let live = service.read_codex_live().expect("read live");
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("tokens"))
                .and_then(|t| t.get("refresh_token"))
                .and_then(|v| v.as_str()),
            Some("live-rt"),
            "restore must not roll live tokens back to the takeover-start snapshot"
        );
    }

    /// Regression for #6277 with the metadata-residue backup shape: an
    /// `OPENAI_API_KEY` accompanied by `last_refresh` / `tokens.account_id`
    /// is NOT a login and must not be restored over a real live login.
    #[tokio::test]
    #[serial]
    async fn restore_keeps_codex_oauth_login_when_backup_key_carries_metadata_residue() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": {
                    "OPENAI_API_KEY": "sk-third-party",
                    "last_refresh": "2026-08-01T00:00:00Z",
                    "tokens": { "account_id": "acc" }
                },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        crate::codex_config::write_codex_live_atomic(
            &json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": { "refresh_token": "rt" }
            }),
            Some("model_provider = \"any\"\n"),
        )
        .expect("seed live codex files");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        let live = service.read_codex_live().expect("read live");
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("tokens"))
                .and_then(|t| t.get("refresh_token"))
                .and_then(|v| v.as_str()),
            Some("rt"),
            "metadata residue must not shield the backup API key from demotion"
        );
        assert!(
            live.get("config")
                .and_then(|v| v.as_str())
                .is_some_and(|cfg| cfg.contains("sk-third-party")),
            "backup API key must still be demoted into config.toml"
        );
    }

    /// Metadata residue on the LIVE side is not a login either: restore must
    /// still write the backup auth verbatim instead of protecting stale
    /// `last_refresh` / `tokens.account_id` leftovers.
    #[tokio::test]
    #[serial]
    async fn restore_writes_codex_backup_auth_when_live_has_only_metadata_residue() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": { "OPENAI_API_KEY": "sk-restored" },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        crate::codex_config::write_codex_live_atomic(
            &json!({
                "OPENAI_API_KEY": "sk-stale",
                "last_refresh": "2026-08-01T00:00:00Z",
                "tokens": { "account_id": "acc" }
            }),
            Some("model_provider = \"any\"\n"),
        )
        .expect("seed live codex files");

        service
            .restore_live_config_for_app_with_fallback(&AppType::Codex)
            .await
            .expect("restore codex");

        let live = service.read_codex_live().expect("read live");
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str()),
            Some("sk-restored"),
            "live metadata residue is not a login and must not block the restore"
        );
    }

    /// The simple restore path (`restore_live_config_for_app_inner`, used by
    /// takeover rebuild and takeover-failure rollback) must apply the same
    /// OAuth-login protection as the with_fallback path.
    #[tokio::test]
    #[serial]
    async fn simple_restore_path_keeps_codex_oauth_login() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        db.save_live_backup(
            "codex",
            &serde_json::to_string(&json!({
                "auth": { "OPENAI_API_KEY": "sk-third-party" },
                "config": "model_provider = \"any\"\n"
            }))
            .expect("serialize backup"),
        )
        .await
        .expect("seed live backup");

        crate::codex_config::write_codex_live_atomic(
            &json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": { "refresh_token": "rt" }
            }),
            Some("model_provider = \"any\"\n"),
        )
        .expect("seed live codex files");

        service
            .restore_live_config_for_app_inner(&AppType::Codex)
            .await
            .expect("restore codex via simple path");

        let live = service.read_codex_live().expect("read live");
        assert_eq!(
            live.get("auth")
                .and_then(|auth| auth.get("tokens"))
                .and_then(|t| t.get("refresh_token"))
                .and_then(|v| v.as_str()),
            Some("rt"),
            "takeover rebuild / rollback restore must not wipe the ChatGPT login"
        );
        assert!(
            live.get("config")
                .and_then(|v| v.as_str())
                .is_some_and(|cfg| cfg.contains("sk-third-party")),
            "backup API key must be demoted into config.toml on the simple path too"
        );
    }

    /// Regression: when Live is already a proxy placeholder (a corrupted state
    /// where previous stop failed to restore), backup must NOT overwrite a
    /// previously-good backup with the proxy config. This prevents the bug
    /// where stop-then-start cycles permanently corrupt the backup.
    #[tokio::test]
    #[serial]
    async fn backup_skips_when_live_is_already_proxy_placeholder() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        // Seed a GOOD backup (the "real" original Live)
        let good_backup = serde_json::to_string(&json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://api.minimaxi.com/anthropic",
                "ANTHROPIC_AUTH_TOKEN": "real-token"
            }
        }))
        .expect("serialize good backup");
        db.save_live_backup("claude", &good_backup)
            .await
            .expect("seed good backup");

        // Seed Live with proxy placeholder (the corrupted state)
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
                }
            }))
            .expect("seed taken-over live file");

        // Call backup_live_config_strict: must skip
        service
            .backup_live_config_strict(&AppType::Claude)
            .await
            .expect("backup should succeed (no-op when live is placeholder)");

        // The good backup must still be intact
        let backup_after = db
            .get_live_backup("claude")
            .await
            .expect("get backup")
            .expect("backup still exists");
        assert_eq!(
            backup_after.original_config, good_backup,
            "must not overwrite a good backup with a proxy placeholder"
        );
    }

    /// Regression: when ALL apps have Live=proxy-placeholder (worst-case
    /// corrupted state), the bulk `backup_live_configs` path used by
    /// `start_with_takeover` must skip every save — instead of overwriting
    /// good backups with the proxy config.
    #[tokio::test]
    #[serial]
    async fn bulk_backup_skips_all_when_live_is_proxy_placeholder() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());

        // Seed good backups for all three apps
        let good_backup = serde_json::to_string(&json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "real-token"
            }
        }))
        .expect("serialize good backup");
        db.save_live_backup("claude", &good_backup)
            .await
            .expect("seed claude backup");

        let codex_good_backup = serde_json::to_string(&json!({
            "auth": { "OPENAI_API_KEY": "real-codex-token" }
        }))
        .expect("serialize codex good backup");
        db.save_live_backup("codex", &codex_good_backup)
            .await
            .expect("seed codex backup");

        let gemini_good_backup = serde_json::to_string(&json!({
            "env": { "GEMINI_API_KEY": "real-gemini-key" }
        }))
        .expect("serialize gemini good backup");
        db.save_live_backup("gemini", &gemini_good_backup)
            .await
            .expect("seed gemini backup");

        // Seed all three Live files with proxy placeholders
        service
            .write_claude_live(&json!({
                "env": {
                    "ANTHROPIC_AUTH_TOKEN": PROXY_TOKEN_PLACEHOLDER,
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:15721"
                }
            }))
            .expect("seed claude live");
        let codex_dir = crate::codex_config::get_codex_config_dir();
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        std::fs::write(
            crate::codex_config::get_codex_config_path(),
            r#"model_provider = "custom"

[model_providers.custom]
name = "Custom"
base_url = "http://127.0.0.1:15721/v1"
wire_api = "chat"
experimental_bearer_token = "PROXY_MANAGED"
"#,
        )
        .expect("seed codex config.toml");
        std::fs::write(
            crate::codex_config::get_codex_auth_path(),
            r#"{"OPENAI_API_KEY":"PROXY_MANAGED"}"#,
        )
        .expect("seed codex auth.json");
        let gemini_env_path = crate::gemini_config::get_gemini_env_path();
        if let Some(parent) = gemini_env_path.parent() {
            std::fs::create_dir_all(parent).expect("create gemini dir");
        }
        std::fs::write(&gemini_env_path, "GEMINI_API_KEY=PROXY_MANAGED\n")
            .expect("seed gemini env");

        // Call bulk backup: must skip all three apps
        service
            .backup_live_configs()
            .await
            .expect("bulk backup should succeed (no-op when all live are placeholders)");

        // All three good backups must still be intact
        for (app_type, original) in [
            ("claude", good_backup.as_str()),
            ("codex", codex_good_backup.as_str()),
            ("gemini", gemini_good_backup.as_str()),
        ] {
            let backup_after = db
                .get_live_backup(app_type)
                .await
                .expect("get backup")
                .expect("backup still exists");
            assert_eq!(
                backup_after.original_config, original,
                "must not overwrite good backup for {app_type} with proxy placeholder"
            );
        }
    }

    fn grok_provider_config(base_url: &str, api_key: &str) -> Value {
        json!({
            "config": format!(
                "[models]\ndefault = \"grok-4.5\"\n\n[model.\"grok-4.5\"]\nmodel = \"grok-4.5\"\nbase_url = \"{base_url}\"\nname = \"Grok\"\napi_key = \"{api_key}\"\napi_backend = \"responses\"\ncontext_window = 500000\n"
            )
        })
    }

    #[tokio::test]
    #[serial]
    async fn hot_switch_grokbuild_updates_backup_and_current_provider() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let provider_a = Provider::with_id(
            "grok-a".to_string(),
            "Grok A".to_string(),
            grok_provider_config("https://a.example.com/v1", "a-key"),
            None,
        );
        let provider_b = Provider::with_id(
            "grok-b".to_string(),
            "Grok B".to_string(),
            grok_provider_config("https://b.example.com/v1", "b-key"),
            None,
        );
        db.save_provider("grokbuild", &provider_a)
            .expect("save provider a");
        db.save_provider("grokbuild", &provider_b)
            .expect("save provider b");
        db.set_current_provider("grokbuild", "grok-a")
            .expect("set db current");
        crate::settings::set_current_provider(&AppType::GrokBuild, Some("grok-a"))
            .expect("set local current");
        let mut original_settings = provider_a.settings_config.clone();
        original_settings["config"] = json!(format!(
            "{}\n[mcp_servers.demo]\ncommand = \"demo\"\n",
            original_settings["config"]
                .as_str()
                .expect("provider config")
        ));
        db.save_live_backup(
            "grokbuild",
            &serde_json::to_string(&original_settings).expect("serialize backup"),
        )
        .await
        .expect("seed backup");

        service
            .hot_switch_provider("grokbuild", "grok-b")
            .await
            .expect("hot switch Grok Build");

        assert_eq!(
            crate::settings::get_effective_current_provider(&db, &AppType::GrokBuild)
                .expect("read current")
                .as_deref(),
            Some("grok-b")
        );
        let backup = db
            .get_live_backup("grokbuild")
            .await
            .expect("read backup")
            .expect("backup exists");
        let backup: Value = serde_json::from_str(&backup.original_config).expect("parse backup");
        assert!(backup["config"]
            .as_str()
            .is_some_and(|config| config.contains("https://b.example.com/v1")));
        assert!(backup["config"]
            .as_str()
            .is_some_and(|config| config.contains("[mcp_servers.demo]")));
    }

    #[tokio::test]
    #[serial]
    async fn failed_hot_switch_grokbuild_keeps_previous_current_and_backup() {
        let _home = TempHome::new();
        crate::settings::reload_settings().expect("reload settings");

        let db = Arc::new(Database::memory().expect("init db"));
        let service = ProxyService::new(db.clone());
        let provider_a = Provider::with_id(
            "grok-a".to_string(),
            "Grok A".to_string(),
            grok_provider_config("https://a.example.com/v1", "a-key"),
            None,
        );
        let provider_b = Provider::with_id(
            "grok-b".to_string(),
            "Broken Grok".to_string(),
            json!({ "config": "not valid toml = [" }),
            None,
        );
        db.save_provider("grokbuild", &provider_a)
            .expect("save provider a");
        db.save_provider("grokbuild", &provider_b)
            .expect("save provider b");
        db.set_current_provider("grokbuild", "grok-a")
            .expect("set db current");
        crate::settings::set_current_provider(&AppType::GrokBuild, Some("grok-a"))
            .expect("set local current");

        let original_backup =
            serde_json::to_string(&provider_a.settings_config).expect("serialize backup");
        db.save_live_backup("grokbuild", &original_backup)
            .await
            .expect("seed backup");
        let takeover = crate::grok_config::apply_proxy_takeover(
            provider_a.settings_config["config"]
                .as_str()
                .expect("provider config"),
            "http://127.0.0.1:15721/grokbuild/v1",
            PROXY_TOKEN_PLACEHOLDER,
        )
        .expect("build takeover config");
        service
            .write_grok_live(&json!({ "config": takeover }))
            .expect("seed taken-over live");

        service
            .hot_switch_provider("grokbuild", "grok-b")
            .await
            .expect_err("invalid Grok config must fail");

        assert_eq!(
            crate::settings::get_effective_current_provider(&db, &AppType::GrokBuild)
                .expect("read current")
                .as_deref(),
            Some("grok-a")
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::GrokBuild).as_deref(),
            Some("grok-a")
        );
        let backup = db
            .get_live_backup("grokbuild")
            .await
            .expect("read backup")
            .expect("backup exists");
        assert_eq!(backup.original_config, original_backup);
    }
}
