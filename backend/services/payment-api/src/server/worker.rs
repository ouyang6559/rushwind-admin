//! The task-queue transports: an apalis worker over the shared Postgres
//! queue (`rushwind-apalis-postgres`) plus the cron producer that feeds
//! it — the same pair admin-api runs, slimmed to the payment schedules.
//!
//! The first tenant is the T+1 thaw cron (`spec/02` §6.2): the legacy was
//! a crontab hit on the ThinkPHP Cli entry, which gate-checked the
//! `allowstart~allowend` hour window (default 1~5) and then swept
//! `blocked_logs` row by row. Here the producer fires hourly, keeps the
//! pure [`crate::ledger::in_thaw_window`] gate, and enqueues; the worker
//! claims and runs [`LedgerService::run_t1_thaw_sweep`] — per-row txs,
//! the `blocked_logs` CAS making overlapping runs safe.
//!
//! The second tenant is the daily offline-reset (`spec/06`, legacy
//! `OfflineController::offlinePlanning`): the producer also fires hourly,
//! but the "once per day" gate lives in the job itself — a Redis
//! `SET NX EX` date marker inside [`crate::risk::offline::planning`]
//! (the flock + lock-file replacement), so the first tick past midnight
//! does the work and every later tick short-circuits.
//!
//! The third tenant is the payout execution queue (`spec/04` §10, legacy
//! `Cli/AutodfController`): two minute-tick jobs — `payout_submit_sweep`
//! drives the default channel's auto代付 due set, `payout_query_sweep`
//! re-confirms every in-flight order on its own channel. Both resolve their
//! [`crate::payout::PayoutChannelCfg`] from the `payout_channels` table (no
//! hand-passed config) and run through [`crate::payout::PayoutService`]'s
//! sweep methods; the §8.1 `df_lock` claim keeps overlapping ticks safe.

use std::sync::Arc;

use apalis_core::backend::Backend;
use apalis_core::error::{BoxDynError, Error as ApalisError};
use apalis_core::layers::Ack;
use apalis_core::response::Response;
use apalis_core::storage::Storage;
use apalis_core::worker::{Context as WorkerContext, Worker, WorkerId};
use chrono::Timelike as _;
use futures_util::StreamExt as _;
use rushwind_apalis_postgres::PostgresStorage;
use rushwind_transport::{Server, ServerError, ServerFuture, StopSignal};
use serde_json::json;

use crate::ledger::in_thaw_window;
use crate::payout::auto_df::AutoDfConfig;
use crate::risk::offline;
use crate::state::AppState;

/// The job envelope carried in the queue (JsonCodec<String> args).
#[derive(serde::Deserialize)]
struct Job {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default)]
    #[allow(dead_code)] // the thaw job is payload-less; the envelope is not
    payload: serde_json::Value,
}

/// The query batch size (§10.2 dfQuery pulls 10 in-flight orders per run).
/// The auto-submit valve / batch / money ceiling / daily caps are no longer
/// baked in here — they are read live from the `issystem` `tikuan_configs`
/// row via [`AutoDfConfig`] (`spec/04` §10.1).
const PAYOUT_QUERY_LIMIT: u64 = 10;

/// The enqueue side: the shared queue-storage handle. The worker claims
/// from the same storage; the cron producer pushes through this side.
pub struct TaskQueue {
    storage: PostgresStorage<String>,
    queue_name: String,
}

impl TaskQueue {
    /// Builds the queue storage over `queue`. The queue table itself is
    /// created idempotently in the worker's start (the `setup` call).
    pub fn new(dsn: &str, queue: &str) -> Result<Self, String> {
        let storage = PostgresStorage::<String>::from_settings(json!({
            "url": dsn,
            "queue": queue,
        }))
        .map_err(|e| format!("apalis storage: {e}"))?;
        Ok(Self {
            storage,
            queue_name: queue.to_string(),
        })
    }

