//! DB-gated tests for the §3.1 registration write path
//! (`merchant::register::register_member`) focused on the SITE-SWITCH-driven
//! persisted defaults — the piece the invite-lifecycle file
//! (`invite_code.rs`) does not assert. `invite_code.rs` already pins the
//! invite consumption (groupid/parentid off the code), the invalid-invite
//! rejection, the platform-parented default and the duplicate-username gate;
//! here we drive the same kernel to prove the `register_need_activate` /
//! `authorized` websiteconfig switches land on the persisted `status` /
//! `authorized` columns, plus the seeded secrets (apikey / pay_password).
//! The pure errorno/message mapping is pinned offline in the module's own
//! tests. Same harness as `invite_code.rs`; ids base 150_000_000_000_000.
#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::register::{self, OpenChildInput, RegisterInput, SiteFlags};

const BASE: i64 = 150_000_000_000_000;

async fn member_by_username(s: &Suite, name: &str) -> Option<members::Model> {
    members::Entity::find()
        .filter(members::Column::Username.eq(name))
        .one(&*s.db)
        .await
        .unwrap()
}

fn input<'a>(username: &'a str, email: &'a str) -> RegisterInput<'a> {
    RegisterInput {
        username,
        password: "pw1234",
        confirm_password: "pw1234",
        email,
        invite_code: "",
    }
}

/// With every websiteconfig switch off (the DDL defaults), a plain merchant is
/// created enabled and authorized, platform-parented, carrying the seeded
/// sign/pay secrets — the columns `invite_code.rs` never asserts on.
#[tokio::test]
async fn default_switches_persist_an_enabled_authorized_merchant() {
    let Some(s) = suite().await else { return };
    let username = format!("regx{}", uid(BASE));

    register::register_member(
        &s.db,
        &input(&username, "reg@example.com"),
        &SiteFlags::default(),
    )
    .await
    .unwrap()
    .expect("clean register with switches off");

    let member = member_by_username(&s, &username)
        .await
        .expect("member created");
    assert_eq!(member.groupid, 4, "default registrant is a merchant");
    assert_eq!(member.parentid, 1, "platform-parented without an invite");
    assert_eq!(member.status, 1, "activation off → enabled");
    assert_eq!(member.authorized, 1, "KYC off → authorized");
    assert_eq!(member.email.as_deref(), Some("reg@example.com"));
    let apikey = member.apikey.expect("seeded api key");
    assert_eq!(apikey.len(), 32, "32-hex sign secret");
    assert!(member.pay_password.is_some(), "default pay password seeded");
}

/// Turning on the activation + KYC switches must persist the member pending:
/// `status = 0` (awaits the email-activation seam) and `authorized = 0`
/// (awaits KYC), while still defaulting to a platform-parented merchant.
#[tokio::test]
async fn activation_and_kyc_switches_persist_a_pending_member() {
    let Some(s) = suite().await else { return };
    let username = format!("regx{}", uid(BASE));
    let flags = SiteFlags {
        invitecode: false,
        authorized: true,
        register_need_activate: true,
        ..Default::default()
    };

    register::register_member(&s.db, &input(&username, "pending@example.com"), &flags)
        .await
        .unwrap()
        .expect("no invite required");

    let member = member_by_username(&s, &username)
        .await
        .expect("member created");
    assert_eq!(
        member.status, 0,
        "activation required → disabled until email"
    );
    assert_eq!(member.authorized, 0, "KYC required → not authorized");
    assert_eq!(member.groupid, 4);
    assert_eq!(member.parentid, 1);
}

