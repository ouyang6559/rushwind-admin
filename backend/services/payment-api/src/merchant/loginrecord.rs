//! Login audit records — the port of the `pay_loginrecord` write (during
//! `User/LoginController::check` L150-194) and the merchant `loginrecord()`
//! page (`AccountController::loginrecord` L380-399, `spec/05` §5.4 / §10).
//!
//! A row is appended on every successful front-console login ([`record_login`]
//! — `type = 0`, the merchant / agent portal; the back office logs `type = 1`
//! on a different surface). The legacy enriches `loginaddress` via an IP
//! geolocation lookup (`NIpLocation`) — an external service — so we keep the
//! column and pass `None` (a documented seam): the record still carries the
//! raw `loginip` and the clock, only the resolved province-city is unfilled.
//!
//! [`list_page`] mirrors the page's query: the acting merchant's OWN `type = 0`
//! rows (`where userid, type`), newest id first, with a page count + limit for
//! the legacy `Page($count, $rows)` widget.

use chrono::NaiveDateTime;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};

use crate::data::loginrecords;
use crate::state::{GatewayError, GatewayResult};

/// The `type` discriminator for the front console (`spec/05` §5.4).
pub const TYPE_FRONT: i32 = 0;

/// Appends one login row. `at` is the login wall clock (parameterised so tests
/// stay deterministic); `loginaddress` is `None` until an IP-geolocation
/// provider is wired. Returns the new row id.
pub async fn record_login(
    db: &DatabaseConnection,
    userid: i64,
    loginip: &str,
    loginaddress: Option<&str>,
    at: NaiveDateTime,
) -> GatewayResult<i64> {
    let model = loginrecords::ActiveModel {
        userid: Set(userid),
        logindatetime: Set(at),
        loginip: Set(loginip.to_string()),
        loginaddress: Set(loginaddress.map(str::to_string)),
        logintype: Set(TYPE_FRONT),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(model.id)
}

/// Total front-console login rows for a merchant (the page's `count`).
pub async fn count_for_user(db: &DatabaseConnection, userid: i64) -> GatewayResult<u64> {
    loginrecords::Entity::find()
        .filter(loginrecords::Column::Userid.eq(userid))
        .filter(loginrecords::Column::Logintype.eq(TYPE_FRONT))
        .count(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// One page of a merchant's own `type = 0` login rows, newest id first
/// (`order('id desc')` + `limit firstRow, listRows`). `page` is 1-based; a
/// `page` below 1 is clamped to the first page.
pub async fn list_page(
    db: &DatabaseConnection,
    userid: i64,
    page: u64,
    rows: u64,
) -> GatewayResult<Vec<loginrecords::Model>> {
    let page = page.max(1);
    let offset = (page - 1) * rows;
    loginrecords::Entity::find()
        .filter(loginrecords::Column::Userid.eq(userid))
        .filter(loginrecords::Column::Logintype.eq(TYPE_FRONT))
        .order_by_desc(loginrecords::Column::Id)
        .offset(offset)
        .limit(rows)
        .all(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}
