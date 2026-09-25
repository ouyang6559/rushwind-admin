//! Agent downline ORDER detail READ (`spec/05` §6.4 `User/AgentController::
//! order` + `childord`'s per-order list, and `exportorder`'s CSV). §7
//! [`crate::merchant::profit_report`] already folds the downline's earnings
//! into ONE row per child; this module is the complementary per-order view —
//! the agent paging through every settled / pending order placed by its OWN
//! direct downline merchants.
//!
//! Scope (§6.4 `order` L523-560): the legacy built a `pay_memberid IN (…) `
//! whitelist from `Member.parentid = agent` (`id + 10000` each), then narrowed
//! to one child when a `memberid` was supplied. Here `orders.user_id` IS the
//! merchant uid (the `+10000` wire form is only `mch_id`), so the scope is the
//! agent's direct, non-platform children (`members.parentid = agent AND
//! groupid <> 1`) and a supplied `memberid` resolves through
//! [`user_id_of_mch`] back into that set — a `memberid` outside it collapses
//! to the empty scope (the legacy's `pay_memberid = 1` "return nothing"
//! sentinel).
//!
//! Two statistics shapes ride the same scope, exactly as the legacy branches on
//! whether a time window was posted: with NO window it reports the 今日 / 累计
//! success roll-up (`status IN (1,2)`, success-day anchored, ignoring the
//! other filters — [`OrderStats::Summary`]); WITH a window it instead folds the
//! filtered `SUM(amount) / SUM(actual_amount) / count` over the whole list
//! predicate (`status IN (0,1,2)` — [`OrderStats::Windowed`]). Money is integer
//! units (1/10000 元) throughout; the legacy `number_format` / `date()` 元
//! rendering is a view concern (the CSV formats timestamps but emits raw units).

use chrono::TimeZone;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Select, Statement, Value,
};

use crate::data::{members, orders};
use crate::merchant::downline::csv_field;
use crate::merchant::user_id_of_mch;
use crate::reconcile::day_window;
use crate::state::{GatewayError, GatewayResult};

/// The list page size (legacy `$size = 15`).
pub const PAGE_SIZE: u64 = 15;
/// The export cap — the legacy `exportorder` ran an unbounded `->select()`; a
/// single ordered fetch bounds memory while keeping the whole-downline export
/// intent (the same guardrail as §6.5 [`crate::merchant::downline::EXPORT_CAP`]).
pub const EXPORT_CAP: u64 = 10_000;

/// The list-leg order statuses (`order` / `childord` keep unpaid rows:
/// `pay_status IN (0,1,2)`).
const LIST_STATUS: [i32; 3] = [0, 1, 2];
/// The export-leg + statistics success statuses (`exportorder` narrows to
/// `pay_status IN (1,2)`; the 今日 / 累计 roll-up counts only successes).
const SUCCESS_STATUS: [i32; 2] = [1, 2];

/// The order-detail filter matrix. `None` / empty legs are not applied. Time
/// windows arrive as epoch seconds (the rewrite's uniform convention, matching
/// §7), the caller having parsed the legacy `start|end` picklist.
#[derive(Debug, Default, Clone)]
pub struct DownlineOrderFilter {
    /// `pay_memberid` — a single child's wire merchant number.
    pub memberid: Option<i64>,
    /// `orderid` — the merchant order number (`orders.order_id`).
    pub order_id: Option<String>,
    /// `body` — the product name (`orders.product_name`), exact match.
    pub product_name: Option<String>,
    /// Inclusive `orders.apply_date` (提交时间) window, unix seconds.
    pub apply_start: Option<i64>,
    pub apply_end: Option<i64>,
    /// Inclusive `orders.success_date` (成功时间) window, unix seconds.
    pub success_start: Option<i64>,
    pub success_end: Option<i64>,
}

impl DownlineOrderFilter {
    /// Whether ANY time window is posted — the branch that picks the summary
    /// roll-up vs the filtered window total (§6.4 `order` L534 / L562).
    pub fn has_window(&self) -> bool {
        self.apply_start.is_some()
            || self.apply_end.is_some()
            || self.success_start.is_some()
            || self.success_end.is_some()
    }
}

/// The scoped statistics, in the two mutually-exclusive legacy shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStats {
    /// No window: 今日成功金额 / 笔数 + 累计成功金额 / 笔数 (`status IN (1,2)`,
    /// the 今日 leg anchored on `success_date` within the caller's local day).
    Summary {
        today_amount: i64,
        today_count: i64,
        total_amount: i64,
        total_count: i64,
    },
    /// A window was posted: the filtered list total (`status IN (0,1,2)`).
    Windowed {
        amount: i64,
        actual_amount: i64,
        count: i64,
    },
}

