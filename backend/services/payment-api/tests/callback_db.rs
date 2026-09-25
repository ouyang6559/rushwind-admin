//! Sync cashier-return integration (`spec/02` §4.2 step 4 / §4.6). Needs
//! a real Postgres; runs alone:
//!
//! ```sh
//! docker start juhepay-pg
//! cargo test -p payment-api --test callback_db
//! ```
#![allow(dead_code)]

use payment_api::gateway::callback::{callback_core, Callback};

mod common;
use common::{
    create_only, order_row, seed_world, settled_order, settled_order_with_return, suite, uid,
};

const BASE: i64 = 10_100_000_000_000;

#[tokio::test]
async fn unpaid_or_unknown_orders_answer_error() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;

    // Unknown order id — the legacy `getField` on nothing, same text.
    let missing = format!("C{}", uid(BASE));
    assert_eq!(
        callback_core(&s.db, &missing).await.unwrap(),
        Callback::Error
    );

    // Created but never settled: this route never credits (§4.2 step 4) —
    // the async notify + sweep remain the only crediting paths.
    let oid = format!("C{}", uid(BASE));
    create_only(&s, user, ch, "", "https://m.test/return", &oid).await;
    assert_eq!(callback_core(&s.db, &oid).await.unwrap(), Callback::Error);
    assert_eq!(order_row(&s.db, &oid).await.status, 0);
}

#[tokio::test]
async fn settled_order_renders_the_autosubmit_form() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;
    let oid = format!("C{}", uid(BASE));
    settled_order_with_return(&s, user, ch, "", "https://m.test/return?a=1&b=2", &oid).await;

    let Callback::Form { html } = callback_core(&s.db, &oid).await.unwrap() else {
        panic!("expected the auto-submit form");
    };
    // The setHtml shell, action url escaped (& → &amp; — the hardening).
    assert!(html.contains(
        "<form id=\"Form1\" name=\"Form1\" method=\"post\" action=\"https://m.test/return?a=1&amp;b=2\">"
    ));
    assert!(html.ends_with("</form><script>document.Form1.submit();</script>"));
    // The same signed message as the async notify (§4.6's shared
    // return_array): six fields + sign + unsigned attach.
    for name in [
        "memberid",
        "orderid",
        "transaction_id",
        "amount",
        "datetime",
        "returncode",
        "sign",
        "attach",
    ] {
        assert!(html.contains(&format!("name=\"{name}\"")), "missing {name}");
    }
    assert!(html.contains("value=\"100.00\""));
    assert!(html.contains("value=\"meta\""));

    // No side effects: still 1 (never 2 from this route), num untouched.
    let row = order_row(&s.db, &oid).await;
    assert_eq!(row.status, 1);
    assert_eq!(row.num, 0);
}

#[tokio::test]
async fn settled_without_a_stored_page_url_falls_back_to_text() {
    let Some(s) = suite().await else {
        eprintln!("PAYMENT_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let (user, ch) = (uid(BASE), uid(BASE));
    seed_world(&s, user, ch).await;
    let oid = format!("C{}", uid(BASE));
    // settled_order stores an empty callback_url (the notify-path shape).
    settled_order(&s, user, ch, "", &oid).await;
    assert_eq!(
        callback_core(&s.db, &oid).await.unwrap(),
        Callback::SuccessText
    );
}
