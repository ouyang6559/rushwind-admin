//! ApalisServer — the port of the reference `internal/server/asynq_server.go`
//! as a proper lifecycle transport: an apalis worker over the
//! `rushwind-apalis-postgres` storage (Postgres queue table instead of
//! Redis lists — same push/claim/ack shape, framework-native). The
//! scheduler (`crate::scheduler`) is the cron **producer** that enqueues
//! due jobs; this server is the **consumer** that claims and executes
//! them, registered into the same App lifecycle as REST + SSE.

use std::sync::Arc;

use apalis_core::backend::Backend;
use apalis_core::error::{BoxDynError, Error as ApalisError};
use apalis_core::layers::Ack;
use apalis_core::storage::Storage;
use apalis_core::request::Request;
use apalis_core::response::Response;
use apalis_core::worker::{Context as WorkerContext, Worker, WorkerId};
use rushwind_apalis_postgres::{PgContext, PostgresStorage};
use rushwind_transport::{Server, ServerError, ServerFuture, StopSignal};
use futures_util::StreamExt as _;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

use crate::state::AppState;

/// The job envelope carried in the queue (JsonCodec<String> args).
#[derive(serde::Deserialize)]
struct Job {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default)]
    payload: serde_json::Value,
}

pub struct ApalisServer {
    state: Arc<AppState>,
    storage: PostgresStorage<String>,
    queue: String,
}

impl ApalisServer {
    /// Builds the worker-side storage over the `queue` (default queue).
    /// The queue table itself is created idempotently in `start`.
    pub fn new(state: Arc<AppState>, dsn: &str, queue: &str) -> Result<Self, String> {
        let storage = PostgresStorage::<String>::from_settings(json!({
            "url": dsn,
            "queue": queue,
        }))
        .map_err(|e| format!("apalis storage: {e}"))?;
        Ok(Self { state, storage, queue: queue.to_string() })
    }

    /// Enqueues a job (the scheduler's producer side).
    pub async fn enqueue(
        &self,
        type_name: &str,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let job = json!({ "type": type_name, "payload": payload }).to_string();
        let mut storage = self.storage.clone();
        storage
            .push(job)
            .await
            .map_err(|e| format!("apalis push: {e}"))?;
        Ok(())
    }

    /// The task handlers (asynq_server.go registrations).
    async fn run_handler(
        db: &sea_orm::DatabaseConnection,
        type_name: &str,
        payload: &serde_json::Value,
    ) {
        match type_name {
            "tenant_expiry_scan" => {
                use sea_orm::{ActiveModelTrait as _, ColumnTrait as _, EntityTrait as _, QueryFilter as _, Set};
                let expired = crate::data::sys_tenants::Entity::find()
                    .filter(
                        sea_orm::sea_query::Condition::all()
                            .add(crate::data::sys_tenants::Column::Status.eq("ON"))
                            .add(crate::data::sys_tenants::Column::ExpiredAt.is_not_null())
                            .add(crate::data::sys_tenants::Column::ExpiredAt.lte(crate::data::now())),
                    )
                    .all(db)
                    .await
                    .unwrap_or_default();
                let count = expired.len();
                for row in expired {
                    let mut t: crate::data::sys_tenants::ActiveModel = row.into();
                    t.status = Set(Some("EXPIRED".into()));
                    let _ = t.update(db).await;
                }
                eprintln!("[scheduler] tenant_expiry_scan: {count} expired");
            }
            "audit_log_archive" => {
                eprintln!("[scheduler] audit_log_archive: retention export skipped (no archive dir)");
            }
            "backup" | "broadcast_message" | "script_task" => {
                eprintln!("[scheduler] {type_name} fired with payload {payload}");
            }
            other => eprintln!("[scheduler] unknown task type {other}"),
        }
    }
}

#[async_trait::async_trait]
impl Server for ApalisServer {
    fn endpoint(&self) -> Result<String, ServerError> {
        Ok(format!("apalis-postgres://{}", self.queue))
    }

