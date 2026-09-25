//! Agent invite codes (`spec/05` §6.3) — the `User/AgentController`
//! create / delete / list surface plus the §3.1 registration gate that reads
//! and consumes them. Everything that can be decided without the database
//! (code shape, the lifecycle / display state machines, the tier gate, and
//! the invite → new-member mapping) is a pure function pinned by offline
//! tests; the DB-touching services (`create_invite` / `delete_invite` /
//! `find_usable` / `consume_invite` / `list_for_agent`) sit beside them.
//!
//! Faithful legacy quirks (registered as a decision memory):
//! - TWO parallel state columns coexist. `status` (0 禁用 / 1 未用 / 2 已用)
//!   is what `checkRegister` validates (`status = 1 AND yxdatetime >= now`)
//!   and what registration flips to 2 on use; `inviteconfigzt` is the
//!   secondary flag `addInvitecode` writes (1) and `getinviteconfigzt`
//!   renders (禁用 / 可过期 / 已使用). The register path NEVER updates
//!   `inviteconfigzt`, so a consumed code's agent-list display stays "可过期"
//!   until it clock-expires — reproduced, not "fixed".
//! - `addInvitecode` bounds the minted tier strictly below the acting
//!   agent's own groupid (`regtype >= groupid` → `没有权限`, reusing the
//!   [`can_open_child`] ladder); the platform's admin codes are minted with
//!   `is_admin = 1` and neither parent the registrant nor delete here.
//! - `createInvitecode` regenerates on collision via unbounded recursion; we
//!   loop a bounded number of tries then fail — behaviourally identical for
//!   the 36⁴ code space but non-hanging.

use rand::Rng;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::data::{invite_codes, now_ts};
use crate::merchant::rbac::can_open_child;
use crate::state::{GatewayError, GatewayResult};

/// The invite-code length, `C('INVITECODE')` (`Common/Conf/config.php:36`).
pub const CODE_LEN: usize = 4;
/// The `addInvite` pre-generated validity (`time() + 86400`, one day).
pub const DEFAULT_TTL_SECS: i64 = 86_400;
/// The `random_str` alphabet: lowercase ASCII letters then digits.
const CODE_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
/// Max collision retries before giving up (`createInvitecode` recursion, bounded).
const MAX_CODE_TRIES: u32 = 100;

/// The exact `addInvitecode` / register rejection messages.
pub const MSG_NO_PERMISSION: &str = "没有权限";

/// The register-facing lifecycle state, read off `status` + the expiry clock
/// (the `checkRegister` `status = 1 AND yxdatetime >= now` predicate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteState {
    /// `status = 1` and not yet expired — registration may consume it.
    Usable,
    /// `status = 0` (or any other value) — disabled.
    Disabled,
    /// `status = 2` — already used.
    Used,
    /// `status = 1` but past `yxdatetime` — expired.
    Expired,
}

/// Maps a code's stored `status` + expiry to its [`InviteState`] at `now`.
pub fn validity(status: i32, yxdatetime: i64, now: i64) -> InviteState {
    match status {
        1 if yxdatetime >= now => InviteState::Usable,
        1 => InviteState::Expired,
        2 => InviteState::Used,
        _ => InviteState::Disabled,
    }
}

/// Whether registration may consume the code (the 10001 gate; `>= now` is
/// inclusive, matching the legacy `EGT` comparison).
pub fn is_usable(status: i32, yxdatetime: i64, now: i64) -> bool {
    matches!(validity(status, yxdatetime, now), InviteState::Usable)
}

/// The agent-list display state (`getinviteconfigzt`), driven by the
/// PARALLEL `inviteconfigzt` flag + the expiry clock — independent of
/// `status`. Note the display expiry is strict (`now < yxdatetime` usable),
/// one second tighter than the register gate's `>=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayState {
    /// `inviteconfigzt = 0` — 禁用.
    Disabled,
    /// `inviteconfigzt = 1` and still in its window — 可以使用.
    Usable,
    /// `inviteconfigzt = 1` past the window — 已过期.
    Expired,
    /// `inviteconfigzt = 2` — 已使用.
    Used,
}

pub fn display_state(inviteconfigzt: i32, yxdatetime: i64, now: i64) -> DisplayState {
    match inviteconfigzt {
        1 if now < yxdatetime => DisplayState::Usable,
        1 => DisplayState::Expired,
        2 => DisplayState::Used,
        _ => DisplayState::Disabled,
    }
}

