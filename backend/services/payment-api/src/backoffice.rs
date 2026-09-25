//! Back-office member operations — the interface shells for
//! `Admin/UserController::saveUser` (add), `incrMoney` (manual +/-, §11) and
//! `frozenMoney` (freeze/unfreeze, §11). The balance *arithmetic and guards*
//! are pure and offline-tested here ([`apply_op`]); the actual persistence is
//! an atomic [`crate::ledger`] move plus a `money_changes` row
//! ([`LedgerService::admin_move`], the four iron rules), and the member
//! insert reuses the registration writer. The admin JWT/RBAC HTTP gate that
//! will call these remains the Phase-7 seam — until then nothing exposes
//! them, and no path performs a partial write.

use crate::ledger::AdminMove;
use crate::merchant::register::{self, NewMember, SiteFlags};
use crate::merchant::MembersRepo;
use crate::state::{AppState, GatewayError, GatewayResult};

/// A manual balance move, tagged with the legacy `lx` flow type
/// (`spec/05` §11: 3 手动增加 / 4 手动减少 / 7 冻结 / 8 解冻).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceOp {
    ManualAdd,
    ManualSub,
    Freeze,
    Unfreeze,
}

impl BalanceOp {
    /// The `money_changes.lx` this move writes.
    pub fn lx(self) -> i32 {
        match self {
            BalanceOp::ManualAdd => 3,
            BalanceOp::ManualSub => 4,
            BalanceOp::Freeze => 7,
            BalanceOp::Unfreeze => 8,
        }
    }
}

/// A member's two balances, in money units (1/10000 元).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    /// Available balance (`member.balance`).
    pub available: i64,
    /// T+1 blocked balance (`member.blocked_balance`).
    pub blocked: i64,
}

/// A rejected balance move (the guarded pre-conditions of `incrMoney` /
/// `frozenMoney`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceError {
    /// `$bgmoney > 0` — a non-positive amount.
    NonPositiveAmount,
    /// The debit/freeze would overdraw the available balance.
    InsufficientAvailable,
    /// The unfreeze would overdraw the blocked balance.
    InsufficientBlocked,
}

impl BalanceError {
    /// The caller-facing legacy message.
    pub fn message(self) -> &'static str {
        match self {
            BalanceError::NonPositiveAmount => "金额必须大于0",
            BalanceError::InsufficientAvailable => "可用余额不足",
            BalanceError::InsufficientBlocked => "冻结余额不足",
        }
    }
}

/// The pure guarded move: `amount <= 0` is rejected; a debit/freeze needs
/// enough available balance; an unfreeze needs enough blocked balance and
/// moves it back to available. In production this is an atomic SQL under the
/// `Member` lock (`ledger` iron rules); here it is the arithmetic the ledger
/// will reproduce, so the outcome is unit-tested without a database.
pub fn apply_op(from: Balance, op: BalanceOp, amount: i64) -> Result<Balance, BalanceError> {
    if amount <= 0 {
        return Err(BalanceError::NonPositiveAmount);
    }
    match op {
        BalanceOp::ManualAdd => Ok(Balance {
            available: from.available + amount,
            blocked: from.blocked,
        }),
        BalanceOp::ManualSub => {
            if from.available < amount {
                Err(BalanceError::InsufficientAvailable)
            } else {
                Ok(Balance {
                    available: from.available - amount,
                    blocked: from.blocked,
                })
            }
        }
        BalanceOp::Freeze => {
            if from.available < amount {
                Err(BalanceError::InsufficientAvailable)
            } else {
                Ok(Balance {
                    available: from.available - amount,
                    blocked: from.blocked + amount,
                })
            }
        }
        BalanceOp::Unfreeze => {
            if from.blocked < amount {
                Err(BalanceError::InsufficientBlocked)
            } else {
                Ok(Balance {
                    available: from.available + amount,
                    blocked: from.blocked - amount,
                })
            }
        }
    }
}

/// The "add user" record assembly (`Admin/UserController::saveUser` reuses
/// `generateUser`): a merchant defaults to groupid 4 under the platform
/// (parentid 1); the caller supplies the chosen tier/parent. Persistence and
/// the password email go through [`add_user`].
pub fn plan_add_user(
    username: &str,
    password_plaintext: &str,
    email: &str,
    groupid: i32,
    parentid: i64,
    flags: &SiteFlags,
) -> NewMember {
    register::build_member_record(
        username,
        password_plaintext,
        email,
        groupid,
        parentid,
        flags,
    )
}

/// Runs [`apply_op`] purely to reject a bad request, then defers the write.
fn guard(current: Balance, op: BalanceOp, amount: i64) -> Result<(), GatewayError> {
    apply_op(current, op, amount)
        .map(|_| ())
        .map_err(|e| GatewayError::BadRequest(e.message().to_string()))
}