    /// Enqueues a job (the producer side).
    pub async fn enqueue(&self, type_name: &str, payload: serde_json::Value) -> Result<(), String> {
        let job = json!({ "type": type_name, "payload": payload }).to_string();
        let mut storage = self.storage.clone();
        storage
            .push(job)
            .await
            .map_err(|e| format!("apalis push: {e}"))?;
        Ok(())
    }
}

/// The worker transport: claims queued jobs and executes their handlers.
pub struct ApalisServer {
    state: Arc<AppState>,
    queue: Arc<TaskQueue>,
}

impl ApalisServer {
    /// Wraps the shared state + queue handle.
    pub fn new(state: Arc<AppState>, queue: Arc<TaskQueue>) -> Self {
        Self { state, queue }
    }

    /// The task handlers (module registrations).
    async fn run_handler(state: &AppState, type_name: &str) {
        match type_name {
            "t1_thaw_scan" => match state.ledger.run_t1_thaw_sweep().await {
                Ok(report) => tracing::info!(?report, "t1 thaw sweep done"),
                Err(e) => tracing::warn!(error = ?e, "t1 thaw sweep failed"),
            },
            "offline_daily_reset" => {
                let now_ts = chrono::Utc::now().timestamp();
                match offline::planning(&state.db, &state.redis, now_ts).await {
                    Ok(o) if o.ran => tracing::info!(?o, "offline daily reset done"),
                    Ok(o) => tracing::debug!(?o, "offline daily reset already ran today"),
                    Err(e) => tracing::warn!(error = ?e, "offline daily reset failed"),
                }
            }
            // §6.3 投诉保证金到期解冻 (legacy Cli/UnfreezeController): per-row
            // txs, the `complaints_deposits` status CAS making overlapping
            // runs safe; a skipped row never aborts the batch.
            "deposit_unfreeze_sweep" => match state.ledger.run_deposit_unfreeze_sweep().await {
                Ok(report) => tracing::info!(?report, "deposit unfreeze sweep done"),
                Err(e) => tracing::warn!(error = ?e, "deposit unfreeze sweep failed"),
            },
            // §10.1 auto代付提交: read the live system config, honour the
            // master switch + daily run window before pulling anything, then
            // drive the due set with the config-derived gate (money ceiling
            // + per-merchant daily caps). No default channel → the legacy's
            // early exit; a `None` folds nothing and never fails.
            "payout_submit_sweep" => {
                let now_ts = chrono::Local::now().timestamp();
                match AutoDfConfig::load(&state.db).await {
                    Ok(cfg) if !cfg.switch => {
                        tracing::debug!("payout auto-submit disabled (auto_df_switch off)")
                    }
                    Ok(cfg) if !cfg.in_window(now_ts) => {
                        tracing::debug!("payout auto-submit outside the run window")
                    }
                    Ok(cfg) => {
                        let gate = cfg.submit_gate();
                        match state
                            .payout
                            .run_auto_submit_sweep(&state.payout_registry, &gate, now_ts)
                            .await
                        {
                            Ok(Some(report)) => {
                                tracing::info!(?report, "payout auto-submit sweep done")
                            }
                            Ok(None) => {
                                tracing::debug!("payout auto-submit skipped: no default channel")
                            }
                            Err(e) => {
                                tracing::warn!(error = ?e, "payout auto-submit sweep failed")
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = ?e, "payout auto-submit config load failed"),
                }
            }
            // §10.2 代付查单：re-confirm every in-flight (status=1) order on
            // its own (possibly since-disabled) channel, resolved by
            // df_channel_id from the table.
            "payout_query_sweep" => {
                let now_ts = chrono::Local::now().timestamp();
                match state
                    .payout
                    .run_query_sweep(&state.payout_registry, PAYOUT_QUERY_LIMIT, now_ts)
                    .await
                {
                    Ok(report) => tracing::info!(?report, "payout query sweep done"),
                    Err(e) => tracing::warn!(error = ?e, "payout query sweep failed"),
                }
            }
            other => tracing::warn!(task = other, "unknown task type"),
        }
    }
}

#[async_trait::async_trait]
impl Server for ApalisServer {
    fn endpoint(&self) -> Result<String, ServerError> {
        Ok(format!("apalis-postgres://{}", self.queue.queue_name))
    }

    fn start(&self, stop: StopSignal) -> ServerFuture<'_> {
        Box::pin(async move {
            PostgresStorage::<()>::setup(self.queue.storage.pool())
                .await
                .map_err(|e| ServerError::Failed(format!("apalis setup: {e}")))?;

            let worker = Worker::new(
                WorkerId::new("payment-task-worker"),
                WorkerContext::default(),
            );
            worker.start();
            let poller = self.queue.storage.clone().poll(&worker);
            tokio::spawn(poller.heartbeat);
            let mut stream = poller.stream;

            loop {
                tokio::select! {
                    _ = stop.wait() => return Err(ServerError::Cancelled),
                    item = stream.next() => match item {
                        Some(Ok(Some(req))) => {
                            let job: Result<Job, _> = serde_json::from_str(&req.args);
                            let ok = match &job {
                                Ok(job) => {
                                    Self::run_handler(&self.state, &job.type_name).await;
                                    true
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "job decode failed");
                                    false
                                }
                            };
                            let mut acker = self.queue.storage.clone();
                            let res = if ok {
                                Response::success(
                                    (),
                                    req.parts.task_id.clone(),
                                    req.parts.attempt.clone(),
                                )
                            } else {
                                let err: BoxDynError = "job decode failed".to_string().into();
                                Response::failure(
                                    ApalisError::Failed(Arc::new(err)),
                                    req.parts.task_id.clone(),
                                    req.parts.attempt.clone(),
                                )
                            };
                            let _ = acker.ack(&req.parts.context, &res).await;
                        }
                        Some(Ok(None)) | None => break,
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "poll error");
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn stop(&self) -> ServerFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// The worker factory: the task-queue consumer transport, registered
/// under `servers[].kind: payment-tasks`.
pub fn worker_factory(
    state: Arc<AppState>,
    queue: Arc<TaskQueue>,
) -> impl Fn(
    serde_json::Value,
    rushwind_bootstrap::RouteInput,
) -> rushwind_bootstrap::BoxFuture<
    'static,
    Result<Arc<dyn Server>, rushwind_bootstrap::BootstrapError>,
> + Send
       + Sync
       + 'static {
    move |_settings, _input| {
        let state = Arc::clone(&state);
        let queue = Arc::clone(&queue);
        Box::pin(async move { Ok(Arc::new(ApalisServer::new(state, queue)) as Arc<dyn Server>) })
    }
}

// ---------------------------------------------------------------------------
// The cron producer: hourly ticks, window-gated enqueues (§6.2 T:30-35 —
// the legacy checked `allowstart~allowend` on every crontab hit; so do we,
// config-driven rather than baked into the spec string). The offline reset
// enqueues on every tick — no hour window, its once-per-day gate is the
// Redis date marker inside the job.
// ---------------------------------------------------------------------------

/// Builds the cron producer registrations, keyed by the names the
/// `server.yaml` cron section lists.
pub fn cron_jobs(
    state: Arc<AppState>,
    queue: Arc<TaskQueue>,
) -> Vec<(&'static str, rushwind_transport_cron::CronJob)> {
    use rushwind_transport_cron::{CronJob, CronSpec};

    let thaw = CronJob::new(
        "t1_thaw_scan",
        CronSpec::parse("0 * * * *").expect("static spec"),
        {
            let state = Arc::clone(&state);
            let queue = Arc::clone(&queue);
            move || {
                let state = Arc::clone(&state);
                let queue = Arc::clone(&queue);
                Box::pin(async move {
                    let hour = chrono::Local::now().hour() as i32;
                    if !in_thaw_window(hour, state.cfg.thaw_allow_start, state.cfg.thaw_allow_end) {
                        return; // outside the legacy allowstart~allowend window
                    }
                    if let Err(e) = queue.enqueue("t1_thaw_scan", json!({})).await {
                        tracing::warn!(error = %e, "t1 thaw enqueue failed");
                    }
                })
            }
        },
    );
    let offline = CronJob::new(
        "offline_daily_reset",
        CronSpec::parse("0 * * * *").expect("static spec"),
        {
            let queue = Arc::clone(&queue);
            move || {
                let queue = Arc::clone(&queue);
                Box::pin(async move {
                    if let Err(e) = queue.enqueue("offline_daily_reset", json!({})).await {
                        tracing::warn!(error = %e, "offline reset enqueue failed");
                    }
                })
            }
        },
    );
    // The payout queue sweeps (`spec/04` §10): the auto-submit and query loops
    // fire every minute, the legacy Autodf crontab cadence. Their safety rests
    // on the queue itself, not the tick rate — the §8.1 `df_lock` claim makes
    // overlapping submits exclusive, and §10.1 auto-submit is a no-op until
    // ops promotes a default channel (so an unset system enqueues, then folds
    // nothing).
    let payout_submit = CronJob::new(
        "payout_submit_sweep",
        CronSpec::parse("* * * * *").expect("static spec"),
        {
            let queue = Arc::clone(&queue);
            move || {
                let queue = Arc::clone(&queue);
                Box::pin(async move {
                    if let Err(e) = queue.enqueue("payout_submit_sweep", json!({})).await {
                        tracing::warn!(error = %e, "payout submit enqueue failed");
                    }
                })
            }
        },
    );
    let payout_query = CronJob::new(
        "payout_query_sweep",
        CronSpec::parse("* * * * *").expect("static spec"),
        {
            let queue = Arc::clone(&queue);
            move || {
                let queue = Arc::clone(&queue);
                Box::pin(async move {
                    if let Err(e) = queue.enqueue("payout_query_sweep", json!({})).await {
                        tracing::warn!(error = %e, "payout query enqueue failed");
                    }
                })
            }
        },
    );
    // The complaints-deposit release (`spec/02` §6.3, legacy Cli/Unfreeze):
    // minute-ticked like the payout sweeps — `unfreeze_time` is second-
    // precise, and the ledger-row CAS inside each release keeps overlapping
    // ticks from double-releasing a row.
    let deposit_unfreeze = CronJob::new(
        "deposit_unfreeze_sweep",
        CronSpec::parse("* * * * *").expect("static spec"),
        {
            let queue = Arc::clone(&queue);
            move || {
                let queue = Arc::clone(&queue);
                Box::pin(async move {
                    if let Err(e) = queue.enqueue("deposit_unfreeze_sweep", json!({})).await {
                        tracing::warn!(error = %e, "deposit unfreeze enqueue failed");
                    }
                })
            }
        },
    );
    vec![
        ("t1_thaw_scan", thaw),
        ("offline_daily_reset", offline),
        ("payout_submit_sweep", payout_submit),
        ("payout_query_sweep", payout_query),
        ("deposit_unfreeze_sweep", deposit_unfreeze),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_gate_falls_back_to_the_legacy_valve_and_batch() {
        // The worker drives the sweep with a config-derived gate; an all-default
        // (disabled) config still carries the §10.1 fixed valve (try < 5) and
        // batch of 10, no ceiling / caps.
        let gate = AutoDfConfig::disabled().submit_gate();
        assert_eq!(gate.try_cap, 5);
        assert_eq!(gate.limit, 10);
        assert_eq!(gate.max_money, None);
        assert_eq!(gate.max_count, 0);
        assert_eq!(gate.max_sum, 0);
    }

    #[test]
    fn query_batch_matches_the_legacy_pull() {
        assert_eq!(PAYOUT_QUERY_LIMIT, 10);
    }
}