/// The §3.4 activation link round-trip: a pending registrant carries an
/// `activate` token that, when consumed by [`register::activate_member`],
/// flips `status 0 → 1`; a re-click is idempotent (`AlreadyActive`) and an
/// unknown / empty token is rejected (`InvalidToken`).
#[tokio::test]
async fn activate_link_flips_a_pending_member_once() {
    let Some(s) = suite().await else { return };
    let username = format!("regx{}", uid(BASE));
    let flags = SiteFlags {
        register_need_activate: true,
        data_auth_key: "test-key".to_string(),
        ..Default::default()
    };

    register::register_member(&s.db, &input(&username, "act@example.com"), &flags)
        .await
        .unwrap()
        .expect("pending register");
    let token = member_by_username(&s, &username)
        .await
        .expect("member created")
        .activate
        .expect("activation token seeded");

    let activated = register::activate_member(&s.db, &token).await.unwrap();
    assert_eq!(activated, register::ActivateOutcome::Activated);
    assert_eq!(
        member_by_username(&s, &username).await.unwrap().status,
        1,
        "status flipped to enabled"
    );

    let again = register::activate_member(&s.db, &token).await.unwrap();
    assert_eq!(again, register::ActivateOutcome::AlreadyActive);
    let bad = register::activate_member(&s.db, "not-a-real-token")
        .await
        .unwrap();
    assert_eq!(bad, register::ActivateOutcome::InvalidToken);
    let empty = register::activate_member(&s.db, "").await.unwrap();
    assert_eq!(empty, register::ActivateOutcome::InvalidToken);
}

async fn member_count_by_email(s: &Suite, email: &str) -> usize {
    members::Entity::find()
        .filter(members::Column::Email.eq(email))
        .all(&*s.db)
        .await
        .unwrap()
        .len()
}

/// §6.1 代理开商户: the kernel always files a `groupid = 4` merchant parented
/// to the acting agent, enabled/authorized under the default switches, and
/// supplies a password (explicit or the random fallback) hashed for login.
#[tokio::test]
async fn agent_open_files_a_merchant_parented_to_the_agent() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let username = format!("regx{}", uid(BASE));
    let email = format!("{}@open.example", uid(BASE));
    let flags = SiteFlags::default();
    let child = OpenChildInput {
        username: &username,
        email: &email,
        password: "agentsetpw",
    };

    let new_uid = register::open_downline_merchant(&s.db, agent, &child, &flags)
        .await
        .unwrap()
        .expect("clean open");

    let member = member_by_username(&s, &username)
        .await
        .expect("downline created");
    assert_eq!(member.id, new_uid);
    assert_eq!(member.groupid, 4, "always a merchant under the agent");
    assert_eq!(member.parentid, agent, "parented to the acting agent");
    assert_eq!(member.status, 1);
    assert_eq!(member.authorized, 1);
    assert!(!member.password.is_empty(), "login hash seeded");

    // An empty password still files a member (the random fallback).
    let username2 = format!("regx{}", uid(BASE));
    let email2 = format!("{}@open2.example", uid(BASE));
    let child2 = OpenChildInput {
        username: &username2,
        email: &email2,
        password: "",
    };
    register::open_downline_merchant(&s.db, agent, &child2, &flags)
        .await
        .unwrap()
        .expect("random-password open");
    assert!(member_by_username(&s, &username2).await.is_some());
}

/// The duplicate gate rejects a taken username first, then a taken email, and
/// writes no row on either path.
#[tokio::test]
async fn agent_open_rejects_a_duplicate_username_then_email() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let flags = SiteFlags::default();
    let username = format!("regx{}", uid(BASE));
    let email = format!("{}@dup.example", uid(BASE));

    let first = OpenChildInput {
        username: &username,
        email: &email,
        password: "pw",
    };
    register::open_downline_merchant(&s.db, agent, &first, &flags)
        .await
        .unwrap()
        .expect("first open");

    // Same username, different email → 用户名已存在.
    let other_email = format!("{}@dup2.example", uid(BASE));
    let dup_user = OpenChildInput {
        username: &username,
        email: &other_email,
        password: "pw",
    };
    let res = register::open_downline_merchant(&s.db, agent, &dup_user, &flags)
        .await
        .unwrap();
    assert!(matches!(res, Err(register::OpenError::UsernameTaken)));

    // Different username, same email → 邮箱已存在.
    let other_user = format!("regx{}", uid(BASE));
    let dup_email = OpenChildInput {
        username: &other_user,
        email: &email,
        password: "pw",
    };
    let res = register::open_downline_merchant(&s.db, agent, &dup_email, &flags)
        .await
        .unwrap();
    assert!(matches!(res, Err(register::OpenError::EmailTaken)));
    assert_eq!(
        member_count_by_email(&s, &email).await,
        1,
        "no second row written for the rejected email"
    );
}
