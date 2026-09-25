//! Agent downline management (`spec/05` §6.4) — the two `User/AgentController`
//! surfaces a logged-in agent (groupid 5/7) uses over its DIRECT children:
//!
//! * [`list_page`] / [`count_filtered`] back the 下级会员 list (`member()`):
//!   members scoped to `parentid = agent` and `groupid != 1` (never the
//!   platform), with the legacy `username` filter doubling as a 商户号
//!   (`id = username - 10000`) match when it is numeric, plus optional
//!   `status` / `authorized` matches, newest id first, 15 per page.
//! * [`set_child_status`] backs the 启停 toggle (`editStatus()`): a member that
//!   is not the caller's direct child is refused.
//!
//! Faithful legacy quirks (registered as a decision memory):
//! - The `!empty($status)` / `!empty($authorized)` guards mean a posted `0`
//!   selection is silently IGNORED (PHP `empty("0")` is true), so you cannot
//!   filter to "only disabled / only unauthorized" rows — the caller collapses
//!   `0` to `None` to reproduce it.
//! - The `member()` page's optional `regdatetime` range filter is dropped: the
//!   rewrite never models or populates `regdatetime` (a display-only column,
//!   like `activatedatetime`), so the list cannot key on it.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Select, Set,
};

use crate::data::members;
use crate::merchant::{mch_id_of, user_id_of_mch};
use crate::state::{GatewayError, GatewayResult};

/// The 下级会员 page's rows-per-page (legacy `new Page($count, 15)`).
pub const PAGE_SIZE: u64 = 15;

/// The list filters, layered over the always-on `parentid` + `groupid != 1`
/// scoping. `None` legs are not applied.
#[derive(Debug, Default, Clone)]
pub struct DownlineFilter {
    /// A text username `LIKE` match, or — when numeric — an exact 商户号
    /// (`id = value - 10000`, applied only while `id > 0`, the legacy guard).
    pub username: Option<String>,
    /// `status` exact match; the caller maps the legacy-empty `0` to `None`.
    pub status: Option<i32>,
    /// `authorized` exact match; same `0`-as-empty collapse.
    pub authorized: Option<i32>,
}

/// Binds the always-on downline scope: this agent's children, never the
/// platform account.
fn scoped(agent_uid: i64) -> Select<members::Entity> {
    members::Entity::find()
        .filter(members::Column::Parentid.eq(agent_uid))
        .filter(members::Column::Groupid.ne(1))
}

/// Applies the optional username / 商户号 / status / authorized filters.
fn with_filters(q: Select<members::Entity>, f: &DownlineFilter) -> Select<members::Entity> {
    let mut q = q;
    if let Some(u) = f
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // A numeric search box is a 商户号; anything else is a name substring.
        match u.parse::<i64>() {
            Ok(mchno) => {
                if let Some(id) = user_id_of_mch(mchno).filter(|id| *id > 0) {
                    q = q.filter(members::Column::Id.eq(id));
                }
            }
            Err(_) => q = q.filter(members::Column::Username.contains(u)),
        }
    }
    if let Some(status) = f.status {
        q = q.filter(members::Column::Status.eq(status));
    }
    if let Some(authorized) = f.authorized {
        q = q.filter(members::Column::Authorized.eq(authorized));
    }
    q
}

