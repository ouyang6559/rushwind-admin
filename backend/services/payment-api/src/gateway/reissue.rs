//! The reissue sweep — `RepostController::postUrl` re-stated (`spec/02`
//! §7.1): the cron-hit scan over orders that are settled (`pay_status = 1`,
//! money is in the ledger) but whose merchant never answered `ok`. Each
//! due order gets its merchant notify re-POSTed, its attempt counter
//! bumped, and the sweep moves on — one POST failing never aborts the
//! batch (the legacy serial `foreach`).
//!
//! Two faithful constants and one hardening:
//! * cap `postnum` 5 and gap 10s ([`MIN_REISSUE_INTERVAL_SECS`]) — the
//!   admission of §7.1, mirroring the pure [`crate::ledger::reissue_admit`];
//! * batch limit 50 rows per run, oldest id first;
//! * the legacy consumed attempts AFTER sending with an unprotected
//!   `num+1` write, so two concurrent crons double-POSTed (§7 并发保护缺口).
//!   Here each attempt is CLAIMED first via [`LedgerService::claim_reissue`]
//!   (the whole predicate as one CAS) and only the winner sends —
//!   registered in the 语义差异清单 as an intentional fix.

use serde::Serialize;

use crate::gateway::callback::attr_escape;
use crate::gateway::notify::{MerchantNotifier, NotifyOutcome};
use crate::ledger::{LedgerService, PayStatus};
use crate::state::GatewayResult;

/// One sweep's knobs. Defaults are the legacy values
/// (`config('PLANNING.postnum') ?: 5`, `limit(50)`, §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepPolicy {
    /// `postnum` — attempts per order before it leaves the sweep set.
    pub max_attempts: i32,
    /// Rows per run (`limit(50)`).
    pub batch: u64,
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            batch: 50,
        }
    }
}

/// Per-run accounting, logged in full and returned nowhere else (the
/// legacy cron only ever saw the leading `ok`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    /// Orders whose attempt this run claimed (POSTs actually made).
    pub claimed: usize,
    /// Of those, merchants whose reply carried `ok` (order now at 2).
    pub acked: usize,
    /// Claimed but still unconfirmed (no-`ok` reply, unreachable, error) —
    /// the attempt is spent, the order stays at 1 for the next window.
    pub pending: usize,
    /// Due hints whose claim was lost to a racing sweep — skipped silent.
    pub raced: usize,
    /// The claimed orders' ids, in sweep order (observability; the shared
    /// DB's test suite asserts against exactly this, not the counters).
    pub targets: Vec<String>,
}

/// One serial sweep: list due, claim, re-POST through the notifier.
/// `now` is injected so the 10s window is deterministic under test.
pub async fn run_sweep(
    ledger: &LedgerService,
    notifier: &MerchantNotifier,
    policy: &SweepPolicy,
    now: i64,
) -> GatewayResult<SweepReport> {
    let due = ledger
        .due_reissues(policy.max_attempts, now, policy.batch)
        .await?;
    let mut report = SweepReport::default();
    for order_id in due {
        if !ledger
            .claim_reissue(&order_id, policy.max_attempts, now)
            .await?
        {
            report.raced += 1;
            continue;
        }
        report.claimed += 1;
        report.targets.push(order_id.clone());
        let acked = match notifier.notify_order(&order_id).await {
            Ok(NotifyOutcome::Replied { acked, .. }) => acked,
            Ok(other) => {
                tracing::debug!(%order_id, ?other, "reissue notify not delivered");
                false
            }
            Err(e) => {
                tracing::warn!(%order_id, error = ?e, "reissue notify error");
                false
            }
        };
        if acked {
            report.acked += 1;
        } else {
            report.pending += 1;
        }
    }
    Ok(report)
}

/// `PayController::bufa` (P:702-717, `spec/03` §9.2) — the manual
/// one-order repost behind the admin order page's「补发通知」link:
/// `GET /Pay_Pay_bufa?TransID=..&tongdao=..`. The gate is exactly the
/// legacy `intval(getField('pay_status')) == 1` — unknown orders and
/// statuses 0/2 all answer 补发失败. An admitted order re-POSTs the
/// merchant notify WITHOUT spending an attempt (no `num+1`, no claim:
/// postUrl's counter write lived in its loop, which bufa never shared) —
/// a deliberate overlap with the sweep's §7 并发保护缺口 that the manual
/// button keeps, since merchants dedupe by orderid either way.
///
/// The legacy echoed the raw `$_GET` values straight into the page (an
/// XSS surface); the text here escapes both, joining the 语义差异清单.
pub fn bufa_text(trans_id: &str, tongdao: &str) -> String {
    format!(
        "订单号：{}|{}已补发服务器点对点通知，请稍后刷新查看结果！<a href='javascript:window.close();'>关闭</a>",
        attr_escape(trans_id),
        attr_escape(tongdao)
    )
}

/// The bufa gate: does `TransID` exist and sit at `pay_status = 1`?
/// (The echo-vs-EditMoney ordering stays in the handler: legacy printed
/// the 已补发 line BEFORE running the notify.)
pub async fn bufa_admit(ledger: &LedgerService, trans_id: &str) -> GatewayResult<bool> {
    Ok(ledger
        .find_order(trans_id)
        .await?
        .is_some_and(|o| o.status == PayStatus::Paid.code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bufa_text_keeps_the_legacy_line_and_escapes_input() {
        let t = bufa_text("E2018.1216", "WxSm");
        assert!(t.starts_with("订单号：E2018.1216|WxSm已补发服务器点对点通知"));
        assert!(t.ends_with("<a href='javascript:window.close();'>关闭</a>"));
        // Raw $_GET echo was the legacy; quotes/tags come back escaped.
        let t = bufa_text(r#"x" onload="alert(1)"#, "<b>ch</b>");
        assert!(!t.contains(r#"onload=""#));
        assert!(t.contains("&quot;"));
        assert!(t.contains("&lt;b&gt;"));
    }

    #[test]
    fn defaults_are_the_legacy_constants() {
        let p = SweepPolicy::default();
        assert_eq!(p.max_attempts, 5); // config('PLANNING.postnum') ?: 5
        assert_eq!(p.batch, 50); // limit(50)
    }

    #[test]
    fn report_counts_are_additive() {
        let mut r = SweepReport::default();
        r.claimed += 1;
        r.pending += 1;
        r.raced += 1;
        r.targets.push("O1".into());
        assert_eq!(
            r,
            SweepReport {
                claimed: 1,
                acked: 0,
                pending: 1,
                raced: 1,
                targets: vec!["O1".into()],
            },
        );
    }
}
