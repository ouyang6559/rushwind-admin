//! The viewer context — standalone + go-crud's
//! `TenantPrivacy` rule. Every repository receives a [`Viewer`] and
//! derives its query predicates from it; there is exactly one place that
//! decides tenancy filtering.
//!
//! * platform viewer (`tid == 0`): no tenant predicate (rows of all
//!   tenants visible);
//! * tenant viewer: every query/update/delete is constrained to
//!   `tenant_id = tid`, creates are force-stamped, cross-tenant writes
//!   are denied;
//! * system viewer (startup seeds, token-cleanup jobs): bypasses all
//!   tenant predicates (`NewSystemViewerContext`).

use rushwind_http_binding::ctx::RequestContext;

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub enum ViewerKind {
    /// No credentials — `NewNoopContext` (public routes).
    Noop,
    /// A verified user context: platform when tenant 0.
    User,
    /// `NewSystemViewerContext` — bypasses tenancy (seeds, internal jobs).
    System,
}

/// The full viewer surface mirrors viewer API; methods a
/// service has not migrated to yet stay as part of the data-layer API.
#[allow(dead_code)]
#[derive(Clone, Debug, Copy)]
pub struct Viewer {
    pub kind: ViewerKind,
    pub user_id: u32,
    pub tenant_id: u32,
    /// UNIT-scoped rows stay visible through data scopes only; repos use
    /// this flag for the row-level scope pilot (sys_positions).
    pub data_scope_all: bool,
}

#[allow(dead_code)] // the full viewer surface is the data-layer API
impl Viewer {
    /// The noop viewer (public routes).
    pub fn noop() -> Self {
        Self {
            kind: ViewerKind::Noop,
            user_id: 0,
            tenant_id: 0,
            data_scope_all: false,
        }
    }

    /// The system viewer — `NewSystemViewerContext`.
    pub fn system() -> Self {
        Self {
            kind: ViewerKind::System,
            user_id: 0,
            tenant_id: 0,
            data_scope_all: true,
        }
    }

    /// From the request's claim bag; `None` claims yield the noop viewer.
    pub fn from_ctx(ctx: &RequestContext) -> Self {
        match ctx
            .claims
            .as_ref()
            .and_then(crate::token::UserTokenPayload::from_claims)
        {
            Some(payload) => Self {
                kind: ViewerKind::User,
                user_id: payload.user_id,
                tenant_id: payload.tenant_id,
                data_scope_all: payload.data_scopes.iter().any(|s| s == "ALL"),
            },
            None => Self::noop(),
        }
    }

    /// `viewer.EnforceTenant` — `Some(tenant)` when rows must be
    /// constrained, `None` for platform/system/noop wide reads.
    pub fn tenant_scope(&self) -> Option<u32> {
        match self.kind {
            ViewerKind::User if self.tenant_id > 0 => Some(self.tenant_id),
            _ => None,
        }
    }

    /// TenantMutationGuard: a non-system viewer may only mutate rows of
    /// its own tenant; platform users pass.
    pub fn tenant_mutation_scope(&self) -> Option<u32> {
        match self.kind {
            ViewerKind::User if self.tenant_id > 0 => Some(self.tenant_id),
            ViewerKind::Noop => Some(u32::MAX), // deny-all sentinel
            _ => None,
        }
    }

    /// Create stamping: tenant viewers force their tenant onto new rows.
    pub fn stamp_tenant(&self, requested: Option<u32>) -> Option<u32> {
        match self.kind {
            ViewerKind::User if self.tenant_id > 0 => Some(self.tenant_id),
            _ => requested.or(Some(0)),
        }
    }
}
