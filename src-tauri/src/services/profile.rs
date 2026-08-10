//! 项目 Profile 编排服务
//!
//! Profile 是**全应用共享的项目实体**（用户拥有的项目就那几个），payload
//! 按 app 分槽存配置快照（供应商 / MCP / Skills / Prompt）。快照与应用
//! 均**按分组（scope）操作**：Claude Code 与 Codex 的工作目录往往不同
//! （各在各的项目里），因此各组独立指向自己的当前项目、只拍/只应用组内
//! 槽位，互不牵连；重命名/删除作用于共享实体本身。
//! 应用（apply）时复用现有切换原语批量落地：
//! - 供应商：`ProviderService::switch`（内建代理接管热切换与接管下禁切官方）
//! - MCP：`McpService::toggle_app`（改标志 + 单 server 物化）
//! - Skills：`SkillService::toggle_app`（改标志 + 单 skill 物化）
//! - Prompt：`PromptService::enable_prompt`（互斥激活 + 原子写 live）
//!
//! Provider/MCP/Skill/Prompt 单项失败保持 best-effort warning；Codex Agent
//! Role 投影是 Profile 提交门，失败时回切 Provider 并保留旧 current Profile。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::app_config::AppType;
use crate::config::atomic_write;
use crate::database::Profile;
use crate::error::AppError;
use crate::prompt::Prompt;
use crate::proxy::{AppProxyConfig, LiveBackup};
use crate::services::{McpService, PromptService, ProviderService, SkillService};
use crate::store::AppState;

static PROFILE_MUTATION_LOCK: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));
static PROFILE_SCOPE_LOCKS: Lazy<[AsyncMutex<()>; 3]> = Lazy::new(|| {
    [
        AsyncMutex::new(()),
        AsyncMutex::new(()),
        AsyncMutex::new(()),
    ]
});

#[cfg(test)]
static FAIL_NEXT_PROFILE_COMMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static PROFILE_APPLY_TEST_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static PROFILE_APPLY_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PROFILE_APPLY_MAX_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Profile 操作的应用分组：项目实体全应用共享，但快照/应用/当前指针按组进行。
///
/// Claude Code 与 Claude Desktop 的供应商在 cc-switch 中是独立切换的，
/// 因此各自拥有独立的项目分组。两者 live 文件零交集
///（`~/.claude` / `Application Support/Claude-3p`），分组切换互不干扰。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileScope {
    Claude,
    #[serde(rename = "claude-desktop")]
    ClaudeDesktop,
    Codex,
}

impl ProfileScope {
    /// 全部分组（扩展新分组时同步扩展 apps/for_app 与前端 scope.ts 镜像）
    pub const ALL: [ProfileScope; 3] = [
        ProfileScope::Claude,
        ProfileScope::ClaudeDesktop,
        ProfileScope::Codex,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileScope::Claude => "claude",
            ProfileScope::ClaudeDesktop => "claude-desktop",
            ProfileScope::Codex => "codex",
        }
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "claude" => Ok(ProfileScope::Claude),
            "claude-desktop" => Ok(ProfileScope::ClaudeDesktop),
            "codex" => Ok(ProfileScope::Codex),
            other => Err(AppError::InvalidInput(format!(
                "Unknown profile scope: {other}"
            ))),
        }
    }

    fn lock_index(self) -> usize {
        match self {
            ProfileScope::Claude => 0,
            ProfileScope::ClaudeDesktop => 1,
            ProfileScope::Codex => 2,
        }
    }

    /// 组内受管应用（快照与 apply 只作用于这些 app 的槽位）
    pub fn apps(&self) -> &'static [AppType] {
        match self {
            ProfileScope::Claude => &[AppType::Claude],
            ProfileScope::ClaudeDesktop => &[AppType::ClaudeDesktop],
            ProfileScope::Codex => &[AppType::Codex],
        }
    }

    /// 应用页 → 所属分组（Profile 不支持的应用返回 None）
    pub fn for_app(app: &AppType) -> Option<Self> {
        match app {
            AppType::Claude => Some(ProfileScope::Claude),
            AppType::ClaudeDesktop => Some(ProfileScope::ClaudeDesktop),
            AppType::Codex => Some(ProfileScope::Codex),
            _ => None,
        }
    }
}

/// 按 app 分槽的载荷容器；字段名与 AppType 的 serde 形式一致
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerApp<T> {
    pub claude: T,
    #[serde(rename = "claude-desktop")]
    pub claude_desktop: T,
    pub codex: T,
}

impl<T> PerApp<T> {
    pub fn get(&self, app: &AppType) -> Option<&T> {
        match app {
            AppType::Claude => Some(&self.claude),
            AppType::ClaudeDesktop => Some(&self.claude_desktop),
            AppType::Codex => Some(&self.codex),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, app: &AppType) -> Option<&mut T> {
        match app {
            AppType::Claude => Some(&mut self.claude),
            AppType::ClaudeDesktop => Some(&mut self.claude_desktop),
            AppType::Codex => Some(&mut self.codex),
            _ => None,
        }
    }
}

/// Profile 的 JSON 快照结构（与前端 TS 类型严格对应）
///
/// 所有槽位都是 Option：None = 该侧从未拍过快照（应用时不动），
/// 与"拍到的就是空集/无激活项"（Some(空)，应用时清空启用）严格区分——
/// 在 Codex 页选中一个只在 Claude 页建过的项目不能误清 Codex 的启用状态。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfilePayload {
    /// 每 app 的当前供应商 id
    pub providers: PerApp<Option<String>>,
    /// 每 app 启用的 MCP server id 集合
    pub mcp: PerApp<Option<Vec<String>>>,
    /// 每 app 启用的 Skill id 集合
    pub skills: PerApp<Option<Vec<String>>>,
    /// 每 app 激活的 prompt id
    pub prompts: PerApp<Option<String>>,
}

impl ProfilePayload {
    /// 用另一份快照覆盖本载荷中某分组的槽位，其余分组原样保留
    /// （"以当前状态更新"只更新发起页所属分组，避免把别的应用
    /// 正处于其他项目的状态串进来）
    pub fn merge_scope_from(&mut self, other: &ProfilePayload, scope: ProfileScope) {
        for app in scope.apps() {
            if let (Some(dst), Some(src)) = (self.providers.get_mut(app), other.providers.get(app))
            {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.mcp.get_mut(app), other.mcp.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.skills.get_mut(app), other.skills.get(app)) {
                *dst = src.clone();
            }
            if let (Some(dst), Some(src)) = (self.prompts.get_mut(app), other.prompts.get(app)) {
                *dst = src.clone();
            }
        }
    }

    /// 某分组是否拍过快照（任一槽位非 None 即视为拍过）
    pub fn scope_captured(&self, scope: ProfileScope) -> bool {
        scope.apps().iter().any(|app| {
            self.providers.get(app).is_some_and(|s| s.is_some())
                || self.mcp.get(app).is_some_and(|s| s.is_some())
                || self.skills.get(app).is_some_and(|s| s.is_some())
                || self.prompts.get(app).is_some_and(|s| s.is_some())
        })
    }
}

/// 计算从当前启用状态到目标集合的最小 toggle 集
///
/// 返回 (需要执行的 (id, enabled) 列表, payload 中已不存在于 DB 的悬空 id 列表)
fn plan_toggles(
    current: &[(String, bool)],
    target_ids: &[String],
) -> (Vec<(String, bool)>, Vec<String>) {
    let existing: HashSet<&str> = current.iter().map(|(id, _)| id.as_str()).collect();
    let target: HashSet<&str> = target_ids.iter().map(|s| s.as_str()).collect();

    let toggles = current
        .iter()
        .filter(|(id, enabled)| target.contains(id.as_str()) != *enabled)
        .map(|(id, enabled)| (id.clone(), !enabled))
        .collect();

    let dangling = target_ids
        .iter()
        .filter(|id| !existing.contains(id.as_str()))
        .cloned()
        .collect();

    (toggles, dangling)
}

pub struct ProfileService;

#[derive(Debug, Clone)]
struct ProviderSelectionSnapshot {
    app: AppType,
    local: Option<String>,
    database: Option<String>,
    effective: Option<String>,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

impl FileSnapshot {
    fn capture(path: impl Into<PathBuf>) -> Result<Self, AppError> {
        let path = path.into();
        let content = match std::fs::read(&path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(AppError::io(&path, error)),
        };
        Ok(Self { path, content })
    }

