//! Manual repost (`bufa`, `spec/03` §9.2) integration. Needs a real
//! Postgres; runs alone:
//!
//! ```sh
//! docker start juhepay-pg
//! cargo test -p payment-api --test bufa_db
//! ```
#![allow(dead_code)]

mod common;
use common::{create_only, mock_merchant, order_row, seed_world, settled_order, suite, uid};
use payment_api::gateway::notify::NotifyOutcome;
use payment_api::gateway::reissue::{bufa_admit, bufa_text};

const BASE: i64 = 10_500_000_000_000;

#[tokio::test]
async fn gate_admits_only_status_one() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;

    // Unknown id — the legacy getField on nothing, intval(false) == 0.
    assert!(!bufa_admit(&s.ledger, &format!("B{}", uid(BASE)))
        .await
        .unwrap());

    // Unpaid (0) is not repostable.
    let fresh = format!("B{}", uid(BASE));
    create_only(&s, user, ch, "http://127.0.0.1:1/notify", "", &fresh).await;
    assert!(!bufa_admit(&s.ledger, &fresh).await.unwrap());

    // Settled (1) passes the gate.
    let paid = format!("B{}", uid(BASE));
    settled_order(&s, user, ch, "http://127.0.0.1:1/notify", &paid).await;
    assert!(bufa_admit(&s.ledger, &paid).await.unwrap());

    // Already returned (2) leaves the set, too.
    s.ledger.mark_order_notified(&paid).await.unwrap();
    assert!(!bufa_admit(&s.ledger, &paid).await.unwrap());
}

#[tokio::test]
async fn repost_delivers_without_spending_an_attempt() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;
    let (url, req_handle) = mock_merchant("ok");
    let oid = format!("B{}", uid(BASE));
    settled_order(&s, user, ch, &url, &oid).await;
    assert!(bufa_admit(&s.ledger, &oid).await.unwrap());

    // The handler spawns this exact call after echoing bufa_text.
    let outcome = s.notifier.notify_order(&oid).await.unwrap();
    assert!(matches!(
        outcome,
        NotifyOutcome::Replied {
            acked: true,
            status: 200
        }
    ));
    let req = String::from_utf8(req_handle.join().unwrap()).unwrap();
    assert!(req.contains("orderid="), "{req}");

    // Closed by the ok reply — and `num` still 0: bufa never spends the
    // sweep's attempt budget (legacy P:702-717 had no counter write).
    let row = order_row(&s.db, &oid).await;
    assert_eq!(row.status, 2);
    assert_eq!(row.num, 0);

    // The success line keeps the operator-facing legacy wording.
    assert!(bufa_text(&oid, "WxSm").contains("已补发服务器点对点通知"));
}
