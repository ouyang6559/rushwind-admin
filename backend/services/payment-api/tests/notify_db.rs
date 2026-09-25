//! DB-gated merchant-notify integration tests (`spec/02` §4.6 / §2.1 1→2):
//! the outbound form POST against a loopback mock merchant, the ack
//! criterion on the reply, and the notify-acked CAS closing the order.
//! Runs only with `PAYMENT_TEST_DATABASE_URL` set; shares the harness in
//! `tests/common` (base 9.5e12 keeps ids disjoint from the other binaries).

mod common;

use sea_orm::{ActiveModelTrait, Set};

use common::{mock_merchant, order_row, seed_world, settled_order, suite, uid};
use payment_api::data::orders;
use payment_api::gateway::notify::NotifyOutcome;

const BASE: i64 = 9_500_000_000_000;

#[tokio::test]
async fn ok_reply_delivers_and_closes_the_order() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let (url, req_handle) = mock_merchant("ok");
    let oid = format!("N{}", uid(BASE));
    settled_order(&s, user, channel, &url, &oid).await;

    let outcome = s.notifier.notify_order(&oid).await.unwrap();
    assert!(
        matches!(
            outcome,
            NotifyOutcome::Replied {
                acked: true,
                status: 200
            }
        ),
        "{outcome:?}"
    );
    // The 1 -> 2 close fired on the "ok" reply.
    assert_eq!(order_row(&s.db, &oid).await.status, 2);

    // The wire payload is the legacy curl --data set, urlencoded.
    let raw = req_handle.join().unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    for field in [
        "memberid=",
        &format!("orderid={oid}"),
        &format!("transaction_id={oid}"),
        "amount=100.00",
        "datetime=",
        "returncode=00",
        "sign=",
        "attach=meta",
    ] {
        assert!(body.contains(field), "missing {field:?} in {body:?}");
    }
}

#[tokio::test]
async fn no_ok_reply_leaves_the_order_paid_for_the_next_sweep() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let (url, req_handle) = mock_merchant("no");
    let oid = format!("N{}", uid(BASE));
    settled_order(&s, user, channel, &url, &oid).await;

    let outcome = s.notifier.notify_order(&oid).await.unwrap();
    assert!(
        matches!(
            outcome,
            NotifyOutcome::Replied {
                acked: false,
                status: 200
            }
        ),
        "{outcome:?}"
    );
    drop(req_handle);
    // Still 1 — reissue sweeps will pick it up (§7.1).
    assert_eq!(order_row(&s.db, &oid).await.status, 1);
}

#[tokio::test]
async fn unreachable_merchant_and_empty_urls_skip_cleanly() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;

    // A port nothing listens on: failure surfaced, order untouched.
    let oid = format!("N{}", uid(BASE));
    settled_order(&s, user, channel, "http://127.0.0.1:1/notify", &oid).await;
    let outcome = s.notifier.notify_order(&oid).await.unwrap();
    assert!(
        matches!(outcome, NotifyOutcome::Unreachable(_)),
        "{outcome:?}"
    );
    assert_eq!(order_row(&s.db, &oid).await.status, 1);

    // Empty notify_url: skipped, nothing sent.
    let mut am: orders::ActiveModel = order_row(&s.db, &oid).await.into();
    am.notify_url = Set(String::new());
    am.update(&*s.db).await.unwrap();
    let outcome = s.notifier.notify_order(&oid).await.unwrap();
    assert_eq!(outcome, NotifyOutcome::Skipped("empty notify_url"));
}

/// The §4.6 audit trail: every ATTEMPT (delivered or unreachable) lands a
/// `notify_logs` row — the legacy `log_server_notify` file line, made
/// queryable. Skips (nothing sent) write nothing.
#[tokio::test]
async fn every_attempt_lands_in_the_notify_log() {
    use payment_api::data::notify_logs;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };

    // A delivered attempt: 200 reply "ok" → acked row with the payload.
    let (user, channel) = (uid(BASE), uid(BASE));
    seed_world(&s, user, channel).await;
    let (url, req_handle) = mock_merchant("ok");
    let oid = format!("N{}", uid(BASE));
    settled_order(&s, user, channel, &url, &oid).await;
    s.notifier.notify_order(&oid).await.unwrap();
    drop(req_handle);

    let rows = notify_logs::Entity::find()
        .filter(notify_logs::Column::OrderId.eq(&oid))
        .all(&*s.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.notify_url, url);
    assert_eq!(row.http_code, 200);
    assert_eq!(row.acked, 1);
    // The audited payload is the same six-field signed body (unencoded).
    for field in [
        "memberid=",
        &format!("orderid={oid}"),
        "amount=100.00",
        "sign=",
    ] {
        assert!(row.notify_str.contains(field), "missing {field:?}");
    }

    // A transport failure still writes its row, httpCode 0, not acked.
    let oid2 = format!("N{}", uid(BASE));
    settled_order(&s, user, channel, "http://127.0.0.1:1/notify", &oid2).await;
    s.notifier.notify_order(&oid2).await.unwrap();
    let rows2 = notify_logs::Entity::find()
        .filter(notify_logs::Column::OrderId.eq(&oid2))
        .all(&*s.db)
        .await
        .unwrap();
    assert_eq!(rows2.len(), 1);
    assert_eq!(rows2[0].http_code, 0);
    assert_eq!(rows2[0].acked, 0);
    assert!(!rows2[0].contents.is_empty());

    // A skip (empty URL) sends nothing and logs nothing.
    let oid3 = format!("N{}", uid(BASE));
    settled_order(&s, user, channel, "", &oid3).await;
    let outcome = s.notifier.notify_order(&oid3).await.unwrap();
    assert!(matches!(outcome, NotifyOutcome::Skipped(_)));
    let rows3 = notify_logs::Entity::find()
        .filter(notify_logs::Column::OrderId.eq(&oid3))
        .all(&*s.db)
        .await
        .unwrap();
    assert!(rows3.is_empty());
}