impl OrderStats {
    fn zeroes(windowed: bool) -> Self {
        if windowed {
            OrderStats::Windowed {
                amount: 0,
                actual_amount: 0,
                count: 0,
            }
        } else {
            OrderStats::Summary {
                today_amount: 0,
                today_count: 0,
                total_amount: 0,
                total_count: 0,
            }
        }
    }
}

/// One page of the downline order list plus the scoped statistics.
#[derive(Debug, Clone)]
pub struct DownlineOrderPage {
    pub total: u64,
    pub orders: Vec<orders::Model>,
    pub stats: OrderStats,
}

/// Resolves the effective owner-uid scope: the agent's direct children, or the
/// single child a supplied `memberid` maps to when it is one of them (an
/// outsider / malformed id → the empty scope).
async fn resolve_scope(
    db: &DatabaseConnection,
    agent: i64,
    f: &DownlineOrderFilter,
) -> GatewayResult<Vec<i64>> {
    let children: Vec<i64> = members::Entity::find()
        .filter(members::Column::Parentid.eq(agent))
        .filter(members::Column::Groupid.ne(1))
        .all(db)
        .await?
        .into_iter()
        .map(|m| m.id)
        .collect();
    Ok(match f.memberid {
        None => children,
        Some(mch) => user_id_of_mch(mch)
            .filter(|u| children.contains(u))
            .map(|u| vec![u])
            .unwrap_or_default(),
    })
}

/// The shared list predicate: `user_id ∈ scope`, `status ∈ statuses`, plus the
/// order-number / product-name / date-window legs. Reused by the page list and
/// the export.
fn scoped_select(
    scope: &[i64],
    f: &DownlineOrderFilter,
    statuses: &[i32],
) -> Select<orders::Entity> {
    let mut q = orders::Entity::find()
        .filter(orders::Column::UserId.is_in(scope.iter().copied()))
        .filter(orders::Column::Status.is_in(statuses.iter().copied()));
    if let Some(o) = f.order_id.as_deref().filter(|s| !s.is_empty()) {
        q = q.filter(orders::Column::OrderId.eq(o));
    }
    if let Some(b) = f.product_name.as_deref().filter(|s| !s.is_empty()) {
        q = q.filter(orders::Column::ProductName.eq(b));
    }
    if let Some(s) = f.apply_start {
        q = q.filter(orders::Column::ApplyDate.gte(s));
    }
    if let Some(e) = f.apply_end {
        q = q.filter(orders::Column::ApplyDate.lte(e));
    }
    if let Some(s) = f.success_start {
        q = q.filter(orders::Column::SuccessDate.gte(s));
    }
    if let Some(e) = f.success_end {
        q = q.filter(orders::Column::SuccessDate.lte(e));
    }
    q
}

/// Appends `AND <col> >= $k` / `AND <col> <= $k` for whichever bounds are
/// present (the §7 idiom, kept local so both raw builders share it).
fn push_range(
    sql: &mut String,
    params: &mut Vec<Value>,
    idx: &mut i64,
    col: &str,
    start: Option<i64>,
    end: Option<i64>,
) {
    if let Some(s) = start {
        *idx += 1;
        sql.push_str(&format!(" AND {col} >= ${idx}"));
        params.push(Value::from(s));
    }
    if let Some(e) = end {
        *idx += 1;
        sql.push_str(&format!(" AND {col} <= ${idx}"));
        params.push(Value::from(e));
    }
}

/// Builds the NO-window roll-up: today (`success_date BETWEEN $1 AND $2`,
/// `status IN (1,2)`) and cumulative (`status IN (1,2)`) amount / count, over
/// `user_id IN ($3 …)` (the scope). `$1 / $2` are reused across `FILTER` legs.
/// Exposed for offline placeholder tests.
pub(crate) fn stats_summary_sql(scope: &[i64], from: i64, to: i64) -> (String, Vec<Value>) {
    let mut params = vec![Value::from(from), Value::from(to)];
    let mut idx: i64 = 2;
    let mut ph = Vec::with_capacity(scope.len());
    for id in scope {
        idx += 1;
        ph.push(format!("${idx}"));
        params.push(Value::from(*id));
    }
    let sql = format!(
        "SELECT \
            cast(coalesce(sum(amount) FILTER (WHERE success_date BETWEEN $1 AND $2 AND status IN (1,2)), 0) as bigint), \
            cast(count(*) FILTER (WHERE success_date BETWEEN $1 AND $2 AND status IN (1,2)) as bigint), \
            cast(coalesce(sum(amount) FILTER (WHERE status IN (1,2)), 0) as bigint), \
            cast(count(*) FILTER (WHERE status IN (1,2)) as bigint) \
         FROM orders WHERE user_id IN ({})",
        ph.join(",")
    );
    (sql, params)
}