/// Adds a member through the registration writer (the record is assembled
/// and validated upstream by [`plan_add_user`]; `register_member`'s
/// site-flag gates are the self-service path — the back office inserts
/// directly). Exposing this over authenticated HTTP is the Phase-7 seam.
pub async fn add_user(state: &AppState, row: &NewMember) -> GatewayResult<i64> {
    let repo = MembersRepo::new(&state.db);
    repo.create(row).await.map_err(crate::state::db_err)
}

/// Manual balance increase (`cztype==3`) / decrease (`cztype==4`). The pure
/// guard answers the legacy message first; the ledger performs the atomic
/// move + `money_changes(lx)` write in one transaction.
pub async fn incr_money(
    state: &AppState,
    user_id: i64,
    increase: bool,
    amount: i64,
    current: Balance,
) -> GatewayResult<()> {
    let op = if increase {
        BalanceOp::ManualAdd
    } else {
        BalanceOp::ManualSub
    };
    guard(current, op, amount)?;
    let mv = if increase {
        AdminMove::ManualAdd
    } else {
        AdminMove::ManualSub
    };
    state
        .ledger
        .admin_move(mv, user_id, amount)
        .await?
        .ok_or_else(|| GatewayError::BadRequest("可用余额不足".into()))?;
    Ok(())
}

/// Freeze (`cztype==7`, available→blocked) / unfreeze (`cztype==8`,
/// blocked→available). The pure guard answers first; the ledger performs the
/// move (and writes the lx=7/8 flow) in one transaction. The optional
/// scheduled `auto_unfrozen_order` release of the legacy frozen order stays
/// with the order domain, not the raw balance move.
pub async fn frozen_money(
    state: &AppState,
    user_id: i64,
    freeze: bool,
    amount: i64,
    current: Balance,
) -> GatewayResult<()> {
    let op = if freeze {
        BalanceOp::Freeze
    } else {
        BalanceOp::Unfreeze
    };
    guard(current, op, amount)?;
    let mv = if freeze {
        AdminMove::Freeze
    } else {
        AdminMove::Unfreeze
    };
    state
        .ledger
        .admin_move(mv, user_id, amount)
        .await?
        .ok_or_else(|| {
            GatewayError::BadRequest(if freeze {
                "可用余额不足".into()
            } else {
                "冻结余额不足".into()
            })
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bal(available: i64, blocked: i64) -> Balance {
        Balance { available, blocked }
    }

    #[test]
    fn non_positive_amount_is_always_rejected() {
        for op in [
            BalanceOp::ManualAdd,
            BalanceOp::ManualSub,
            BalanceOp::Freeze,
            BalanceOp::Unfreeze,
        ] {
            assert_eq!(
                apply_op(bal(100, 50), op, 0),
                Err(BalanceError::NonPositiveAmount)
            );
            assert_eq!(
                apply_op(bal(100, 50), op, -5),
                Err(BalanceError::NonPositiveAmount)
            );
        }
    }

    #[test]
    fn freeze_and_unfreeze_move_between_buckets() {
        assert_eq!(
            apply_op(bal(100, 20), BalanceOp::Freeze, 30),
            Ok(bal(70, 50))
        );
        assert_eq!(
            apply_op(bal(70, 50), BalanceOp::Unfreeze, 50),
            Ok(bal(120, 0))
        );
        // freezing more than available / unfreezing more than blocked fails
        assert_eq!(
            apply_op(bal(10, 0), BalanceOp::Freeze, 30),
            Err(BalanceError::InsufficientAvailable)
        );
        assert_eq!(
            apply_op(bal(100, 5), BalanceOp::Unfreeze, 30),
            Err(BalanceError::InsufficientBlocked)
        );
    }

    #[test]
    fn manual_add_sub_and_sufficiency() {
        assert_eq!(
            apply_op(bal(100, 0), BalanceOp::ManualAdd, 25),
            Ok(bal(125, 0))
        );
        assert_eq!(
            apply_op(bal(100, 0), BalanceOp::ManualSub, 100),
            Ok(bal(0, 0))
        );
        assert_eq!(
            apply_op(bal(50, 0), BalanceOp::ManualSub, 51),
            Err(BalanceError::InsufficientAvailable)
        );
    }

    #[test]
    fn lx_codes_match_legacy_flow_types() {
        assert_eq!(BalanceOp::ManualAdd.lx(), 3);
        assert_eq!(BalanceOp::ManualSub.lx(), 4);
        assert_eq!(BalanceOp::Freeze.lx(), 7);
        assert_eq!(BalanceOp::Unfreeze.lx(), 8);
    }

    #[test]
    fn plan_reuses_registration_assembly() {
        let row = plan_add_user("mch9", "pw", "a@b.co", 5, 1, &SiteFlags::default());
        assert_eq!(row.groupid, 5);
        assert_eq!(row.parentid, 1);
        assert_eq!(row.status, 1);
        assert_eq!(row.apikey.len(), 32);
    }
}
