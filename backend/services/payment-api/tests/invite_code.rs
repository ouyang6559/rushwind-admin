//! DB-gated tests for the §6.3 agent invite-code lifecycle
//! (`merchant::invite` + the §3.1 register consumption in
//! `merchant::register::register_member`). The pure state machines
//! (validity / display / tier gate / invite→member mapping) are pinned in the
//! module's offline tests; here we drive the DB services directly: the mint
//! (row shape + the `没有权限` tier abort writing nothing), the register-time
//! usable lookup and by-code consumption, the ownership-scoped delete, and the
//! full register path fixing the new member's groupid/parentid off the invite.
//! Same harness as `panel_agent_rate.rs`; ids base 11_800_000_000_000.

mod common;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use common::{suite, uid, Suite};
use payment_api::data::{invite_codes, members};
use payment_api::merchant::invite::{self, MSG_NO_PERMISSION};
use payment_api::merchant::register::{self, RegisterInput, SiteFlags};

const BASE: i64 = 11_800_000_000_000;

/// Inserts a raw invite row for the read / delete / consume tests (bypassing
/// the mint, so expiry and the parallel state flags can be set explicitly).
#[allow(clippy::too_many_arguments)]
async fn seed_invite(
    s: &Suite,
    code: &str,
    owner: i64,
    regtype: i32,
    yxdatetime: i64,
    status: i32,
    inviteconfigzt: i32,
    is_admin: i32,
) -> i64 {
    let id = uid(BASE);
    invite_codes::ActiveModel {
        id: Set(id),
        invitecode: Set(code.to_string()),
        fmusernameid: Set(owner),
        syusernameid: Set(0),
        regtype: Set(regtype),
        fbdatetime: Set(0),
        yxdatetime: Set(yxdatetime),
        sydatetime: Set(0),
        status: Set(status),
        inviteconfigzt: Set(inviteconfigzt),
        is_admin: Set(is_admin),
    }
    .insert(&*s.db)
    .await
    .unwrap();
    id
}

async fn invite_by_code(s: &Suite, code: &str) -> Option<invite_codes::Model> {
    invite_codes::Entity::find()
        .filter(invite_codes::Column::Invitecode.eq(code))
        .one(&*s.db)
        .await
        .unwrap()
}

async fn member_by_username(s: &Suite, name: &str) -> Option<members::Model> {
    members::Entity::find()
        .filter(members::Column::Username.eq(name))
        .one(&*s.db)
        .await
        .unwrap()
}

async fn count_invite_codes(s: &Suite, owner: i64) -> i64 {
    invite_codes::Entity::find()
        .filter(invite_codes::Column::Fmusernameid.eq(owner))
        .all(&*s.db)
        .await
        .unwrap()
        .len() as i64
}

#[tokio::test]
async fn mint_writes_an_unused_agent_code() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let now = payment_api::data::now_ts();

    let res = invite::create_invite(&s.db, agent, 6, 4, now + 86_400)
        .await
        .unwrap();
    let model = res.expect("lower tier mints cleanly");

    assert_eq!(model.invitecode.len(), invite::CODE_LEN);
    assert!(model
        .invitecode
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    assert_eq!(model.fmusernameid, agent);
    assert_eq!(model.regtype, 4);
    assert_eq!(model.status, 1, "unused lifecycle state");
    assert_eq!(model.inviteconfigzt, 1, "usable display flag");
    assert_eq!(model.is_admin, 0, "agent-minted");
    assert!(model.fbdatetime > 0, "creation stamp set");
}

#[tokio::test]
async fn mint_rejects_a_non_lower_tier_writing_nothing() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let now = payment_api::data::now_ts();

    // regtype 6 == the agent's own group → 没有权限, no row persists.
    let res = invite::create_invite(&s.db, agent, 6, 6, now + 86_400)
        .await
        .unwrap();
    assert!(matches!(res, Err(MSG_NO_PERMISSION)));
    assert_eq!(count_invite_codes(&s, agent).await, 0);
}

#[tokio::test]
async fn find_usable_requires_status_one_and_live_window() {
    let Some(s) = suite().await else { return };
    let now = 1_000_000_000;
    let owner = uid(BASE);
    let live = format!("lv{}", uid(BASE) % 100000);
    let dead = format!("dd{}", uid(BASE) % 100000);
    let used = format!("us{}", uid(BASE) % 100000);
    seed_invite(&s, &live, owner, 4, now + 100, 1, 1, 0).await;
    seed_invite(&s, &dead, owner, 4, now - 100, 1, 1, 0).await;
    seed_invite(&s, &used, owner, 4, now + 100, 2, 2, 0).await;

    assert!(invite::find_usable(&s.db, &live, now)
        .await
        .unwrap()
        .is_some());
    assert!(
        invite::find_usable(&s.db, &dead, now)
            .await
            .unwrap()
            .is_none(),
        "expired"
    );
    assert!(
        invite::find_usable(&s.db, &used, now)
            .await
            .unwrap()
            .is_none(),
        "already used"
    );
    assert!(
        invite::find_usable(&s.db, "zzzz", now)
            .await
            .unwrap()
            .is_none(),
        "absent"
    );
}

