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
// The cron producer — the asynq scheduler's job: static system crons are
// registered directly; the DB-driven PERIODIC rows ride a wildcard job
// whose handler scans `sys_tasks` each minute, matches specs, dedupes by
// type name across tenants, and enqueues.
// ---------------------------------------------------------------------------

/// Builds the cron transport wired to this server's enqueue side.
pub fn cron_server(
    state: Arc<AppState>,
    tasks: Arc<ApalisServer>,
) -> rushwind_transport_cron::CronServer {
    use rushwind_transport_cron::{CronJob, CronServer, CronSpec};

    fn spec(spec: &str) -> CronSpec {
        CronSpec::parse(spec).expect("static cron spec parses")
    }

    // System crons (task_service.go:357-379).
    let server = CronServer::new("cron://admin")
        .with_job(CronJob::new(
            "tenant_expiry_scan",
            spec("0 * * * *"),
            {
                let state = Arc::clone(&state);
                move || {
                    let state = Arc::clone(&state);
                    Box::pin(async move {
                        ApalisServer::run_handler(
                            &state.db,
                            "tenant_expiry_scan",
                            &serde_json::json!({}),
                        )
                        .await;
                    })
                }
            },
        ))
        .with_job(CronJob::new(
            "audit_log_archive",
            spec("30 3 * * *"),
            {
                let state = Arc::clone(&state);
                move || {
                    let state = Arc::clone(&state);
                    Box::pin(async move {
                        ApalisServer::run_handler(
                            &state.db,
                            "audit_log_archive",
                            &serde_json::json!({}),
                        )
                        .await;
                    })
                }
            },
        ));

    // The wildcard job: DB-driven PERIODIC rows. The handler re-reads
    // `sys_tasks` each tick, so ControlTask/Update/Delete take effect
    // without a restart.
    server.with_job(CronJob::new("sys_tasks_periodic", spec("* * * * *"), {
        let state = Arc::clone(&state);
        let tasks = Arc::clone(&tasks);
        move || {
            let state = Arc::clone(&state);
            let tasks = Arc::clone(&tasks);
            Box::pin(async move {
                use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};
                use chrono::Timelike as _;
                let now = crate::data::now();
                if now.second() != 0 {
                    return;
                }
                let rows = crate::data::sys_tasks::Entity::find()
                    .filter(
                        sea_orm::sea_query::Condition::all()
                            .add(crate::data::sys_tasks::Column::Enable.eq(true))
                            .add(crate::data::sys_tasks::Column::TypeColumn.eq("PERIODIC")),
                    )
                    .all(&state.db)
                    .await
                    .unwrap_or_default();
                let mut fired: Vec<String> = Vec::new();
                for row in rows {
                    let Some(s) = row.cron_spec.as_deref() else {
                        continue;
                    };
                    let Ok(spec) = CronSpec::parse(s) else {
                        eprintln!("[scheduler] invalid cron spec '{s}' for {}", row.type_name);
                        continue;
                    };
                    if !spec.matches(now) || fired.contains(&row.type_name) {
                        continue;
                    }
                    fired.push(row.type_name.clone());
                    let payload = row
                        .task_payload
                        .as_ref()
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let _ = tasks.enqueue(&row.type_name, payload).await;
                }
            })
        }
    }))
}
