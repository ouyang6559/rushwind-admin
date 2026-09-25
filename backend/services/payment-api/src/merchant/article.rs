//! Platform公告 / news reads — the port of `User/IndexController::gonggao`
//! (the paginated公告 list, L122-140) and the `gglist` block of `main`
//! (the latest-2 notices, L62-69), `spec/05` §10.
//!
//! Both read the same visible set: `status = 1` and a `groupid` targeted at the
//! acting account — the legacy branches on `member.groupid == 4` (merchant):
//! a merchant sees audience `0`(所有人) / `1`(商户), an agent sees `0` / `2`(代理).
//! [`audience_groups`] encodes that branch; [`list_visible`] / [`count_visible`]
//! drive the `Page($count, 5)` widget and [`latest_visible`] the `main` block
//! (`limit 2, order id desc`).
//!
//! `showcontent()` (the article detail page) is a thin by-id read on the same
//! visibility rule; it is out of scope for this console aggregation slice.

use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};

use crate::data::articles;
use crate::state::{GatewayError, GatewayResult};

/// A visible notice targets everyone.
pub const GROUP_ALL: i32 = 0;
/// The audience group a notice can pin to merchants (`groupid == 4` members).
pub const GROUP_MERCHANT: i32 = 1;
/// The audience group a notice can pin to agents.
pub const GROUP_AGENT: i32 = 2;

/// The `groupid`s visible to the caller: a merchant (`member.groupid == 4`) sees
/// `{all, merchant}`, an agent sees `{all, agent}` — the legacy `if/else` branch.
pub fn audience_groups(is_merchant: bool) -> [i32; 2] {
    if is_merchant {
        [GROUP_ALL, GROUP_MERCHANT]
    } else {
        [GROUP_ALL, GROUP_AGENT]
    }
}

/// The visible-article count for the caller's audience (`$count` for the page
/// widget; `status = 1 AND groupid IN audience`).
pub async fn count_visible(db: &DatabaseConnection, is_merchant: bool) -> GatewayResult<u64> {
    articles::Entity::find()
        .filter(articles::Column::Status.eq(1))
        .filter(articles::Column::Groupid.is_in(audience_groups(is_merchant)))
        .count(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// One page of visible articles, newest id first
/// (`order('id desc')->limit(firstRow, listRows)`). `offset` is the widget's
/// `firstRow` (already `(page-1)*size`), `limit` the rows-per-page.
pub async fn list_visible(
    db: &DatabaseConnection,
    is_merchant: bool,
    offset: u64,
    limit: u64,
) -> GatewayResult<Vec<articles::Model>> {
    articles::Entity::find()
        .filter(articles::Column::Status.eq(1))
        .filter(articles::Column::Groupid.is_in(audience_groups(is_merchant)))
        .order_by_desc(articles::Column::Id)
        .offset(offset)
        .limit(limit)
        .all(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// The `main` block's latest-N visible notices (`limit 2, order id desc`).
pub async fn latest_visible(
    db: &DatabaseConnection,
    is_merchant: bool,
    n: u64,
) -> GatewayResult<Vec<articles::Model>> {
    list_visible(db, is_merchant, 0, n).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_groups_branch_on_merchant() {
        assert_eq!(audience_groups(true), [0, 1]);
        assert_eq!(audience_groups(false), [0, 2]);
    }
}