#[tokio::test]
async fn consume_matches_by_code_and_clears_usability() {
    let Some(s) = suite().await else { return };
    let now = payment_api::data::now_ts();
    let owner = uid(BASE);
    let registrant = uid(BASE);
    let code = format!("cn{}", uid(BASE) % 100000);
    let id = seed_invite(&s, &code, owner, 4, now + 86_400, 1, 1, 0).await;

    let affected = invite::consume_invite(&s.db, &code, registrant, now + 5)
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let row = invite_by_code(&s, &code).await.unwrap();
    assert_eq!(row.status, 2, "used");
    assert_eq!(row.syusernameid, registrant);
    assert_eq!(row.sydatetime, now + 5);
    assert_eq!(
        row.inviteconfigzt, 1,
        "register never touches the display flag (§6.3 quirk)"
    );
    assert_eq!(row.id, id);
    // A consumed code is no longer register-usable.
    assert!(invite::find_usable(&s.db, &code, now)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn delete_is_scoped_to_the_owner_and_non_admin() {
    let Some(s) = suite().await else { return };
    let now = payment_api::data::now_ts();
    let owner = uid(BASE);
    let other = uid(BASE);
    let mine = format!("om{}", uid(BASE) % 100000);
    let theirs = format!("ot{}", uid(BASE) % 100000);
    let admin = format!("ad{}", uid(BASE) % 100000);
    let mine_id = seed_invite(&s, &mine, owner, 4, now + 100, 1, 1, 0).await;
    seed_invite(&s, &theirs, other, 4, now + 100, 1, 1, 0).await;
    let admin_id = seed_invite(&s, &admin, owner, 4, now + 100, 1, 1, 1).await;

    // A foreign id deletes nothing.
    assert_eq!(
        invite::delete_invite(&s.db, mine_id, other).await.unwrap(),
        0
    );
    // An admin-minted id deletes nothing even for its owner.
    assert_eq!(
        invite::delete_invite(&s.db, admin_id, owner).await.unwrap(),
        0
    );
    // The owner's own non-admin code goes.
    assert_eq!(
        invite::delete_invite(&s.db, mine_id, owner).await.unwrap(),
        1
    );
    assert!(invite_by_code(&s, &mine).await.is_none());
    assert!(
        invite_by_code(&s, &admin).await.is_some(),
        "admin row survives"
    );
}

fn reg_input<'a>(username: &'a str, code: &'a str) -> RegisterInput<'a> {
    RegisterInput {
        username,
        password: "pw",
        confirm_password: "pw",
        email: "a@b.co",
        invite_code: code,
    }
}

#[tokio::test]
async fn register_consumes_invite_and_parents_to_the_agent() {
    let Some(s) = suite().await else { return };
    let now = payment_api::data::now_ts();
    let agent = uid(BASE);
    let code = format!("rg{}", uid(BASE) % 100000);
    seed_invite(&s, &code, agent, 4, now + 86_400, 1, 1, 0).await;

    let username = format!("u{}", uid(BASE));
    let flags = SiteFlags {
        invitecode: true,
        ..Default::default()
    };
    let new_uid = register::register_member(&s.db, &reg_input(&username, &code), &flags)
        .await
        .unwrap()
        .expect("clean register");

    let member = member_by_username(&s, &username)
        .await
        .expect("member created");
    assert_eq!(member.id, new_uid);
    assert_eq!(member.groupid, 4, "groupid = invite.regtype");
    assert_eq!(
        member.parentid, agent,
        "a non-admin invite owner becomes the parent"
    );

    let row = invite_by_code(&s, &code).await.unwrap();
    assert_eq!(row.status, 2, "consumed");
    assert_eq!(row.syusernameid, new_uid);
}

#[tokio::test]
async fn register_rejects_an_invalid_invite_creating_no_member() {
    let Some(s) = suite().await else { return };
    let username = format!("u{}", uid(BASE));
    let flags = SiteFlags {
        invitecode: true,
        ..Default::default()
    };

    let res = register::register_member(&s.db, &reg_input(&username, "nope"), &flags)
        .await
        .unwrap();
    assert!(matches!(res, Err(register::RegisterError::InviteInvalid)));
    assert!(member_by_username(&s, &username).await.is_none());
}

#[tokio::test]
async fn register_without_the_switch_creates_a_platform_merchant() {
    let Some(s) = suite().await else { return };
    let username = format!("u{}", uid(BASE));
    let flags = SiteFlags::default(); // invitecode off

    register::register_member(&s.db, &reg_input(&username, ""), &flags)
        .await
        .unwrap()
        .expect("no invite required");

    let member = member_by_username(&s, &username)
        .await
        .expect("member created");
    assert_eq!(member.groupid, 4);
    assert_eq!(
        member.parentid, 1,
        "platform-parented when no invite applies"
    );
}

#[tokio::test]
async fn register_rejects_a_duplicate_username() {
    let Some(s) = suite().await else { return };
    let username = format!("u{}", uid(BASE));
    let flags = SiteFlags::default();

    register::register_member(&s.db, &reg_input(&username, ""), &flags)
        .await
        .unwrap()
        .expect("first register");

    let res = register::register_member(&s.db, &reg_input(&username, ""), &flags)
        .await
        .unwrap();
    assert!(matches!(res, Err(register::RegisterError::UsernameTaken)));
}
