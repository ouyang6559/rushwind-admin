//! The pure order status machine (`spec/02-funds-order.md` §2, §4.2, §6, §7).
//!
//! It owns the `pay_status` (`0/1/2`) lifecycle, the settle CAS semantics,
//! the merchant-notify acknowledgement rule that advances `1 → 2`, the
//! reissue admission gate (the `postUrl` / `jiankong` / `bufa` entry points
//! unified), and the freeze / thaw `lock_status` CAS — all offline-unit-tested
//! with no IO. The [`crate::ledger`] service wraps these decisions in the
//! `WHERE status = ?` conditional UPDATEs the four iron rules mandate; here we
//! only model the *rules*, so a wrong transition is caught before it ever
//! reaches the database.
//!
//! The three legacy traps this pins down:
//! 1. **入账去重** — only `Unpaid` may be settled; a duplicate callback on a
//!    `Paid` / `Notified` order is an idempotent no-op, never a second credit
//!    (§2.1 note, PM:27 / P:238 wrap the accounting in `if (pay_status == 0)`).
//! 2. **置 2 的唯一标准** — the order leaves `Paid` for `Notified` only when
//!    the merchant's notify reply body (case-insensitively) contains `ok`
//!    (§4.2 step 3); the sync `callbackurl` jump never flips the status.
//! 3. **单向冻结** — `lock_status` advances `未冻结 → 冻结中 → 已解冻(历史)` and
//!    has no reverse write path (§2.2).

/// The stored `pay_status` (`orders.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayStatus {
    /// `0` 未支付.
    Unpaid,
    /// `1` 已支付未返回 (入账完成, 商户尚未回执).
    Paid,
    /// `2` 已支付已返回 (商户回执 `ok`).
    Notified,
}

impl PayStatus {
    /// The stored `pay_status` code.
    pub fn code(self) -> i32 {
        match self {
            PayStatus::Unpaid => 0,
            PayStatus::Paid => 1,
            PayStatus::Notified => 2,
        }
    }

    /// Parses a stored `pay_status` code; unknown codes read as [`None`].
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(PayStatus::Unpaid),
            1 => Some(PayStatus::Paid),
            2 => Some(PayStatus::Notified),
            _ => None,
        }
    }

    /// Whether 入账 has already happened (`1` or `2`) — the precondition for
    /// freeze / thaw and the state a duplicate settle must not re-credit.
    pub fn is_settled(self) -> bool {
        matches!(self, PayStatus::Paid | PayStatus::Notified)
    }
}

/// The outcome of the `0 → 1` settle CAS (`spec/02` §4.1 / §11.1 iron rule #3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleCas {
    /// The caller observed `Unpaid` and owns the `0 → 1` transition: run the
    /// accounting exactly once inside the transaction.
    Won,
    /// The order was already `Paid` / `Notified` — finish idempotently, do not
    /// re-credit (a duplicate callback still gets a re-notification, but never
    /// a second balance move).
    AlreadySettled,
}

/// The settle CAS decision: only [`PayStatus::Unpaid`] is won.
pub fn settle_transition(current: PayStatus) -> SettleCas {
    if current == PayStatus::Unpaid {
        SettleCas::Won
    } else {
        SettleCas::AlreadySettled
    }
}

/// Whether a merchant notify reply confirms receipt — the exact legacy rule
/// (`strstr(strtolower($contents), "ok") != false`, `spec/02` §4.2 step 3).
pub fn notify_acked(reply: &str) -> bool {
    reply.to_lowercase().contains("ok")
}

/// The `1 → 2` advance. Returns the new status only when a [`PayStatus::Paid`]
/// order got an `ok` acknowledgement; every other case yields [`None`] (no
/// write), so a `Unpaid` order is never flipped to `2` out of band and a reply
/// without `ok` leaves it at `1` awaiting reissue.
pub fn mark_notified(current: PayStatus, reply: &str) -> Option<PayStatus> {
    if current == PayStatus::Paid && notify_acked(reply) {
        Some(PayStatus::Notified)
    } else {
        None
    }
}

/// The minimum gap between two reissue attempts (the `postUrl` admission
/// `last_reissue_time < time() - 10`, `spec/02` §7.1 / §2.3).
pub const MIN_REISSUE_INTERVAL_SECS: i64 = 10;

/// Whether an order qualifies for a re-notification sweep — the `postUrl`
/// model unified (`spec/02` §7): only a [`PayStatus::Paid`] order whose
/// `num < max_num` and whose last attempt is older than
/// [`MIN_REISSUE_INTERVAL_SECS`] is (re)sent. A `Notified` (`2`) order is
/// terminal for the sweep; `Unpaid` was never credited.
pub fn reissue_admit(
    current: PayStatus,
    num: i32,
    max_num: i32,
    last_reissue_time: i64,
    now: i64,
) -> bool {
    current == PayStatus::Paid
        && num < max_num
        && last_reissue_time < now.saturating_sub(MIN_REISSUE_INTERVAL_SECS)
}

