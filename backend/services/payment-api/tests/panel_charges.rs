//! DB coverage for the §10台卡 / 收款码 receiver write. The URL builders
//! ([`charges::charges_url`] / [`charges::qr_target_path`]) are pure and covered
//! offline; here we drive [`charges::get_receiver`] / [`charges::save_receiver`]
//! against a real Postgres (the `receiver` column added by
//! `m20260924_000003_member_receiver`):
//!
//! - a member starts with no receiver (`None`) and the write files the payee
//!   line, read back verbatim;
//! - the write is idempotent on a same-value re-submit (Postgres counts the
//!   update as 1 affected row).
//!
//! Harness mirrors `panel_loginrecord.rs`; ids base 100_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::charges;

const BASE: i64 = 100_000_000_000_000;

async fn seed_member(s: &Suite, user: i64) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("ch{user}")),
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

#[tokio::test]
async fn save_and_read_receiver_db() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user).await;
    assert_eq!(charges::get_receiver(&s.db, user).await.unwrap(), None);

    let rows = charges::save_receiver(&s.db, user, "张三便利店")
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        charges::get_receiver(&s.db, user).await.unwrap(),
        Some("张三便利店".to_string())
    );
}

#[tokio::test]
async fn save_receiver_is_idempotent_on_same_value() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user).await;
    charges::save_receiver(&s.db, user, "收银台A")
        .await
        .unwrap();
    // a same-value re-write still affects 1 row and keeps the value
    let rows = charges::save_receiver(&s.db, user, "收银台A")
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        charges::get_receiver(&s.db, user).await.unwrap(),
        Some("收银台A".to_string())
    );
}
