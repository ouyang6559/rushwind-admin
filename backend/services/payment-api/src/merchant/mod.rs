//! Merchant / agent identity (`spec/05-merchant-agent.md`). This module
//! owns the pure identity rules — the merchant-number ↔ user-id offset, the
//! `groupid` → role mapping, and API-key generation — plus the DB-touching
//! [`MembersRepo`] lookups the gateway and back-office share.
//!
//! The three-level agent profit tree (`parentid`) and the KYC state machine
//! ride the `members` columns; the concrete account operations (recharge /
//! freeze) close through [`crate::ledger`] in Phase 3, not here.

pub mod agent_rate;
pub mod apikey;
pub mod article;
pub mod attachment;
pub mod bankcard;
pub mod charges;
pub mod console;
pub mod deposit;
pub mod downline;
pub mod downline_order;
pub mod forgetpwd;
pub mod google;
pub mod invite;
pub mod login;
pub mod loginrecord;
pub mod mobile;
pub mod password;
pub mod profile;
pub mod profit_report;
pub mod rbac;
pub mod register;
pub mod twofactor;

use rand::RngCore;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::data::members;

/// The legacy merchant number is `member.id + 10000` on the wire
/// (`PayController` `$return["memberid"] = userid + 10000`; `spec/05` §7.1).
pub const MCH_ID_OFFSET: i64 = 10_000;

/// Wire `pay_memberid` (mch id) → internal user id. Rejects ids below the
/// offset so a malformed `12345`-style value cannot address a bogus member.
pub fn user_id_of_mch(mch_id: i64) -> Option<i64> {
    mch_id.checked_sub(MCH_ID_OFFSET).filter(|u| *u >= 0)
}

/// Internal user id → wire `pay_memberid`.
pub fn mch_id_of(user_id: i64) -> i64 {
    user_id + MCH_ID_OFFSET
}

/// The member role, derived from `groupid` (`spec/00` §"角色映射",
/// `spec/05` §2/§11). Tiers 5/6/7 are the three agent levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// groupid 1 — the platform super-admin.
    Platform,
    /// groupid 4 — a pure merchant (no downstream agents).
    Merchant,
    /// groupid 5/6/7 — an agent that owns downline merchants/agents.
    Agent,
}

impl Role {
    /// Maps a raw `groupid`; unknown ids default to [`Role::Merchant`] (the
    /// safest minimal-privilege reading — it can never be treated as an agent
    /// or the platform by mistake).
    pub fn from_groupid(groupid: i32) -> Self {
        match groupid {
            1 => Role::Platform,
            4 => Role::Merchant,
            5..=7 => Role::Agent,
            _ => Role::Merchant,
        }
    }

    pub fn is_agent(self) -> bool {
        matches!(self, Role::Agent)
    }

    /// Only the pure merchant (groupid 4) sees the developer API doc
    /// (`ChannelController::apidocumnet`, `spec/05` §9).
    pub fn can_view_apidoc(self) -> bool {
        matches!(self, Role::Merchant)
    }
}

/// Generates a merchant API key — the MD5 sign secret. 32 lowercase hex
/// chars from the OS CSPRNG, matching the legacy `random_str()` length
/// (`function.php:824`). The value is a shared secret, so casing only needs
/// to be stable between generation and signing (both lowercase here).
pub fn generate_apikey() -> String {
    let mut buf = [0u8; 16];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Thin repository over the `members` table.
pub struct MembersRepo<'a> {
    db: &'a DatabaseConnection,
}

impl<'a> MembersRepo<'a> {
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }

    /// Loads a member by internal user id.
    pub async fn by_id(&self, user_id: i64) -> Result<Option<members::Model>, sea_orm::DbErr> {
        members::Entity::find_by_id(user_id).one(self.db).await
    }

    /// Loads a member by login username.
    pub async fn by_username(
        &self,
        username: &str,
    ) -> Result<Option<members::Model>, sea_orm::DbErr> {
        members::Entity::find()
            .filter(members::Column::Username.eq(username))
            .one(self.db)
            .await
    }

    /// Generates, persists and returns a fresh API key for the member
    /// (the reset path behind the payment-password second factor,
    /// `spec/05` §9). Returns `Ok(None)` when the member does not exist.
    pub async fn rotate_apikey(&self, user_id: i64) -> Result<Option<String>, sea_orm::DbErr> {
        let Some(model) = self.by_id(user_id).await? else {
            return Ok(None);
        };
        let key = generate_apikey();
        let mut active: members::ActiveModel = model.into();
        active.apikey = Set(Some(key.clone()));
        active.update(self.db).await?;
        Ok(Some(key))
    }

    /// Writes a fresh single-sign-on `session_version` on the member
    /// (`User/LoginController::check` L130-131 `randpw(32)` → `session_random`,
    /// `spec/05` §4.6). Every panel login bumps it once, so any older session
    /// still carrying the previous value becomes stale and is rejected on its
    /// next request.
    pub async fn bump_session_version(
        &self,
        user_id: i64,
        version: &str,
    ) -> Result<(), sea_orm::DbErr> {
        let Some(model) = self.by_id(user_id).await? else {
            return Ok(());
        };
        let mut active: members::ActiveModel = model.into();
        active.session_version = Set(Some(version.to_string()));
        active.update(self.db).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mch_id_roundtrips() {
        assert_eq!(mch_id_of(1), 10_001);
        assert_eq!(user_id_of_mch(10_001), Some(1));
        // below the offset, or exactly the offset minus one, is rejected
        assert_eq!(user_id_of_mch(9_999), None);
        assert_eq!(user_id_of_mch(MCH_ID_OFFSET), Some(0));
    }

    #[test]
    fn groupid_maps_to_roles() {
        assert_eq!(Role::from_groupid(1), Role::Platform);
        assert_eq!(Role::from_groupid(4), Role::Merchant);
        assert_eq!(Role::from_groupid(5), Role::Agent);
        assert_eq!(Role::from_groupid(6), Role::Agent);
        assert_eq!(Role::from_groupid(7), Role::Agent);
        // unknown defaults to the minimal-privilege merchant
        assert_eq!(Role::from_groupid(99), Role::Merchant);
        assert!(Role::from_groupid(5).is_agent());
        assert!(Role::from_groupid(4).can_view_apidoc());
        assert!(!Role::from_groupid(5).can_view_apidoc());
    }

    #[test]
    fn apikey_is_32_lowercase_hex_and_unique() {
        let a = generate_apikey();
        let b = generate_apikey();
        assert_eq!(a.len(), 32);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b, "two CSPRNG draws must differ");
    }
}
