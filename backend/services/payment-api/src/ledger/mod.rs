//! The single atomic accounting service — the rewrite's consolidation of
//! the two byte-identical PHP accounting paths
//! (`PayController::EditMoney` and `PayModel::completeOrder`; see
//! `spec/00-project-overview.md` §9 "重复代码警告"). Its contract is
//! [`spec/02-funds-order.md`] §11.2.
//!
//! Phase 0 laid the type and the four iron rules as code comments; the
//! bodies now live in [`db`] — the async seams below the [`LedgerService`]
//! type replay the pure kernels as conditional UPDATEs + atomic balance SQL
//! + flow INSERTs, and surface unwired paths as clean internal errors.
//!
//! Iron rules (from `spec/02` §11.1):
//! 1. every balance change is an atomic SQL (`balance = balance + x` or a
//!    guarded `WHERE balance >= x`) — never read-modify-write;
//! 2. every balance change writes a `money_changes` row in the SAME tx;
//! 3. every order transition is a CAS (`WHERE order_id=? AND status=?`),
//!    failure returns a conflict the caller treats as idempotent;
//! 4. fixed lock order `Member -> Order -> Channel -> Commit`, all InnoDB
//!    (Postgres here), to avoid deadlock.
//!
//! The *decisions* those rules encode are pure and live in the submodules
//! below, each offline-unit-tested with no database — order admission
//! ([`admit`]), the order status machine ([`state`]), the orderadd /
//! settlement amount math and flow intents ([`snapshot`]), the agent profit
//! walk ([`profit`]), the composed settle write-plan ([`settle`]) and the
//! scheduled-thaw decisions ([`thaw`]). The async methods on
//! [`LedgerService`] ([`db`]) are typed against exactly those plan types and
//! only ever replay them; they never re-derive money.

pub mod admit;
pub mod db;
pub mod profit;
pub mod settle;
pub mod snapshot;
pub mod state;
pub mod thaw;

pub use admit::{admit, AdmitError, AdmitRequest};
pub use db::{AdminMove, BalanceAfter, NewOrder, RedoType};
pub use profit::{split as split_profit, Brokerage, ChainNode, DEFAULT_MAX_LEVELS};
pub use settle::{
    settle, AgentLevel, DepositEntry, MerchantBuckets, SettleError, SettleInput, SettleOutcome,
    SettleWrites,
};
pub use snapshot::{
    destination_of, flow, is_valid_cycle, lx, plan_settlement, DepositRule, Destination,
    FlowIntent, OrderAmounts, SettlePlan,
};
pub use state::{
    can_freeze, can_thaw, mark_notified, notify_acked, reissue_admit, settle_transition, PayStatus,
    SettleCas, LOCK_FROZEN, LOCK_NONE, LOCK_THAWED, MIN_REISSUE_INTERVAL_SECS,
};
pub use thaw::{
    apply as apply_thaw, in_thaw_window, t1_blockedlog_due, unfreeze_due, ThawBuckets, ThawKind,
    ThawResult, T1_THAW_BUFFER_SECS,
};

use sea_orm::DatabaseConnection;

/// The concrete ledger over the shared connection. Transactions are
/// opened per call from `db`; the method bodies live in [`db`].
pub struct LedgerService {
    db: DatabaseConnection,
}

impl LedgerService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// The underlying connection (repos / workers borrow it).
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}
