//! Merchant settlement bank cards — the port of
//! `User/AccountController::addBankcard / editBankStatus / delBankcard`
//! (`spec/05` §10). Same SERVICE-LAYER-ONLY scoping as [`crate::merchant::
//! profile`]: `addBankcard` is SMS-gated upstream (`auth_type = 2`), a second
//! factor the Rust side has not wired yet, so these are the DB primitives and
//! their ownership rules, covered by integration tests; the panel HTTP write
//! stays a documented seam.
//!
//! Ownership is the whole safety story: every targeted op filters
//! `(id, userid)` — the legacy `where(['id' => $id, 'userid' => $this->fans
//! ['uid']])` — so one merchant can never edit, default or delete another's
//! card (a foreign id simply affects 0 rows).

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::data::bankcards;
use crate::state::GatewayResult;

/// A card's text fields as `addBankcard` submits them. The empty string is a
/// real write (legacy `save($rows)` stores what the form posts).
#[derive(Debug, Clone, Default)]
pub struct BankcardForm<'a> {
    pub bankname: &'a str,
    pub subbranch: &'a str,
    pub accountname: &'a str,
    pub cardnumber: &'a str,
    pub province: &'a str,
    pub city: &'a str,
    pub alias: &'a str,
}

/// Inserts a new card for `userid` (`id = None`, `isdefault = 0`) or updates an
/// OWNED one (`id = Some`, silently a no-op on a foreign / unknown id). Returns
/// the affected-row count (0 for a rejected foreign update; 1 on insert), the
/// legacy `ajaxReturn(['status' => $res])` value.
pub async fn upsert_card(
    db: &DatabaseConnection,
    id: Option<i64>,
    userid: i64,
    form: &BankcardForm<'_>,
    now: i64,
) -> GatewayResult<u64> {
    match id {
        Some(id) => {
            let res = bankcards::Entity::update_many()
                .col_expr(
                    bankcards::Column::Bankname,
                    Expr::value(Some(form.bankname.to_string())),
                )
                .col_expr(
                    bankcards::Column::Subbranch,
                    Expr::value(Some(form.subbranch.to_string())),
                )
                .col_expr(
                    bankcards::Column::Accountname,
                    Expr::value(Some(form.accountname.to_string())),
                )
                .col_expr(
                    bankcards::Column::Cardnumber,
                    Expr::value(Some(form.cardnumber.to_string())),
                )
                .col_expr(
                    bankcards::Column::Province,
                    Expr::value(Some(form.province.to_string())),
                )
                .col_expr(
                    bankcards::Column::City,
                    Expr::value(Some(form.city.to_string())),
                )
                .col_expr(
                    bankcards::Column::Alias,
                    Expr::value(Some(form.alias.to_string())),
                )
                .col_expr(bankcards::Column::Updatetime, Expr::value(now))
                .filter(bankcards::Column::Id.eq(id))
                .filter(bankcards::Column::Userid.eq(userid))
                .exec(db)
                .await?;
            Ok(res.rows_affected)
        }
        None => {
            let active = bankcards::ActiveModel {
                userid: Set(userid),
                bankname: Set(Some(form.bankname.to_string())),
                subbranch: Set(Some(form.subbranch.to_string())),
                accountname: Set(Some(form.accountname.to_string())),
                cardnumber: Set(Some(form.cardnumber.to_string())),
                province: Set(Some(form.province.to_string())),
                city: Set(Some(form.city.to_string())),
                alias: Set(Some(form.alias.to_string())),
                isdefault: Set(0),
                updatetime: Set(now),
                ..Default::default()
            };
            active.insert(db).await?;
            Ok(1)
        }
    }
}

/// Sets a card's default flag. Mirrors `editBankStatus` L317-330: when making a
/// card the default (`isdefault != 0`) the user's OTHER cards are cleared to
/// `isdefault = 0` first, then the target (owned) row is set — so at most one
/// default per merchant survives. A falsy `isdefault` only un-sets the target.
pub async fn set_default(
    db: &DatabaseConnection,
    id: i64,
    userid: i64,
    isdefault: i32,
    now: i64,
) -> GatewayResult<u64> {
    if isdefault != 0 {
        bankcards::Entity::update_many()
            .col_expr(bankcards::Column::Isdefault, Expr::value(0))
            .filter(bankcards::Column::Userid.eq(userid))
            .exec(db)
            .await?;
    }
    let res = bankcards::Entity::update_many()
        .col_expr(bankcards::Column::Isdefault, Expr::value(isdefault))
        .col_expr(bankcards::Column::Updatetime, Expr::value(now))
        .filter(bankcards::Column::Id.eq(id))
        .filter(bankcards::Column::Userid.eq(userid))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// Deletes an OWNED card (`where(id, userid)`); a foreign / unknown id removes
/// nothing (returns 0). The legacy `delBankcard` `ajaxReturn(['status' => $res])`.
pub async fn delete_card(db: &DatabaseConnection, id: i64, userid: i64) -> GatewayResult<u64> {
    let res = bankcards::Entity::delete_many()
        .filter(bankcards::Column::Id.eq(id))
        .filter(bankcards::Column::Userid.eq(userid))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// Lists a merchant's own cards, newest id last (the panel's plain ordering).
pub async fn list_for_user(
    db: &DatabaseConnection,
    userid: i64,
) -> GatewayResult<Vec<bankcards::Model>> {
    Ok(bankcards::Entity::find()
        .filter(bankcards::Column::Userid.eq(userid))
        .order_by_asc(bankcards::Column::Id)
        .all(db)
        .await?)
}
