//! DB coverage for the §5.4 / §10 login-audit surface. The handler is thin接线,
//! so (per the established service-layer-direct discipline) we drive
//! [`loginrecord::record_login`] / [`loginrecord::count_for_user`] /
//! [`loginrecord::list_page`] against a real Postgres:
//!
//! - a merchant's own front-console rows are returned newest-id-first and
//!   paginate by `limit/offset`;
//! - the count matches the rows, and a `type = 1` (back office) row or another
//!   merchant's row is always excluded from the front-console page;
//! - `record_login` files the raw `loginip` and leaves `loginaddress` `None`
//!   (the IP-geolocation seam).
//!
//! Harness mirrors `panel_attachment.rs`; ids base 90_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use chrono::{NaiveDate, NaiveDateTime};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::{loginrecords, members};
use payment_api::merchant::loginrecord::{self, TYPE_FRONT};

const BASE: i64 = 90_000_000_000_000;

fn at(day: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 9, day)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
}

async fn seed_member(s: &Suite, user: i64) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("lr{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(1),
        df_api: Set(0),
        df_auto_check: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// Files a back-office (`type = 1`) row directly, to prove the page filters it.
async fn seed_backoffice_row(s: &Suite, user: i64) {
    loginrecords::ActiveModel {
        userid: Set(user),
        logindatetime: Set(at(20)),
        loginip: Set("10.0.0.9".into()),
        loginaddress: Set(Some("后台-地址".into())),
        logintype: Set(1),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn record_and_list_orders_newest_first_with_paging() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user).await;

    let mut ids = Vec::new();
    for d in 1..=3u32 {
        ids.push(
            loginrecord::record_login(&s.db, user, "1.2.3.4", None, at(d))
                .await
                .unwrap(),
        );
    }
    assert_eq!(loginrecord::count_for_user(&s.db, user).await.unwrap(), 3);

    // first page (rows = 2): newest two, id descending
    let p1 = loginrecord::list_page(&s.db, user, 1, 2).await.unwrap();
    assert_eq!(p1.len(), 2);
    assert_eq!(p1[0].id, ids[2]);
    assert_eq!(p1[1].id, ids[1]);
    // second page: the oldest one
    let p2 = loginrecord::list_page(&s.db, user, 2, 2).await.unwrap();
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0].id, ids[0]);
}

#[tokio::test]
async fn list_is_scoped_to_uid_and_front_type() {
    let Some(s) = suite().await else { return };
    let mine = uid(BASE);
    let other = uid(BASE);
    seed_member(&s, mine).await;
    seed_member(&s, other).await;

    loginrecord::record_login(&s.db, mine, "1.1.1.1", None, at(10))
        .await
        .unwrap();
    loginrecord::record_login(&s.db, other, "2.2.2.2", None, at(11))
        .await
        .unwrap();
    // the merchant's OWN back-office login must not surface on the front page
    seed_backoffice_row(&s, mine).await;

    let rows = loginrecord::list_page(&s.db, mine, 1, 10).await.unwrap();
    assert_eq!(rows.len(), 1, "only mine + type=0");
    assert_eq!(rows[0].userid, mine);
    assert_eq!(rows[0].logintype, TYPE_FRONT);
    assert_eq!(loginrecord::count_for_user(&s.db, mine).await.unwrap(), 1);
}

#[tokio::test]
async fn record_login_stores_ip_and_nulls_address() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user).await;

    let id = loginrecord::record_login(&s.db, user, "203.0.113.7", None, at(15))
        .await
        .unwrap();
    let row = loginrecords::Entity::find_by_id(id)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.loginip, "203.0.113.7");
    assert_eq!(row.loginaddress, None, "geolocation stays a seam");
    assert_eq!(row.logintype, TYPE_FRONT);
}