/// Builds the WITH-window filtered total: `SUM(amount) / SUM(actual_amount) /
/// count(*)` over `user_id IN ($1 …)` (`status IN (0,1,2)`) plus the order /
/// body / window legs — the fold of the same predicate the list pages.
pub(crate) fn stats_windowed_sql(scope: &[i64], f: &DownlineOrderFilter) -> (String, Vec<Value>) {
    let mut idx: i64 = 0;
    let mut params: Vec<Value> = Vec::new();
    let mut ph = Vec::with_capacity(scope.len());
    for id in scope {
        idx += 1;
        ph.push(format!("${idx}"));
        params.push(Value::from(*id));
    }
    let mut sql = format!(
        "SELECT cast(coalesce(sum(amount), 0) as bigint), \
                cast(coalesce(sum(actual_amount), 0) as bigint), \
                cast(count(*) as bigint) \
         FROM orders WHERE user_id IN ({}) AND status IN (0,1,2)",
        ph.join(",")
    );
    if let Some(o) = f.order_id.as_deref().filter(|s| !s.is_empty()) {
        idx += 1;
        sql.push_str(&format!(" AND order_id = ${idx}"));
        params.push(Value::from(o.to_string()));
    }
    if let Some(b) = f.product_name.as_deref().filter(|s| !s.is_empty()) {
        idx += 1;
        sql.push_str(&format!(" AND product_name = ${idx}"));
        params.push(Value::from(b.to_string()));
    }
    push_range(
        &mut sql,
        &mut params,
        &mut idx,
        "apply_date",
        f.apply_start,
        f.apply_end,
    );
    push_range(
        &mut sql,
        &mut params,
        &mut idx,
        "success_date",
        f.success_start,
        f.success_end,
    );
    (sql, params)
}

/// Runs the scoped statistics for the non-empty `scope`, branching on the
/// window shape. `today` anchors the 今日 leg (the caller's local day) so the
/// window is deterministic under test.
async fn compute_stats(
    db: &DatabaseConnection,
    scope: &[i64],
    f: &DownlineOrderFilter,
    today: chrono::NaiveDate,
) -> GatewayResult<OrderStats> {
    if f.has_window() {
        let (sql, params) = stats_windowed_sql(scope, f);
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                sql,
                params,
            ))
            .await
            .map_err(|e| GatewayError::Internal(format!("db: {e}")))?
            .expect("one aggregate row");
        let n = |i: usize| row.try_get_by_index::<i64>(i).unwrap_or_default();
        return Ok(OrderStats::Windowed {
            amount: n(0),
            actual_amount: n(1),
            count: n(2),
        });
    }
    let (from, to) = day_window(today);
    let (sql, params) = stats_summary_sql(scope, from, to);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            params,
        ))
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?
        .expect("one aggregate row");
    let n = |i: usize| row.try_get_by_index::<i64>(i).unwrap_or_default();
    Ok(OrderStats::Summary {
        today_amount: n(0),
        today_count: n(1),
        total_amount: n(2),
        total_count: n(3),
    })
}

/// The §6.4 order-detail page: scoping + statistics + the newest-first paged
/// order list. An empty scope (no children, or a `memberid` outside them) yields
/// zero rows and zeroed stats without touching the order table.
pub async fn downline_order_page(
    db: &DatabaseConnection,
    agent: i64,
    f: &DownlineOrderFilter,
    page: u64,
    rows: u64,
) -> GatewayResult<DownlineOrderPage> {
    let page = page.max(1);
    let rows = rows.max(1);
    let scope = resolve_scope(db, agent, f).await?;
    if scope.is_empty() {
        return Ok(DownlineOrderPage {
            total: 0,
            orders: Vec::new(),
            stats: OrderStats::zeroes(f.has_window()),
        });
    }
    let today = chrono::Local::now().naive_local().date();
    let stats = compute_stats(db, &scope, f, today).await?;
    let q = scoped_select(&scope, f, &LIST_STATUS);
    let total = q.clone().count(db).await?;
    let list = q
        .order_by_desc(orders::Column::Id)
        .offset((page - 1) * rows)
        .limit(rows)
        .all(db)
        .await?;
    Ok(DownlineOrderPage {
        total,
        orders: list,
        stats,
    })
}