/// Counts the agent's downline members matching [`DownlineFilter`].
pub async fn count_filtered(
    db: &DatabaseConnection,
    agent_uid: i64,
    f: &DownlineFilter,
) -> GatewayResult<u64> {
    with_filters(scoped(agent_uid), f)
        .count(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// One page of the agent's filtered downline, newest id first. `page` is
/// 1-based (clamped up), `rows` the page size.
pub async fn list_page(
    db: &DatabaseConnection,
    agent_uid: i64,
    f: &DownlineFilter,
    page: u64,
    rows: u64,
) -> GatewayResult<Vec<members::Model>> {
    let page = page.max(1);
    let offset = (page - 1) * rows;
    with_filters(scoped(agent_uid), f)
        .order_by_desc(members::Column::Id)
        .offset(offset)
        .limit(rows)
        .all(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// The outcome of [`set_child_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusOutcome {
    /// No member with that id (`用户不存在！`).
    NotFound,
    /// The member exists but is not the caller's direct child (`您没有权限...`).
    NotOwned,
    /// The child's `status` was persisted.
    Updated,
}

/// Runs the §6.4 `editStatus` toggle: look the target up, refuse unless it is a
/// DIRECT child of the acting agent (`parentid = agent`), then write the new
/// `status`. The legacy derives the value as `isopen ? isopen : 0`; the caller
/// has already resolved that to the concrete `status` to persist.
pub async fn set_child_status(
    db: &DatabaseConnection,
    agent_uid: i64,
    child_uid: i64,
    status: i32,
) -> GatewayResult<StatusOutcome> {
    let Some(member) = members::Entity::find_by_id(child_uid)
        .one(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?
    else {
        return Ok(StatusOutcome::NotFound);
    };
    if member.parentid != agent_uid {
        return Ok(StatusOutcome::NotOwned);
    }
    let mut active: members::ActiveModel = member.into();
    active.status = Set(status);
    active
        .update(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(StatusOutcome::Updated)
}

// --- §6.5 export (`User/AgentController::exportuser`) -----------------------

/// The 下级会员 export cap: the legacy `exportuser` did an unbounded
/// `->select()` over the whole downline; a single-page fetch (well above any
/// realistic agent downline) bounds memory while keeping the whole-tree export
/// intent. Registered as a deliberate guardrail deviation.
pub const EXPORT_CAP: u64 = 10_000;

/// The export header row, mirroring the legacy `$title` (column parity kept,
/// 注册时间 left blank per the un-modelled `regdatetime`).
pub const EXPORT_COLUMNS: &[&str] = &[
    "用户名",
    "商户号",
    "用户类型",
    "上级用户名",
    "状态",
    "认证",
    "可用余额",
    "冻结余额",
    "注册时间",
];

/// The 用户类型 label — the legacy `switch ($item['groupid'])` mapped only 4
/// (商户) and 5 (代理商); any other group renders as `""` (faithful quirk).
pub fn user_type_str(groupid: i32) -> &'static str {
    match groupid {
        4 => "商户",
        5 => "代理商",
        _ => "",
    }
}

/// The 状态 label (`0 未激活 / 1 正常 / 2 已禁用`, other → `""`).
pub fn status_str(status: i32) -> &'static str {
    match status {
        0 => "未激活",
        1 => "正常",
        2 => "已禁用",
        _ => "",
    }
}

/// The 认证 label (`0 未认证 / 1 已认证 / 2 等待审核`, other → `""`).
pub fn authorized_str(authorized: i32) -> &'static str {
    match authorized {
        0 => "未认证",
        1 => "已认证",
        2 => "等待审核",
        _ => "",
    }
}

/// A minimal RFC-4180 field escape: quote only when the value contains a
/// comma / double-quote / CR / LF, doubling embedded quotes.
pub(crate) fn csv_field(value: String) -> String {
    if value.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

/// Renders the §6.5 export as UTF-8 CSV bytes: a BOM first (Excel Chinese
/// compat), the [`EXPORT_COLUMNS`] header, then one row per downline member.
/// Every row's 上级用户名 is the acting `agent_username` (the export scope is
/// the agent's own DIRECT children, so the parent is constant). Balances are
/// emitted as raw money units; 注册时间 is blank (the rewrite never models
/// `regdatetime`).
pub fn render_export_csv(agent_username: &str, rows: &[members::Model]) -> Vec<u8> {
    let mut out = String::from("\u{FEFF}");
    out.push_str(
        &EXPORT_COLUMNS
            .iter()
            .map(|c| csv_field((*c).to_string()))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for r in rows {
        let fields = [
            r.username.clone(),
            mch_id_of(r.id).to_string(),
            user_type_str(r.groupid).to_string(),
            agent_username.to_string(),
            status_str(r.status).to_string(),
            authorized_str(r.authorized).to_string(),
            r.balance.to_string(),
            r.blocked_balance.to_string(),
            String::new(), // 注册时间 (regdatetime un-modelled)
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

    fn member(id: i64, groupid: i32, status: i32, authorized: i32, balance: i64) -> members::Model {
        members::Model {
            id,
            username: format!("u{id}"),
            password: "x".into(),
            groupid,
            salt: String::new(),
            parentid: 50,
            balance,
            blocked_balance: 7,
            email: Some("e@x".into()),
            mobile: Some(String::new()),
            realname: Some(String::new()),
            apikey: Some(String::new()),
            pay_password: Some(String::new()),
            status,
            authorized,
            df_api: 0,
            df_domain: Some(String::new()),
            df_ip: Some(String::new()),
            df_auto_check: 0,
            google_secret_key: Some(String::new()),
            login_ip: Some(String::new()),
            session_version: None,
            sex: Some(0),
            birthday: Some(0),
            sfznumber: Some(String::new()),
            qq: Some(String::new()),
            address: Some(String::new()),
            receiver: Some(String::new()),
            activate: Some(String::new()),
        }
    }

    #[test]
    fn label_maps_follow_the_legacy_switches() {
        assert_eq!(user_type_str(4), "商户");
        assert_eq!(user_type_str(5), "代理商");
        // 6 / 7 (and anything else) render blank — the legacy switch had only
        // the 4 / 5 arms.
        assert_eq!(user_type_str(7), "");
        assert_eq!(status_str(0), "未激活");
        assert_eq!(status_str(1), "正常");
        assert_eq!(status_str(2), "已禁用");
        assert_eq!(status_str(9), "");
        assert_eq!(authorized_str(0), "未认证");
        assert_eq!(authorized_str(1), "已认证");
        assert_eq!(authorized_str(2), "等待审核");
        assert_eq!(authorized_str(8), "");
    }

    #[test]
    fn csv_quotes_only_when_needed() {
        assert_eq!(csv_field("plain".into()), "plain");
        assert_eq!(csv_field("a,b".into()), "\"a,b\"");
        assert_eq!(csv_field("q\"x".into()), "\"q\"\"x\"");
        assert_eq!(csv_field("line\nbreak".into()), "\"line\nbreak\"");
    }

    #[test]
    fn render_export_csv_boms_headers_and_rows() {
        let rows = [member(5, 4, 1, 1, 12345), member(9, 7, 2, 0, 0)];
        let bytes = render_export_csv("boss", &rows);
        let text = String::from_utf8(bytes).unwrap();
        // Leading BOM + full header line.
        assert!(text.starts_with('\u{FEFF}'));
        let mut lines = text
            .trim_start_matches('\u{FEFF}')
            .trim_end_matches('\n')
            .split('\n');
        assert_eq!(
            lines.next().unwrap(),
            "用户名,商户号,用户类型,上级用户名,状态,认证,可用余额,冻结余额,注册时间"
        );
        // id 5 → 商户号 10005, groupid 4 → 商户, parent = the acting agent.
        assert_eq!(
            lines.next().unwrap(),
            "u5,10005,商户,boss,正常,已认证,12345,7,"
        );
        // groupid 7 → blank type, status 2 → 已禁用, authorized 0 → 未认证.
        assert_eq!(lines.next().unwrap(), "u9,10009,,boss,已禁用,未认证,0,7,");
        assert!(lines.next().is_none());
    }
}
