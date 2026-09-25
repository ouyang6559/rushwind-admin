//! DB-gated coverage for the §10 merchant profile / bank-card service layer.
//! Both write paths are gated upstream by an SMS / Google second factor the Rust
//! side has not wired, so (like `apikey` view but stronger) there is NO HTTP
//! surface yet — these tests drive the service primitives directly:
//!
//! - [`profile::apply_profile`] persists ONLY the whitelisted columns, ignores
//!   foreign keys (`balance` / `parentid`), and reproduces the discarded-
//!   `parentid` quirk (an `agentname` swap never changes the parent), and
//!   [`profile::plan_profile`]'s "代理商不存在" gate rejects before any write.
//! - the [`bankcard`] primitives are strictly owner-scoped `(id, userid)` and
//!   `set_default` keeps at most one default per merchant.
//!
//! Harness mirrors `panel_login_session.rs`; ids base 30_000_000_000_000.

#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, EntityTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::{bankcards, members};
use payment_api::merchant::bankcard::{self, BankcardForm};
use payment_api::merchant::{profile, MembersRepo};

const BASE: i64 = 30_000_000_000_000;

/// Seeds a plain merchant (groupid 4, `parentid = 1` platform boundary).
async fn seed_member(s: &Suite, user: i64, username: &str) {
    members::ActiveModel {
        id: Set(user),
        username: Set(username.to_string()),
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

fn post<'a>(pairs: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, String)> {
    pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
}

async fn member(s: &Suite, user: i64) -> members::Model {
    MembersRepo::new(&s.db).by_id(user).await.unwrap().unwrap()
}

// --- profile (saveProfile) --------------------------------------------------

#[tokio::test]
async fn apply_profile_writes_only_whitelisted_columns() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, &format!("pr{user}")).await;

    let u = profile::plan_profile(
        &post(&[
            ("realname", "张三"),
            ("sex", "2"),
            ("birthday", "2000-01-02 03:04:05"),
            ("df_api", "1"),
            ("login_ip", "1.2.3.4"),
            // foreign keys the whitelist must strip:
            ("balance", "999999"),
            ("parentid", "7"),
            ("username", "hacked"),
        ]),
        |_| true,
    )
    .unwrap();
    assert!(profile::apply_profile(&s.db, user, &u).await.unwrap());

    let m = member(&s, user).await;
    assert_eq!(m.realname.as_deref(), Some("张三"));
    assert_eq!(m.sex, Some(2));
    assert!(m.birthday.unwrap() > 0);
    assert_eq!(m.df_api, 1);
    assert_eq!(m.login_ip.as_deref(), Some("1.2.3.4"));
    // untouched, because they are not in the whitelist:
    assert_eq!(m.parentid, 1, "parentid must not be written");
    assert_eq!(m.balance, 0, "balance must not be written");
    assert_eq!(
        m.username,
        format!("pr{user}"),
        "username must not be written"
    );
}

#[tokio::test]
async fn an_agentname_swap_never_persists_the_parent() {
    let Some(s) = suite().await else { return };
    // A merchant parented to the platform (1); the agent lookup is a synthetic
    // closure below, so no real agent row is needed.
    let user = uid(BASE);
    seed_member(&s, user, &format!("rp{user}")).await;

    let u = profile::plan_profile(
        &post(&[("agentname", "topagent"), ("realname", "李四")]),
        // the agent DOES exist → the gate passes...
        |name| name == "topagent",
    )
    .unwrap();
    profile::apply_profile(&s.db, user, &u).await.unwrap();

    // ...yet the parent is unchanged (the reassignment is discarded upstream).
    let m = member(&s, user).await;
    assert_eq!(m.parentid, 1);
    assert_eq!(m.realname.as_deref(), Some("李四"));
}

#[tokio::test]
async fn an_unknown_agentname_rejects_before_any_write() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, &format!("ua{user}")).await;

    let err = profile::plan_profile(
        &post(&[("agentname", "ghost"), ("realname", "王五")]),
        |_| false,
    )
    .unwrap_err();
    assert_eq!(err, profile::MSG_AGENT_NOT_FOUND);
    // nothing was applied — the member still has no realname.
    assert!(member(&s, user).await.realname.is_none());
}

#[tokio::test]
async fn apply_profile_is_scoped_to_an_existing_member() {
    let Some(s) = suite().await else { return };
    let missing = uid(BASE);
    let u = profile::plan_profile(&post(&[("realname", "nobody")]), |_| true).unwrap();
    assert!(!profile::apply_profile(&s.db, missing, &u).await.unwrap());
}

#[tokio::test]
async fn an_empty_plan_changes_nothing() {
    let Some(s) = suite().await else { return };
    let user = uid(BASE);
    seed_member(&s, user, &format!("em{user}")).await;
    // only a non-whitelisted key → empty plan.
    let u = profile::plan_profile(&post(&[("id", "5")]), |_| true).unwrap();
    assert!(u.is_empty());
    assert!(profile::apply_profile(&s.db, user, &u).await.unwrap());
    assert!(member(&s, user).await.realname.is_none());
}