/// `lock_status` — never frozen.
pub const LOCK_NONE: i32 = 0;
/// `lock_status` — frozen (a live freeze; the balance sits in `blocked_balance`).
pub const LOCK_FROZEN: i32 = 1;
/// `lock_status` — once frozen, now thawed (terminal marker, no reverse).
pub const LOCK_THAWED: i32 = 2;

/// The freeze CAS (`O::doForzen`, `spec/02` §6.1): a settled order
/// (`pay_status in (1,2)`) that has never been frozen (`lock_status < 1`).
pub fn can_freeze(status: PayStatus, lock: i32) -> bool {
    status.is_settled() && lock < LOCK_FROZEN
}

/// The thaw CAS (`O::thawOrder`, `spec/02` §6.6): a settled order that is
/// currently frozen (`lock_status == 1`). A `LOCK_THAWED` (`2`) row is never
/// re-thawed (no double credit back to `balance`).
pub fn can_thaw(status: PayStatus, lock: i32) -> bool {
    status.is_settled() && lock == LOCK_FROZEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_roundtrip() {
        for (s, c) in [
            (PayStatus::Unpaid, 0),
            (PayStatus::Paid, 1),
            (PayStatus::Notified, 2),
        ] {
            assert_eq!(s.code(), c);
            assert_eq!(PayStatus::from_code(c), Some(s));
        }
        assert_eq!(PayStatus::from_code(9), None);
    }

    #[test]
    fn only_unpaid_is_settled_now() {
        assert!(matches!(
            settle_transition(PayStatus::Unpaid),
            SettleCas::Won
        ));
        // a duplicate callback on an already-credited order must not re-enter
        assert!(matches!(
            settle_transition(PayStatus::Paid),
            SettleCas::AlreadySettled
        ));
        assert!(matches!(
            settle_transition(PayStatus::Notified),
            SettleCas::AlreadySettled
        ));
    }

    #[test]
    fn ack_is_case_insensitive_substring() {
        assert!(notify_acked("OK"));
        assert!(notify_acked("ok"));
        assert!(notify_acked("result=okay,thanks")); // `ok` appears as substring
        assert!(!notify_acked("fail"));
        assert!(!notify_acked(""));
    }

    #[test]
    fn notified_only_advances_from_paid_on_ack() {
        assert_eq!(
            mark_notified(PayStatus::Paid, "ok"),
            Some(PayStatus::Notified)
        );
        assert_eq!(mark_notified(PayStatus::Paid, "nope"), None);
        // never skip 0 → 2 out of band
        assert_eq!(mark_notified(PayStatus::Unpaid, "ok"), None);
        // already notified stays (no further write)
        assert_eq!(mark_notified(PayStatus::Notified, "ok"), None);
    }

    #[test]
    fn reissue_admits_only_paid_within_budget_and_interval() {
        // Paid, num under the cap, last attempt older than 10s → admit.
        assert!(reissue_admit(PayStatus::Paid, 1, 5, 1_000, 1_011));
        // boundary: exactly 10s gap is NOT older than 10s → reject.
        assert!(!reissue_admit(PayStatus::Paid, 1, 5, 1_000, 1_010));
        // num at the cap → reject.
        assert!(!reissue_admit(PayStatus::Paid, 5, 5, 1_000, 2_000));
        // terminal / unsettled states never reissue.
        assert!(!reissue_admit(PayStatus::Notified, 0, 5, 0, 2_000));
        assert!(!reissue_admit(PayStatus::Unpaid, 0, 5, 0, 2_000));
    }

    #[test]
    fn freeze_and_thaw_cas_guards() {
        // freeze needs a settled, never-frozen order
        assert!(can_freeze(PayStatus::Paid, LOCK_NONE));
        assert!(can_freeze(PayStatus::Notified, LOCK_NONE));
        assert!(!can_freeze(PayStatus::Unpaid, LOCK_NONE));
        assert!(!can_freeze(PayStatus::Paid, LOCK_FROZEN));
        assert!(!can_freeze(PayStatus::Paid, LOCK_THAWED));
        // thaw needs a settled, currently-frozen order
        assert!(can_thaw(PayStatus::Paid, LOCK_FROZEN));
        assert!(!can_thaw(PayStatus::Paid, LOCK_NONE));
        // a once-thawed order is never re-thawed (no double credit)
        assert!(!can_thaw(PayStatus::Paid, LOCK_THAWED));
    }
}
