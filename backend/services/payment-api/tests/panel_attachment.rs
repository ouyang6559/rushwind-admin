//! DB + local-FS coverage for the §8.5 merchant认证 attachment surface. The
//! handlers are thin接线, so (per the established service-layer-direct
//! discipline) we drive [`attachment::add`] / [`attachment::list_for_user`] /
//! [`attachment::submit_certification`] and the [`attachment::FileStorage`]
//! store against a real Postgres and a temp uploads root:
//!
//! - FileStorage writes bytes under `<root>/verifyinfo/<uniqid>.<ext>` and
//!   returns the site-relative `Uploads/verifyinfo/...` record path;
//! - a merchant's evidence rows round-trip through the DB, ordered by id and
//!   scoped strictly to their own `userid`;
//! - `submit_certification` files `authorized = 2` (待审核) unconditionally and
//!   is idempotent when already pending (Postgres counts the same-value update
//!   as 1 affected row, so a re-submit still reports success).
//!
//! Harness mirrors `panel_google.rs`; ids base 80_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use sea_orm::{ActiveModelTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::attachment::{self, FileStorage};

const BASE: i64 = 80_000_000_000_000;

/// Seeds a groupid-4 merchant with a chosen `authorized` state.
async fn seed_member(s: &Suite, user: i64, authorized: i32) {
    members::ActiveModel {
        id: Set(user),
        username: Set(format!("at{user}")),
        password: Set("x".into()),
        groupid: Set(4),
        salt: Set(String::new()),
        parentid: Set(1),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(1),
        authorized: Set(authorized),
        df_api: Set(0),
        df_auto_check: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

/// A process-unique temp uploads root, removed when the guard drops.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("payment-att-{}-{tag}", uid(BASE)));
        Self(dir)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn file_storage_writes_and_returns_record_path() {
    let root = TempRoot::new("store");
    let store = FileStorage::new(root.0.clone());
    let name = attachment::uniqid(1_700_000_000_000_000);
    let bytes = b"fake-jpeg-bytes";
    let (fname, record) = store.store(&name, "jpg", bytes).await.unwrap();

    assert_eq!(fname, format!("{name}.jpg"));
    assert_eq!(record, format!("Uploads/verifyinfo/{name}.jpg"));
    let abs = root.0.join("verifyinfo").join(&fname);
    assert!(abs.exists(), "the bytes must land on disk");
    assert_eq!(std::fs::read(&abs).unwrap(), bytes);
}

#[tokio::test]
async fn add_and_list_roundtrip_db() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, 0).await;

    let id1 = attachment::add(&s.db, user, "a.jpg", "Uploads/verifyinfo/x1.jpg")
        .await
        .unwrap();
    let id2 = attachment::add(&s.db, user, "b.png", "Uploads/verifyinfo/x2.png")
        .await
        .unwrap();
    assert!(id2 > id1, "auto-increment ids must ascend");

    let rows = attachment::list_for_user(&s.db, user).await.unwrap();
    assert_eq!(rows.len(), 2);
    // ordered oldest-id-first, columns faithful to what was filed
    assert_eq!(rows[0].id, id1);
    assert_eq!(rows[0].filename, "a.jpg");
    assert_eq!(rows[0].path, "Uploads/verifyinfo/x1.jpg");
    assert_eq!(rows[1].id, id2);
}

#[tokio::test]
async fn list_for_user_is_scoped_to_uid() {
    let Some(s) = suite().await else { return };
    let mine = uid(BASE);
    let other = uid(BASE);
    seed_member(&s, mine, 0).await;
    seed_member(&s, other, 0).await;

    attachment::add(&s.db, mine, "mine.jpg", "Uploads/verifyinfo/m.jpg")
        .await
        .unwrap();
    attachment::add(&s.db, other, "theirs.jpg", "Uploads/verifyinfo/t.jpg")
        .await
        .unwrap();

    let rows = attachment::list_for_user(&s.db, mine).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].filename, "mine.jpg");
    assert!(rows.iter().all(|r| r.userid == mine));
}

#[tokio::test]
async fn submit_certification_sets_authorized_2_db() {
    let Some(s) = suite().await else { return };
    for start in [0i32, 1] {
        let user = uid(BASE);
        seed_member(&s, user, start).await;
        let rows = attachment::submit_certification(&s.db, user).await.unwrap();
        assert_eq!(rows, 1, "one member row is targeted");
        let after = attachment::get_authorized(&s.db, user).await.unwrap();
        assert_eq!(after, 2, "{start} -> 2 (待审核), faithfully unconditional");
    }
}

#[tokio::test]
async fn submit_certification_idempotent_when_already_pending() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, 2).await;
    // a re-submit against an already-pending member still affects 1 row and
    // leaves the state at 2 (Postgres same-value update semantics).
    let rows = attachment::submit_certification(&s.db, user).await.unwrap();
    assert_eq!(rows, 1);
    assert_eq!(attachment::get_authorized(&s.db, user).await.unwrap(), 2);
}

#[tokio::test]
async fn get_authorized_reads_current_state_db() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, 1).await;
    assert_eq!(attachment::get_authorized(&s.db, user).await.unwrap(), 1);
    // a missing member defaults to 0 (defensive; the session gates existence)
    assert_eq!(
        attachment::get_authorized(&s.db, uid(BASE)).await.unwrap(),
        0
    );
}
