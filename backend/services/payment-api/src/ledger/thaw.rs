//! The pure scheduled-thaw decisions (`spec/02-funds-order.md` §6.2 / §6.3 /
//! §6.4) — the three到期解冻 paths (Cli cron + 后台手动) that release money
//! back into a member's **available** balance. Each family differs only in its
//! `lx` code and whether the credit is drawn from `blocked_balance` or is a
//! standalone deposit release; the balance-move arithmetic and the anti-
//! overdraft guard are shared and pinned here, offline.
//!
//! The row *selection* (`status = 0`, `is_pause = 0`, the window / `unfreeze_time`
//! filters, `limit`, the `FOR UPDATE` single-row lock) is DB-side; the helpers
//! here only decide *given a candidate row, is it due and how does the move
//! land*, so the guard the legacy `WHERE blocked_balance >= amount` enforces is
//! reproduced as a pure [`Result`] (an `Err` means "0 rows updated, skip").

use crate::ledger::snapshot::{flow, lx, FlowIntent};

/// The T+1 sweep tolerance: a `blocked_log` whose `thawtime` is up to two hours
/// past today's midnight still counts as due (`thawtime <= today + 7200`,
/// `spec/02` §6.2 T:39 — matching the `tomorrow + rand(0,7200)` write window).
pub const T1_THAW_BUFFER_SECS: i64 = 7_200;

/// Rows per cron run (`limit(600)`, §6.2 T:38) — the legacy batch cap.
pub const T1_THAW_BATCH: u64 = 600;

/// Rows per deposit-unfreeze cron run (§6.3). The legacy `select` pulled the
/// whole due set unbounded; the cap keeps one runaway day from an unbounded
/// sweep (the leftover releases ride the next tick).
pub const DEPOSIT_UNFREEZE_BATCH: u64 = 600;

/// The thaw family, carrying its ledger semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThawKind {
    /// T+1 blocked-balance release (`thawPlanning`, `lx = 8`).
    T1Blocked,
    /// Complaints-deposit release (`doUnfreeze`, `lx = 13`).
    ComplaintsDeposit,
    /// Auto-unfreeze queue release (`auto_unfrozen_order`, `lx = 8`).
    AutoUnfreeze,
}

impl ThawKind {
    /// The `money_changes.lx` the release writes.
    pub fn lx(self) -> i32 {
        match self {
            ThawKind::ComplaintsDeposit => lx::DEPOSIT_UNFREEZE,
            ThawKind::T1Blocked | ThawKind::AutoUnfreeze => lx::UNFREEZE,
        }
    }

    /// Whether the credit is drawn from `blocked_balance` (T+1 / auto) — the
    /// complaints deposit was never part of `balance`, so its release only
    /// *adds* and debits nothing (§6.3).
    pub fn draws_blocked(self) -> bool {
        !matches!(self, ThawKind::ComplaintsDeposit)
    }
}

/// A member's balance buckets before a thaw (money units).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThawBuckets {
    pub available: i64,
    pub blocked: i64,
}

/// A completed thaw: the buckets after the move and the flow row to insert in
/// the same transaction (iron rule #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThawResult {
    pub buckets_after: ThawBuckets,
    pub flow: FlowIntent,
}

/// Applies one thaw move purely. The flow always tracks the **available**
/// bucket (§6.2 T:57: `ymoney = user.balance`), regardless of source. A
/// blocked-drawing kind whose `blocked < amount` returns [`Err`] (the guard the
/// legacy conditional UPDATE enforces); a deposit release never overdrafts.
pub fn apply(
    kind: ThawKind,
    user_id: i64,
    before: ThawBuckets,
    amount: i64,
    order_id: Option<String>,
) -> Result<ThawResult, ThawBuckets> {
    if kind.draws_blocked() && before.blocked < amount {
        return Err(before);
    }
    let buckets_after = if kind.draws_blocked() {
        ThawBuckets {
            available: before.available + amount,
            blocked: before.blocked - amount,
        }
    } else {
        ThawBuckets {
            available: before.available + amount,
            blocked: before.blocked,
        }
    };
    Ok(ThawResult {
        buckets_after,
        flow: flow(user_id, before.available, amount, kind.lx(), None, order_id),
    })
}

/// Whether a T+1 `blocked_log` row is due for the sweep at `today_midnight`:
/// its `thawtime` has arrived (within the [`T1_THAW_BUFFER_SECS`] tolerance)
/// and it was created strictly before today (§6.2 T:39, `createtime < today`).
pub fn t1_blockedlog_due(thaw_time: i64, create_time: i64, today_midnight: i64) -> bool {
    thaw_time <= today_midnight + T1_THAW_BUFFER_SECS && create_time < today_midnight
}

