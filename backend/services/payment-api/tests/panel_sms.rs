//! DB coverage for the §5 SMS下发面 config model (`pay_sms`). The provider HTTP
//! seam (`SmsBaoProvider`) is covered offline against a loopback stub in
//! `sms.rs`; here we drive the DB-backed reads that unlock `sms_status()`:
//!
//! - an empty `pay_sms` table reads closed (`sms_status() == false`);
//! - a row with `is_open = 1` / channel `smsbao` reads open and maps into
//!   [`SmsConfig`] (channel, credentials, 【签名】) so the caller can pick the
//!   provider;
//! - flipping `is_open` back to 0 closes it again.
//!
//! The table is cleared at the top so the singleton `MIN(id)` read is stable
//! across reruns (this is the only binary touching `sms_configs`). Harness
//! mirrors `panel_console.rs`; ids base 120_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, EntityTrait, Set};

use common::{suite, uid};
use payment_api::data::sms_configs;
use payment_api::sms::{self, SmsConfig};

const BASE: i64 = 120_000_000_000_000;

async fn upsert_config(s: &common::Suite, id: i64, is_open: i32) {
    sms_configs::ActiveModel {
        id: Set(id),
        app_key: Set(None),
        app_secret: Set(None),
        sign_name: Set(Some("多宝".into())),
        is_open: Set(is_open),
        admin_mobile: Set(None),
        is_receive: Set(0),
        sms_channel: Set("smsbao".into()),
        smsbao_user: Set("acct".into()),
        smsbao_pass: Set("secret".into()),
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

#[tokio::test]
async fn sms_status_reflects_the_pay_sms_row() {
    let Some(s) = suite().await else { return };
    sms_configs::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
    // no row → closed
    assert!(!sms::sms_status(&s.db).await.unwrap());

    let id = uid(BASE);
    upsert_config(&s, id, 1).await;
    assert!(sms::sms_status(&s.db).await.unwrap());

    let cfg = SmsConfig::load(&s.db).await.unwrap().unwrap();
    assert!(cfg.is_open);
    assert_eq!(cfg.sms_channel, "smsbao");
    assert_eq!(cfg.sign_name, "多宝");
    assert_eq!(cfg.smsbao_user, "acct");
    // A fully-configured smsbao row → `provider()` selects the real gateway
    // (the actual HTTP POST is covered offline against a loopback stub in
    // `sms.rs`, so we do NOT send here — no outbound call from tests).

    // flip closed again
    sms_configs::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
    upsert_config(&s, id, 0).await;
    assert!(!sms::sms_status(&s.db).await.unwrap());

    sms_configs::Entity::delete_many()
        .exec(&*s.db)
        .await
        .unwrap();
}
