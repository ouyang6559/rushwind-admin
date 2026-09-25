//! Merchant console首页 aggregation — the read-side port of
//! `User/IndexController::main` (L38-104, `spec/05` §10). The `main` block
//! folds today's six headline numbers into one `stat` map; this module owns
//! only that fold ([`today_stats`]). The article (`gglist`) and login-record
//! blocks are separate services the handler composes.
//!
//! Faithful window per line (straight from the PHP, kept deliberately mixed):
//! * `today_order_count` — `pay_applydate` in today, all statuses;
//! * `today_order_paid_count` — `pay_successdate` in today, `status IN (1,2)`;
//! * `today_order_unpaid_count` — `pay_applydate` in today, `status = 0`;
//! * `today_order_actual_sum` — `pay_successdate` in today, `status IN (1,2)`,
//!   `SUM(actual_amount)`;
//! * `complaints_deposit` — the still-frozen deposit balance ([`deposit::frozen_sum`]);
//! * `today_income` — `today_order_actual_sum` + the agent-split (`lx = 9`)
//!   ledger `SUM(money)` booked in today.
//!
//! The four order lines ride one PG `FILTER` scan (the legacy issued four
//! round-trips over the same table); `user_id` is the raw member uid, not the
//! `+10000` wire number the order-add uses.
//!
//! The legacy's month-chart SQL (`$ordertotal` / `$ordernum` grouped by day,
//! L40-61) feeds an ECharts widget on the page, not the `stat` map; that trend
//! chart is out of scope for this aggregation slice.

use chrono::{NaiveDate, NaiveDateTime};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, Value};

use crate::merchant::deposit;
use crate::reconcile::day_window;
use crate::state::{GatewayError, GatewayResult};

/// The `money_changes` flow type booked as the merchant's own income share
/// (legacy `lx = 9`, the agent-split credit).
pub const LX_INCOME_SHARE: i32 = 9;

/// Today's headline numbers for one merchant, money in units.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TodayStats {
    pub today_order_count: i64,
    pub today_order_paid_count: i64,
    pub today_order_unpaid_count: i64,
    pub today_order_actual_sum: i64,
    pub complaints_deposit: i64,
    pub today_income: i64,
}

/// Folds today's `stat` map for `uid`, anchored on `date` (the caller's local
/// "today") so the day window is testable against a fixed clock.
pub async fn today_stats(
    db: &DatabaseConnection,
    uid: i64,
    date: NaiveDate,
) -> GatewayResult<TodayStats> {
    let (from, to) = day_window(date);
    // Four order lines, one scan. count(*) binds bigint, sum(bigint) numeric —
    // the money line casts to bigint (exact integer units).
    let sql = "SELECT \
                 count(*) FILTER (WHERE apply_date BETWEEN $2 AND $3), \
                 count(*) FILTER (WHERE success_date BETWEEN $2 AND $3 AND status IN (1,2)), \
                 count(*) FILTER (WHERE apply_date BETWEEN $2 AND $3 AND status = 0), \
                 cast(coalesce(sum(actual_amount) FILTER (WHERE success_date BETWEEN $2 AND $3 AND status IN (1,2)), 0) as bigint) \
               FROM orders WHERE user_id = $1";
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            vec![Value::from(uid), Value::from(from), Value::from(to)],
        ))
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    // The aggregate always returns exactly one row.
    let row = row.expect("one aggregate row");
    let n = |i: usize| -> i64 { row.try_get_by_index::<i64>(i).unwrap_or_default() };
    let (start, end) = day_bounds(date);

    // The still-frozen投诉保证金 balance.
    let complaints_deposit = deposit::frozen_sum(db, uid).await?;

    // The agent-split (`lx = 9`) income booked in the day, over the naive-local
    // datetime column — the ledger's own clock, not the unix window.
    let yj = {
        let yj_sql = "SELECT cast(coalesce(sum(money), 0) as bigint) \
                      FROM money_changes \
                      WHERE user_id = $1 AND lx = $2 AND datetime BETWEEN $3 AND $4";
        let yj_row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                yj_sql,
                vec![
                    Value::from(uid),
                    Value::from(LX_INCOME_SHARE),
                    Value::from(start),
                    Value::from(end),
                ],
            ))
            .await
            .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
        let yj_row = yj_row.expect("one aggregate row");
        yj_row.try_get_by_index::<i64>(0).unwrap_or_default()
    };

    let today_order_actual_sum = n(3);
    Ok(TodayStats {
        today_order_count: n(0),
        today_order_paid_count: n(1),
        today_order_unpaid_count: n(2),
        today_order_actual_sum,
        complaints_deposit,
        today_income: today_order_actual_sum + yj,
    })
}

/// A day's `[00:00:00, 23:59:59]` as naive-local datetimes, the bounds the
/// `money_changes.datetime` ledger column is filtered on.
fn day_bounds(date: NaiveDate) -> (NaiveDateTime, NaiveDateTime) {
    let start = date.and_hms_opt(0, 0, 0).expect("midnight is valid");
    let end = date.and_hms_opt(23, 59, 59).expect("last second is valid");
    (start, end)
}