/// The `exportorder` read: the WHOLE (capped) success-only (`status IN (1,2)`)
/// downline order set, newest first.
pub async fn downline_order_export(
    db: &DatabaseConnection,
    agent: i64,
    f: &DownlineOrderFilter,
) -> GatewayResult<Vec<orders::Model>> {
    let scope = resolve_scope(db, agent, f).await?;
    if scope.is_empty() {
        return Ok(Vec::new());
    }
    Ok(scoped_select(&scope, f, &SUCCESS_STATUS)
        .order_by_desc(orders::Column::Id)
        .limit(EXPORT_CAP)
        .all(db)
        .await?)
}

// --- §6.4 export CSV (`exportorder`) ----------------------------------------

/// The export header, mirroring the legacy `$title`.
pub const ORDER_EXPORT_COLUMNS: &[&str] = &[
    "订单号",
    "商户编号",
    "交易金额",
    "手续费",
    "实际金额",
    "提交时间",
    "成功时间",
    "支付通道",
    "支付状态",
];

/// The 支付状态 label — the legacy `switch ($item['pay_status'])` mapped only
/// 0 / 1 / 2 (other → `""`, though the export pre-filters to 1/2 anyway).
pub fn order_status_str(status: i32) -> &'static str {
    match status {
        0 => "未处理",
        1 => "成功，未返回",
        2 => "成功，已返回",
        _ => "",
    }
}

/// A unix-seconds → `Y-m-d H:i:s` local timestamp (empty on an out-of-range /
/// null value), the legacy `date('Y-m-d H:i:s', …)` rendering.
fn fmt_ts(ts: i64) -> String {
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// Renders the §6.4 order export as UTF-8 CSV bytes (BOM + header + one row per
/// order). 订单号 prefers `out_trade_id` and falls back to `order_id` (the
/// legacy `out_trade_id ? : pay_orderid`); 商户编号 is the wire `mch_id`;
/// amounts are raw money units (the 元 division is a view concern, unlike the
/// legacy's `number_format`).
pub fn render_order_csv(rows: &[orders::Model]) -> Vec<u8> {
    let mut out = String::from("\u{FEFF}");
    out.push_str(
        &ORDER_EXPORT_COLUMNS
            .iter()
            .map(|c| csv_field((*c).to_string()))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for r in rows {
        let order_no = r
            .out_trade_id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| r.order_id.clone());
        let fields = [
            order_no,
            r.mch_id.clone(),
            r.amount.to_string(),
            r.poundage.to_string(),
            r.actual_amount.to_string(),
            fmt_ts(r.apply_date),
            r.success_date.map(fmt_ts).unwrap_or_default(),
            r.channel_code.clone().unwrap_or_default(),
            order_status_str(r.status).to_string(),
        ];
        out.push_str(
            &fields
                .into_iter()
                .map(csv_field)
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_status_maps_the_three_success_states() {
        assert_eq!(order_status_str(0), "未处理");
        assert_eq!(order_status_str(1), "成功，未返回");
        assert_eq!(order_status_str(2), "成功，已返回");
        assert_eq!(order_status_str(9), "");
    }

    #[test]
    fn summary_sql_puts_the_window_first_then_the_scope() {
        let (sql, params) = stats_summary_sql(&[11, 22, 33], 100, 200);
        // $1/$2 are the today window (reused across FILTER legs); scope starts $3.
        assert!(sql.contains("success_date BETWEEN $1 AND $2"));
        assert!(sql.contains("user_id IN ($3,$4,$5)"));
        assert_eq!(params.len(), 5); // from, to, 3 ids
        assert_eq!(params[0], Value::from(100i64));
        assert_eq!(params[2], Value::from(11i64));
    }

    #[test]
    fn windowed_sql_numbers_placeholders_from_the_scope_up() {
        let f = DownlineOrderFilter {
            order_id: Some("ORD-1".into()),
            product_name: None,
            apply_start: Some(1_000),
            apply_end: Some(2_000),
            success_start: None,
            success_end: Some(3_000),
            memberid: None,
        };
        let (sql, params) = stats_windowed_sql(&[7, 8], &f);
        // scope $1,$2; order_id $3; apply >= $4; apply <= $5; success <= $6
        assert!(sql.contains("user_id IN ($1,$2)"));
        assert!(sql.contains("AND order_id = $3"));
        assert!(sql.contains("AND apply_date >= $4"));
        assert!(sql.contains("AND apply_date <= $5"));
        assert!(!sql.contains("success_date >="));
        assert!(sql.contains("AND success_date <= $6"));
        assert_eq!(params.len(), 6);
    }

    #[test]
    fn has_window_keys_off_any_date_leg() {
        assert!(!DownlineOrderFilter::default().has_window());
        let f = DownlineOrderFilter {
            success_end: Some(1),
            ..Default::default()
        };
        assert!(f.has_window());
    }
}
