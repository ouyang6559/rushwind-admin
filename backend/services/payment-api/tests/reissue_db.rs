//! DB-gated reissue-sweep integration tests (`spec/02` §7.1 `postUrl`):
//! the due-set scan (status 1, num < postnum, gap >= 10s), the CAS claim
//! that spends an attempt, and the notify re-post per winner. Runs only
//! with `PAYMENT_TEST_DATABASE_URL` set; harness shared via `tests/common`
//! (base 9.7e12 keeps ids disjoint from the other binaries).
//!
//! The sweep is a GLOBAL scan, and the shared DB keeps `status = 1`
//! leftovers from the earlier notify/ledger binaries — so every assertion
//! is directional (`targets contains ours`), and one in-process mutex
//! serialises the cases so no parallel case sweeps another's orders.

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{mock_merchant, order_row, seed_world, settled_order, suite, uid};
use payment_api::data::orders;
use payment_api::gateway::reissue::{run_sweep, SweepPolicy};

const BASE: i64 = 9_700_000_000_000;

static SERIALIZED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn alone() -> tokio::sync::MutexGuard<'static, ()> {
    SERIALIZED.lock().await
}

fn now() -> i64 {
    chrono::Local::now().timestamp()
}

/// The legacy policy with a batch wide enough that residual due orders
/// from the shared library (or sibling test binaries) can never crowd
/// this binary's own order out of the `id asc limit N` window — the
/// assertions stay targeted AND deterministic.
fn sweep_policy() -> SweepPolicy {
    SweepPolicy {
        batch: 100_000,
        ..SweepPolicy::default()
    }
}

const DUE_LIMIT: u64 = 100_000;

async fn set_num(db: &sea_orm::DatabaseConnection, oid: &str, num: i32) {
    let mut am: orders::ActiveModel = order_row(db, oid).await.into();
    am.num = Set(num);
    am.update(db).await.unwrap();
}

#[tokio::test]
async fn sweep_resends_and_spends_one_attempt() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let (url, req_handle) = mock_merchant("ok");
    let oid = format!("R{}", uid(BASE));
    // Freshly settled at status 1, num = 0, last_reissue_time = 0 — due.
    settled_order(&s, user, channel, &url, &oid).await;

    let report = run_sweep(&s.ledger, &s.notifier, &sweep_policy(), now())
        .await
        .unwrap();
    assert!(report.targets.contains(&oid), "{:?}", report.targets);
    let _ = req_handle.join().unwrap();

    // The attempt was consumed and the window timestamp written; the ok
    // reply additionally closed the order (1 -> 2), taking it out of the
    // sweep set for good.
    let row = order_row(&s.db, &oid).await;
    assert_eq!(row.num, 1);
    assert!(row.last_reissue_time >= now() - 1);
    assert_eq!(row.status, 2);
}

#[tokio::test]
async fn unacked_repost_stays_paid_for_the_next_window() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    // Dead port: the claim is spent, nothing is confirmed.
    let oid = format!("R{}", uid(BASE));
    settled_order(&s, user, channel, "http://127.0.0.1:1/notify", &oid).await;

    let report = run_sweep(&s.ledger, &s.notifier, &sweep_policy(), now())
        .await
        .unwrap();
    assert!(report.targets.contains(&oid), "{:?}", report.targets);

    // Still at 1 with an attempt spent — the next window re-lists it
    // until postnum is exhausted.
    let row = order_row(&s.db, &oid).await;
    assert_eq!(row.status, 1);
    assert!(row.num >= 1);
}

#[tokio::test]
async fn gate_blocks_exhausted_and_recent_orders() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;

    // Exhausted: num at the postnum cap.
    let tired = format!("R{}", uid(BASE));
    settled_order(&s, user, channel, "http://127.0.0.1:1/notify", &tired).await;
    set_num(&s.db, &tired, SweepPolicy::default().max_attempts).await;
    let due = s.ledger.due_reissues(5, now(), DUE_LIMIT).await.unwrap();
    assert!(!due.contains(&tired), "{due:?}");

    // Recent: a fresh claim moves the timestamp past the 10s window, so
    // the next sweep inside the gap finds nothing.
    let fresh = format!("R{}", uid(BASE));
    settled_order(&s, user, channel, "http://127.0.0.1:1/notify", &fresh).await;
    assert!(s.ledger.claim_reissue(&fresh, 5, now()).await.unwrap());
    let due = s.ledger.due_reissues(5, now(), DUE_LIMIT).await.unwrap();
    assert!(!due.contains(&fresh), "{due:?}");

    // A full sweep past them both, too.
    let report = run_sweep(&s.ledger, &s.notifier, &sweep_policy(), now())
        .await
        .unwrap();
    assert!(!report.targets.contains(&tired), "{:?}", report.targets);
    assert!(!report.targets.contains(&fresh), "{:?}", report.targets);
}

#[tokio::test]
async fn claim_cas_is_exclusive_within_the_window() {
    let _guard = alone().await;
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let oid = format!("R{}", uid(BASE));
    settled_order(&s, user, channel, "http://127.0.0.1:1/notify", &oid).await;

    // First claim wins; an immediate second one loses on the gap — the
    // hardening that stops two crons from double-POSTing (§7 并发缺口).
    assert!(s.ledger.claim_reissue(&oid, 5, now()).await.unwrap());
    assert!(!s.ledger.claim_reissue(&oid, 5, now()).await.unwrap());
    // Only one attempt was consumed by the race.
    assert_eq!(order_row(&s.db, &oid).await.num, 1);
}
