use crate::database::Database;
use crate::services::{ProxyService, UsageCache};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// 全局应用状态
pub struct AppState {
    pub db: Arc<Database>,
    pub proxy_service: ProxyService,
    pub usage_cache: Arc<UsageCache>,
    codex_provider_lifecycle_lock: Arc<AsyncMutex<()>>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(db: Arc<Database>) -> Self {
        let proxy_service = ProxyService::new(db.clone());

        Self {
            db,
            proxy_service,
            usage_cache: Arc::new(UsageCache::new()),
            codex_provider_lifecycle_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    pub(crate) fn owned_clone(&self) -> Arc<Self> {
        Arc::new(Self {
            db: Arc::clone(&self.db),
            proxy_service: self.proxy_service.clone(),
            usage_cache: Arc::clone(&self.usage_cache),
            codex_provider_lifecycle_lock: Arc::clone(&self.codex_provider_lifecycle_lock),
        })
    }

    pub(crate) async fn lock_codex_provider_lifecycle(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.codex_provider_lifecycle_lock
            .clone()
            .lock_owned()
            .await
    }
}