    fn restore(&self) -> Result<(), AppError> {
        match self.content.as_deref() {
            Some(content) => atomic_write(&self.path, content),
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(AppError::io(&self.path, error)),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct SyncScopeTransactionSnapshot {
    scope: ProfileScope,
    provider: ProviderSelectionSnapshot,
    payload: ProfilePayload,
    profiles: Vec<Profile>,
    prompts: Vec<Prompt>,
    prompt_file: Option<FileSnapshot>,
    codex_files: Vec<FileSnapshot>,
    current_profile_id: Option<String>,
}

#[derive(Debug)]
struct ScopeTransactionSnapshot {
    sync: SyncScopeTransactionSnapshot,
    app_proxy_config: AppProxyConfig,
    live_backup: Option<LiveBackup>,
    proxy_was_running: bool,
}

#[derive(Debug)]
struct PreparedProfileApply {
    warnings: Vec<String>,
    profile_id: String,
    scope: ProfileScope,
}

#[cfg(test)]
struct ProfileApplyActivity;

#[cfg(test)]
impl ProfileApplyActivity {
    fn enter() -> Self {
        use std::sync::atomic::Ordering;
        let active = PROFILE_APPLY_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        PROFILE_APPLY_MAX_ACTIVE.fetch_max(active, Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for ProfileApplyActivity {
    fn drop(&mut self) {
        PROFILE_APPLY_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ProfileService {
    fn owned_state(state: &AppState) -> Arc<AppState> {
        state.owned_clone()
    }

    fn effective_current_provider_without_cleanup(
        state: &AppState,
        app: &AppType,
    ) -> Result<Option<String>, AppError> {
        let local = crate::settings::get_current_provider(app);
        let database = state.db.get_current_provider(app.as_str())?;
        if let Some(local_id) = local {
            let providers = state.db.get_all_providers(app.as_str())?;
            if providers.contains_key(&local_id) {
                return Ok(Some(local_id));
            }
        }
        Ok(database)
    }

    /// 抓取分组内应用的当前配置状态生成快照（组外槽位保持默认值）
    pub fn snapshot_current(
        state: &AppState,
        scope: ProfileScope,
    ) -> Result<ProfilePayload, AppError> {
        let mut payload = ProfilePayload::default();
        let mcp_servers = state.db.get_all_mcp_servers()?;
        let skills = state.db.get_all_installed_skills()?;

        for app in scope.apps().iter() {
            if let Some(slot) = payload.providers.get_mut(app) {
                *slot = Self::effective_current_provider_without_cleanup(state, app)?;
            }
            if let Some(slot) = payload.mcp.get_mut(app) {
                *slot = Some(
                    mcp_servers
                        .values()
                        .filter(|s| s.apps.is_enabled_for(app))
                        .map(|s| s.id.clone())
                        .collect(),
                );
            }
            if let Some(slot) = payload.skills.get_mut(app) {
                *slot = Some(
                    skills
                        .values()
                        .filter(|s| s.apps.is_enabled_for(app))
                        .map(|s| s.id.clone())
                        .collect(),
                );
            }
            if let Some(slot) = payload.prompts.get_mut(app) {
                *slot = state
                    .db
                    .get_prompts(app.as_str())?
                    .values()
                    .find(|p| p.enabled)
                    .map(|p| p.id.clone());
            }
        }
        Ok(payload)
    }

    /// 列出所有项目（项目实体全应用共享，current 标记按分组单独读取）
    pub fn list(state: &AppState) -> Result<Vec<Profile>, AppError> {
        state.db.get_all_profiles()
    }

    /// 创建新项目：只拍发起页所属分组的当前状态，其余分组槽位留 None
    /// （其他应用可能正处于别的项目，不能替用户拍进来）
    fn create_unlocked(
        state: &AppState,
        name: &str,
        scope: ProfileScope,
    ) -> Result<Profile, AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::InvalidInput("Profile name is empty".to_string()));
        }
        let payload = Self::snapshot_current(state, scope)?;
        let now = chrono::Utc::now().timestamp();
        let profile = Profile {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            payload: serde_json::to_string(&payload)
                .map_err(|e| AppError::Config(format!("序列化 profile payload 失败: {e}")))?,
            sort_order: None,
            created_at: Some(now),
            updated_at: Some(now),
        };
        state.db.save_profile(&profile)?;
        Ok(profile)
    }

    pub async fn create_async(
        state: Arc<AppState>,
        name: String,
        scope: ProfileScope,
    ) -> Result<Profile, AppError> {
        let _mutation_guard = PROFILE_MUTATION_LOCK.lock().await;
        let _scope_guard = PROFILE_SCOPE_LOCKS[scope.lock_index()].lock().await;
        let _codex_lifecycle_guard = if matches!(scope, ProfileScope::Codex) {
            Some(state.lock_codex_provider_lifecycle().await)
        } else {
            None
        };
        tokio::task::spawn_blocking(move || Self::create_unlocked(&state, &name, scope))
            .await
            .map_err(|error| Self::join_error("create", error))?
    }

    pub fn create(state: &AppState, name: &str, scope: ProfileScope) -> Result<Profile, AppError> {
        tauri::async_runtime::block_on(Self::create_async(
            Self::owned_state(state),
            name.to_string(),
            scope,
        ))
    }

    /// 更新项目：重命名（作用于共享实体）和/或以当前状态重拍快照
    /// （resnapshot 只覆盖 scope 分组的槽位，其余分组原样保留；
    /// 快照重拍仅由 [`Self::apply`] 切换前的自动保存触发，UI 不再暴露手动入口）
    fn update_unlocked(
        state: &AppState,
        id: &str,
        name: Option<String>,
        resnapshot: bool,
        scope: Option<ProfileScope>,
    ) -> Result<Profile, AppError> {
        let mut profile = state
            .db
            .get_profile(id)?
            .ok_or_else(|| AppError::InvalidInput(format!("Profile not found: {id}")))?;

        if let Some(name) = name {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(AppError::InvalidInput("Profile name is empty".to_string()));
            }
            profile.name = name;
        }
        if resnapshot {
            let scope = scope.ok_or_else(|| {
                AppError::InvalidInput("Resnapshot requires a profile scope".to_string())
            })?;
            let mut payload: ProfilePayload = serde_json::from_str(&profile.payload)
                .map_err(|e| AppError::Config(format!("解析 profile payload 失败: {e}")))?;
            payload.merge_scope_from(&Self::snapshot_current(state, scope)?, scope);
            profile.payload = serde_json::to_string(&payload)
                .map_err(|e| AppError::Config(format!("序列化 profile payload 失败: {e}")))?;
        }
        profile.updated_at = Some(chrono::Utc::now().timestamp());
        state.db.save_profile(&profile)?;
        Ok(profile)
    }

    pub async fn update_async(
        state: Arc<AppState>,
        id: String,
        name: Option<String>,
        resnapshot: bool,
        scope: Option<ProfileScope>,
    ) -> Result<Profile, AppError> {
        let _mutation_guard = PROFILE_MUTATION_LOCK.lock().await;
        let _scope_guard = match scope {
            Some(scope) => Some(PROFILE_SCOPE_LOCKS[scope.lock_index()].lock().await),
            None => None,
        };
        let _codex_lifecycle_guard = if resnapshot && matches!(scope, Some(ProfileScope::Codex)) {
            Some(state.lock_codex_provider_lifecycle().await)
        } else {
            None
        };
        tokio::task::spawn_blocking(move || {
            Self::update_unlocked(&state, &id, name, resnapshot, scope)
        })
        .await
        .map_err(|error| Self::join_error("update", error))?
    }

    pub fn update(
        state: &AppState,
        id: &str,
        name: Option<String>,
        resnapshot: bool,
        scope: Option<ProfileScope>,
    ) -> Result<Profile, AppError> {
        tauri::async_runtime::block_on(Self::update_async(
            Self::owned_state(state),
            id.to_string(),
            name,
            resnapshot,
            scope,
        ))
    }

    /// 删除项目；若删除的是某分组当前激活项目，一并清除该分组的激活标记
    fn delete_unlocked(state: &AppState, id: &str) -> Result<(), AppError> {
        state.db.delete_profile(id)?;
        for scope in ProfileScope::ALL {
            if state.db.get_current_profile_id(scope.as_str())?.as_deref() == Some(id) {
                state.db.set_current_profile_id(scope.as_str(), None)?;
            }
        }
        Ok(())
    }

    pub async fn delete_async(state: Arc<AppState>, id: String) -> Result<(), AppError> {
        // Profiles are shared entities. The global mutation lock is the safety
        // boundary against apply/resnapshot/current/payload changes in every scope.
        let _mutation_guard = PROFILE_MUTATION_LOCK.lock().await;
        tokio::task::spawn_blocking(move || Self::delete_unlocked(&state, &id))
            .await
            .map_err(|error| Self::join_error("delete", error))?
    }

    pub fn delete(state: &AppState, id: &str) -> Result<(), AppError> {
        tauri::async_runtime::block_on(Self::delete_async(Self::owned_state(state), id.to_string()))
    }

    pub async fn clear_current_async(
        state: Arc<AppState>,
        scope: ProfileScope,
    ) -> Result<(), AppError> {
        let _mutation_guard = PROFILE_MUTATION_LOCK.lock().await;
        let _scope_guard = PROFILE_SCOPE_LOCKS[scope.lock_index()].lock().await;
        tokio::task::spawn_blocking(move || state.db.set_current_profile_id(scope.as_str(), None))
            .await
            .map_err(|error| Self::join_error("clear current", error))?
    }

    /// 准备应用项目快照（单项 best-effort，返回 warnings）
    ///
    /// 只作用于发起页所属分组内的应用，不碰其他分组的配置与 current 标记。
    /// 该分组从未拍过快照时不改动任何配置，仅标记 current 并返回提示
    /// （下次从该项目切走时，自动保存会补拍该侧快照）。
    ///
    /// **切换前会自动保存旧项目**：若当前分组已绑定到另一个项目，先把当前
    /// 状态写入那个旧项目（仅当前分组槽位），再加载目标项目。这样切走后
    /// 旧项目仍保留离开时的配置，回来时状态一致。自动保存失败时作为 warning
    /// 继续，不阻塞切换。
    ///
    /// 应用指定项目的快照到当前分组内的所有应用。
    ///
    /// 返回 `(warnings, should_stop_proxy)`：当当前分组内所有接管都被关闭、且
    /// 其它应用也没有接管时，建议调用者停止代理服务，以便 Claude Desktop 的
    /// "本地路由"总开关同步显示为关闭。
    fn prepare_apply(
        state: &AppState,
        profile_id: &str,
        scope: ProfileScope,
    ) -> Result<PreparedProfileApply, AppError> {
        let mut warnings = Vec::new();

        // 自动保存旧项目当前状态（仅当前分组），失败不阻塞切换
        if let Some(current_id) = state.db.get_current_profile_id(scope.as_str())? {
            if current_id != profile_id {
                if let Err(e) = Self::update_unlocked(state, &current_id, None, true, Some(scope)) {
                    warnings.push(format!(
                        "autosave profile '{current_id}' before switch failed: {e}"
                    ));
                }
            }
        }

        let profile = state
            .db
            .get_profile(profile_id)?
            .ok_or_else(|| AppError::InvalidInput(format!("Profile not found: {profile_id}")))?;
        let payload: ProfilePayload = serde_json::from_str(&profile.payload)
            .map_err(|e| AppError::Config(format!("解析 profile payload 失败: {e}")))?;

        if !payload.scope_captured(scope) {
            warnings.push(format!(
                "no {} configuration captured in this project yet; marked as current without changes (it will be saved automatically when you switch away)",
                scope.as_str()
            ));
        }

        for app in scope.apps().iter() {
            let app_str = app.as_str();

            // 1. 切换项目前无条件关闭当前应用的代理接管。
            // 接管态下 live 文件属于代理；用户希望切换工作目录时总是退出当前
            // 代理环境，再按快照写入真实供应商配置。
            if let Err(e) = state.proxy_service.disable_takeover_for_app_sync(app) {
                warnings.push(format!(
                    "[{app_str}] auto-disable proxy takeover before profile switch failed: {e}"
                ));
            }

            // 2. 供应商
            if let Some(Some(target_pid)) = payload.providers.get(app) {
                let providers = state.db.get_all_providers(app_str)?;
                if !providers.contains_key(target_pid) {
                    warnings.push(format!(
                        "[{app_str}] provider '{target_pid}' no longer exists, skipped"
                    ));
                } else {
                    let current = crate::settings::get_effective_current_provider(&state.db, app)?;
                    if current.as_deref() != Some(target_pid.as_str()) {
                        match ProviderService::switch(state, app.clone(), target_pid) {
                            Ok(result) => {
                                warnings.extend(result.warnings);
                            }
                            Err(e) => warnings.push(format!(
                                "[{app_str}] switch provider '{target_pid}' failed: {e}"
                            )),
                        }
                    }
                }
            }

            // 3. MCP diff（最小 toggle：仅动目标态≠当前态的条目；None = 该侧未拍过，不动）
            if let Some(Some(target_ids)) = payload.mcp.get(app) {
                let servers = state.db.get_all_mcp_servers()?;
                let current: Vec<(String, bool)> = servers
                    .values()
                    .map(|s| (s.id.clone(), s.apps.is_enabled_for(app)))
                    .collect();
                let (toggles, dangling) = plan_toggles(&current, target_ids);
                for id in dangling {
                    warnings.push(format!("[{app_str}] MCP '{id}' no longer exists, skipped"));
                }
                for (id, enabled) in toggles {
                    if let Err(e) = McpService::toggle_app(state, &id, app.clone(), enabled) {
                        warnings.push(format!(
                            "[{app_str}] toggle MCP '{id}' -> {enabled} failed: {e}"
                        ));
                    }
                }
            }

            // 4. Skills diff（SkillService 返回 anyhow::Result，收进 warning）
            if let Some(Some(target_ids)) = payload.skills.get(app) {
                let skills = state.db.get_all_installed_skills()?;
                let current: Vec<(String, bool)> = skills
                    .values()
                    .map(|s| (s.id.clone(), s.apps.is_enabled_for(app)))
                    .collect();
                let (toggles, dangling) = plan_toggles(&current, target_ids);
                for id in dangling {
                    warnings.push(format!(
                        "[{app_str}] skill '{id}' no longer exists, skipped"
                    ));
                }
                for (id, enabled) in toggles {
                    if let Err(e) = SkillService::toggle_app(&state.db, &id, app, enabled) {
                        warnings.push(format!(
                            "[{app_str}] toggle skill '{id}' -> {enabled} failed: {e}"
                        ));
                    }
                }
            }

            // 5. Prompt（None = 不动；已激活则幂等跳过，避免无谓的文件写与备份）
            if let Some(Some(target_prompt)) = payload.prompts.get(app) {
                let prompts = state.db.get_prompts(app_str)?;
                match prompts.get(target_prompt) {
                    None => warnings.push(format!(
                        "[{app_str}] prompt '{target_prompt}' no longer exists, skipped"
                    )),
                    Some(p) if p.enabled => {}
                    Some(_) => {
                        if let Err(e) =
                            PromptService::enable_prompt(state, app.clone(), target_prompt)
                        {
                            warnings.push(format!(
                                "[{app_str}] enable prompt '{target_prompt}' failed: {e}"
                            ));
                        }
                    }
                }
            }
        }

        Ok(PreparedProfileApply {
            warnings,
            profile_id: profile_id.to_string(),
            scope,
        })
    }

    fn capture_provider_selection(
        state: &AppState,
        app: AppType,
    ) -> Result<ProviderSelectionSnapshot, AppError> {
        // Capture both persisted planes before effective lookup can clean an
        // invalid device-local value.
        let local = crate::settings::get_current_provider(&app);
        let database = state.db.get_current_provider(app.as_str())?;
        let effective = Self::effective_current_provider_without_cleanup(state, &app)?;
        Ok(ProviderSelectionSnapshot {
            app,
            local,
            database,
            effective,
        })
    }

    fn capture_sync_scope_snapshot(
        state: &AppState,
        scope: ProfileScope,
    ) -> Result<SyncScopeTransactionSnapshot, AppError> {
        let app = scope.apps()[0].clone();
        let provider = Self::capture_provider_selection(state, app.clone())?;
        let payload = Self::snapshot_current(state, scope)?;
        let profiles = state.db.get_all_profiles()?;
        let prompts = state.db.get_prompts(app.as_str())?.into_values().collect();
        let prompt_file = crate::prompt_files::prompt_file_path(&app)
            .ok()
            .map(FileSnapshot::capture)
            .transpose()?;
        let codex_files = if matches!(scope, ProfileScope::Codex) {
            vec![
                FileSnapshot::capture(crate::codex_config::get_codex_auth_path())?,
                FileSnapshot::capture(crate::codex_config::get_codex_config_path())?,
                FileSnapshot::capture(crate::codex_config::get_codex_model_catalog_path())?,
            ]
        } else {
            Vec::new()
        };
        let current_profile_id = state.db.get_current_profile_id(scope.as_str())?;
        Ok(SyncScopeTransactionSnapshot {
            scope,
            provider,
            payload,
            profiles,
            prompts,
            prompt_file,
            codex_files,
            current_profile_id,
        })
    }

    async fn capture_scope_snapshot(
        state: Arc<AppState>,
        scope: ProfileScope,
    ) -> Result<ScopeTransactionSnapshot, AppError> {
        let sync_state = Arc::clone(&state);
        let sync = tokio::task::spawn_blocking(move || {
            Self::capture_sync_scope_snapshot(&sync_state, scope)
        })
        .await
        .map_err(|error| Self::join_error("snapshot", error))??;
        let app_type = sync.provider.app.as_str();
        let app_proxy_config = state.db.get_proxy_config_for_app(app_type).await?;
        let live_backup = state.db.get_live_backup(app_type).await?;
        let proxy_was_running = state.proxy_service.is_running().await;
        Ok(ScopeTransactionSnapshot {
            sync,
            app_proxy_config,
            live_backup,
            proxy_was_running,
        })
    }

    fn set_database_current_provider(
        state: &AppState,
        app: &AppType,
        provider_id: Option<&str>,
    ) -> Result<(), AppError> {
        match provider_id {
            Some(provider_id) => state.db.set_current_provider(app.as_str(), provider_id),
            None => state
                .db
                .conn
                .lock()
                .map_err(|error| AppError::Database(format!("Mutex lock failed: {error}")))
                .and_then(|conn| {
                    conn.execute(
                        "UPDATE providers SET is_current = 0 WHERE app_type = ?1",
                        [app.as_str()],
                    )
                    .map(|_| ())
                    .map_err(|error| AppError::Database(error.to_string()))
                }),
        }
    }

    fn set_provider_selection(
        state: &AppState,
        provider: &ProviderSelectionSnapshot,
        local: Option<&str>,
        database: Option<&str>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if let Err(error) = Self::set_database_current_provider(state, &provider.app, database) {
            errors.push(format!(
                "restore database {} current Provider: {error}",
                provider.app.as_str()
            ));
        }
        if let Err(error) = crate::settings::set_current_provider(&provider.app, local) {
            errors.push(format!(
                "restore local {} current Provider: {error}",
                provider.app.as_str()
            ));
        }
        errors
    }

    fn restore_payload_state(
        state: &AppState,
        snapshot: &SyncScopeTransactionSnapshot,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let app = &snapshot.provider.app;
        let app_str = app.as_str();

        if let Some(Some(target_ids)) = snapshot.payload.mcp.get(app) {
            match state.db.get_all_mcp_servers() {
                Ok(servers) => {
                    let current: Vec<(String, bool)> = servers
                        .values()
                        .map(|server| (server.id.clone(), server.apps.is_enabled_for(app)))
                        .collect();
                    let (toggles, _) = plan_toggles(&current, target_ids);
                    for (id, enabled) in toggles {
                        if let Err(error) = McpService::toggle_app(state, &id, app.clone(), enabled)
                        {
                            errors.push(format!(
                                "restore [{app_str}] MCP '{id}' -> {enabled}: {error}"
                            ));
                        }
                    }
                }
                Err(error) => errors.push(format!("read [{app_str}] MCP state: {error}")),
            }
        }

        if let Some(Some(target_ids)) = snapshot.payload.skills.get(app) {
            match state.db.get_all_installed_skills() {
                Ok(skills) => {
                    let current: Vec<(String, bool)> = skills
                        .values()
                        .map(|skill| (skill.id.clone(), skill.apps.is_enabled_for(app)))
                        .collect();
                    let (toggles, _) = plan_toggles(&current, target_ids);
                    for (id, enabled) in toggles {
                        if let Err(error) = SkillService::toggle_app(&state.db, &id, app, enabled) {
                            errors.push(format!(
                                "restore [{app_str}] skill '{id}' -> {enabled}: {error}"
                            ));
                        }
                    }
                }
                Err(error) => errors.push(format!("read [{app_str}] skill state: {error}")),
            }
        }

        match state.db.get_prompts(app_str) {
            Ok(current) => {
                let snapshot_ids: HashSet<&str> = snapshot
                    .prompts
                    .iter()
                    .map(|prompt| prompt.id.as_str())
                    .collect();
                for id in current
                    .keys()
                    .filter(|id| !snapshot_ids.contains(id.as_str()))
                {
                    if let Err(error) = state.db.delete_prompt(app_str, id) {
                        errors.push(format!("remove added [{app_str}] prompt '{id}': {error}"));
                    }
                }
                for prompt in &snapshot.prompts {
                    if let Err(error) = state.db.save_prompt(app_str, prompt) {
                        errors.push(format!(
                            "restore [{app_str}] prompt '{}': {error}",
                            prompt.id
                        ));
                    }
                }
            }
            Err(error) => errors.push(format!("read [{app_str}] prompt state: {error}")),
        }
        if let Some(prompt_file) = snapshot.prompt_file.as_ref() {
            if let Err(error) = prompt_file.restore() {
                errors.push(format!("restore [{app_str}] prompt file: {error}"));
            }
        }
        errors
    }

    fn restore_sync_before_roles(
        state: &AppState,
        snapshot: &SyncScopeTransactionSnapshot,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for profile in &snapshot.profiles {
            if let Err(error) = state.db.save_profile(profile) {
                errors.push(format!("restore profile '{}': {error}", profile.id));
            }
        }

        if let Some(previous_provider) = snapshot.provider.effective.as_deref() {
            match ProviderService::switch(state, snapshot.provider.app.clone(), previous_provider) {
                Ok(result) => errors.extend(result.warnings.into_iter().map(|warning| {
                    format!("rollback Provider '{previous_provider}' warning: {warning}")
                })),
                Err(error) => errors.push(format!(
                    "rollback Provider '{previous_provider}' failed: {error}"
                )),
            }
        }

        errors.extend(Self::restore_payload_state(state, snapshot));
        if let Err(error) = state.db.set_current_profile_id(
            snapshot.scope.as_str(),
            snapshot.current_profile_id.as_deref(),
        ) {
            errors.push(format!("restore current Profile: {error}"));
        }

        // Role reconciliation needs the previous effective Provider even when the
        // original raw local/database pointers intentionally differed.
        errors.extend(Self::set_provider_selection(
            state,
            &snapshot.provider,
            snapshot.provider.effective.as_deref(),
            snapshot.provider.effective.as_deref(),
        ));
        errors
    }

    fn restore_live_backup_exact(
        state: &AppState,
        snapshot: &ScopeTransactionSnapshot,
    ) -> Result<(), AppError> {
        let conn = state
            .db
            .conn
            .lock()
            .map_err(|error| AppError::Database(format!("Mutex lock failed: {error}")))?;
        match snapshot.live_backup.as_ref() {
            Some(backup) => conn
                .execute(
                    "INSERT OR REPLACE INTO proxy_live_backup (app_type, original_config, backed_up_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![backup.app_type, backup.original_config, backup.backed_up_at],
                )
                .map(|_| ())
                .map_err(|error| AppError::Database(error.to_string())),
            None => conn
                .execute(
                    "DELETE FROM proxy_live_backup WHERE app_type = ?1",
                    [snapshot.sync.provider.app.as_str()],
                )
                .map(|_| ())
                .map_err(|error| AppError::Database(error.to_string())),
        }
    }

    fn restore_sync_after_roles(
        state: &AppState,
        snapshot: &ScopeTransactionSnapshot,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for file in &snapshot.sync.codex_files {
            if let Err(error) = file.restore() {
                errors.push(format!("restore {}: {error}", file.path.display()));
            }
        }
        if let Err(error) = Self::restore_live_backup_exact(state, snapshot) {
            errors.push(format!("restore Live backup: {error}"));
        }
        errors.extend(Self::set_provider_selection(
            state,
            &snapshot.sync.provider,
            snapshot.sync.provider.local.as_deref(),
            snapshot.sync.provider.database.as_deref(),
        ));
        errors
    }

    async fn restore_proxy_snapshot(
        state: &AppState,
        snapshot: &ScopeTransactionSnapshot,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let app_str = snapshot.sync.provider.app.as_str();
        match state.db.get_proxy_config_for_app(app_str).await {
            Ok(current) if current.enabled != snapshot.app_proxy_config.enabled => {
                let restore_result = if matches!(snapshot.sync.scope, ProfileScope::Codex) {
                    state
                        .proxy_service
                        .set_takeover_for_app_inner(app_str, snapshot.app_proxy_config.enabled)
                        .await
                } else {
                    state
                        .proxy_service
                        .set_takeover_for_app(app_str, snapshot.app_proxy_config.enabled)
                        .await
                };
                if let Err(error) = restore_result {
                    errors.push(format!("restore [{app_str}] takeover: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => errors.push(format!("read [{app_str}] takeover state: {error}")),
        }
        if let Err(error) = state
            .db
            .update_proxy_config_for_app(snapshot.app_proxy_config.clone())
            .await
        {
            errors.push(format!("restore [{app_str}] proxy config: {error}"));
        }

        let running = state.proxy_service.is_running().await;
        if snapshot.proxy_was_running && !running {
            let start_result = if matches!(snapshot.sync.scope, ProfileScope::Codex) {
                state.proxy_service.start_inner().await.map(|_| ())
            } else {
                state.proxy_service.start().await.map(|_| ())
            };
            if let Err(error) = start_result {
                errors.push(format!("restart proxy: {error}"));
            }
        } else if !snapshot.proxy_was_running && running {
            let stop_result = if matches!(snapshot.sync.scope, ProfileScope::Codex) {
                state.proxy_service.stop_inner().await
            } else {
                state.proxy_service.stop().await
            };
            if let Err(error) = stop_result {
                errors.push(format!("stop rollback-started proxy: {error}"));
            }
        }
        errors
    }

    async fn rollback_scope_snapshot(
        state: Arc<AppState>,
        snapshot: ScopeTransactionSnapshot,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let app_str = snapshot.sync.provider.app.as_str().to_string();

        match state.db.get_proxy_config_for_app(&app_str).await {
            Ok(current) if current.enabled => {
                let disable_result = if matches!(snapshot.sync.scope, ProfileScope::Codex) {
                    state
                        .proxy_service
                        .set_takeover_for_app_inner(&app_str, false)
                        .await
                } else {
                    state
                        .proxy_service
                        .set_takeover_for_app(&app_str, false)
                        .await
                };
                if let Err(error) = disable_result {
                    errors.push(format!("disable target [{app_str}] takeover: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => errors.push(format!("read target [{app_str}] takeover: {error}")),
        }

        let sync_state = Arc::clone(&state);
        match tokio::task::spawn_blocking({
            let sync_snapshot = snapshot.sync.clone();
            move || Self::restore_sync_before_roles(&sync_state, &sync_snapshot)
        })
        .await
        {
            Ok(mut sync_errors) => errors.append(&mut sync_errors),
            Err(error) => errors.push(Self::join_error("rollback sync", error).to_string()),
        }

        if matches!(snapshot.sync.scope, ProfileScope::Codex) {
            if let Err(error) =
                crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(
                    state.as_ref(),
                )
                .await
            {
                errors.push(format!("restore Codex Agent Role projection: {error}"));
            }
        }
        errors.extend(Self::restore_proxy_snapshot(state.as_ref(), &snapshot).await);

        let final_state = Arc::clone(&state);
        match tokio::task::spawn_blocking(move || {
            Self::restore_sync_after_roles(final_state.as_ref(), &snapshot)
        })
        .await
        {
            Ok(mut final_errors) => errors.append(&mut final_errors),
            Err(error) => errors.push(Self::join_error("rollback final", error).to_string()),
        }
        errors
    }

    fn commit_apply(
        state: &AppState,
        prepared: PreparedProfileApply,
    ) -> Result<Vec<String>, AppError> {
        #[cfg(test)]
        if FAIL_NEXT_PROFILE_COMMIT.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(AppError::Database(
                "injected Profile commit failure".to_string(),
            ));
        }

        state
            .db
            .set_current_profile_id(prepared.scope.as_str(), Some(prepared.profile_id.as_str()))?;
        Ok(prepared.warnings)
    }

    fn join_error(stage: &str, error: tokio::task::JoinError) -> AppError {
        AppError::Message(format!("Profile {stage} task failed: {error}"))
    }

    fn with_rollback_errors(primary: AppError, rollback_errors: Vec<String>) -> AppError {
        if rollback_errors.is_empty() {
            primary
        } else {
            AppError::Message(format!(
                "{primary}; rollback encountered: {}",
                rollback_errors.join("; ")
            ))
        }
    }

    async fn rollback_apply_error(
        state: Arc<AppState>,
        snapshot: ScopeTransactionSnapshot,
        primary: AppError,
    ) -> AppError {
        let rollback_errors = Self::rollback_scope_snapshot(state, snapshot).await;
        Self::with_rollback_errors(primary, rollback_errors)
    }

    /// Apply a profile while keeping filesystem/database work on blocking threads
    /// and Codex Agent Role/proxy coordination on the Tokio runtime.
    pub async fn apply_async(
        state: Arc<AppState>,
        profile_id: String,
        scope: ProfileScope,
    ) -> Result<(Vec<String>, bool), AppError> {
        tokio::spawn(async move { Self::apply_transaction(state, profile_id, scope).await })
            .await
            .map_err(|error| Self::join_error("supervisor", error))?
    }

    async fn apply_transaction(
        state: Arc<AppState>,
        profile_id: String,
        scope: ProfileScope,
    ) -> Result<(Vec<String>, bool), AppError> {
        let _mutation_guard = PROFILE_MUTATION_LOCK.lock().await;
        let _scope_guard = PROFILE_SCOPE_LOCKS[scope.lock_index()].lock().await;
        let _codex_lifecycle_guard = if matches!(scope, ProfileScope::Codex) {
            Some(state.lock_codex_provider_lifecycle().await)
        } else {
            None
        };
        let _codex_proxy_transaction_guard = if matches!(scope, ProfileScope::Codex) {
            Some(state.proxy_service.lock_transaction().await)
        } else {
            None
        };

        #[cfg(test)]
        let _activity = ProfileApplyActivity::enter();
        #[cfg(test)]
        {
            let delay_ms = PROFILE_APPLY_TEST_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst);
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        let snapshot = Self::capture_scope_snapshot(Arc::clone(&state), scope).await?;

        let prepare_state = Arc::clone(&state);
        let prepared = match tokio::task::spawn_blocking(move || {
            Self::prepare_apply(prepare_state.as_ref(), &profile_id, scope)
        })
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(primary)) => {
                return Err(Self::rollback_apply_error(state, snapshot, primary).await)
            }
            Err(error) => {
                let primary = Self::join_error("prepare", error);
                return Err(Self::rollback_apply_error(state, snapshot, primary).await);
            }
        };

        if matches!(scope, ProfileScope::Codex) {
            if let Err(error) =
                crate::services::codex_agent_roles::reconcile_current_codex_agent_roles_under_proxy_transaction(
                    state.as_ref(),
                )
                .await
            {
                let primary =
                    AppError::Message(format!("[codex] sync Codex Agent Role failed: {error}"));
                return Err(Self::rollback_apply_error(state, snapshot, primary).await);
            }
        }

        let commit_state = Arc::clone(&state);
        let mut warnings = match tokio::task::spawn_blocking(move || {
            Self::commit_apply(commit_state.as_ref(), prepared)
        })
        .await
        {
            Ok(Ok(warnings)) => warnings,
            Ok(Err(primary)) => {
                return Err(Self::rollback_apply_error(state, snapshot, primary).await)
            }
            Err(error) => {
                let primary = Self::join_error("commit", error);
                return Err(Self::rollback_apply_error(state, snapshot, primary).await);
            }
        };

        match state.db.is_live_takeover_active().await {
            Ok(false) if state.proxy_service.is_running().await => {
                let stop_result = if matches!(scope, ProfileScope::Codex) {
                    state.proxy_service.stop_inner().await
                } else {
                    state.proxy_service.stop().await
                };
                if let Err(error) = stop_result {
                    warnings.push(format!("[proxy] stop after Profile apply failed: {error}"));
                }
            }
            Ok(_) => {}
            Err(error) => warnings.push(format!(
                "[proxy] recheck takeover before stop failed: {error}"
            )),
        }

        Ok((warnings, false))
    }

    /// Blocking compatibility wrapper used by the tray handler. The Tauri command
    /// calls [`Self::apply_async`] directly and never blocks a runtime worker.
    pub fn apply(
        state: &AppState,
        profile_id: &str,
        scope: ProfileScope,
    ) -> Result<(Vec<String>, bool), AppError> {
        tauri::async_runtime::block_on(Self::apply_async(
            Self::owned_state(state),
            profile_id.to_string(),
            scope,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::{InstalledSkill, McpApps, McpServer, SkillApps};
    use crate::database::Database;
    use crate::provider::{CodexAgentRoleRouting, Provider, ProviderMeta};
    use crate::services::codex_agent_roles::{CodexAgentRolePaths, MANAGED_MARKER};
    use serde_json::json;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct TestHome {
        _dir: TempDir,
        old_home: Option<OsString>,
        old_test_home: Option<OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let old_home = std::env::var_os("HOME");
            let old_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("HOME", dir.path());
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload isolated settings");
            Self {
                _dir: dir,
                old_home,
                old_test_home,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.old_home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.old_test_home.take() {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    fn codex_provider(id: &str, name: &str, base_url: &str) -> Provider {
        Provider::with_id(
            id.to_string(),
            name.to_string(),
            json!({
                "auth": { "OPENAI_API_KEY": format!("{id}-key") },
                "config": format!(
                    "model_provider = \"test\"\nmodel = \"{id}-model\"\n[model_providers.test]\nbase_url = \"{base_url}\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
                )
            }),
            None,
        )
    }

    fn codex_provider_with_role_routing(id: &str, name: &str, base_url: &str) -> Provider {
        let mut provider = codex_provider(id, name, base_url);
        provider.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(CodexAgentRoleRouting {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        provider
    }

    fn save_codex_profile(db: &Database, id: &str, provider_id: &str) {
        let payload = ProfilePayload {
            providers: PerApp {
                codex: Some(provider_id.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        db.save_profile(&Profile {
            id: id.to_string(),
            name: id.to_string(),
            payload: serde_json::to_string(&payload).expect("serialize profile"),
            sort_order: None,
            created_at: None,
            updated_at: None,
        })
        .expect("save profile");
    }

    fn save_profile_payload(db: &Database, id: &str, payload: &ProfilePayload) {
        db.save_profile(&Profile {
            id: id.to_string(),
            name: id.to_string(),
            payload: serde_json::to_string(payload).expect("serialize profile"),
            sort_order: None,
            created_at: None,
            updated_at: None,
        })
        .expect("save profile");
    }

    fn set_current_codex_provider(db: &Database, provider_id: &str) {
        db.set_current_provider(AppType::Codex.as_str(), provider_id)
            .expect("set database current provider");
        crate::settings::set_current_provider(&AppType::Codex, Some(provider_id))
            .expect("set device current provider");
    }

    async fn use_ephemeral_proxy_port(db: &Database) {
        let mut config = db.get_proxy_config().await.expect("read proxy config");
        config.listen_port = 0;
        db.update_proxy_config(config)
            .await
            .expect("set ephemeral proxy port");
    }

    fn profile_payload(db: &Database, profile_id: &str) -> ProfilePayload {
        let profile = db
            .get_profile(profile_id)
            .expect("read profile")
            .expect("profile exists");
        serde_json::from_str(&profile.payload).expect("parse profile payload")
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_payload_serde_roundtrip() {
        let payload = ProfilePayload {
            providers: PerApp {
                claude: Some("p1".into()),
                claude_desktop: Some("d1".into()),
                codex: None,
            },
            mcp: PerApp {
                claude: Some(ids(&["m1", "m2"])),
                claude_desktop: Some(vec![]),
                codex: None,
            },
            skills: PerApp {
                claude: Some(vec![]),
                claude_desktop: Some(vec![]),
                codex: Some(ids(&["s1"])),
            },
            prompts: PerApp {
                claude: None,
                claude_desktop: None,
                codex: Some("pr1".into()),
            },
        };
        let json = serde_json::to_string(&payload).unwrap();
        // per-app key 必须与 AppType 的 serde 形式一致（claude-desktop 是连字符）
        assert!(json.contains("\"claude\""));
        assert!(json.contains("\"claude-desktop\""));
        assert!(json.contains("\"codex\""));
        let back: ProfilePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn test_payload_tolerates_missing_fields() {
        // 前向兼容：旧版/部分字段缺失时应落到 None（"该侧未拍过"）而不是报错，
        // 应用时对缺失槽位不做任何改动
        let back: ProfilePayload =
            serde_json::from_str(r#"{"providers":{"claude":"p1"},"mcp":{"claude":["m1"]}}"#)
                .unwrap();
        assert_eq!(back.providers.claude, Some("p1".to_string()));
        assert_eq!(back.providers.claude_desktop, None);
        assert_eq!(back.providers.codex, None);
        assert_eq!(back.mcp.claude, Some(ids(&["m1"])));
        assert_eq!(back.mcp.claude_desktop, None);
        assert_eq!(back.mcp.codex, None, "missing slot means untouched");
        assert_eq!(back.prompts.codex, None);

        let empty: ProfilePayload = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, ProfilePayload::default());
    }

    #[test]
    fn test_merge_scope_from_only_touches_scope_slots() {
        // 项目 A：两侧都已拍过快照
        let mut payload = ProfilePayload {
            providers: PerApp {
                claude: Some("p1".into()),
                claude_desktop: Some("d1".into()),
                codex: Some("c1".into()),
            },
            mcp: PerApp {
                claude: Some(ids(&["m1"])),
                claude_desktop: Some(vec![]),
                codex: Some(ids(&["m9"])),
            },
            ..Default::default()
        };
        // 在 Claude 页"以当前状态更新"：只覆盖 claude 组槽位
        let fresh = ProfilePayload {
            providers: PerApp {
                claude: Some("p2".into()),
                claude_desktop: None,
                codex: Some("SHOULD-NOT-LEAK".into()),
            },
            mcp: PerApp {
                claude: Some(ids(&["m2"])),
                claude_desktop: Some(vec![]),
                codex: None,
            },
            ..Default::default()
        };
        payload.merge_scope_from(&fresh, ProfileScope::Claude);

        assert_eq!(payload.providers.claude, Some("p2".to_string()));
        assert_eq!(
            payload.providers.claude_desktop,
            Some("d1".to_string()),
            "claude-desktop slot is in its own scope, untouched by claude merge"
        );
        assert_eq!(payload.mcp.claude, Some(ids(&["m2"])));
        // codex 侧完好：既没被覆盖也没被 fresh 的值污染
        assert_eq!(payload.providers.codex, Some("c1".to_string()));
        assert_eq!(payload.mcp.codex, Some(ids(&["m9"])));
    }

    #[test]
    fn test_scope_captured_detects_per_scope_snapshot() {
        let mut payload = ProfilePayload::default();
        assert!(!payload.scope_captured(ProfileScope::Claude));
        assert!(!payload.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!payload.scope_captured(ProfileScope::Codex));

        // 只拍过 claude 组（哪怕拍到的是空集）
        payload.mcp.claude = Some(vec![]);
        assert!(payload.scope_captured(ProfileScope::Claude));
        assert!(!payload.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!payload.scope_captured(ProfileScope::Codex));

        // Desktop 槽位属于独立的 claude-desktop 组
        let mut desktop_only = ProfilePayload::default();
        desktop_only.providers.claude_desktop = Some("d1".into());
        assert!(desktop_only.scope_captured(ProfileScope::ClaudeDesktop));
        assert!(!desktop_only.scope_captured(ProfileScope::Claude));
    }

    #[test]
    fn test_per_app_get_only_supports_profile_apps() {
        let per: PerApp<Option<String>> = PerApp::default();
        assert!(per.get(&AppType::Claude).is_some());
        assert!(per.get(&AppType::ClaudeDesktop).is_some());
        assert!(per.get(&AppType::Codex).is_some());
        assert!(per.get(&AppType::Gemini).is_none());
    }

    #[test]
    fn test_scope_serde_and_parse_roundtrip() {
        for scope in ProfileScope::ALL {
            // DB 存储字符串（as_str/parse）与 JSON 序列化必须是同一形式
            assert_eq!(
                serde_json::to_string(&scope).unwrap(),
                format!("\"{}\"", scope.as_str())
            );
            assert_eq!(ProfileScope::parse(scope.as_str()).unwrap(), scope);
        }
        assert!(ProfileScope::parse("gemini").is_err());
        assert!(ProfileScope::parse("").is_err());
    }

    #[test]
    fn test_scope_app_grouping() {
        // Claude Code 与 Claude Desktop 各自独立成组；
        // 组内应用与 for_app 反向映射必须一致
        assert_eq!(ProfileScope::Claude.apps(), &[AppType::Claude]);
        assert_eq!(
            ProfileScope::ClaudeDesktop.apps(),
            &[AppType::ClaudeDesktop]
        );
        assert_eq!(ProfileScope::Codex.apps(), &[AppType::Codex]);
        for scope in ProfileScope::ALL {
            for app in scope.apps() {
                assert_eq!(ProfileScope::for_app(app), Some(scope));
            }
        }
        assert_eq!(ProfileScope::for_app(&AppType::Gemini), None);
    }

    #[test]
    fn test_plan_toggles_minimal_diff() {
        let current = vec![
            ("a".to_string(), true),  // 目标含 a：不动
            ("b".to_string(), false), // 目标含 b：开
            ("c".to_string(), true),  // 目标不含 c：关
            ("d".to_string(), false), // 目标不含 d：不动
        ];
        let (toggles, dangling) = plan_toggles(&current, &ids(&["a", "b", "ghost"]));
        assert_eq!(
            toggles,
            vec![("b".to_string(), true), ("c".to_string(), false)]
        );
        assert_eq!(dangling, ids(&["ghost"]));
    }

    #[test]
    fn test_plan_toggles_empty_target_disables_all_enabled() {
        let current = vec![("a".to_string(), true), ("b".to_string(), false)];
        let (toggles, dangling) = plan_toggles(&current, &[]);
        assert_eq!(toggles, vec![("a".to_string(), false)]);
        assert!(dangling.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn apply_codex_profile_reconciles_roles_when_provider_is_unchanged() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider = codex_provider("provider-a", "Provider A", "https://a.invalid/v1");
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("seed provider");
        set_current_codex_provider(&db, &provider.id);
        save_codex_profile(&db, "profile-a", &provider.id);

        let paths = CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::write(
            &paths.frontend,
            format!("{MANAGED_MARKER}\nname = \"stale-frontend\"\n"),
        )
        .expect("seed managed frontend role");
        fs::write(
            &paths.backend,
            format!("{MANAGED_MARKER}\nname = \"stale-backend\"\n"),
        )
        .expect("seed managed backend role");

        let (warnings, _) = ProfileService::apply_async(
            Arc::clone(&state),
            "profile-a".to_string(),
            ProfileScope::Codex,
        )
        .await
        .expect("apply Codex profile");

        assert!(
            !warnings
                .iter()
                .any(|warning| warning.contains("sync Codex Agent Role failed")),
            "unexpected role reconciliation warning: {warnings:?}"
        );
        assert!(!paths.frontend.exists());
        assert!(!paths.backend.exists());
        assert!(paths.frontend_disabled.exists());
        assert!(paths.backend_disabled.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn apply_codex_profile_enables_routing_and_starts_a_stopped_proxy() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        use_ephemeral_proxy_port(db.as_ref()).await;
        let state = Arc::new(AppState::new(db.clone()));
        let provider_a = codex_provider("provider-a", "Provider A", "https://a.invalid/v1");
        let provider_b =
            codex_provider_with_role_routing("provider-b", "Provider B", "https://b.invalid/v1");
        db.save_provider(AppType::Codex.as_str(), &provider_a)
            .expect("seed provider A");
        db.save_provider(AppType::Codex.as_str(), &provider_b)
            .expect("seed provider B");
        set_current_codex_provider(&db, &provider_a.id);
        save_codex_profile(&db, "profile-a", &provider_a.id);
        save_codex_profile(&db, "profile-b", &provider_b.id);
        db.set_current_profile_id(ProfileScope::Codex.as_str(), Some("profile-a"))
            .expect("set current profile A");

        assert!(!state.proxy_service.is_running().await);

        let (_, should_stop_proxy) = ProfileService::apply_async(
            Arc::clone(&state),
            "profile-b".to_string(),
            ProfileScope::Codex,
        )
        .await
        .expect("apply routed Codex profile");

        assert!(!should_stop_proxy);
        assert!(state.proxy_service.is_running().await);
        assert!(
            state
                .proxy_service
                .get_takeover_status()
                .await
                .expect("read takeover status")
                .codex
        );
        assert_eq!(
            db.get_current_profile_id(ProfileScope::Codex.as_str())
                .expect("read current profile")
                .as_deref(),
            Some("profile-b")
        );

        let paths = CodexAgentRolePaths::default_codex_home();
        let frontend = fs::read_to_string(&paths.frontend).expect("read frontend role");
        assert!(frontend.contains("x-cc-switch-role-owner = \"provider-b\""));
        assert!(frontend.contains("base_url = \"http://127.0.0.1:"));

        state
            .proxy_service
            .set_takeover_for_app(AppType::Codex.as_str(), false)
            .await
            .expect("clean up Codex takeover");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn apply_codex_profile_failure_restores_raw_currents_and_preserves_target_payload() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        use_ephemeral_proxy_port(db.as_ref()).await;
        let state = Arc::new(AppState::new(db.clone()));
        let provider_a = codex_provider("provider-a", "Provider A", "https://a.invalid/v1");
        let provider_b =
            codex_provider_with_role_routing("provider-b", "Provider B", "https://b.invalid/v1");
        let provider_c = codex_provider("provider-c", "Provider C", "https://c.invalid/v1");
        for provider in [&provider_a, &provider_b, &provider_c] {
            db.save_provider(AppType::Codex.as_str(), provider)
                .expect("seed provider");
        }
        set_current_codex_provider(&db, &provider_a.id);
        // Device-local current A intentionally differs from synchronized DB current C.
        db.set_current_provider(AppType::Codex.as_str(), &provider_c.id)
            .expect("set divergent database current provider");
        save_codex_profile(&db, "profile-a", &provider_a.id);
        save_codex_profile(&db, "profile-b", &provider_b.id);
        save_codex_profile(&db, "profile-c", &provider_c.id);
        db.set_current_profile_id(ProfileScope::Codex.as_str(), Some("profile-a"))
            .expect("set current profile A");
        let provider_b_payload = profile_payload(db.as_ref(), "profile-b");
        let original_config =
            "model_provider = \"test\"\n[model_providers.test]\nbase_url = \"https://a.invalid/v1\"\n";
        let config_path = crate::codex_config::get_codex_config_path();
        fs::create_dir_all(config_path.parent().expect("Codex config parent"))
            .expect("create Codex config directory");
        fs::write(&config_path, original_config).expect("seed original Codex config");

        let paths = CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::write(&paths.frontend, "name = \"user-frontend\"\n")
            .expect("create conflicting user role");

        let error = ProfileService::apply_async(
            Arc::clone(&state),
            "profile-b".to_string(),
            ProfileScope::Codex,
        )
        .await
        .expect_err("role projection failure must fail the profile apply");

        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some(provider_a.id.as_str()),
            "device-local current Provider must be restored exactly"
        );
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current Provider")
                .as_deref(),
            Some(provider_c.id.as_str()),
            "database current Provider must be restored independently"
        );
        assert_eq!(
            db.get_current_profile_id(ProfileScope::Codex.as_str())
                .expect("read current profile")
                .as_deref(),
            Some("profile-a"),
            "failed target profile must never become current"
        );
        assert!(error.to_string().contains("sync Codex Agent Role failed"));
        assert!(!state.proxy_service.is_running().await);
        assert!(
            !state
                .proxy_service
                .get_takeover_status()
                .await
                .expect("read takeover status")
                .codex
        );
        assert_eq!(
            fs::read_to_string(&paths.frontend).expect("read user role"),
            "name = \"user-frontend\"\n"
        );
        assert_eq!(
            fs::read_to_string(&config_path).expect("read rolled-back Codex config"),
            original_config,
            "the live Codex config must be restored byte-for-byte"
        );

        ProfileService::apply_async(
            Arc::clone(&state),
            "profile-c".to_string(),
            ProfileScope::Codex,
        )
        .await
        .expect("a later valid profile switch succeeds");
        assert_eq!(
            profile_payload(db.as_ref(), "profile-b"),
            provider_b_payload,
            "a failed target profile payload must not be overwritten by later autosave"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn concurrent_applies_for_the_same_scope_are_serialized() {
        use std::sync::atomic::Ordering;

        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider = codex_provider("provider-a", "Provider A", "https://a.invalid/v1");
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("seed provider");
        set_current_codex_provider(&db, &provider.id);
        save_codex_profile(&db, "profile-a", &provider.id);
        save_codex_profile(&db, "profile-b", &provider.id);

        PROFILE_APPLY_ACTIVE.store(0, Ordering::SeqCst);
        PROFILE_APPLY_MAX_ACTIVE.store(0, Ordering::SeqCst);
        PROFILE_APPLY_TEST_DELAY_MS.store(100, Ordering::SeqCst);

        let first = tokio::spawn(ProfileService::apply_async(
            Arc::clone(&state),
            "profile-a".to_string(),
            ProfileScope::Codex,
        ));
        let second = tokio::spawn(ProfileService::apply_async(
            Arc::clone(&state),
            "profile-b".to_string(),
            ProfileScope::Codex,
        ));

        let (first, second) = tokio::join!(first, second);
        PROFILE_APPLY_TEST_DELAY_MS.store(0, Ordering::SeqCst);

        first.expect("first apply task").expect("first apply");
        second.expect("second apply task").expect("second apply");
        assert_eq!(
            PROFILE_APPLY_MAX_ACTIVE.load(Ordering::SeqCst),
            1,
            "same-scope Profile applies must never overlap"
        );
        assert_eq!(PROFILE_APPLY_ACTIVE.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn cancelled_caller_does_not_cancel_profile_transaction() {
        use std::sync::atomic::Ordering;

        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));
        let provider = codex_provider("provider-a", "Provider A", "https://a.invalid/v1");
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("seed provider");
        set_current_codex_provider(&db, &provider.id);
        save_codex_profile(&db, "profile-b", &provider.id);

        PROFILE_APPLY_ACTIVE.store(0, Ordering::SeqCst);
        PROFILE_APPLY_TEST_DELAY_MS.store(150, Ordering::SeqCst);
        let caller = tokio::spawn(ProfileService::apply_async(
            Arc::clone(&state),
            "profile-b".to_string(),
            ProfileScope::Codex,
        ));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while PROFILE_APPLY_ACTIVE.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("profile supervisor should start");
        caller.abort();
        let _ = caller.await;

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while PROFILE_APPLY_ACTIVE.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervised profile transaction should finish after caller cancellation");
        PROFILE_APPLY_TEST_DELAY_MS.store(0, Ordering::SeqCst);

        assert_eq!(
            db.get_current_profile_id(ProfileScope::Codex.as_str())
                .expect("read current profile")
                .as_deref(),
            Some("profile-b")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn failed_codex_apply_restores_an_empty_previous_provider_selection() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        use_ephemeral_proxy_port(db.as_ref()).await;
        let state = Arc::new(AppState::new(db.clone()));
        let provider =
            codex_provider_with_role_routing("provider-b", "Provider B", "https://b.invalid/v1");
        db.save_provider(AppType::Codex.as_str(), &provider)
            .expect("seed provider");
        crate::settings::set_current_provider(&AppType::Codex, None)
            .expect("clear device current provider");
        save_codex_profile(&db, "profile-b", &provider.id);

        let paths = CodexAgentRolePaths::default_codex_home();
        fs::create_dir_all(paths.frontend.parent().expect("agents parent"))
            .expect("create agents directory");
        fs::write(&paths.frontend, "name = \"user-frontend\"\n")
            .expect("create conflicting user role");

        ProfileService::apply_async(
            Arc::clone(&state),
            "profile-b".to_string(),
            ProfileScope::Codex,
        )
        .await
        .expect_err("role conflict must fail the apply");

        assert_eq!(crate::settings::get_current_provider(&AppType::Codex), None);
        assert_eq!(
            db.get_current_provider(AppType::Codex.as_str())
                .expect("read database current provider"),
            None
        );
        assert_eq!(
            db.get_current_profile_id(ProfileScope::Codex.as_str())
                .expect("read current profile"),
            None
        );
        assert!(!state.proxy_service.is_running().await);
        assert!(
            !state
                .proxy_service
                .get_takeover_status()
                .await
                .expect("read takeover status")
                .codex
        );
        assert_eq!(
            fs::read_to_string(&paths.frontend).expect("read user role"),
            "name = \"user-frontend\"\n"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn commit_failure_restores_codex_takeover_runtime_and_live_backup() {
        use std::sync::atomic::Ordering;

        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        use_ephemeral_proxy_port(db.as_ref()).await;
        let state = Arc::new(AppState::new(db.clone()));
        let provider_a = codex_provider("provider-a", "Provider A", "https://a.invalid/v1");
        let provider_b = codex_provider("provider-b", "Provider B", "https://b.invalid/v1");
        for provider in [&provider_a, &provider_b] {
            db.save_provider(AppType::Codex.as_str(), provider)
                .expect("seed provider");
        }
        set_current_codex_provider(&db, &provider_a.id);
        save_codex_profile(&db, "profile-a", &provider_a.id);
        save_codex_profile(&db, "profile-b", &provider_b.id);
        db.set_current_profile_id(ProfileScope::Codex.as_str(), Some("profile-a"))
            .expect("set current profile");

        let config_path = crate::codex_config::get_codex_config_path();
        fs::create_dir_all(config_path.parent().expect("Codex config parent"))
            .expect("create Codex config directory");
        fs::write(&config_path, "model = \"original-model\"\n")
            .expect("seed original Codex config");
        state
            .proxy_service
            .set_takeover_for_app(AppType::Codex.as_str(), true)
            .await
            .expect("enable Codex takeover");

        let backup_before = db
            .get_live_backup(AppType::Codex.as_str())
            .await
            .expect("read live backup")
            .expect("live backup exists");
        assert!(state.proxy_service.is_running().await);

        FAIL_NEXT_PROFILE_COMMIT.store(true, Ordering::SeqCst);
        let error = ProfileService::apply_async(
            Arc::clone(&state),
            "profile-b".to_string(),
            ProfileScope::Codex,
        )
        .await
        .expect_err("injected commit failure must roll back");

        assert!(error
            .to_string()
            .contains("injected Profile commit failure"));
        assert!(state.proxy_service.is_running().await);
        assert!(
            state
                .proxy_service
                .get_takeover_status()
                .await
                .expect("read takeover status")
                .codex
        );
        assert_eq!(
            crate::settings::get_current_provider(&AppType::Codex).as_deref(),
            Some(provider_a.id.as_str())
        );
        assert_eq!(
            db.get_current_profile_id(ProfileScope::Codex.as_str())
                .expect("read current profile")
                .as_deref(),
            Some("profile-a")
        );
        let backup_after = db
            .get_live_backup(AppType::Codex.as_str())
            .await
            .expect("read restored live backup")
            .expect("restored live backup exists");
        assert_eq!(backup_after.app_type, backup_before.app_type);
        assert_eq!(backup_after.original_config, backup_before.original_config);
        assert_eq!(backup_after.backed_up_at, backup_before.backed_up_at);

        state
            .proxy_service
            .set_takeover_for_app(AppType::Codex.as_str(), false)
            .await
            .expect("clean up Codex takeover");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn commit_failure_restores_claude_mcp_skill_and_prompt_payload() {
        use std::sync::atomic::Ordering;

        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("in-memory database"));
        let state = Arc::new(AppState::new(db.clone()));

        let old_mcp = McpServer {
            id: "mcp-old".to_string(),
            name: "Old MCP".to_string(),
            server: json!({ "command": "old-mcp" }),
            apps: McpApps::default(),
            description: None,
            homepage: None,
            docs: None,
            tags: Vec::new(),
        };
        let new_mcp = McpServer {
            id: "mcp-new".to_string(),
            name: "New MCP".to_string(),
            server: json!({ "command": "new-mcp" }),
            apps: McpApps::default(),
            description: None,
            homepage: None,
            docs: None,
            tags: Vec::new(),
        };
        db.save_mcp_server(&old_mcp).expect("save old MCP");
        db.save_mcp_server(&new_mcp).expect("save new MCP");
        McpService::toggle_app(&state, &old_mcp.id, AppType::Claude, true).expect("enable old MCP");

        let ssot_dir = SkillService::get_ssot_dir().expect("skill SSOT directory");
        for directory in ["skill-old", "skill-new"] {
            let source = ssot_dir.join(directory);
            fs::create_dir_all(&source).expect("create skill source");
            fs::write(source.join("SKILL.md"), format!("# {directory}\n"))
                .expect("write skill source");
        }
        let old_skill = InstalledSkill {
            id: "skill-old-id".to_string(),
            name: "Old Skill".to_string(),
            description: None,
            directory: "skill-old".to_string(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            apps: SkillApps::default(),
            installed_at: 1,
            content_hash: None,
            updated_at: 0,
        };
        let new_skill = InstalledSkill {
            id: "skill-new-id".to_string(),
            name: "New Skill".to_string(),
            description: None,
            directory: "skill-new".to_string(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            apps: SkillApps::default(),
            installed_at: 2,
            content_hash: None,
            updated_at: 0,
        };
        db.save_skill(&old_skill).expect("save old skill");
        db.save_skill(&new_skill).expect("save new skill");
        SkillService::toggle_app(&db, &old_skill.id, &AppType::Claude, true)
            .expect("enable old skill");

        let old_prompt = Prompt {
            id: "prompt-old".to_string(),
            name: "Old Prompt".to_string(),
            content: "old prompt content".to_string(),
            description: None,
            enabled: true,
            created_at: Some(1),
            updated_at: Some(1),
        };
        let new_prompt = Prompt {
            id: "prompt-new".to_string(),
            name: "New Prompt".to_string(),
            content: "new prompt content".to_string(),
            description: None,
            enabled: false,
            created_at: Some(2),
            updated_at: Some(2),
        };
        db.save_prompt(AppType::Claude.as_str(), &old_prompt)
            .expect("save old prompt");
        db.save_prompt(AppType::Claude.as_str(), &new_prompt)
            .expect("save new prompt");
        let prompt_path =
            crate::prompt_files::prompt_file_path(&AppType::Claude).expect("Claude prompt path");
        fs::create_dir_all(prompt_path.parent().expect("prompt parent"))
            .expect("create prompt directory");
        fs::write(&prompt_path, old_prompt.content.as_bytes()).expect("write old prompt file");

        let target = ProfilePayload {
            mcp: PerApp {
                claude: Some(vec![new_mcp.id.clone()]),
                ..Default::default()
            },
            skills: PerApp {
                claude: Some(vec![new_skill.id.clone()]),
                ..Default::default()
            },
            prompts: PerApp {
                claude: Some(new_prompt.id.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        save_profile_payload(&db, "profile-target", &target);

        FAIL_NEXT_PROFILE_COMMIT.store(true, Ordering::SeqCst);
        ProfileService::apply_async(
            Arc::clone(&state),
            "profile-target".to_string(),
            ProfileScope::Claude,
        )
        .await
        .expect_err("injected commit failure must roll back payload state");

        let mcps = db.get_all_mcp_servers().expect("read MCP state");
        assert!(mcps[&old_mcp.id].apps.claude);
        assert!(!mcps[&new_mcp.id].apps.claude);
        let skills = db.get_all_installed_skills().expect("read skill state");
        assert!(skills[&old_skill.id].apps.claude);
        assert!(!skills[&new_skill.id].apps.claude);
        let prompts = db
            .get_prompts(AppType::Claude.as_str())
            .expect("read prompt state");
        assert_eq!(prompts.len(), 2);
        assert!(prompts[&old_prompt.id].enabled);
        assert_eq!(prompts[&old_prompt.id].content, old_prompt.content);
        assert!(!prompts[&new_prompt.id].enabled);
        assert_eq!(prompts[&new_prompt.id].content, new_prompt.content);
        assert_eq!(
            fs::read_to_string(&prompt_path).expect("read restored prompt file"),
            old_prompt.content
        );
        assert_eq!(
            db.get_current_profile_id(ProfileScope::Claude.as_str())
                .expect("read current profile"),
            None
        );
    }
}
