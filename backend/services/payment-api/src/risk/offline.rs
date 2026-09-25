//! The daily offline-reset plan — the Rust shape of
//! `OfflineController::offlinePlanning` (`spec/06` §1 "每日重置", OFF:28-88).
//!
//! The legacy cron did three things behind a flock-guarded "once per day"
//! lock-file marker: restore `offline_status = 1` for risk-controlled
//! channels and sub-accounts, and zero the day accumulators (members too).
//! Only the restore survives the rewrite: the accumulators now live in
//! Redis day/unit buckets that self-expire at midnight
//! ([`super::counters`]), so there is nothing left to zero — and the legacy
//! zeroing was broken anyway. Its channel branch wrote a `pay_money` column
//! that does not exist (ThinkPHP silently filtered it, so `paying_money`
//! was only ever cleared lazily by the `theTotalVolume` cross-day callback,
//! OFF:47), and the member branch named four wrong columns
//! (`pay_money`/`unit_pay_money`/`unit_pay_number`/`unit_frist_pay_time`
//! vs the real `paying_money`/`unit_paying_amount`/`unit_paying_number`/
//! `unit_frist_paying_time`), making the merchant "reset" a full no-op
//! (OFF:69-74). Members never had an offline mechanism (`spec/06` §4.3),
//! so this plan touches exactly the two offline-bearing tables.
//!
//! The once-per-day guard replaces flock + lock-file timestamps with a
//! Redis `SET NX EX` marker keyed by the UTC date — idempotent across
//! instances and ticks, and self-expiring at midnight. Unlike the legacy,
//! which stamped the lock file even after a partial failure (leaving the
//! day half-reset until tomorrow), a DB failure here releases the marker so
//! the next hourly tick retries (recorded divergence, `spec/06` §9.1).

use redis::aio::ConnectionManager;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::{channel_accounts, channels};
use crate::risk::counters::{secs_to_end_of_day, ymd};
use crate::state::{GatewayError, GatewayResult};

/// The per-day marker key: one reset per UTC date, any number of ticks.
pub fn reset_marker(now_ts: i64) -> String {
    format!("risk:reset:offline:{}", ymd(now_ts))
}

/// What one planning run did. `ran = false` means the day's marker was
/// already taken by an earlier tick — nothing was touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResetOutcome {
    /// Whether this run won the day marker and performed the restore.
    pub ran: bool,
    /// `channels` rows matched by the restore (shared-table total).
    pub channels: u64,
    /// `channel_accounts` rows matched by the restore.
    pub accounts: u64,
}

/// The cron body: claim the day marker, then restore `offline_status = 1`
/// for every row with `control_status = 1` on channels and sub-accounts
/// (`spec/06` §4.1/§4.2 — the same set the day-cap trips apply to). A
/// manual `offline_status = 0` set during the day is therefore re-opened
/// at the next midnight tick, exactly like the legacy did.
pub async fn planning(
    db: &DatabaseConnection,
    redis: &ConnectionManager,
    now_ts: i64,
) -> GatewayResult<ResetOutcome> {
    let marker = reset_marker(now_ts);
    let mut conn = redis.clone();
    // `SET NX` replies +OK when the key was created and nil when it existed.
    let claim: redis::Value = redis::cmd("SET")
        .arg(&marker)
        .arg(now_ts)
        .arg("NX")
        .arg("EX")
        .arg(secs_to_end_of_day(now_ts).max(1))
        .query_async(&mut conn)
        .await
        .map_err(|e| GatewayError::Internal(format!("redis: {e}")))?;
    if !matches!(claim, redis::Value::Okay) {
        return Ok(ResetOutcome::default());
    }

    let restored = restore_online(db).await;
    if restored.is_err() {
        // Release the marker so the next tick retries the half-run.
        let mut conn = redis.clone();
        let _: redis::RedisResult<i64> =
            redis::cmd("DEL").arg(&marker).query_async(&mut conn).await;
    }
    restored.map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// The two UPDATEs; row counts are table-wide (the shared-DB tests assert
/// on targeted rows, not on these totals).
async fn restore_online(db: &DatabaseConnection) -> Result<ResetOutcome, sea_orm::DbErr> {
    let channels = channels::Entity::update_many()
        .col_expr(channels::Column::OfflineStatus, Expr::value(1))
        .filter(channels::Column::ControlStatus.eq(1))
        .exec(db)
        .await?;
    let accounts = channel_accounts::Entity::update_many()
        .col_expr(channel_accounts::Column::OfflineStatus, Expr::value(1))
        .filter(channel_accounts::Column::ControlStatus.eq(1))
        .exec(db)
        .await?;
    Ok(ResetOutcome {
        ran: true,
        channels: channels.rows_affected,
        accounts: accounts.rows_affected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn marker_is_keyed_by_the_utc_date() {
        assert_eq!(reset_marker(0), "risk:reset:offline:19700101");
        assert_eq!(reset_marker(DAY - 1), "risk:reset:offline:19700101");
        assert_eq!(reset_marker(DAY), "risk:reset:offline:19700102");
    }

    #[test]
    fn not_run_outcome_is_the_short_circuit_shape() {
        let o = ResetOutcome::default();
        assert!(!o.ran);
        assert_eq!((o.channels, o.accounts), (0, 0));
    }
}
