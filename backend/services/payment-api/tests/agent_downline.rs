//! DB-gated tests for the §6.4 agent downline management (`merchant::downline`)
//! — the 下级会员 list (`User/AgentController::member`), the direct-child status
//! toggle (`editStatus`), and the §6.5 export (`exportuser`) CSV rendering wired
//! over the same list read. Same harness as `panel_register.rs`; ids base
//! 160_000_000_000_000.
#![allow(clippy::unwrap_used)]

mod common;

use sea_orm::{ActiveModelTrait, EntityTrait, Set};

use common::{suite, uid, Suite};
use payment_api::data::members;
use payment_api::merchant::downline::{self, DownlineFilter, StatusOutcome, PAGE_SIZE};
use payment_api::merchant::mch_id_of;

const BASE: i64 = 160_000_000_000_000;

/// Inserts a raw member with explicit parent / group / state columns so the
/// scoping and filter legs can be exercised independently.
#[allow(clippy::too_many_arguments)]
async fn seed_member(
    s: &Suite,
    id: i64,
    parentid: i64,
    groupid: i32,
    status: i32,
    authorized: i32,
    username: &str,
) {
    members::ActiveModel {
        id: Set(id),
        username: Set(username.to_string()),
        password: Set("x".into()),
        groupid: Set(groupid),
        salt: Set(String::new()),
        parentid: Set(parentid),
        balance: Set(0),
        blocked_balance: Set(0),
        status: Set(status),
        authorized: Set(authorized),
        df_api: Set(0),
        ..Default::default()
    }
    .insert(&*s.db)
    .await
    .unwrap();
}

async fn member_by_id(s: &Suite, id: i64) -> members::Model {
    members::Entity::find_by_id(id)
        .one(&*s.db)
        .await
        .unwrap()
        .unwrap()
}

/// The list scopes to the caller's DIRECT children (`parentid = agent`), drops
/// the platform (`groupid = 1`), orders newest id first, and honours the
/// `status` / `authorized` / 商户号 filters.
#[tokio::test]
async fn downline_list_scopes_to_direct_children_and_filters() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let other_agent = uid(BASE);
    let c1 = uid(BASE);
    let c2 = uid(BASE);
    let c3 = uid(BASE);
    let platform = uid(BASE);
    let outsider = uid(BASE);

    let tag = uid(BASE);
    seed_member(&s, agent, 1, 7, 1, 1, &format!("ag{tag}")).await;
    seed_member(&s, other_agent, 1, 7, 1, 1, &format!("ob{tag}")).await;
    seed_member(&s, c1, agent, 4, 1, 1, &format!("dl{tag}a")).await;
    seed_member(&s, c2, agent, 5, 1, 0, &format!("dl{tag}b")).await;
    seed_member(&s, c3, agent, 4, 0, 1, &format!("dl{tag}c")).await;
    // Parented to the agent but the platform account → excluded by groupid.
    seed_member(&s, platform, agent, 1, 1, 1, &format!("pl{tag}")).await;
    // A child of a DIFFERENT agent → never in this caller's scope.
    seed_member(&s, outsider, other_agent, 4, 1, 1, &format!("dl{tag}x")).await;

    let all = DownlineFilter::default();
    assert_eq!(
        downline::count_filtered(&s.db, agent, &all).await.unwrap(),
        3,
        "only the three direct children (no platform, no outsider)"
    );

    let rows = downline::list_page(&s.db, agent, &all, 1, PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].id, c3, "newest id first");
    assert_eq!(rows[2].id, c1);

    // status = 1 → c1, c2 (c3 is disabled).
    let by_status = DownlineFilter {
        status: Some(1),
        ..Default::default()
    };
    assert_eq!(
        downline::count_filtered(&s.db, agent, &by_status)
            .await
            .unwrap(),
        2
    );
    // authorized = 0 → c2 only (the service honours Some(0); the caller's
    // `0`-as-empty collapse is a handler concern).
    let by_auth = DownlineFilter {
        authorized: Some(0),
        ..Default::default()
    };
    assert_eq!(
        downline::count_filtered(&s.db, agent, &by_auth)
            .await
            .unwrap(),
        1
    );
    // A numeric search box is a 商户号 → the exact child.
    let by_mchno = DownlineFilter {
        username: Some(mch_id_of(c2).to_string()),
        ..Default::default()
    };
    assert_eq!(
        downline::count_filtered(&s.db, agent, &by_mchno)
            .await
            .unwrap(),
        1
    );
    // A non-numeric box is a username substring; the shared prefix `dl{tag}`
    // hits all three children (and none of the platform / outsiders).
    let by_like = DownlineFilter {
        username: Some(format!("dl{tag}")),
        ..Default::default()
    };
    assert_eq!(
        downline::count_filtered(&s.db, agent, &by_like)
            .await
            .unwrap(),
        3
    );
}

