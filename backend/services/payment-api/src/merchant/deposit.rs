//! 投诉保证金 freeze ledger reads — `pay_complaints_deposit`, `spec/05` §10.
//!
//! Two read surfaces share this ledger module:
//! * the console `main` block needs only the merchant's still-frozen balance
//!   ([`frozen_sum`], the legacy `sum('freeze_money')` over `status = 0` rows);
//! * the full 证金明细 page (`complaintsDeposit`) needs a filtered count +
//!   paged list ([`count_filtered`] / [`list_filtered`]) plus the three-way
//!   amount summary ([`stats`], the legacy `all` / `freezed` / `unfreezed`).
//!
//! Money is in units (1/10000 元), matching the `decimal(15,4)` → i64 mapping
//! used across the rewrite; an empty ledger sums to `0` (SQL `COALESCE`).
//!
//! The [`DepositFilter`] reproduces the legacy `$where`: an optional
//! `out_trade_id` exact match (GET `orderid`), an optional `status` match
//! (request `status`, empty = all), and an optional inclusive `create_at`
//! range. The [`stats`] summary honours only the `user_id` + create-range part
//! (the legacy `$map`), NOT the list-level `orderid` / `status` filters — the
//! deliberate legacy asymmetry, preserved: the amounts describe the whole
//! (date-scoped) ledger, while the table rows reflect the tighter filters.

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Select, Statement, Value,
};

use crate::data::complaints_deposits;
use crate::state::{GatewayError, GatewayResult};

/// The merchant's still-frozen deposit total (`status = 0`), money units.
/// One aggregate read; a merchant with no freezes sums to `0`.
pub async fn frozen_sum(db: &DatabaseConnection, uid: i64) -> GatewayResult<i64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT cast(coalesce(sum(freeze_money), 0) as bigint) \
             FROM complaints_deposits WHERE user_id = $1 AND status = 0",
            vec![Value::from(uid)],
        ))
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    // The SUM aggregate always returns exactly one row.
    let row = row.expect("one aggregate row");
    Ok(row.try_get_by_index::<i64>(0).unwrap_or_default())
}

/// The optional `complaintsDeposit` page filters, layered over the always-on
/// `user_id` scoping. `None` / empty legs are simply not applied (the legacy
/// `if ($orderid)` / `if ($status != '')` guards).
#[derive(Debug, Default, Clone)]
pub struct DepositFilter {
    /// `out_trade_id` exact match (legacy GET `orderid`).
    pub out_trade_id: Option<String>,
    /// `status` exact match: `0` 待解冻 / `1` 已解冻; `None` = all.
    pub status: Option<i32>,
    /// Inclusive `create_at` range, unix seconds (legacy `createtime` split).
    pub create_start: Option<i64>,
    pub create_end: Option<i64>,
}

/// The three-way deposit amount summary (money units), mirroring the legacy
/// `$stats`: `all` = every row's freeze, `freezed` = released rows (`status
/// = 1`), `unfreezed` = still-frozen rows (`status = 0`). Only `user_id` and
/// the create range scope it (the `$map`, not the list `$where`).
#[derive(Debug, Default, Clone, Copy)]
pub struct DepositStats {
    pub all: i64,
    pub freezed: i64,
    pub unfreezed: i64,
}

/// Binds the always-on `user_id` predicate.
fn scoped(uid: i64) -> Select<complaints_deposits::Entity> {
    complaints_deposits::Entity::find().filter(complaints_deposits::Column::UserId.eq(uid))
}

/// Applies the inclusive create range when both bounds are present.
fn with_range(
    q: Select<complaints_deposits::Entity>,
    start: Option<i64>,
    end: Option<i64>,
) -> Select<complaints_deposits::Entity> {
    match (start, end) {
        (Some(s), Some(e)) => q.filter(complaints_deposits::Column::CreateAt.between(s, e)),
        _ => q,
    }
}

/// The list `$where`: `user_id` + create range + optional `out_trade_id` +
/// optional `status`.
fn with_list_filters(
    q: Select<complaints_deposits::Entity>,
    f: &DepositFilter,
) -> Select<complaints_deposits::Entity> {
    let mut q = with_range(q, f.create_start, f.create_end);
    if let Some(orderid) = f.out_trade_id.as_deref().filter(|s| !s.is_empty()) {
        q = q.filter(complaints_deposits::Column::OutTradeId.eq(orderid));
    }
    if let Some(status) = f.status {
        q = q.filter(complaints_deposits::Column::Status.eq(status));
    }
    q
}

/// Counts the merchant's deposit rows matching [`DepositFilter`].
pub async fn count_filtered(
    db: &DatabaseConnection,
    uid: i64,
    f: &DepositFilter,
) -> GatewayResult<u64> {
    with_list_filters(scoped(uid), f)
        .count(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// One page of the merchant's filtered deposit rows, newest id first. `page`
/// is 1-based (clamped up), `rows` the page size.
pub async fn list_filtered(
    db: &DatabaseConnection,
    uid: i64,
    f: &DepositFilter,
    page: u64,
    rows: u64,
) -> GatewayResult<Vec<complaints_deposits::Model>> {
    let page = page.max(1);
    let offset = (page - 1) * rows;
    with_list_filters(scoped(uid), f)
        .order_by_desc(complaints_deposits::Column::Id)
        .offset(offset)
        .limit(rows)
        .all(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// The `all` / `freezed` / `unfreezed` summary over the merchant's date-scoped
/// ledger — one aggregate read with three `FILTER (WHERE …)` sums.
pub async fn stats(
    db: &DatabaseConnection,
    uid: i64,
    create_start: Option<i64>,
    create_end: Option<i64>,
) -> GatewayResult<DepositStats> {
    let mut sql = String::from(
        "SELECT cast(coalesce(sum(freeze_money), 0) as bigint), \
                cast(coalesce(sum(freeze_money) filter (where status = 1), 0) as bigint), \
                cast(coalesce(sum(freeze_money) filter (where status = 0), 0) as bigint) \
         FROM complaints_deposits WHERE user_id = $1",
    );
    let mut params = vec![Value::from(uid)];
    if let (Some(s), Some(e)) = (create_start, create_end) {
        let n = params.len() + 1;
        sql.push_str(&format!(" AND create_at between ${} and ${}", n, n + 1));
        params.push(Value::from(s));
        params.push(Value::from(e));
    }
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            params,
        ))
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    // The SUM aggregate always returns exactly one row.
    let row = row.expect("one aggregate row");
    Ok(DepositStats {
        all: row.try_get_by_index::<i64>(0).unwrap_or_default(),
        freezed: row.try_get_by_index::<i64>(1).unwrap_or_default(),
        unfreezed: row.try_get_by_index::<i64>(2).unwrap_or_default(),
    })
}
