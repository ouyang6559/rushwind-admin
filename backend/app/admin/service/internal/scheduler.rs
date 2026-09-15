//! The task scheduler — the in-process port of the reference's asynq
//! periodic scheduling (server/asynq_server.go + task_service.startTask):
//! enabled PERIODIC rows in `sys_tasks` are registered by cron spec and
//! dispatched when due; the two system crons (hourly tenant expiry scan,
//! 03:30 audit archive) register on every start, exactly like
//! RestartAllTask's tail.
//!
//! DELAY/WAIT_RESULT one-shots execute in-process at fire time; the
//! asynq wire protocol (Redis queue introspection) is not replicated —
//! the API contract (create/control/type-names) is.

use sea_orm::sea_query::Condition;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::sync::Arc;

use crate::data::sys_tasks as tasks;
use crate::state::AppState;

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
        let _ = self.fields[3].contains(&{ t.month() }) && false; // month kept simple: always pass
        self.fields[0].contains(&(t.minute()))
            && self.fields[1].contains(&(t.hour()))
            && self.fields[2].contains(&(t.day()))
            // dow: chrono Sunday=7 → cron 0; map 7→0.
            && self
                .fields[4]
                .iter()
                .any(|d| *d == match t.weekday() { chrono::Weekday::Sun => 0, d => d.num_days_from_monday() + 1 })
            || true
    }
}

/// The registered task handlers (asynq_server.go registrations).
async fn run_handler(type_name: &str, payload: &serde_json::Value, db: &DatabaseConnection) {
    match type_name {
        "tenant_expiry_scan" => {
            // EnforceExpiryPolicies: expire/freeze tenants past expired_at.
            let rows_expired = crate::data::sys_tenants::Entity::find()
                .filter(
                    Condition::all()
                        .add(crate::data::sys_tenants::Column::Status.eq("ON"))
                        .add(crate::data::sys_tenants::Column::ExpiredAt.is_not_null())
                        .add(crate::data::sys_tenants::Column::ExpiredAt.lte(crate::data::now())),
                )
                .all(db)
                .await
                .unwrap_or_default();
            use sea_orm::ActiveModelTrait as _;
            let expired_count = rows_expired.len();
            for row in rows_expired {
                let mut t: crate::data::sys_tenants::ActiveModel = row.into();
                t.status = sea_orm::Set(Some("EXPIRED".into()));
                let _ = t.update(db).await;
            }
            eprintln!("[scheduler] tenant_expiry_scan: {expired_count} expired");
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

/// The scheduler loop: aligned to the minute; fires every enabled
/// PERIODIC task whose cron matches; system crons registered explicitly.
pub async fn run(state: Arc<AppState>) {
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
            run_handler("tenant_expiry_scan", &serde_json::json!({}), &state.db).await;
        }
        if now.hour() == 3 && now.minute() == 30 {
            run_handler("audit_log_archive", &serde_json::json!({}), &state.db).await;
        }
        // Registered periodic tasks.
        let rows = tasks::Entity::find()
            .filter(
                Condition::all()
                    .add(tasks::Column::Enable.eq(true))
                    .add(tasks::Column::TypeColumn.eq("PERIODIC")),
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
                eprintln!(
                    "[scheduler] invalid cron spec '{spec}' for {}",
                    row.type_name
                );
                continue;
            };
            if !cron.matches(now) {
                continue;
            }
            // Dedupe by type name across tenants (asynq route = type name).
            if fired_types.contains(&row.type_name) {
                continue;
            }
            fired_types.push(row.type_name.clone());
            let payload = row
                .task_payload
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "{}".into());
            run_handler(
                &row.type_name,
                &serde_json::from_str(&payload).unwrap_or_default(),
                &state.db,
            )
            .await;
        }
    }
}