// --- bank cards (addBankcard / editBankStatus / delBankcard) ----------------

fn form<'a>(bank: &'a str, number: &'a str) -> BankcardForm<'a> {
    BankcardForm {
        bankname: bank,
        subbranch: "某支行",
        accountname: "持卡人",
        cardnumber: number,
        province: "广东",
        city: "深圳",
        alias: "",
    }
}

async fn owned_card_ids(s: &Suite, user: i64) -> Vec<i64> {
    bankcard::list_for_user(&s.db, user)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect()
}

#[tokio::test]
async fn upsert_inserts_then_updates_and_rejects_a_foreign_owner() {
    let Some(s) = suite().await else { return };
    let owner = uid(BASE);
    let other = uid(BASE);
    seed_member(&s, owner, &format!("bo{owner}")).await;
    seed_member(&s, other, &format!("bo{other}")).await;

    assert_eq!(
        bankcard::upsert_card(&s.db, None, owner, &form("ICBC", "6222"), 100)
            .await
            .unwrap(),
        1
    );
    let ids = owned_card_ids(&s, owner).await;
    assert_eq!(ids.len(), 1);
    let card = ids[0];

    // Owner update lands.
    assert_eq!(
        bankcard::upsert_card(&s.db, Some(card), owner, &form("CMB", "6225"), 200)
            .await
            .unwrap(),
        1
    );
    let saved = bankcards::Entity::find_by_id(card)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bankname.as_deref(), Some("CMB"));
    assert_eq!(saved.updatetime, 200);

    // The SAME id under another owner affects 0 rows and stays put.
    assert_eq!(
        bankcard::upsert_card(&s.db, Some(card), other, &form("EVIL", "0000"), 300)
            .await
            .unwrap(),
        0
    );
    let saved = bankcards::Entity::find_by_id(card)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.bankname.as_deref(), Some("CMB"));
    assert_eq!(owned_card_ids(&s, other).await.len(), 0);
}

#[tokio::test]
async fn set_default_keeps_at_most_one_default_per_merchant() {
    let Some(s) = suite().await else { return };
    let owner = uid(BASE);
    seed_member(&s, owner, &format!("sd{owner}")).await;
    bankcard::upsert_card(&s.db, None, owner, &form("A", "1"), 1)
        .await
        .unwrap();
    bankcard::upsert_card(&s.db, None, owner, &form("B", "2"), 1)
        .await
        .unwrap();
    let ids = owned_card_ids(&s, owner).await;
    let (first, second) = (ids[0], ids[1]);

    bankcard::set_default(&s.db, first, owner, 1, 10)
        .await
        .unwrap();
    assert_eq!(
        bankcards::Entity::find_by_id(first)
            .one(&*s.db)
            .await
            .unwrap()
            .unwrap()
            .isdefault,
        1
    );

    // Promoting the second card demotes the first.
    bankcard::set_default(&s.db, second, owner, 1, 20)
        .await
        .unwrap();
    assert_eq!(
        bankcards::Entity::find_by_id(first)
            .one(&*s.db)
            .await
            .unwrap()
            .unwrap()
            .isdefault,
        0
    );
    assert_eq!(
        bankcards::Entity::find_by_id(second)
            .one(&*s.db)
            .await
            .unwrap()
            .unwrap()
            .isdefault,
        1
    );
}

#[tokio::test]
async fn set_default_is_owner_scoped() {
    let Some(s) = suite().await else { return };
    let owner = uid(BASE);
    let other = uid(BASE);
    seed_member(&s, owner, &format!("o{owner}")).await;
    seed_member(&s, other, &format!("o{other}")).await;
    bankcard::upsert_card(&s.db, None, owner, &form("A", "1"), 1)
        .await
        .unwrap();
    let card = owned_card_ids(&s, owner).await[0];

    // Another merchant cannot default someone else's card.
    assert_eq!(
        bankcard::set_default(&s.db, card, other, 1, 5)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        bankcards::Entity::find_by_id(card)
            .one(&*s.db)
            .await
            .unwrap()
            .unwrap()
            .isdefault,
        0
    );
}

#[tokio::test]
async fn delete_is_owner_scoped() {
    let Some(s) = suite().await else { return };
    let owner = uid(BASE);
    let other = uid(BASE);
    seed_member(&s, owner, &format!("d{owner}")).await;
    seed_member(&s, other, &format!("d{other}")).await;
    bankcard::upsert_card(&s.db, None, owner, &form("A", "1"), 1)
        .await
        .unwrap();
    let card = owned_card_ids(&s, owner).await[0];

    assert_eq!(bankcard::delete_card(&s.db, card, other).await.unwrap(), 0);
    assert_eq!(owned_card_ids(&s, owner).await.len(), 1, "still there");
    assert_eq!(bankcard::delete_card(&s.db, card, owner).await.unwrap(), 1);
    assert_eq!(owned_card_ids(&s, owner).await.len(), 0);
}