    fn start(&self, stop: StopSignal) -> ServerFuture<'_> {
        Box::pin(async move {
            PostgresStorage::<()>::setup(self.storage.pool())
                .await
                .map_err(|e| ServerError::Failed(format!("apalis setup: {e}")))?;

            let worker =
                Worker::new(WorkerId::new("admin-task-worker"), WorkerContext::default());
            worker.start();
            let poller = self.storage.clone().poll(&worker);
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
                                    Self::run_handler(&self.state.db, &job.type_name, &job.payload)
                                        .await;
                                    true
                                }
                                Err(e) => {
                                    eprintln!("[scheduler] job decode failed: {e}");
                                    false
                                }
                            };
                            let mut acker = self.storage.clone();
                            let res = if ok {
                                Response::success(
                                    (),
                                    req.parts.task_id.clone(),
                                    req.parts.attempt.clone(),
                                )
                            } else {
                                let err: BoxDynError = "job decode/execute failed".to_string().into();
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
                            eprintln!("[scheduler] poll error: {e}");
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

// ---------------------------------------------------------------------------
// The cron producer — the asynq scheduler's job: every minute, enabled
// PERIODIC rows whose cron spec matches are enqueued (deduped by type
// name across tenants), plus the two system crons from
// task_service.go:357-379 (hourly tenant expiry scan, 03:30 audit
// archive) that register on every RestartAllTask/start.
// ---------------------------------------------------------------------------

/// The cron producer loop: aligned to the minute.
pub async fn run_cron_producer(state: Arc<AppState>, tasks: Arc<ApalisServer>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        use chrono::Timelike as _;
        let now = crate::data::now();
        if now.second() != 0 {
            continue; // aligned to the minute
        }
        // System crons (task_service.go:357-379).
        if now.minute() == 0 {
            let _ = tasks.enqueue("tenant_expiry_scan", serde_json::json!({})).await;
        }
        if now.hour() == 3 && now.minute() == 30 {
            let _ = tasks.enqueue("audit_log_archive", serde_json::json!({})).await;
        }
        // Registered periodic tasks (deduped by type name across tenants —
        // the asynq route IS the type name).
        let rows = crate::data::sys_tasks::Entity::find()
            .filter(
                sea_orm::sea_query::Condition::all()
                    .add(crate::data::sys_tasks::Column::Enable.eq(true))
                    .add(crate::data::sys_tasks::Column::TypeColumn.eq("PERIODIC")),
            )
            .all(&state.db)
            .await
            .unwrap_or_default();
        let mut fired_types: Vec<String> = Vec::new();
        for row in rows {
            let Some(spec) = row.cron_spec.as_deref() else {
                continue;
            };
            let Ok(cron) = CronSpec::parse(spec) else {
                eprintln!("[scheduler] invalid cron spec '{spec}' for {}", row.type_name);
                continue;
            };
            if !cron.matches(now) {
                continue;
            }
            if fired_types.contains(&row.type_name) {
                continue;
            }
            fired_types.push(row.type_name.clone());
            let payload = row
                .task_payload
                .as_ref()
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let _ = tasks.enqueue(&row.type_name, payload).await;
        }
    }
}

/// A parsed 5-field cron spec (minute hour dom month dow).
pub struct CronSpec {
    fields: [Vec<u32>; 5],
}

#[derive(Debug)]
pub struct CronParseError;

impl CronSpec {
    /// `m h dom mon dow`, `*`, lists `a,b`, ranges `a-b`, steps `*/n`.
    pub fn parse(spec: &str) -> Result<Self, CronParseError> {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(CronParseError);
        }
        let bounds: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6)];
        let mut fields: [Vec<u32>; 5] = Default::default();
        for (i, part) in parts.iter().enumerate() {
            let (lo, hi) = bounds[i];
            let mut allowed = Vec::new();
            for atom in part.split(',') {
                let (base, step) = match atom.split_once('/') {
                    Some((b, s)) => (b, s.parse::<u32>().map_err(|_| CronParseError)?),
                    None => (atom, 1),
                };
                let (start, end) = if base == "*" {
                    (lo, hi)
                } else if let Some((a, b)) = base.split_once('-') {
                    (
                        a.parse::<u32>().map_err(|_| CronParseError)?,
                        b.parse::<u32>().map_err(|_| CronParseError)?,
                    )
                } else {
                    let v: u32 = base.parse().map_err(|_| CronParseError)?;
                    (v, v)
                };
                let mut v = start;
                while v <= end.min(hi) {
                    allowed.push(v);
                    v += step.max(1);
                }
            }
            fields[i] = allowed;
        }
        Ok(Self { fields })
    }

    /// Whether the spec fires at the given local time.
    pub fn matches(&self, t: chrono::NaiveDateTime) -> bool {
        use chrono::{Datelike, Timelike};
        self.fields[0].contains(&(t.minute()))
            && self.fields[1].contains(&(t.hour()))
            && self.fields[2].contains(&(t.day()))
            && self.fields[4].iter().any(|d| {
                *d == match t.weekday() {
                    chrono::Weekday::Sun => 0,
                    d => d.num_days_from_monday() + 1,
                }
            })
    }
}
