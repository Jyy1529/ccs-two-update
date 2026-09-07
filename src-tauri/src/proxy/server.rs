//! HTTP代理服务器
//!
//! 基于Axum的HTTP服务器，处理代理请求
//!
//! Uses a manual hyper HTTP/1.1 accept loop with `preserve_header_case(true)` so
//! that the original header-name casing from the CLI client is captured in a
//! `HeaderCaseMap` extension.  This map is later forwarded to the upstream via
//! the hyper-based HTTP client, producing wire-level header casing identical to
//! a direct (non-proxied) CLI request.

use super::{
    failover_switch::FailoverSwitchManager,
    handlers,
    log_codes::srv as log_srv,
    provider_router::ProviderRouter,
    providers::{codex_chat_history::CodexChatHistoryStore, gemini_shadow::GeminiShadowStore},
    types::*,
    ProxyError,
};
use crate::database::Database;
use axum::{
    extract::DefaultBodyLimit,
    routing::{any, get, post},
    Router,
};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;

/// 代理服务器状态（共享）
#[derive(Clone)]
pub struct ProxyState {
    pub db: Arc<Database>,
    pub config: Arc<RwLock<ProxyConfig>>,
    pub status: Arc<RwLock<ProxyStatus>>,
    pub start_time: Arc<RwLock<Option<std::time::Instant>>>,
    /// 每个应用类型当前使用的 provider (app_type -> (provider_id, provider_name))
    pub current_providers: Arc<RwLock<std::collections::HashMap<String, (String, String)>>>,
    /// 共享的 ProviderRouter（持有熔断器状态，跨请求保持）
    pub provider_router: Arc<ProviderRouter>,
    /// Gemini Native shadow state，用于 thoughtSignature / tool call 回放
    pub gemini_shadow: Arc<GeminiShadowStore>,
    /// Codex Chat bridge history，用于恢复 previous_response_id 指向的 tool call
    pub codex_chat_history: Arc<CodexChatHistoryStore>,
    /// AppHandle，用于发射事件和更新托盘菜单
    pub app_handle: Option<tauri::AppHandle>,
    /// 故障转移切换管理器
    pub failover_manager: Arc<FailoverSwitchManager>,
}

/// 代理HTTP服务器
pub struct ProxyServer {
    config: ProxyConfig,
    state: ProxyState,
    lifecycle: Mutex<ServerLifecycle>,
    active_generation: AtomicU64,
    stop_timeout: std::time::Duration,
    #[cfg(test)]
    start_barrier: Arc<RwLock<Option<Arc<tokio::sync::Barrier>>>>,
    #[cfg(test)]
    start_bind_pause: Arc<RwLock<Option<StopPublishPause>>>,
    #[cfg(test)]
    stop_publish_pause: Arc<RwLock<Option<StopPublishPause>>>,
}

