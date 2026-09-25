//! The merchant×day reconciliation statement — `AC::getDayReconciliation`
//! re-stated (`spec/02` §8, AC:1146-1190). There is no cron: the legacy
//! aggregated LAZILY on read (the merchant statement page calls in per
//! visible day) and cached the result in `pay_reconciliation`. Two rules
//! define the domain:
//! * the 30-day freshness horizon — a snapshot whose day is older than 30
//!   days is frozen and served verbatim, younger rows are recomputed and
//!   written back (AC:1155);
//! * the seven metrics mix two windows ON PURPOSE, straight from the PHP:
//!   the three COUNTs and the total/success0 amounts ride the order
//!   CREATE time (`pay_applydate`), while the success amount and the
//!   poundage sum ride the SUCCESS time (`pay_successdate`) — a yesterday
//!   order settled today moves the money lines but not the count lines.
//!   The doc comment on [`DayMetrics`] pins each line's window.
//!
//! Hardening over the legacy (语义差异清单): find-then-`add` raced two
//! concurrent readers into twin rows for one merchant×day; here the
//! unique index `uq_reconciliations_user_date` backs one
//! `INSERT .. ON CONFLICT DO UPDATE`, so the snapshot converges instead
//! of forking. `ctime` keeps the legacy create-only semantics.
//!
//! Not wired to an HTTP surface yet: the legacy entry is the merchant
//! back-office page (login session, pagination shell) — the protobuf
//! merchant API hosts it in the backoffice round.

use chrono::{NaiveDate, TimeZone};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Statement, Value,
};

use crate::data::reconciliations;
use crate::state::GatewayResult;

/// Legacy `diffBetweenTwoDays(date('Y-m-d'), $date) <= 30` — within the
/// horizon a statement is recomputed on every read, beyond it frozen.
pub const STALE_AFTER_DAYS: i64 = 30;

/// True when `date` lies beyond the freshness horizon as seen from
/// `today` — the frozen-snapshot branch of AC:1155.
pub fn stale_snapshot(today: NaiveDate, date: NaiveDate) -> bool {
    (today - date).num_days() > STALE_AFTER_DAYS
}

/// The legacy `begin ~ end` window for one day: local midnight through
/// 23:59:59 as unix seconds (the int timestamps `apply_date` /
/// `success_date` carry).
pub fn day_window(date: NaiveDate) -> (i64, i64) {
    let start = date
        .and_hms_opt(0, 0, 0)
        .and_then(|n| chrono::Local.from_local_datetime(&n).single())
        .map(|d| d.timestamp())
        .unwrap_or_default();
    (start, start + 86_400 - 1)
}

/// The seven aggregates of §8, money in units. Window per line:
/// * `total_count` / `fail_count` / `total_amount` / `success0_amount` —
///   CREATE window, all statuses / status 0;
/// * `success_count` — CREATE window, status 1/2 (the legacy mixed it
///   with the money windows below and kept it on the create day —
///   reproduced on purpose, AC:1159 vs AC:1165);
/// * `success_amount` / `poundage_amount` — SUCCESS window, status 1/2.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DayMetrics {
    pub total_count: i64,
    pub success_count: i64,
    pub fail_count: i64,
    pub total_amount: i64,
    pub success_amount: i64,
    pub success0_amount: i64,
    pub poundage_amount: i64,
}

/// One lazy statement run for merchant `user_id` on `date`, exactly what
/// the statement page did per visible day: frozen snapshot beyond the
/// horizon, otherwise recompute + UPSERT and return the stored row.
/// `today` is injected so the 30-day rule is testable against a fixed
/// clock.
pub async fn day(
    db: &DatabaseConnection,
    user_id: i64,
    date: NaiveDate,
    today: NaiveDate,
) -> GatewayResult<reconciliations::Model> {
    let existing = reconciliations::Entity::find()
        .filter(reconciliations::Column::UserId.eq(user_id))
        .filter(reconciliations::Column::Date.eq(date))
        .one(db)
        .await?;
    if let Some(row) = existing {
        if stale_snapshot(today, row.date) {
            return Ok(row); // AC:1155 — serve the frozen snapshot verbatim
        }
    }
    let metrics = aggregate(db, user_id, date).await?;
    upsert(db, user_id, date, &metrics).await?;
    let row = reconciliations::Entity::find()
        .filter(reconciliations::Column::UserId.eq(user_id))
        .filter(reconciliations::Column::Date.eq(date))
        .one(db)
        .await?;
    row.ok_or_else(|| crate::state::GatewayError::Internal("statement row vanished".into()))
}