/// Whether an hour-of-day falls inside the allowed thaw window `[start, end)`
/// (§6.2 T:30-35, the cron's `allowstart` / `allowend` gate). A `end == 0`
/// window is treated as "no restriction" so a mis-set config never blocks a
/// release that is otherwise due.
pub fn in_thaw_window(hour: i32, start: i32, end: i32) -> bool {
    if end == 0 {
        return true;
    }
    start <= hour && hour < end
}

/// Whether a deposit / auto-unfreeze ledger row is due: `unfreeze_time <= now`
/// (§6.3 UC:26, §6.4 UM:26). The `status = 0` / `is_pause = 0` guards ride the
/// DB query.
pub fn unfreeze_due(now: i64, unfreeze_time: i64) -> bool {
    now >= unfreeze_time
}

/// Per-run accounting of the T+1 cron sweep (`spec/02` §6.2's `foreach`).
/// The legacy echoed nothing durable; this rides the worker's tracing so
/// a released/lost attempt is at least observable in the log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepReport {
    /// Due `blocked_logs` rows the scan listed (<= the legacy `limit(600)`).
    pub scanned: usize,
    /// Releases whose guarded UPDATE + log CAS both landed.
    pub released: usize,
    /// Rows skipped: overdraft guard or a raced `status = 0` CAS (§6.2's
    /// silent rollback branch), never retried inside the same run.
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_carry_the_right_lx_and_source() {
        assert_eq!(ThawKind::T1Blocked.lx(), 8);
        assert_eq!(ThawKind::AutoUnfreeze.lx(), 8);
        assert_eq!(ThawKind::ComplaintsDeposit.lx(), 13);
        assert!(ThawKind::T1Blocked.draws_blocked());
        assert!(ThawKind::AutoUnfreeze.draws_blocked());
        assert!(!ThawKind::ComplaintsDeposit.draws_blocked());
    }

    #[test]
    fn blocked_thaw_moves_bucket_and_flows_on_available() {
        let r = apply(
            ThawKind::T1Blocked,
            101,
            ThawBuckets {
                available: 5_000_000,
                blocked: 994_000,
            },
            994_000,
            Some("P2026".into()),
        )
        .unwrap();
        assert_eq!(
            r.buckets_after,
            ThawBuckets {
                available: 5_994_000,
                blocked: 0
            }
        );
        // the flow snapshots the available bucket, lx = 8
        assert_eq!(r.flow.y_money, 5_000_000);
        assert_eq!(r.flow.g_money, 5_994_000);
        assert_eq!(r.flow.money, 994_000);
        assert_eq!(r.flow.lx, 8);
    }

    #[test]
    fn blocked_thaw_guard_rejects_overdraft() {
        let before = ThawBuckets {
            available: 0,
            blocked: 100,
        };
        // asking to thaw more than is blocked fails (0-row UPDATE)
        assert_eq!(
            apply(ThawKind::AutoUnfreeze, 1, before, 200, None),
            Err(before)
        );
        // exactly the blocked amount succeeds
        assert!(apply(ThawKind::AutoUnfreeze, 1, before, 100, None).is_ok());
    }

    #[test]
    fn deposit_release_only_adds_to_available() {
        let r = apply(
            ThawKind::ComplaintsDeposit,
            101,
            ThawBuckets {
                available: 0,
                blocked: 0,
            },
            50_000,
            None,
        )
        .unwrap();
        assert_eq!(
            r.buckets_after,
            ThawBuckets {
                available: 50_000,
                blocked: 0
            }
        );
        assert_eq!(r.flow.lx, 13);
        assert_eq!(r.flow.g_money, 50_000);
    }

    #[test]
    fn t1_due_requires_arrival_and_prior_creation() {
        let today = 1_000_000;
        // thawtime within the +7200 buffer, created before today → due
        assert!(t1_blockedlog_due(today + 7_200, today - 1, today));
        // thawtime too far out → not due
        assert!(!t1_blockedlog_due(today + 7_201, today - 1, today));
        // created today → not yet (only yesterday-and-earlier rows release)
        assert!(!t1_blockedlog_due(today, today, today));
    }

    #[test]
    fn thaw_window_is_half_open_with_zero_meaning_no_limit() {
        assert!(in_thaw_window(3, 1, 5));
        assert!(!in_thaw_window(5, 1, 5)); // end-exclusive
        assert!(!in_thaw_window(0, 1, 5));
        assert!(in_thaw_window(23, 0, 0)); // end 0 → unrestricted
    }

    #[test]
    fn unfreeze_due_at_exact_boundary() {
        assert!(unfreeze_due(100, 100)); // now == unfreeze_time is due
        assert!(unfreeze_due(101, 100));
        assert!(!unfreeze_due(99, 100));
    }

    #[test]
    fn sweep_report_partitions_the_scan() {
        let r = SweepReport {
            scanned: 3,
            released: 2,
            skipped: 1,
        };
        assert_eq!(r.released + r.skipped, r.scanned);
        assert_eq!(SweepReport::default().scanned, 0);
    }
}
