//! Short-lived prepared plans, idempotent starts, cancellation and local history.

use super::{
    input_error, probes,
    target::{redact, PinnedTarget},
    transport::{Cancellation, Executor, REQUEST_TIMEOUT},
    PrepareRequest, Probe, ProbeResult, ProbeStatus, RunStatus, ValidationMode, ValidationPlan,
    ValidationRun,
};
use crate::{database::Database, error::AppError, store::AppState};
use chrono::Utc;
use futures::FutureExt;
use once_cell::sync::Lazy;
use std::{
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

const MAX_PLANS: usize = 32;
const MAX_RUNNING: usize = 2;
const MAX_CACHED_RUNS: usize = 64;
const PLAN_TTL_SECONDS: u64 = 300;

static RUNTIME: Lazy<Arc<Runtime>> = Lazy::new(|| Arc::new(Runtime::default()));

pub struct ModelValidationService;

impl ModelValidationService {
    pub fn prepare(state: &AppState, request: PrepareRequest) -> Result<ValidationPlan, AppError> {
        RUNTIME.prepare(&state.db, request)
    }
    pub fn start(state: &AppState, plan_id: &str) -> Result<ValidationRun, AppError> {
        RUNTIME.start(state.db.clone(), Some(state.owned_clone()), plan_id)
    }
    pub fn get(state: &AppState, run_id: &str) -> Result<ValidationRun, AppError> {
        RUNTIME.get(&state.db, run_id)
    }
    pub fn list(
        state: &AppState,
        app_id: Option<&str>,
        provider_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<ValidationRun>, AppError> {
        RUNTIME.list(&state.db, app_id, provider_id, limit.unwrap_or(50))
    }
    pub fn cancel(state: &AppState, run_id: &str) -> Result<bool, AppError> {
        RUNTIME.cancel(&state.db, run_id)
    }
}

struct Prepared {
    plan: ValidationPlan,
    owner: Weak<Database>,
    target: PinnedTarget,
    comparison: Option<PinnedTarget>,
    repeats: u32,
    expires: Instant,
}

struct ActiveRun {
    owner: Weak<Database>,
    run: Arc<Mutex<ValidationRun>>,
    cancellation: Cancellation,
    inserted: Instant,
}

#[derive(Default)]
struct Registry {
    plans: HashMap<String, Prepared>,
    runs: HashMap<String, ActiveRun>,
}

#[derive(Default)]
pub(super) struct Runtime {
    registry: Mutex<Registry>,
}

fn same_owner(owner: &Weak<Database>, db: &Arc<Database>) -> bool {
    owner
        .upgrade()
        .is_some_and(|original| Arc::ptr_eq(&original, db))
}
fn lock_error() -> AppError {
    input_error("模型验证状态不可用；未执行隐式重试")
}

impl Runtime {
    pub(super) fn prepare(
        self: &Arc<Self>,
        db: &Arc<Database>,
        request: PrepareRequest,
    ) -> Result<ValidationPlan, AppError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| input_error("验证准备需要异步运行环境"))?;
        if request.probes.is_empty() || request.probes.len() > 11 {
            return Err(input_error("请选择至少一个且最多 11 个不同的探针"));
        }
        let unique: HashSet<_> = request.probes.iter().collect();
        if unique.len() != request.probes.len() {
            return Err(input_error("探针不能重复；重复对比请设置 repeatCount"));
        }
        let repeats = request.repeat_count.unwrap_or(3);
        if !(2..=5).contains(&repeats) {
            return Err(input_error("受控重复次数须为 2 至 5"));
        }
        let target = PinnedTarget::resolve(db, &request.target, request.mode)?;
        if request.comparison_target.is_some()
            && !request
                .probes
                .iter()
                .any(|p| matches!(p, Probe::Comparison | Probe::CrossSignature))
        {
            return Err(input_error("只有跨供应商签名或受控对比才使用第二个目标"));
        }
        let comparison = request
            .comparison_target
            .as_ref()
            .map(|t| PinnedTarget::resolve(db, t, request.mode))
            .transpose()?;
        if let Some(other) = &comparison {
            for (summary, key) in [
                (&target.summary, other.key.as_str()),
                (&other.summary, target.key.as_str()),
            ] {
                if [&summary.endpoint, &summary.model, &summary.provider_id]
                    .into_iter()
                    .any(|s| s.contains(key))
                {
                    return Err(input_error(
                        "目标标识中包含另一目标的凭据；请修正配置后重新准备",
                    ));
                }
            }
        }
        if request.probes.contains(&Probe::CrossSignature) {
            let other = comparison
                .as_ref()
                .ok_or_else(|| input_error("跨供应商签名检测必须明确选择第二个目标及其费用"))?;
            if other.summary.model != target.summary.model {
                return Err(input_error(
                    "跨供应商签名对照须使用相同的模型名称，避免混入型号差异",
                ));
            }
            if other.app == target.app && other.provider.id == target.provider.id {
                return Err(input_error("跨供应商签名对照须选择不同的供应商成员"));
            }
        }
        let (max_requests, max_output_tokens) = request
            .probes
            .iter()
            .map(|p| probes::budget(*p, repeats, comparison.is_some()))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let now = Utc::now();
        let mut warnings = vec![
            "手动检测会产生真实 API 请求费用；价格或实际输入 Token 数未知，费用估算不可用。maxOutputTokens 是全部请求所声明的输出 Token 上限之和，不保证上游遵守或最终账单；取消前发出的请求仍可能计费。".into(),
            "使用合成测试输入；不读取真实会话、项目代码或系统提示词。每次固定端点、Provider 与凭据，不重试或自动换 Key。".into(),
            "响应模型名、渠道文字和签名行为只提供本次能力线索，不能认证模型身份或保证后续请求的路由。".into(),
        ];
        if target.summary.endpoint.starts_with("http:")
            || comparison
                .as_ref()
                .is_some_and(|t| t.summary.endpoint.starts_with("http:"))
        {
            warnings.push("目标使用明文 HTTP，凭据和合成输入可能暴露给网络中间方；仅应在受信任的本地测试环境使用。".into());
        }
        if request.probes.contains(&Probe::Cache) {
            warnings.push("缓存测试会连续发送两次约 6.5 万字符的合成前缀；输入也计费，阈值与建缓存时延会影响结果。".into());
        }
        if request
            .probes
            .iter()
            .any(|p| matches!(p, Probe::Signature | Probe::CrossSignature))
        {
            warnings.push("签名对照启用 Thinking，单请求最多 2048 输出 Token；跨供应商对照会将合成请求生成的签名与内容发送给第二个所选目标。".into());
        }
        if request.probes.contains(&Probe::Comparison) {
            warnings.push(format!("实验性对比：3 个固定任务 × {repeats} 次重复 × {} 个目标，temperature=0；不支持该参数时不降级重试。", if comparison.is_some() {2} else {1}));
        }
        if request.mode == ValidationMode::Ccs {
            warnings.push("经 ccs 模式使用原生请求协议与冻结的供应商配置，通过诊断上下文隔离生产路由、Key 池和健康状态；仅展示转换后能观察到的证据。".into());
            warnings.push("诊断链路在转换前后均限制响应为 1 MiB，并请求 identity 编码；上游仍返回压缩响应时安全停止，不使用通用代理的较大解压预算。".into());
        }
        let mut plan = ValidationPlan {
            id: uuid::Uuid::new_v4().to_string(),
            target: target.summary.clone(),
            mode: request.mode,
            probes: request.probes,
            max_requests,
            max_output_tokens,
            max_duration_seconds: (max_requests * REQUEST_TIMEOUT).min(1800),
            estimated_cost_usd: None,
            warnings,
            expires_at: (now + chrono::Duration::seconds(PLAN_TTL_SECONDS as i64)).to_rfc3339(),
            comparison_target: comparison.as_ref().map(|t| t.summary.clone()),
        };
        if let Some(other) = &comparison {
            // Neither target may accidentally expose the other target's key in a label.
            plan.target.provider_name = redact(&plan.target.provider_name, &[&other.key], 160);
            if let Some(summary) = &mut plan.comparison_target {
                summary.provider_name = redact(&summary.provider_name, &[&target.key], 160);
            }
        }
        let mut registry = self.registry.lock().map_err(|_| lock_error())?;
        registry
            .plans
            .retain(|_, p| p.expires > Instant::now() && p.owner.strong_count() > 0);
        if registry.plans.len() >= MAX_PLANS {
            return Err(input_error("准备中的检测过多，请等待未使用的预览到期"));
        }
        registry.plans.insert(
            plan.id.clone(),
            Prepared {
                plan: plan.clone(),
                owner: Arc::downgrade(db),
                target,
                comparison,
                repeats,
                expires: Instant::now() + Duration::from_secs(PLAN_TTL_SECONDS),
            },
        );
        // Drop unused credentials promptly on expiry, even when there is no next IPC call.
        let weak = Arc::downgrade(self);
        let id = plan.id.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_secs(PLAN_TTL_SECONDS)).await;
            if let Some(runtime) = weak.upgrade() {
                if let Ok(mut r) = runtime.registry.lock() {
                    r.plans.remove(&id);
                }
            }
        });
        Ok(plan)
    }

    pub(super) fn start(
        self: &Arc<Self>,
        db: Arc<Database>,
        state: Option<Arc<AppState>>,
        plan_id: &str,
    ) -> Result<ValidationRun, AppError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| input_error("验证启动需要异步运行环境"))?;
        if plan_id.len() > 128 {
            return Err(input_error("检测预览标识无效"));
        }
        let mut registry = self.registry.lock().map_err(|_| lock_error())?;
        // Retrying a lost IPC response must not spend money a second time.
        if let Some(run) = db.get_model_validation_by_plan(plan_id)? {
            drop(registry);
            // Prefer the live snapshot and recover a persisted running row
            // after restart, without replaying any request.
            return self.get(&db, &run.id);
        }
        registry
            .plans
            .retain(|_, p| p.expires > Instant::now() && p.owner.strong_count() > 0);
        let prepared = registry
            .plans
            .get(plan_id)
            .filter(|p| same_owner(&p.owner, &db))
            .ok_or_else(|| input_error("检测预览不存在、已过期或属于其他数据库；请重新准备"))?;
        prepared.target.verify_unchanged(&db)?;
        if let Some(other) = &prepared.comparison {
            other.verify_unchanged(&db)?;
        }
        if prepared.plan.mode == ValidationMode::Ccs && state.is_none() {
            return Err(input_error("ccs 链路缺少应用诊断上下文"));
        }
        let running = registry
            .runs
            .values()
            .filter(|r| {
                r.run
                    .lock()
                    .map(|r| r.status == RunStatus::Running)
                    .unwrap_or(true)
            })
            .count();
        if running >= MAX_RUNNING {
            return Err(input_error("最多同时运行两项检测；请等待或取消当前检测"));
        }
        let initial = ValidationRun {
            id: uuid::Uuid::new_v4().to_string(),
            plan: prepared.plan.clone(),
            status: RunStatus::Running,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            results: prepared
                .plan
                .probes
                .iter()
                .map(|probe| ProbeResult {
                    probe: *probe,
                    status: ProbeStatus::NotTested,
                    summary: "尚未执行".into(),
                    evidence: Vec::new(),
                    request_count: 0,
                    duration_ms: 0,
                })
                .collect(),
        };
        db.save_model_validation_run(&initial)?; // Fail before sending if history is unavailable.
        let prepared = registry.plans.remove(plan_id).ok_or_else(lock_error)?;
        let cancellation = Cancellation::default();
        let run = Arc::new(Mutex::new(initial.clone()));
        registry.runs.insert(
            initial.id.clone(),
            ActiveRun {
                owner: Arc::downgrade(&db),
                run: run.clone(),
                cancellation: cancellation.clone(),
                inserted: Instant::now(),
            },
        );
        // Evict only completed in-memory snapshots; durable history is retained.
        if registry.runs.len() > MAX_CACHED_RUNS {
            let oldest = registry
                .runs
                .iter()
                .filter(|(_, r)| r.run.lock().is_ok_and(|r| r.status != RunStatus::Running))
                .min_by_key(|(_, r)| r.inserted)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                registry.runs.remove(&id);
            }
        }
        drop(registry);
        handle.spawn(async move {
            let outcome = AssertUnwindSafe(execute_run(&db, &run, &prepared, state, cancellation))
                .catch_unwind()
                .await;
            if !matches!(outcome, Ok(Ok(()))) {
                if let Ok(mut value) = run.lock() {
                    value.status = RunStatus::Failed;
                    value.finished_at = Some(Utc::now().to_rfc3339());
                    for result in &mut value.results {
                        if result.status == ProbeStatus::NotTested {
                            result.summary = "诊断运行或历史保存失败，后续请求已停止".into();
                        }
                    }
                    // No raw panic, network error, provider or credentials in the log.
                    if db.save_model_validation_run(&value).is_err() {
                        log::warn!("Model validation stopped; device-local history unavailable");
                    }
                }
            }
            // prepared (including both secrets and frozen Provider configs) drops here.
        });
        Ok(initial)
    }

    pub(super) fn get(&self, db: &Arc<Database>, id: &str) -> Result<ValidationRun, AppError> {
        if id.len() > 128 {
            return Err(input_error("检测记录标识无效"));
        }
        let registry = self.registry.lock().map_err(|_| lock_error())?;
        if let Some(active) = registry.runs.get(id).filter(|a| same_owner(&a.owner, db)) {
            return active
                .run
                .lock()
                .map(|r| r.clone())
                .map_err(|_| lock_error());
        }
        let mut run = db
            .get_model_validation_run(id)?
            .ok_or_else(|| input_error("检测记录不存在"))?;
        if run.status == RunStatus::Running {
            run.status = RunStatus::Interrupted;
            run.finished_at = Some(Utc::now().to_rfc3339());
            for result in &mut run.results {
                if result.status == ProbeStatus::NotTested {
                    result.summary = "应用在上次检测时退出；不会自动重试收费请求".into();
                }
            }
            db.save_model_validation_run(&run)?;
        }
        Ok(run)
    }

    pub(super) fn list(
        &self,
        db: &Arc<Database>,
        app_id: Option<&str>,
        provider_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ValidationRun>, AppError> {
        db.list_model_validation_runs(app_id, provider_id, limit)?
            .into_iter()
            .map(|run| self.get(db, &run.id))
            .collect()
    }

    pub(super) fn cancel(&self, db: &Arc<Database>, id: &str) -> Result<bool, AppError> {
        let registry = self.registry.lock().map_err(|_| lock_error())?;
        let Some(active) = registry.runs.get(id).filter(|a| same_owner(&a.owner, db)) else {
            return Ok(false);
        };
        if active.run.lock().map_err(|_| lock_error())?.status != RunStatus::Running {
            return Ok(false);
        }
        active.cancellation.cancel();
        Ok(true)
    }
}