/// The `addInvitecode` tier gate (L124): a level-N agent may only mint a code
/// for a STRICTLY lower group than its own, else `没有权限`. The platform
/// (groupid 1) is outside the agent ladder and minted via the back-office.
pub fn tier_rejection(parent_groupid: i32, regtype: i32) -> Option<&'static str> {
    if can_open_child(parent_groupid, regtype) {
        None
    } else {
        Some(MSG_NO_PERMISSION)
    }
}

/// The `generateUser` invite → new-member mapping (§3.1): `groupid` is the
/// invite's `regtype` (defaulting to 4 = merchant when unset), and `parentid`
/// is the minting agent — but only a NON-admin one; an admin code (or one
/// without an owner) parents the registrant to the platform (1).
pub fn apply_invite_to_member(invite: &invite_codes::Model) -> (i32, i64) {
    let groupid = if invite.regtype != 0 {
        invite.regtype
    } else {
        4
    };
    let parentid = if invite.fmusernameid != 0 && invite.is_admin == 0 {
        invite.fmusernameid
    } else {
        1
    };
    (groupid, parentid)
}

/// A `random_str(4)` invite code from the `[a-z0-9]` alphabet.
pub fn generate_code() -> String {
    code_with_rng(&mut rand::rng())
}

/// The code generator over an injected RNG so tests can be deterministic.
pub fn code_with_rng<R: Rng + ?Sized>(rng: &mut R) -> String {
    (0..CODE_LEN)
        .map(|_| {
            let i = rng.random_range(0..CODE_ALPHABET.len());
            CODE_ALPHABET[i] as char
        })
        .collect()
}

/// Parses the form's `yxdatetime` (a `Y-m-d H:i:s` string the `addInvite`
/// pre-generation fills with `now + 86400`) to unix seconds. Empty or
/// unparseable falls back to `now + DEFAULT_TTL_SECS` — legacy `strtotime`
/// would yield a dead `0`, so we default a fresh day instead (a lenient,
/// documented divergence; the UI always submits a valid datetime).
pub fn parse_yxdatetime(raw: &str, now: i64) -> i64 {
    let s = raw.trim();
    if s.is_empty() {
        return now + DEFAULT_TTL_SECS;
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|dt| {
            dt.and_local_timezone(chrono::Local)
                .single()
                .map_or(now + DEFAULT_TTL_SECS, |z| z.timestamp())
        })
        .unwrap_or(now + DEFAULT_TTL_SECS)
}

/// Generates a code not already present in the table (the `createInvitecode`
/// collision-recursion, bounded).
async fn unique_code(db: &DatabaseConnection) -> GatewayResult<String> {
    for _ in 0..MAX_CODE_TRIES {
        let code = generate_code();
        let taken = invite_codes::Entity::find()
            .filter(invite_codes::Column::Invitecode.eq(&code))
            .one(db)
            .await?;
        if taken.is_none() {
            return Ok(code);
        }
    }
    Err(GatewayError::Internal("invite: 无法生成唯一邀请码".into()))
}

/// Mints and persists an agent invite (§6.3 `createInvitecode` + `addInvitecode`).
/// The tier gate runs first — a non-lower `regtype` rejects with `没有权限`
/// and writes nothing. On a clean request a unique code is generated and the
/// row inserted with `status = 1 / inviteconfigzt = 1 / fbdatetime = now /
/// is_admin = 0`. The outer `Err` is an infrastructure failure, the inner
/// `Err(&str)` the caller-facing legacy message.
pub async fn create_invite(
    db: &DatabaseConnection,
    fmusernameid: i64,
    parent_groupid: i32,
    regtype: i32,
    yxdatetime: i64,
) -> GatewayResult<Result<invite_codes::Model, &'static str>> {
    if let Some(msg) = tier_rejection(parent_groupid, regtype) {
        return Ok(Err(msg));
    }
    let code = unique_code(db).await?;
    let now = now_ts();
    let model = invite_codes::ActiveModel {
        invitecode: Set(code),
        fmusernameid: Set(fmusernameid),
        syusernameid: Set(0),
        regtype: Set(regtype),
        fbdatetime: Set(now),
        yxdatetime: Set(yxdatetime),
        sydatetime: Set(0),
        status: Set(1),
        inviteconfigzt: Set(1),
        is_admin: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(Ok(model))
}