/// The 启停 toggle persists a new status only for a DIRECT child; a foreign
/// member and a missing id are refused without touching any row.
#[tokio::test]
async fn set_child_status_requires_direct_ownership() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let other_agent = uid(BASE);
    let child = uid(BASE);
    let stranger = uid(BASE);
    let tag = uid(BASE);

    seed_member(&s, agent, 1, 7, 1, 1, &format!("ag{tag}")).await;
    seed_member(&s, other_agent, 1, 7, 1, 1, &format!("ob{tag}")).await;
    seed_member(&s, child, agent, 4, 1, 1, &format!("dl{tag}c")).await;
    seed_member(&s, stranger, other_agent, 4, 1, 1, &format!("dl{tag}s")).await;

    // Own child → disabled, persisted.
    assert_eq!(
        downline::set_child_status(&s.db, agent, child, 0)
            .await
            .unwrap(),
        StatusOutcome::Updated
    );
    assert_eq!(member_by_id(&s, child).await.status, 0);

    // Foreign member → refused, unchanged.
    assert_eq!(
        downline::set_child_status(&s.db, agent, stranger, 0)
            .await
            .unwrap(),
        StatusOutcome::NotOwned
    );
    assert_eq!(
        member_by_id(&s, stranger).await.status,
        1,
        "a refused toggle writes nothing"
    );

    // Missing id → not found.
    let missing = uid(BASE);
    assert_eq!(
        downline::set_child_status(&s.db, agent, missing, 1)
            .await
            .unwrap(),
        StatusOutcome::NotFound
    );
}

/// §6.5: the CSV export reuses the §6.4 list read (same scope / filter) and
/// renders the human-readable label columns over the acting agent's children.
#[tokio::test]
async fn export_csv_renders_direct_children_with_labels() {
    let Some(s) = suite().await else { return };
    let agent = uid(BASE);
    let other_agent = uid(BASE);
    let c1 = uid(BASE);
    let c2 = uid(BASE);
    let outsider = uid(BASE);
    let tag = uid(BASE);

    let agent_name = format!("ag{tag}");
    seed_member(&s, agent, 1, 7, 1, 1, &agent_name).await;
    seed_member(&s, other_agent, 1, 7, 1, 1, &format!("ob{tag}")).await;
    // c1: 商户(4) / 正常(1) / 已认证(1); c2: 代理(7→空类型) / 未激活(0) / 待审(2).
    seed_member(&s, c1, agent, 4, 1, 1, &format!("c1{tag}")).await;
    seed_member(&s, c2, agent, 7, 0, 2, &format!("c2{tag}")).await;
    seed_member(&s, outsider, other_agent, 4, 1, 1, &format!("ou{tag}")).await;

    let rows = downline::list_page(&s.db, agent, &DownlineFilter::default(), 1, PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "only the two direct children export");
    let csv = String::from_utf8(downline::render_export_csv(&agent_name, &rows)).unwrap();
    assert!(csv.starts_with('\u{FEFF}'));
    assert!(csv.contains("用户名,商户号,用户类型,上级用户名,状态,认证,可用余额,冻结余额,注册时间"));
    assert!(csv.contains(&format!(
        "c1{tag},{},商户,{agent_name},正常,已认证,0,0,",
        mch_id_of(c1)
    )));
    assert!(csv.contains(&format!(
        "c2{tag},{},,{agent_name},未激活,等待审核,0,0,",
        mch_id_of(c2)
    )));
    assert!(!csv.contains(&format!("ou{tag}")), "no foreign children");

    // status = 1 keeps only c1 (the `0`-as-empty collapse is a handler concern;
    // the service still honours a real Some(1)).
    let only_live = DownlineFilter {
        status: Some(1),
        ..Default::default()
    };
    let filtered = downline::list_page(&s.db, agent, &only_live, 1, PAGE_SIZE)
        .await
        .unwrap();
    let csv2 = String::from_utf8(downline::render_export_csv(&agent_name, &filtered)).unwrap();
    assert!(csv2.contains(&format!("c1{tag}")));
    assert!(!csv2.contains(&format!("c2{tag}")));
}