async fn execute_run(
    db: &Database,
    run: &Mutex<ValidationRun>,
    prepared: &Prepared,
    state: Option<Arc<AppState>>,
    cancellation: Cancellation,
) -> Result<(), AppError> {
    let plan = &prepared.plan;
    let mut executor = Executor::new(
        plan.mode,
        state,
        cancellation.clone(),
        plan.max_requests,
        plan.max_duration_seconds,
    )
    .map_err(|e| input_error(e.message()))?;
    for (index, probe) in plan.probes.iter().enumerate() {
        if cancellation.is_cancelled() || executor.expired() {
            break;
        }
        let result = probes::run(
            *probe,
            &mut executor,
            &prepared.target,
            prepared.comparison.as_ref(),
            prepared.repeats,
            &plan.id,
        )
        .await;
        let mut value = run.lock().map_err(|_| lock_error())?;
        value.results[index] = result;
        db.save_model_validation_run(&value)?;
    }
    let mut value = run.lock().map_err(|_| lock_error())?;
    value.status = if cancellation.is_cancelled() {
        RunStatus::Cancelled
    } else if executor.expired() {
        RunStatus::Failed
    } else {
        RunStatus::Completed
    };
    value.finished_at = Some(Utc::now().to_rfc3339());
    for result in &mut value.results {
        if result.status == ProbeStatus::NotTested {
            result.summary = if cancellation.is_cancelled() {
                "检测已取消；该项未完成，取消前已发出的请求仍可能计费"
            } else {
                "达到运行时限；该项未执行或未完成"
            }
            .into();
        }
    }
    db.save_model_validation_run(&value)
}