/// Deletes one of the agent's OWN, non-admin codes (§6.3 `delInvitecode`:
/// `where(id, fmusernameid = uid, is_admin = 0)`), returning the affected-row
/// count (0 when the id is foreign / admin-minted / absent).
pub async fn delete_invite(
    db: &DatabaseConnection,
    id: i64,
    fmusernameid: i64,
) -> GatewayResult<u64> {
    let res = invite_codes::Entity::delete_many()
        .filter(invite_codes::Column::Id.eq(id))
        .filter(invite_codes::Column::Fmusernameid.eq(fmusernameid))
        .filter(invite_codes::Column::IsAdmin.eq(0))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// Finds a code registration may consume (§3.1 / `checkinvitecode`:
/// `where(invitecode, status = 1, yxdatetime >= now)`) — the SQL mirror of
/// [`is_usable`].
pub async fn find_usable(
    db: &DatabaseConnection,
    code: &str,
    now: i64,
) -> GatewayResult<Option<invite_codes::Model>> {
    let row = invite_codes::Entity::find()
        .filter(invite_codes::Column::Invitecode.eq(code))
        .filter(invite_codes::Column::Status.eq(1))
        .filter(invite_codes::Column::Yxdatetime.gte(now))
        .one(db)
        .await?;
    Ok(row)
}

/// The §3.1 invalidation (`_failinvitecode`): set `status = 2 / syusernameid /
/// sydatetime`, matched BY CODE (the legacy `where(['invitecode' => $code])`,
/// not by id), leaving `inviteconfigzt` untouched. Returns rows affected.
pub async fn consume_invite(
    db: &DatabaseConnection,
    code: &str,
    used_by: i64,
    now: i64,
) -> GatewayResult<u64> {
    let res = invite_codes::Entity::update_many()
        .col_expr(invite_codes::Column::Status, Expr::value(2))
        .col_expr(invite_codes::Column::Syusernameid, Expr::value(used_by))
        .col_expr(invite_codes::Column::Sydatetime, Expr::value(now))
        .filter(invite_codes::Column::Invitecode.eq(code))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// The agent's own code list (§6.3 `invitecode`), newest first.
pub async fn list_for_agent(
    db: &DatabaseConnection,
    fmusernameid: i64,
) -> GatewayResult<Vec<invite_codes::Model>> {
    let rows = invite_codes::Entity::find()
        .filter(invite_codes::Column::Fmusernameid.eq(fmusernameid))
        .order_by_desc(invite_codes::Column::Id)
        .all(db)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_code_is_four_chars_from_the_alphabet() {
        let c = generate_code();
        assert_eq!(c.len(), CODE_LEN);
        assert!(c
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()));
        assert_ne!(generate_code(), generate_code(), "two CSPRNG draws differ");
    }

    #[test]
    fn seeded_generator_is_deterministic() {
        use rand::SeedableRng;
        let mut a = rand::rngs::StdRng::seed_from_u64(7);
        let mut b = rand::rngs::StdRng::seed_from_u64(7);
        assert_eq!(code_with_rng(&mut a), code_with_rng(&mut b));
    }

    #[test]
    fn validity_matrix() {
        let now = 1_000;
        assert_eq!(
            validity(1, now, now),
            InviteState::Usable,
            "EGT is inclusive"
        );
        assert_eq!(validity(1, now + 1, now), InviteState::Usable);
        assert_eq!(validity(1, now - 1, now), InviteState::Expired);
        assert_eq!(validity(2, now + 10, now), InviteState::Used);
        assert_eq!(validity(0, now + 10, now), InviteState::Disabled);
        assert!(is_usable(1, now, now));
        assert!(!is_usable(1, now - 1, now));
    }

    #[test]
    fn display_state_reads_inviteconfigzt_not_status() {
        let now = 1_000;
        // inviteconfigzt = 1 uses a STRICT window (one second tighter than the
        // register gate): at now == yxdatetime it already reads 已过期.
        assert_eq!(display_state(1, now, now), DisplayState::Expired);
        assert_eq!(display_state(1, now + 1, now), DisplayState::Usable);
        assert_eq!(display_state(2, now + 10, now), DisplayState::Used);
        assert_eq!(display_state(0, now + 10, now), DisplayState::Disabled);
    }

    #[test]
    fn tier_gate_allows_only_strictly_lower_groups() {
        assert_eq!(tier_rejection(6, 4), None); // agent 6 → merchant
        assert_eq!(tier_rejection(6, 5), None); // agent 6 → agent 5
        assert_eq!(tier_rejection(6, 6), Some(MSG_NO_PERMISSION)); // peer
        assert_eq!(tier_rejection(6, 7), Some(MSG_NO_PERMISSION)); // superior
        assert_eq!(tier_rejection(4, 1), Some(MSG_NO_PERMISSION)); // merchant mints nothing
    }
}