#[cfg(test)]
#[derive(Clone)]
struct StopPublishPause {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct RunningServer {
    generation: u64,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_handle: JoinHandle<()>,
}

enum ServerLifecycle {
    Stopped {
        generation: u64,
    },
    Running(RunningServer),
    Stopping {
        generation: u64,
        result: Option<Result<(), ProxyError>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProxyServerReuseState {
    Reusable,
    NeedsReap,
    Stopped,
}

impl ProxyServer {
    pub fn new(
        config: ProxyConfig,
        db: Arc<Database>,
        app_handle: Option<tauri::AppHandle>,
    ) -> Self {
        // 创建共享的 ProviderRouter（熔断器状态将跨所有请求保持）
        let provider_router = Arc::new(ProviderRouter::new(db.clone()));
        // 创建故障转移切换管理器
        let failover_manager = Arc::new(FailoverSwitchManager::new(db.clone()));

        let state = ProxyState {
            db,
            config: Arc::new(RwLock::new(config.clone())),
            status: Arc::new(RwLock::new(ProxyStatus::default())),
            start_time: Arc::new(RwLock::new(None)),
            current_providers: Arc::new(RwLock::new(std::collections::HashMap::new())),
            provider_router,
            gemini_shadow: Arc::new(GeminiShadowStore::default()),
            codex_chat_history: Arc::new(CodexChatHistoryStore::default()),
            app_handle,
            failover_manager,
        };

        Self {
            config,
            state,
            lifecycle: Mutex::new(ServerLifecycle::Stopped { generation: 0 }),
            active_generation: AtomicU64::new(0),
            stop_timeout: std::time::Duration::from_secs(5),
            #[cfg(test)]
            start_barrier: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            start_bind_pause: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            stop_publish_pause: Arc::new(RwLock::new(None)),
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
    async fn set_start_bind_pause_for_test(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.start_bind_pause.write().await = Some(StopPublishPause { entered, release });
    }

    #[cfg(test)]
    async fn wait_at_start_bind_pause_for_test(&self) {
        let pause = self.start_bind_pause.write().await.take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn set_stop_publish_pause_for_test(
        &self,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.stop_publish_pause.write().await = Some(StopPublishPause { entered, release });
    }

    #[cfg(test)]
    async fn wait_at_stop_publish_pause_for_test(&self) {
        let pause = self.stop_publish_pause.write().await.take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_stop_timeout_for_test(&mut self, timeout: std::time::Duration) {
        self.stop_timeout = timeout;
    }

    #[cfg(test)]
    pub(crate) async fn install_server_task_for_test(
        &self,
        shutdown_tx: oneshot::Sender<()>,
        server_handle: JoinHandle<()>,
    ) {
        let mut lifecycle = self.lifecycle.lock().await;
        let generation = match &*lifecycle {
            ServerLifecycle::Stopped { generation } => generation.wrapping_add(1).max(1),
            ServerLifecycle::Running(_) | ServerLifecycle::Stopping { .. } => {
                panic!("test server task already installed")
            }
        };
        self.active_generation.store(generation, Ordering::Release);
        *lifecycle = ServerLifecycle::Running(RunningServer {
            generation,
            shutdown_tx: Some(shutdown_tx),
            server_handle,
        });
        self.state.status.write().await.running = true;
        *self.state.start_time.write().await = Some(std::time::Instant::now());
    }

    pub async fn start(&self) -> Result<ProxyServerInfo, ProxyError> {
        #[cfg(test)]
        self.wait_at_start_barrier_for_test().await;

        let mut lifecycle = self.lifecycle.lock().await;
        let generation = match &*lifecycle {
            ServerLifecycle::Stopped { generation } => generation.wrapping_add(1).max(1),
            ServerLifecycle::Running(_) | ServerLifecycle::Stopping { .. } => {
                return Err(ProxyError::AlreadyRunning);
            }
        };

        let addr: SocketAddr =
            format!("{}:{}", self.config.listen_address, self.config.listen_port)
                .parse()
                .map_err(|e| ProxyError::BindFailed(format!("无效的地址: {e}")))?;

        // 创建关闭通道
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // 构建路由
        let app = self.build_router();

        // 绑定监听器
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ProxyError::BindFailed(e.to_string()))?;
        let actual_port = local_addr.port();

        #[cfg(test)]
        self.wait_at_start_bind_pause_for_test().await;

        // Acquire every async guard before publishing lifecycle side effects.
        let mut status = self.state.status.write().await;
        let mut start_time = self.state.start_time.write().await;

        // 启动服务器 — 使用手动 hyper HTTP/1.1 accept loop
        // 开启 preserve_header_case 以捕获客户端请求头的原始大小写
        let state = self.state.clone();
        let handle = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            let mut connection_tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, remote_addr) = match result {
                            Ok(v) => v,
                            Err(e) => {
                                log::error!("[{SRV}] accept 失败: {e}", SRV = log_srv::ACCEPT_ERR);
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };

                        let app = app.clone();
                        connection_tasks.spawn(async move {
                            // Peek raw TCP bytes to capture original header casing
                            // before hyper parses (and lowercases) the header names.
                            let original_cases = {
                                let mut peek_buf = vec![0u8; 8192];
                                match stream.peek(&mut peek_buf).await {
                                    Ok(n) => {
                                        let cases = super::hyper_client::OriginalHeaderCases::from_raw_bytes(&peek_buf[..n]);
                                        log::debug!(
                                            "[ProxyServer] Peeked {} bytes, captured {} header casings",
                                            n, cases.cases.len()
                                        );
                                        cases
                                    }
                                    Err(e) => {
                                        log::debug!("[ProxyServer] peek failed (non-fatal): {e}");
                                        super::hyper_client::OriginalHeaderCases::default()
                                    }
                                }
                            };

                            // service_fn 将 axum Router（tower::Service）桥接到 hyper
                            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                                let mut router = app.clone();
                                let cases = original_cases.clone();
                                async move {
                                    // 将 hyper::body::Incoming 转为 axum::body::Body，保留 extensions
                                    let (mut parts, body) = req.into_parts();

                                    // Insert our own header case map alongside hyper's internal one
                                    parts.extensions.insert(cases);
                                    parts.extensions.insert(remote_addr);

                                    let body = axum::body::Body::new(body);
                                    let axum_req = http::Request::from_parts(parts, body);
                                    <Router as tower::Service<http::Request<axum::body::Body>>>::call(&mut router, axum_req).await
                                }
                            });

                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .preserve_header_case(true)
                                .serve_connection(TokioIo::new(stream), service)
                                .await
                            {
                                // Connection reset / broken pipe 等在代理场景下很常见，debug 级别
                                log::debug!("[{SRV}] connection error: {e}", SRV = log_srv::CONN_ERR);
                            }
                        });
                    }
                    Some(result) = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                        if let Err(error) = result {
                            if !error.is_cancelled() {
                                log::warn!("[{SRV}] connection task failed: {error}", SRV = log_srv::TASK_ERROR);
                            }
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }

            // Stop owns every accepted connection. Cancelling and draining them here
            // guarantees an in-flight unlimited Provider retry cannot outlive stop().
            connection_tasks.abort_all();
            while let Some(result) = connection_tasks.join_next().await {
                if let Err(error) = result {
                    if !error.is_cancelled() {
                        log::warn!(
                            "[{SRV}] connection task failed during shutdown: {error}",
                            SRV = log_srv::TASK_ERROR
                        );
                    }
                }
            }

            // ActiveConnectionGuard releases the UI counter through a small spawned
            // task. Wait for that RAII cleanup before publishing the stopped state.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while state.status.read().await.active_connections != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        });

        *lifecycle = ServerLifecycle::Running(RunningServer {
            generation,
            shutdown_tx: Some(shutdown_tx),
            server_handle: handle,
        });
        status.running = true;
        status.address = self.config.listen_address.clone();
        status.port = actual_port;
        *start_time = Some(std::time::Instant::now());
        self.active_generation.store(generation, Ordering::Release);

        // 更新全局代理端口，用于系统代理检测
        crate::proxy::http_client::set_proxy_port(actual_port);
        log::info!("[{}] 代理服务器启动于 {local_addr}", log_srv::STARTED);

        Ok(ProxyServerInfo {
            address: self.config.listen_address.clone(),
            port: actual_port,
            started_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub async fn stop(&self) -> Result<(), ProxyError> {
        let mut lifecycle = self.lifecycle.lock().await;
        let completed = match &mut *lifecycle {
            ServerLifecycle::Stopped { .. } => return Err(ProxyError::NotRunning),
            ServerLifecycle::Stopping { .. } => None,
            ServerLifecycle::Running(running) => {
                if let Some(shutdown_tx) = running.shutdown_tx.take() {
                    let _ = shutdown_tx.send(());
                }

                let result =
                    match tokio::time::timeout(self.stop_timeout, &mut running.server_handle).await
                    {
                        Ok(Ok(())) => {
                            log::info!("[{}] 代理服务器已完全停止", log_srv::STOPPED);
                            Ok(())
                        }
                        Ok(Err(error)) => {
                            log::warn!("[{}] 代理服务器任务异常终止: {error}", log_srv::TASK_ERROR);
                            Err(ProxyError::StopFailed(error.to_string()))
                        }
                        Err(_) => {
                            log::warn!(
                                "[{}] 代理服务器停止超时，强制终止任务",
                                log_srv::STOP_TIMEOUT
                            );
                            running.server_handle.abort();
                            match (&mut running.server_handle).await {
                                Ok(()) => {}
                                Err(error) if error.is_cancelled() => {}
                                Err(error) => log::warn!(
                                    "[{}] 强制终止代理服务器任务后等待失败: {error}",
                                    log_srv::TASK_ERROR
                                ),
                            }
                            Err(ProxyError::StopTimeout)
                        }
                    };

                Some((running.generation, result))
            }
        };

        if let Some((generation, result)) = completed {
            *lifecycle = ServerLifecycle::Stopping {
                generation,
                result: Some(result),
            };
        }

        let generation = match &*lifecycle {
            ServerLifecycle::Stopping { generation, .. } => *generation,
            ServerLifecycle::Stopped { .. } | ServerLifecycle::Running(_) => unreachable!(),
        };

        #[cfg(test)]
        self.wait_at_stop_publish_pause_for_test().await;

        self.publish_stopped_if_current(generation).await;
        match std::mem::replace(&mut *lifecycle, ServerLifecycle::Stopped { generation }) {
            ServerLifecycle::Stopping {
                result: Some(result),
                ..
            } => result,
            ServerLifecycle::Stopping { result: None, .. }
            | ServerLifecycle::Stopped { .. }
            | ServerLifecycle::Running(_) => unreachable!(),
        }
    }

    async fn publish_stopped_if_current(&self, generation: u64) {
        if self.active_generation.load(Ordering::Acquire) == generation {
            self.state.status.write().await.running = false;
            *self.state.start_time.write().await = None;
            let _ = self.active_generation.compare_exchange(
                generation,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    pub(crate) async fn reuse_state(&self) -> ProxyServerReuseState {
        match &*self.lifecycle.lock().await {
            ServerLifecycle::Stopped { .. } => ProxyServerReuseState::Stopped,
            ServerLifecycle::Running(running)
                if running.shutdown_tx.is_some() && !running.server_handle.is_finished() =>
            {
                ProxyServerReuseState::Reusable
            }
            ServerLifecycle::Running(_) | ServerLifecycle::Stopping { .. } => {
                ProxyServerReuseState::NeedsReap
            }
        }
    }

    pub(crate) async fn is_running(&self) -> bool {
        self.reuse_state().await == ProxyServerReuseState::Reusable
    }

    pub(crate) async fn bind_config_matches(&self, desired: &ProxyConfig) -> bool {
        if self.reuse_state().await != ProxyServerReuseState::Reusable
            || self.config.listen_address != desired.listen_address
        {
            return false;
        }

        if desired.listen_port == 0 {
            return self.config.listen_port == 0;
        }

        self.state.status.read().await.port == desired.listen_port
    }

    pub async fn get_status(&self) -> ProxyStatus {
        let running = self.is_running().await;
        let mut status = self.state.status.read().await.clone();
        status.running = running;

        // 计算运行时间
        if running {
            if let Some(start) = *self.state.start_time.read().await {
                status.uptime_seconds = start.elapsed().as_secs();
            }
        } else {
            status.uptime_seconds = 0;
        }

        // 从 current_providers HashMap 获取每个应用类型当前正在使用的 provider
        let current_providers = self.state.current_providers.read().await;
        status.active_targets = current_providers
            .iter()
            .map(|(app_type, (provider_id, provider_name))| ActiveTarget {
                app_type: app_type.clone(),
                provider_id: provider_id.clone(),
                provider_name: provider_name.clone(),
            })
            .collect();

        status
    }

    /// 更新某个应用类型当前“目标供应商”（用于 UI 展示 active_targets）
    ///
    /// 注意：这不代表该供应商一定已经处理过请求，而是用于“热切换/启用故障转移立即切 P1”
    /// 等场景下，让 UI 能立刻反映最新目标。
    pub async fn set_active_target(&self, app_type: &str, provider_id: &str, provider_name: &str) {
        let mut current_providers = self.state.current_providers.write().await;
        current_providers.insert(
            app_type.to_string(),
            (provider_id.to_string(), provider_name.to_string()),
        );
    }

    pub(crate) async fn replace_active_targets(&self, targets: &[ActiveTarget]) {
        let mut current_providers = self.state.current_providers.write().await;
        current_providers.clear();
        current_providers.extend(targets.iter().map(|target| {
            (
                target.app_type.clone(),
                (target.provider_id.clone(), target.provider_name.clone()),
            )
        }));
    }

    fn build_router(&self) -> Router {
        Router::new()
            // 健康检查
            .route("/health", get(handlers::health_check))
            .route("/status", get(handlers::get_status))
            // Claude API (支持带前缀和不带前缀两种格式)
            .route("/v1/messages", post(handlers::handle_messages))
            .route("/claude/v1/messages", post(handlers::handle_messages))
            // Claude Desktop 3P 本地 gateway（独立 provider namespace）
            .route(
                "/claude-desktop/v1/models",
                get(handlers::handle_claude_desktop_models),
            )
            .route(
                "/claude-desktop/v1/messages",
                post(handlers::handle_claude_desktop_messages),
            )
            // OpenAI Chat Completions API (Codex CLI，支持带前缀和不带前缀)
            .route("/chat/completions", post(handlers::handle_chat_completions))
            .route(
                "/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route(
                "/v1/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            .route(
                "/codex/v1/chat/completions",
                post(handlers::handle_chat_completions),
            )
            // OpenAI Models API (Codex CLI reachability check)
            .route("/models", get(handlers::handle_models))
            .route("/v1/models", get(handlers::handle_models))
            // OpenAI Responses API (Codex CLI，支持带前缀和不带前缀)
            .route("/responses", post(handlers::handle_responses))
            .route("/v1/responses", post(handlers::handle_responses))
            .route("/v1/v1/responses", post(handlers::handle_responses))
            .route("/codex/v1/responses", post(handlers::handle_responses))
            // Grok Build uses the Responses protocol but has an independent
            // provider namespace and failover queue.
            .route(
                "/grokbuild/v1/responses",
                post(handlers::handle_grokbuild_responses),
            )
            // OpenAI Responses Compact API (Codex CLI 远程压缩，透传)
            .route(
                "/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/v1/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/codex/v1/responses/compact",
                post(handlers::handle_responses_compact),
            )
            .route(
                "/grokbuild/v1/responses/compact",
                post(handlers::handle_grokbuild_responses_compact),
            )
            // Codex standalone Alpha Search API. All local aliases normalize to
            // the selected provider's canonical sibling `/alpha/search` route.
            .route("/alpha/search", post(handlers::handle_alpha_search))
            .route("/v1/alpha/search", post(handlers::handle_alpha_search))
            .route("/v1/v1/alpha/search", post(handlers::handle_alpha_search))
            .route(
                "/codex/v1/alpha/search",
                post(handlers::handle_alpha_search),
            )
            // Gemini API (支持带前缀和不带前缀)
            //
            // 用 `any(..)` 覆盖所有 HTTP 方法：除了 POST `:generateContent` /
            // `:streamGenerateContent` / `:countTokens` 之外，Gemini SDK / CLI 还会发
            // GET `/models`、GET `/models/<id>` 等只读端点。如果只挂 POST，这些 GET
            // 请求会在路由层 404，绕过本地代理的统计、整流和故障转移。
            .route("/v1beta/*path", any(handlers::handle_gemini))
            .route("/gemini/v1beta/*path", any(handlers::handle_gemini))
            // Gemini 的 GA 版本也叫 /v1，给原 SDK 留一条出口
            .route("/gemini/v1/*path", any(handlers::handle_gemini))
            // 提高默认请求体大小限制（避免 413 Payload Too Large）
            .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
            .with_state(self.state.clone())
    }

    /// 在不重启服务的情况下更新运行时配置
    pub async fn apply_runtime_config(&self, config: &ProxyConfig) {
        *self.state.config.write().await = config.clone();
    }

    /// 热更新熔断器配置
    ///
    /// 将新配置应用到所有已创建的熔断器实例
    pub async fn update_circuit_breaker_configs(
        &self,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state.provider_router.update_all_configs(config).await;
    }

    pub async fn update_circuit_breaker_config_for_app(
        &self,
        app_type: &str,
        config: super::circuit_breaker::CircuitBreakerConfig,
    ) {
        self.state
            .provider_router
            .update_app_configs(app_type, config)
            .await;
    }

    /// 重置指定 Provider 的熔断器
    pub async fn fill_key_pool_status(
        &self,
        status: &mut crate::provider_groups::ProviderGroupStatus,
    ) {
        self.state
            .provider_router
            .fill_key_pool_status(status)
            .await;
    }

    pub async fn clear_key_pool_runtime(&self, app_type: &str, group_id: &str) {
        self.state
            .provider_router
            .clear_key_pool_runtime(app_type, group_id)
            .await;
    }

    /// 重置指定 Provider 的熔断器
    pub async fn reset_provider_circuit_breaker(&self, provider_id: &str, app_type: &str) {
        self.state
            .provider_router
            .reset_provider_breaker(provider_id, app_type)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        CodexAgentRoleRouting, CodexFrontendAgentRoleOverride, LocalProxyRetryPolicy, Provider,
        ProviderMeta, DEFAULT_LOCAL_PROXY_RETRY_MESSAGE,
    };
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::{extract::State, response::IntoResponse, routing::post, Json};
    use serde_json::{json, Value};
    use serial_test::serial;
    use std::{
        ffi::OsString,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct CompletionFlag(Arc<AtomicBool>);

    impl Drop for CompletionFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct TestHome {
        _dir: TempDir,
        original_test_home: Option<OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create isolated proxy test home");
            let original_test_home = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload isolated proxy settings");
            Self {
                _dir: dir,
                original_test_home,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.original_test_home {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    #[derive(Debug)]
    struct CapturedSearchRequest {
        path_and_query: String,
        authorization: Option<String>,
        body: Value,
    }

    #[tokio::test]
    #[serial]
    async fn alpha_search_routes_forward_to_canonical_upstream() {
        let _home = TestHome::new();
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<CapturedSearchRequest>::new()));
        let mock_app = Router::new().route(
            "/v1/alpha/search",
            post({
                let captured = captured.clone();
                move |request: axum::extract::Request| {
                    let captured = captured.clone();
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = axum::body::to_bytes(body, 1024 * 1024)
                            .await
                            .expect("read mock request body");
                        captured.lock().await.push(CapturedSearchRequest {
                            path_and_query: parts
                                .uri
                                .path_and_query()
                                .map(|value| value.as_str().to_string())
                                .unwrap_or_else(|| parts.uri.path().to_string()),
                            authorization: parts
                                .headers
                                .get(header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .map(ToString::to_string),
                            body: serde_json::from_slice(&body).expect("parse mock request body"),
                        });

                        let mut headers = HeaderMap::new();
                        headers.insert(
                            header::CONTENT_TYPE,
                            "application/json".parse().expect("content type"),
                        );
                        headers.insert(
                            "x-upstream-request-id",
                            "search-1".parse().expect("request id"),
                        );
                        (
                            StatusCode::ACCEPTED,
                            headers,
                            r#"{"encrypted_output":"ciphertext"}"#,
                        )
                    }
                }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind mock upstream");
        let mock_addr = mock_listener.local_addr().expect("mock upstream address");
        let mock_handle = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app)
                .await
                .expect("serve mock upstream");
        });

        let db = Arc::new(Database::memory().expect("memory database"));
        let provider = Provider::with_id(
            "alpha-search-upstream".to_string(),
            "Alpha Search Upstream".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1"),
                "auth": {"OPENAI_API_KEY": "upstream-secret"}
            }),
            None,
        );
        db.save_provider("codex", &provider)
            .expect("save test provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select test provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                enable_logging: false,
                non_streaming_timeout: 10,
                ..ProxyConfig::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let client = reqwest::Client::new();
        let aliases = [
            "/alpha/search",
            "/v1/alpha/search",
            "/v1/v1/alpha/search",
            "/codex/v1/alpha/search",
        ];

        for (index, path) in aliases.iter().enumerate() {
            let response = client
                .post(format!(
                    "http://127.0.0.1:{}{}?client_version=0.144.6",
                    proxy_info.port, path
                ))
                .header(header::AUTHORIZATION, "Bearer client-secret")
                .json(&json!({
                    "id": format!("search-{index}"),
                    "model": "gpt-5.6-sol",
                    "commands": {"search_query": [{"q": "test"}]}
                }))
                .send()
                .await
                .expect("send alpha search request");

            assert_eq!(response.status(), StatusCode::ACCEPTED, "alias {path}");
            assert_eq!(
                response
                    .headers()
                    .get("x-upstream-request-id")
                    .and_then(|value| value.to_str().ok()),
                Some("search-1"),
                "alias {path}"
            );
            assert_eq!(
                response.text().await.expect("read proxy response"),
                r#"{"encrypted_output":"ciphertext"}"#,
                "alias {path}"
            );
        }

        let mut full_url_provider = Provider::with_id(
            "alpha-search-full-url".to_string(),
            "Alpha Search Full URL".to_string(),
            json!({
                "base_url": format!("http://{mock_addr}/v1/responses?api-version=test"),
                "auth": {"OPENAI_API_KEY": "full-url-secret"}
            }),
            None,
        );
        full_url_provider.meta = Some(ProviderMeta {
            is_full_url: Some(true),
            ..ProviderMeta::default()
        });
        db.save_provider("codex", &full_url_provider)
            .expect("save full URL provider");
        db.set_current_provider("codex", &full_url_provider.id)
            .expect("select full URL provider");

        let response = client
            .post(format!(
                "http://127.0.0.1:{}/v1/alpha/search?client_version=0.144.6",
                proxy_info.port
            ))
            .header(header::AUTHORIZATION, "Bearer client-secret")
            .json(&json!({
                "id": "search-full-url",
                "model": "gpt-5.6-sol",
                "commands": {"search_query": [{"q": "full URL"}]}
            }))
            .send()
            .await
            .expect("send full URL alpha search request");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.text().await.expect("read full URL response"),
            r#"{"encrypted_output":"ciphertext"}"#
        );

        proxy.stop().await.expect("stop test proxy");
        mock_handle.abort();

        let captured = captured.lock().await;
        assert_eq!(captured.len(), aliases.len() + 1);
        for (index, request) in captured.iter().take(aliases.len()).enumerate() {
            assert_eq!(
                request.path_and_query,
                "/v1/alpha/search?client_version=0.144.6"
            );
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer upstream-secret")
            );
            assert_eq!(request.body["id"], format!("search-{index}"));
            assert_eq!(request.body["model"], "gpt-5.6-sol");
            assert_eq!(request.body["commands"]["search_query"][0]["q"], "test");
        }

        let full_url_request = captured.last().expect("full URL request captured");
        assert_eq!(
            full_url_request.path_and_query,
            "/v1/alpha/search?api-version=test&client_version=0.144.6"
        );
        assert_eq!(
            full_url_request.authorization.as_deref(),
            Some("Bearer full-url-secret")
        );
        assert_eq!(full_url_request.body["id"], "search-full-url");
        assert_eq!(
            full_url_request.body["commands"]["search_query"][0]["q"],
            "full URL"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_concurrent_start_allows_one_running_server_task() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let proxy = Arc::new(ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        ));
        proxy
            .set_start_barrier_for_test(Arc::new(tokio::sync::Barrier::new(2)))
            .await;

        let first_proxy = proxy.clone();
        let first = tokio::spawn(async move { first_proxy.start().await });
        let second_proxy = proxy.clone();
        let second = tokio::spawn(async move { second_proxy.start().await });
        let results = [
            first.await.expect("first start task"),
            second.await.expect("second start task"),
        ];
        let successful_ports: Vec<u16> = results
            .iter()
            .filter_map(|result| result.as_ref().ok().map(|info| info.port))
            .collect();
        let already_running = results
            .iter()
            .filter(|result| matches!(result, Err(ProxyError::AlreadyRunning)))
            .count();
        let status = proxy.get_status().await;
        proxy.stop().await.expect("stop surviving proxy task");

        assert_eq!(successful_ports.len(), 1);
        assert_eq!(already_running, 1);
        assert!(status.running);
        assert_eq!(status.port, successful_ports[0]);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_bind_config_comparison_handles_ephemeral_port_semantics() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let ephemeral = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db.clone(),
            None,
        );
        let ephemeral_info = ephemeral.start().await.expect("start ephemeral proxy");
        let persisted_actual = ProxyConfig {
            listen_port: ephemeral_info.port,
            ..Default::default()
        };
        let mut another_explicit = persisted_actual.clone();
        another_explicit.listen_port = persisted_actual.listen_port.wrapping_add(1).max(1);

        assert!(
            ephemeral
                .bind_config_matches(&ProxyConfig {
                    listen_port: 0,
                    ..Default::default()
                })
                .await
        );
        assert!(ephemeral.bind_config_matches(&persisted_actual).await);
        assert!(!ephemeral.bind_config_matches(&another_explicit).await);
        ephemeral.stop().await.expect("stop ephemeral proxy");

        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve fixed proxy port");
        let fixed_port = reserved.local_addr().expect("read fixed port").port();
        drop(reserved);
        let fixed = ProxyServer::new(
            ProxyConfig {
                listen_port: fixed_port,
                ..Default::default()
            },
            db,
            None,
        );
        fixed.start().await.expect("start fixed-port proxy");
        assert!(
            !fixed
                .bind_config_matches(&ProxyConfig {
                    listen_port: 0,
                    ..Default::default()
                })
                .await,
            "requesting a fresh ephemeral bind must restart a fixed-port listener"
        );
        fixed.stop().await.expect("stop fixed-port proxy");
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_start_after_bind_does_not_publish_generation() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let proxy = Arc::new(ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        ));
        let bind_entered = Arc::new(tokio::sync::Notify::new());
        let bind_release = Arc::new(tokio::sync::Notify::new());
        proxy
            .set_start_bind_pause_for_test(bind_entered.clone(), bind_release)
            .await;

        let starting_proxy = proxy.clone();
        let start_task = tokio::spawn(async move { starting_proxy.start().await });
        tokio::time::timeout(Duration::from_secs(1), bind_entered.notified())
            .await
            .expect("start reaches the post-bind publication window");
        start_task.abort();
        let start_join = start_task.await;

        let status_after_cancel = proxy.get_status().await;
        let generation_after_cancel = proxy.active_generation.load(Ordering::Acquire);
        let retry_start = proxy.start().await;
        if retry_start.is_ok() {
            proxy.stop().await.expect("stop retry generation");
        }

        assert!(start_join.is_err_and(|error| error.is_cancelled()));
        assert!(!status_after_cancel.running);
        assert_eq!(generation_after_cancel, 0);
        assert!(retry_start.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_stop_timeout_reaps_old_task_before_next_generation() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let mut proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        );
        proxy.stop_timeout = Duration::from_millis(200);
        let proxy = Arc::new(proxy);

        let release_old_task = Arc::new(tokio::sync::Notify::new());
        let old_task_completed = Arc::new(AtomicBool::new(false));
        let old_state = proxy.state.clone();
        let old_release = release_old_task.clone();
        let old_completed = old_task_completed.clone();
        let old_handle = tokio::spawn(async move {
            let _completion = CompletionFlag(old_completed);
            old_release.notified().await;
            old_state.status.write().await.running = false;
            *old_state.start_time.write().await = None;
        });
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        proxy
            .install_server_task_for_test(shutdown_tx, old_handle)
            .await;

        let stop_result = proxy.stop().await;
        let completed_when_stop_returned = old_task_completed.load(Ordering::SeqCst);
        let next_start = proxy.start().await;

        release_old_task.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !old_task_completed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old task reaches a terminal state");
        let status_after_old_task = proxy.get_status().await;
        if next_start.is_ok() {
            proxy.stop().await.expect("stop next proxy generation");
        }

        assert!(matches!(stop_result, Err(ProxyError::StopTimeout)));
        assert!(
            completed_when_stop_returned,
            "timeout must abort and await the old server task before returning"
        );
        assert!(
            next_start.is_ok(),
            "next generation should start after timeout"
        );
        assert!(
            status_after_old_task.running,
            "an old generation must not publish running=false over the new generation"
        );
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_stop_retains_server_task_ownership() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let proxy = Arc::new(ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        ));

        let stop_entered = Arc::new(tokio::sync::Notify::new());
        let release_old_task = Arc::new(tokio::sync::Notify::new());
        let old_task_completed = Arc::new(AtomicBool::new(false));
        let entered = stop_entered.clone();
        let release = release_old_task.clone();
        let completed = old_task_completed.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let old_handle = tokio::spawn(async move {
            let _completion = CompletionFlag(completed);
            let _ = shutdown_rx.await;
            entered.notify_one();
            release.notified().await;
        });
        proxy
            .install_server_task_for_test(shutdown_tx, old_handle)
            .await;

        let stopping_proxy = proxy.clone();
        let stop_task = tokio::spawn(async move { stopping_proxy.stop().await });
        tokio::time::timeout(Duration::from_secs(1), stop_entered.notified())
            .await
            .expect("stop sends the shutdown signal");
        stop_task.abort();
        let stop_join = stop_task.await;

        let start_result = proxy.start().await;
        release_old_task.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !old_task_completed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old task completes after release");
        let reuse_state_after_completion = proxy.reuse_state().await;
        let running_after_completion = proxy.is_running().await;
        proxy.stop().await.expect("retry stop reaps the owned task");

        assert!(stop_join.is_err_and(|error| error.is_cancelled()));
        assert!(matches!(start_result, Err(ProxyError::AlreadyRunning)));
        assert_eq!(
            reuse_state_after_completion,
            ProxyServerReuseState::NeedsReap
        );
        assert!(!running_after_completion);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_cancelled_stop_after_join_ready_keeps_generation_owned() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let proxy = Arc::new(ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        ));

        let publish_entered = Arc::new(tokio::sync::Notify::new());
        let publish_release = Arc::new(tokio::sync::Notify::new());
        proxy
            .set_stop_publish_pause_for_test(publish_entered.clone(), publish_release)
            .await;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = shutdown_rx.await;
        });
        proxy
            .install_server_task_for_test(shutdown_tx, handle)
            .await;

        let stopping_proxy = proxy.clone();
        let stop_task = tokio::spawn(async move { stopping_proxy.stop().await });
        tokio::time::timeout(Duration::from_secs(1), publish_entered.notified())
            .await
            .expect("stop reaches the pre-publication window");
        stop_task.abort();
        let stop_join = stop_task.await;

        let start_result = proxy.start().await;
        let retry_stop = proxy.stop().await;

        assert!(stop_join.is_err_and(|error| error.is_cancelled()));
        assert!(matches!(start_result, Err(ProxyError::AlreadyRunning)));
        assert!(retry_stop.is_ok());
        assert!(!proxy.get_status().await.running);
    }

    #[derive(Clone)]
    struct RetryUpstreamState {
        attempts: Arc<AtomicUsize>,
        keep_matching: Arc<AtomicBool>,
    }

    async fn retry_upstream_response(State(state): State<RetryUpstreamState>) -> impl IntoResponse {
        state.attempts.fetch_add(1, Ordering::SeqCst);
        if state.keep_matching.load(Ordering::SeqCst) {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": { "message": DEFAULT_LOCAL_PROXY_RETRY_MESSAGE }
                })),
            )
        } else {
            (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "message": "stop test retries" } })),
            )
        }
    }

    async fn spawn_switchable_retry_upstream() -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
        tokio::task::JoinHandle<()>,
    ) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let keep_matching = Arc::new(AtomicBool::new(true));
        let app = Router::new()
            .route("/v1/responses", post(retry_upstream_response))
            .with_state(RetryUpstreamState {
                attempts: attempts.clone(),
                keep_matching: keep_matching.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind retry upstream");
        let address = listener.local_addr().expect("retry upstream address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve retry upstream");
        });
        (format!("http://{address}"), attempts, keep_matching, server)
    }

    async fn abort_and_join_test_task(handle: tokio::task::JoinHandle<()>) {
        handle.abort();
        match handle.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("test fixture task join failed: {error}"),
        }
    }

    async fn spawn_approval_recording_upstream() -> (
        String,
        Arc<tokio::sync::Mutex<Vec<(http::HeaderMap, serde_json::Value)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/responses",
            post(
                move |headers: http::HeaderMap, Json(body): Json<serde_json::Value>| {
                    let captured = Arc::clone(&captured_for_handler);
                    async move {
                        captured.lock().await.push((headers, body));
                        Json(json!({
                            "id": "approval-success",
                            "status": "completed",
                            "output": []
                        }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind approval recording upstream");
        let address = listener
            .local_addr()
            .expect("approval recording upstream address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve approval recording upstream");
        });
        (format!("http://{address}"), captured, server)
    }

    #[tokio::test]
    #[serial]
    async fn accepted_loopback_socket_reaches_role_header_validation_as_loopback() {
        let _home = TestHome::new();
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let body = br#"{"model":"gpt-5.6-sol","input":"role peer test","stream":false}"#;
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nX-CC-Switch-Role-Route: frontend\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            proxy_info.port,
            body.len(),
            String::from_utf8_lossy(body)
        );
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", proxy_info.port))
            .await
            .expect("connect loopback client");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write role request");
        client.flush().await.expect("flush role request");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("role validation response timeout")
            .expect("read role validation response");
        proxy.stop().await.expect("stop test proxy");

        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("provided together"), "{response}");
        assert!(!response.contains("loopback"), "{response}");
    }

    #[tokio::test]
    #[serial]
    async fn codex_auto_review_role_headers_always_use_current_provider() {
        let _home = TestHome::new();
        let (provider_a_url, provider_a_requests, provider_a_server) =
            spawn_approval_recording_upstream().await;
        let (provider_b_url, provider_b_requests, provider_b_server) =
            spawn_approval_recording_upstream().await;
        let db = Arc::new(Database::memory().expect("create proxy test database"));
        let routing = CodexAgentRoleRouting {
            enabled: Some(true),
            frontend: Some(CodexFrontendAgentRoleOverride {
                provider_id: Some("provider-b".to_string()),
                upstream_model: Some("frontend-upstream".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut provider_a = Provider::with_id(
            "provider-a".to_string(),
            "Provider A".to_string(),
            json!({
                "base_url": provider_a_url,
                "auth": { "OPENAI_API_KEY": "test-key-a" }
            }),
            None,
        );
        provider_a.meta = Some(ProviderMeta {
            codex_agent_role_routing: Some(routing.clone()),
            ..Default::default()
        });
        let provider_b = Provider::with_id(
            "provider-b".to_string(),
            "Provider B".to_string(),
            json!({
                "base_url": provider_b_url,
                "auth": { "OPENAI_API_KEY": "test-key-b" }
            }),
            None,
        );
        db.save_provider("codex", &provider_a)
            .expect("save current provider A");
        db.save_provider("codex", &provider_b)
            .expect("save frontend provider B");
        db.set_current_provider("codex", &provider_a.id)
            .expect("set database current provider A");
        crate::settings::set_current_provider(
            &crate::app_config::AppType::Codex,
            Some(&provider_a.id),
        )
        .expect("set local current provider A");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db.clone(),
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");
        let token = crate::services::codex_agent_roles::create_codex_role_route_token(
            &provider_a.id,
            crate::services::codex_agent_roles::FRONTEND_ROLE_ROUTE_VALUE,
            &routing,
        );
        let header_cases = [
            "X-CC-Switch-Role-Route: frontend\r\n".to_string(),
            format!(
                "X-CC-Switch-Role-Route: frontend\r\nX-CC-Switch-Role-Owner: {}\r\nX-CC-Switch-Role-Token: {}\r\n",
                provider_a.id, token
            ),
        ];
        let body = br#"{"model":"codex-auto-review","input":"review","stream":false}"#;

        for role_headers in header_cases {
            let request = format!(
                "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                proxy_info.port,
                role_headers,
                body.len(),
                String::from_utf8_lossy(body)
            );
            let mut client = tokio::net::TcpStream::connect(("127.0.0.1", proxy_info.port))
                .await
                .expect("connect approval client");
            client
                .write_all(request.as_bytes())
                .await
                .expect("write approval request");
            client.flush().await.expect("flush approval request");
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
                .await
                .expect("approval response timeout")
                .expect("read approval response");
            let response = String::from_utf8_lossy(&response);
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        }

        proxy.stop().await.expect("stop test proxy");
        abort_and_join_test_task(provider_a_server).await;
        abort_and_join_test_task(provider_b_server).await;

        let provider_a_requests = provider_a_requests.lock().await;
        assert_eq!(provider_a_requests.len(), 2);
        assert_eq!(provider_b_requests.lock().await.len(), 0);
        for (headers, body) in provider_a_requests.iter() {
            assert_eq!(body["model"], "codex-auto-review");
            assert!(headers.get("x-cc-switch-role-route").is_none());
            assert!(headers.get("x-cc-switch-role-owner").is_none());
            assert!(headers.get("x-cc-switch-role-token").is_none());
        }
        assert_eq!(
            db.get_current_provider("codex")
                .expect("read current provider")
                .as_deref(),
            Some("provider-a")
        );
    }

    #[tokio::test]
    #[serial]
    async fn tcp_disconnect_cancels_unlimited_provider_retry() {
        let _home = TestHome::new();
        let (upstream_url, attempts, keep_matching, upstream_server) =
            spawn_switchable_retry_upstream().await;
        let db = Arc::new(Database::memory().expect("create proxy test database"));

        let mut provider = Provider::with_id(
            "tcp-disconnect-provider".to_string(),
            "TCP disconnect provider".to_string(),
            json!({
                "base_url": upstream_url,
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            local_proxy_retry_policy: Some(LocalProxyRetryPolicy {
                enabled: Some(true),
                max_retries: 0,
                retry_delay_ms: 20,
                custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
                error_types: vec![],
            }),
            ..Default::default()
        });
        db.save_provider("codex", &provider)
            .expect("save retry provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select retry provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");

        let body = br#"{"model":"gpt-5.6-sol","input":"disconnect test","stream":false}"#;
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            proxy_info.port,
            body.len(),
            String::from_utf8_lossy(body)
        );
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", proxy_info.port))
            .await
            .expect("connect test client");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write proxy request");
        client.flush().await.expect("flush proxy request");

        let retries_started = tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();
        let active_before_disconnect = proxy.get_status().await.active_connections;

        drop(client);

        let disconnected = tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.get_status().await.active_connections != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();

        tokio::time::sleep(Duration::from_millis(60)).await;
        let stopped_at = attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let attempts_stable = attempts.load(Ordering::SeqCst) == stopped_at;
        let status_after_disconnect = proxy.get_status().await;

        // A failed cancellation assertion must not leak an unlimited retry task
        // into later tests. Change the upstream response to a non-matching error
        // so any surviving handler exits on its next attempt before cleanup.
        keep_matching.store(false, Ordering::SeqCst);
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.get_status().await.active_connections != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        abort_and_join_test_task(upstream_server).await;
        proxy.stop().await.expect("stop test proxy");

        assert!(retries_started, "proxy should start same-provider retries");
        assert_eq!(
            active_before_disconnect, 1,
            "the in-flight client request should own one active connection"
        );
        assert!(
            disconnected,
            "dropping the real TCP client must cancel the proxy handler"
        );
        assert!(
            attempts_stable,
            "client disconnect must stop creating retry attempts"
        );
        assert_eq!(status_after_disconnect.active_connections, 0);
        assert_eq!(status_after_disconnect.total_requests, 1);
        assert_eq!(status_after_disconnect.success_requests, 0);
        assert_eq!(status_after_disconnect.failed_requests, 0);
    }

    #[tokio::test]
    #[serial]
    async fn lifecycle_review_stopping_proxy_cancels_unlimited_provider_retry_connections() {
        let _home = TestHome::new();
        let (upstream_url, attempts, keep_matching, upstream_server) =
            spawn_switchable_retry_upstream().await;
        let db = Arc::new(Database::memory().expect("create proxy test database"));

        let mut provider = Provider::with_id(
            "proxy-stop-provider".to_string(),
            "Proxy stop provider".to_string(),
            json!({
                "base_url": upstream_url,
                "auth": { "OPENAI_API_KEY": "test-key" }
            }),
            None,
        );
        provider.meta = Some(ProviderMeta {
            local_proxy_retry_policy: Some(LocalProxyRetryPolicy {
                enabled: Some(true),
                max_retries: 0,
                retry_delay_ms: 20,
                custom_messages: vec![DEFAULT_LOCAL_PROXY_RETRY_MESSAGE.to_string()],
                error_types: vec![],
            }),
            ..Default::default()
        });
        db.save_provider("codex", &provider)
            .expect("save retry provider");
        db.set_current_provider("codex", &provider.id)
            .expect("select retry provider");

        let proxy = ProxyServer::new(
            ProxyConfig {
                listen_port: 0,
                ..Default::default()
            },
            db,
            None,
        );
        let proxy_info = proxy.start().await.expect("start test proxy");

        let body = br#"{"model":"gpt-5.6-sol","input":"stop test","stream":false}"#;
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            proxy_info.port,
            body.len(),
            String::from_utf8_lossy(body)
        );
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", proxy_info.port))
            .await
            .expect("connect test client");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write proxy request");
        client.flush().await.expect("flush proxy request");

        let retries_started = tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok();
        let active_before_stop = proxy.get_status().await.active_connections;

        let stop_result = proxy.stop().await;
        let status_after_stop = proxy.get_status().await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let stopped_at = attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let attempts_stable = attempts.load(Ordering::SeqCst) == stopped_at;

        keep_matching.store(false, Ordering::SeqCst);
        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while proxy.get_status().await.active_connections != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        abort_and_join_test_task(upstream_server).await;

        assert!(retries_started, "proxy should start same-provider retries");
        assert_eq!(active_before_stop, 1);
        assert!(stop_result.is_ok(), "proxy stop should succeed");
        assert_eq!(
            status_after_stop.active_connections, 0,
            "stop must wait for active connection cancellation"
        );
        assert!(
            attempts_stable,
            "stop must prevent existing unlimited retry connections from issuing new attempts"
        );
    }
}