/// The seven lines in one scan, PG `FILTER` clauses standing in for the
/// legacy's seven round-trips (identical windows, see [`DayMetrics`]).
async fn aggregate(
    db: &DatabaseConnection,
    user_id: i64,
    date: NaiveDate,
) -> GatewayResult<DayMetrics> {
    let (from, to) = day_window(date);
    // count(*) binds as bigint, sum(bigint) as numeric — the money lines
    // cast to bigint (the units are exact integers). PG `FILTER` clauses
    // stand in for the legacy's seven round-trips over the same table.
    let sql = "SELECT \
                 count(*) FILTER (WHERE apply_date BETWEEN $2 AND $3), \
                 count(*) FILTER (WHERE apply_date BETWEEN $2 AND $3 AND status IN (1,2)), \
                 count(*) FILTER (WHERE apply_date BETWEEN $2 AND $3 AND status = 0), \
                 cast(coalesce(sum(actual_amount) FILTER (WHERE apply_date BETWEEN $2 AND $3), 0) as bigint), \
                 cast(coalesce(sum(actual_amount) FILTER (WHERE success_date BETWEEN $2 AND $3 AND status IN (1,2)), 0) as bigint), \
                 cast(coalesce(sum(actual_amount) FILTER (WHERE apply_date BETWEEN $2 AND $3 AND status = 0), 0) as bigint), \
                 cast(coalesce(sum(poundage) FILTER (WHERE success_date BETWEEN $2 AND $3 AND status IN (1,2)), 0) as bigint) \
               FROM orders WHERE user_id = $1";
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            db.get_database_backend(),
            sql,
            vec![Value::from(user_id), Value::from(from), Value::from(to)],
        ))
        .await?;
    // The aggregate always returns exactly one row.
    let row = row.expect("one aggregate row");
    let n = |i: usize| -> i64 { row.try_get_by_index::<i64>(i).unwrap_or_default() };
    Ok(DayMetrics {
        total_count: n(0),
        success_count: n(1),
        fail_count: n(2),
        total_amount: n(3),
        success_amount: n(4),
        success0_amount: n(5),
        poundage_amount: n(6),
    })
}

/// Write the recomputed line back: create with `ctime = now`, refresh
/// everything but `ctime` on conflict (the legacy kept the original
/// create stamp on `save`).
async fn upsert(
    db: &DatabaseConnection,
    user_id: i64,
    date: NaiveDate,
    m: &DayMetrics,
) -> GatewayResult<()> {
    let sql = "INSERT INTO reconciliations \
                 (user_id, order_total_count, order_success_count, order_fail_count, \
                  order_total_amount, order_success_amount, order_success0_amount, \
                  order_poundage_amount, date, ctime) \
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
               ON CONFLICT (user_id, date) DO UPDATE SET \
                 order_total_count = $2, order_success_count = $3, order_fail_count = $4, \
                 order_total_amount = $5, order_success_amount = $6, \
                 order_success0_amount = $7, order_poundage_amount = $8";
    db.execute_raw(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        vec![
            Value::from(user_id),
            Value::from(m.total_count),
            Value::from(m.success_count),
            Value::from(m.fail_count),
            Value::from(m.total_amount),
            Value::from(m.success_amount),
            Value::from(m.success0_amount),
            Value::from(m.poundage_amount),
            Value::ChronoDate(Some(date)),
            Value::from(crate::data::now_ts()),
        ],
    ))
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn horizon_frozen_exactly_beyond_thirty_days() {
        let today = d(2026, 9, 21);
        assert!(!stale_snapshot(today, today)); // 0 days — fresh
        assert!(!stale_snapshot(today, today - chrono::Duration::days(30))); // <=30 refreshes
        assert!(stale_snapshot(today, today - chrono::Duration::days(31))); // >30 frozen
    }

    #[test]
    fn window_is_the_local_midnight_to_last_second_pair() {
        let (from, to) = day_window(d(2026, 9, 21));
        assert_eq!(to - from, 86_399); // begin 00:00:00 ~ end 23:59:59
                                       // The window's own midnight round-trips through the local zone.
        let naive = chrono::Local
            .timestamp_opt(from, 0)
            .single()
            .unwrap()
            .naive_local()
            .date();
        assert_eq!(naive, d(2026, 9, 21));
    }

    #[test]
    fn metrics_default_to_the_empty_statement() {
        let m = DayMetrics::default();
        assert_eq!(m.total_count + m.total_amount, 0);
    }
}
