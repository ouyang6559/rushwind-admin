//! Role → permission mapping derived from `groupid` (`spec/05` §2.1/§6.3,
//! target model in §13.5). `groupid` is a *member type*, not an ACL, so this
//! module turns [`Role`] into the coarse capabilities the merchant/agent
//! portals and the future back-office gate check, plus the "open a downline"
//! rule that keeps an agent from granting a peer or superior tier.
//!
//! The mapping is pure and table-driven so the groupid tiers (1 / 4 / 5-7)
//! and the strict `child < parent` tier ordering are pinned by unit tests
//! before any HTTP layer (Phase 7) enforces them.

use crate::merchant::Role;

/// A coarse capability the portals and back-office endpoints gate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Platform console (groupid 1 only).
    PlatformConsole,
    /// The merchant cashier / API-doc portal entry (groupid 4).
    MerchantPortal,
    /// The developer API doc, `ChannelController::apidocumnet` (groupid 4).
    ApiDoc,
    /// The agent workbench (`agent` module, groupid 5-7).
    AgentPortal,
    /// Open / manage a direct downline merchant/agent.
    ManageSubMerchant,
    /// Configure a downline's operating rate.
    SetSubRate,
}

/// The capability set for a role. Merchants hold the portal + API doc and no
/// agent powers; agents hold the agent powers and never the API doc
/// (`can_view_apidoc` stays merchant-only); the platform holds the console.
pub fn permissions_for(role: Role) -> &'static [Permission] {
    match role {
        Role::Platform => &[
            Permission::PlatformConsole,
            Permission::ManageSubMerchant,
            Permission::SetSubRate,
        ],
        Role::Merchant => &[Permission::MerchantPortal, Permission::ApiDoc],
        Role::Agent => &[
            Permission::AgentPortal,
            Permission::ManageSubMerchant,
            Permission::SetSubRate,
        ],
    }
}

/// Whether a role carries a permission.
pub fn has_permission(role: Role, perm: Permission) -> bool {
    permissions_for(role).contains(&perm)
}

/// Tiers that can appear on the "open a downline" form for a given parent:
/// the fixed groupid ladder `{4,5,6,7}` minus every tier `>= parent`
/// (`User/UserController::__construct` L42-47 — only strictly lower, i.e.
/// numerically smaller, roles may be granted; a merchant has none).
pub fn child_groupids_allowed(parent_groupid: i32) -> Vec<i32> {
    [4, 5, 6, 7]
        .into_iter()
        .filter(|g| *g < parent_groupid)
        .collect()
}

/// The `addInvitecode`/`saveUser` gate (`User/AgentController::addInvitecode`
/// L112-116): a level-N agent may only create a strictly lower tier. The
/// platform (groupid 1) is out of this ladder and is handled by the
/// back-office path, so a non-agent parent admits no children here.
pub fn can_open_child(parent_groupid: i32, child_groupid: i32) -> bool {
    child_groupids_allowed(parent_groupid).contains(&child_groupid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merchant_and_agent_capability_split() {
        let merchant = Role::from_groupid(4);
        let agent = Role::from_groupid(6);
        assert!(has_permission(merchant, Permission::ApiDoc));
        assert!(has_permission(merchant, Permission::MerchantPortal));
        assert!(!has_permission(merchant, Permission::AgentPortal));
        assert!(!has_permission(merchant, Permission::ManageSubMerchant));

        assert!(has_permission(agent, Permission::AgentPortal));
        assert!(has_permission(agent, Permission::ManageSubMerchant));
        assert!(!has_permission(agent, Permission::ApiDoc));

        let platform = Role::from_groupid(1);
        assert!(has_permission(platform, Permission::PlatformConsole));
    }

    #[test]
    fn only_strictly_lower_tiers_are_openable() {
        // A top (7) agent may open 4/5/6; a 5-agent only a merchant.
        assert_eq!(child_groupids_allowed(7), vec![4, 5, 6]);
        assert_eq!(child_groupids_allowed(5), vec![4]);
        assert_eq!(child_groupids_allowed(4), Vec::<i32>::new());
        assert!(can_open_child(7, 4));
        assert!(can_open_child(6, 5));
        assert!(!can_open_child(6, 6)); // peer tier forbidden
        assert!(!can_open_child(6, 7)); // superior tier forbidden
        assert!(!can_open_child(4, 3)); // merchants open nothing
    }
}
