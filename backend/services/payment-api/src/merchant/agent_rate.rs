//! Agent downline rate configuration (`spec/05` §6.2, the write side of
//! `User/AgentController::saveUserRate`). A level-N agent prices its OWN
//! downline merchants per product (`payapiid`), never below its own cost —
//! the margin it keeps on every downstream transaction. This is the write
//! counterpart to [`crate::rate`]'s read-side `huoqufeilv` resolution.
//!
//! The legacy flow (`AgentController::saveUserRate` L337-368) is reproduced
//! faithfully:
//! 1. per submitted product, the agent's OWN `userrate` row (`userid = the
//!    agent, payapiid = product`) is the cost FLOOR; a submitted `feilv`
//!    below it rejects the WHOLE batch (`T+1费率不能低于代理成本！`, then the
//!    same for `t0feilv` → `T+0费率不能低于代理成本！`) — validation is
//!    all-or-nothing and runs before any write, so nothing persists on a
//!    violation;
//! 2. surviving rows upsert into the downline merchant's `userrate`
//!    (update the existing `(userid, payapiid)` row, else insert).
//!
//! Deliberate legacy quirks kept (registered as a decision memory):
//! - a MISSING agent cost row makes the floor `0`, so ANY non-negative
//!   submitted rate passes — the `item.feilv < null` PHP comparison is
//!   "形同虚设" (§6.2 caveat), reproduced not "fixed";
//! - the floor uses strict `<` (an equal rate is allowed), unlike the
//!   front-end `checkUserrate` preview which flags `>=`; the save path is
//!   authoritative, so only the `<` semantics are enforced here.

use std::collections::HashMap;

use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
};

use crate::data::{product_users, products, user_rates};
use crate::state::{db_err, GatewayResult};

/// One product's submitted rate override, every value already parsed to the
/// crate's integer representation: `rate` / `t0_rate` are `RATE_SCALE`-scaled
/// fractions, `fengding` / `t0_fengding` are money units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRow {
    /// The product id (legacy `payapiid`, our `user_rates.channel_id`).
    pub product_id: i64,
    pub rate: i64,
    pub fengding: i64,
    pub t0_rate: i64,
    pub t0_fengding: i64,
}

/// The agent's own cost for one product — the floor a downline rate may not
/// dip below. Absent product → [`AgentCost::default`] (all zero), matching the
/// legacy's missing-row `null → 0` comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentCost {
    pub rate: i64,
    pub t0_rate: i64,
}

/// The first rate that breaches the agent's cost, with the exact legacy
/// message. The whole batch aborts on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateViolation {
    pub product_id: i64,
    pub msg: &'static str,
}

/// The `T+1` cost-floor message (verbatim from `saveUserRate:353`).
pub const MSG_T1_BELOW_COST: &str = "T+1费率不能低于代理成本！";
/// The `T+0` cost-floor message (verbatim from `saveUserRate:356`).
pub const MSG_T0_BELOW_COST: &str = "T+0费率不能低于代理成本！";

/// The strict cost floor: a rate below the agent's own cost is rejected.
/// A missing cost row floors at `0`, so any non-negative rate passes (the
/// faithful reading of the legacy `feilv < null`).
fn below_cost(submitted: i64, cost: i64) -> bool {
    submitted < cost
}

/// Validates the whole batch against the agent's per-product cost in the
/// submitted order: the FIRST offending product wins, checking `T+1`
/// (`rate`) before `T+0` (`t0_rate`) for each product — the exact order of
/// the legacy `saveUserRate` loop. `None` means every row may be written.
pub fn validate_rate_batch(
    rows: &[RateRow],
    agent_costs: &HashMap<i64, AgentCost>,
) -> Option<RateViolation> {
    for row in rows {
        let cost = agent_costs
            .get(&row.product_id)
            .copied()
            .unwrap_or_default();
        if below_cost(row.rate, cost.rate) {
            return Some(RateViolation {
                product_id: row.product_id,
                msg: MSG_T1_BELOW_COST,
            });
        }
        if below_cost(row.t0_rate, cost.t0_rate) {
            return Some(RateViolation {
                product_id: row.product_id,
                msg: MSG_T0_BELOW_COST,
            });
        }
    }
    None
}

/// Runs the §6.2 write: loads the agent's own cost for every submitted
/// product, validates the batch (all-or-nothing), and — only on a clean pass
/// — upserts each row into the downline merchant's `userrate` inside one
/// transaction. The caller has already checked identity / ownership
/// (the downline exists and is this agent's direct child); this function
/// assumes `downline_id` is legitimately managed by `agent_id`.
///
/// Returns `Ok(Ok(()))` after a write, `Ok(Err(violation))` when a rate
/// breaches the cost floor (nothing was written), or `Err` on a DB failure.
pub async fn apply_agent_rates(
    db: &sea_orm::DatabaseConnection,
    agent_id: i64,
    downline_id: i64,
    rows: &[RateRow],
) -> GatewayResult<Result<(), RateViolation>> {
    // Load the agent's own cost rows for the submitted products (§6.2
    // `agent_rate = Userrate where userid = agent, payapiid = key`). The
    // legacy `find()` keeps one row per product; a duplicate collapses to
    // the first-seen (lowest id) here.
    let product_ids: Vec<i64> = rows.iter().map(|r| r.product_id).collect();
    let costs: HashMap<i64, AgentCost> = if product_ids.is_empty() {
        HashMap::new()
    } else {
        user_rates::Entity::find()
            .filter(user_rates::Column::UserId.eq(agent_id))
            .filter(user_rates::Column::ChannelId.is_in(product_ids))
            .order_by_asc(user_rates::Column::Id)
            .all(db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|m| {
                (
                    m.channel_id,
                    AgentCost {
                        rate: m.rate,
                        t0_rate: m.t0_rate,
                    },
                )
            })
            .collect()
    };

    if let Some(violation) = validate_rate_batch(rows, &costs) {
        return Ok(Err(violation));
    }

    // All rows pass → upsert each into the downline. One transaction keeps the
    // batch atomic (the legacy's validate-then-`addAll` all-or-nothing shape).
    let txn = db.begin().await.map_err(db_err)?;
    for row in rows {
        let existing = user_rates::Entity::find()
            .filter(user_rates::Column::UserId.eq(downline_id))
            .filter(user_rates::Column::ChannelId.eq(row.product_id))
            .one(&txn)
            .await
            .map_err(db_err)?;
        match existing {
            Some(model) => {
                let mut active: user_rates::ActiveModel = model.into();
                active.rate = Set(row.rate);
                active.fengding = Set(row.fengding);
                active.t0_rate = Set(row.t0_rate);
                active.t0_fengding = Set(row.t0_fengding);
                active.update(&txn).await.map_err(db_err)?;
            }
            None => {
                user_rates::ActiveModel {
                    id: NotSet,
                    user_id: Set(downline_id),
                    channel_id: Set(row.product_id),
                    rate: Set(row.rate),
                    fengding: Set(row.fengding),
                    t0_rate: Set(row.t0_rate),
                    t0_fengding: Set(row.t0_fengding),
                }
                .insert(&txn)
                .await
                .map_err(db_err)?;
            }
        }
    }
    txn.commit().await.map_err(db_err)?;
    Ok(Ok(()))
}

// --- §6.2 read side (`User/AgentController::userRateEdit`) ------------------

/// One row of the 下级费率编辑页: an OPENED, displayed product of the downline
/// with that child's current rate overrides (all in the crate's integer
/// representation — `rate` / `t0_rate` `RATE_SCALE`-scaled, `fengding` /
/// `t0_fengding` money units). A product with no `userrate` row yet carries
/// `0` across the board (the legacy `'0.000'` placeholder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableProduct {
    pub product_id: i64,
    pub name: String,
    pub rate: i64,
    pub fengding: i64,
    pub t0_rate: i64,
    pub t0_fengding: i64,
}

/// Pure merge: pair the ordered opened-product list with the child's rate map
/// (keyed by product id), defaulting a missing row to zeros. Exposed for
/// offline testing of the `'0.000'`-placeholder shape.
pub fn assemble_rate_rows(
    prods: &[(i64, String)],
    rate_of: &HashMap<i64, (i64, i64, i64, i64)>,
) -> Vec<EditableProduct> {
    prods
        .iter()
        .map(|(id, name)| {
            let (rate, fengding, t0_rate, t0_fengding) =
                rate_of.get(id).copied().unwrap_or_default();
            EditableProduct {
                product_id: *id,
                name: name.clone(),
                rate,
                fengding,
                t0_rate,
                t0_fengding,
            }
        })
        .collect()
}

/// Loads the §6.2 edit page for a downline child: its ENABLED products
/// (`product_users.status = 1` for this child) that are also live + displayed
/// (`products.status = 1 AND isdisplay = 1`), each merged with the child's
/// current `userrate` override. The caller has already verified the target is
/// the acting agent's direct child (the same ownership gate `save_user_rate`
/// runs); products ordered by id ascending (the legacy `select()` default).
pub async fn downline_rate_edit(
    db: &sea_orm::DatabaseConnection,
    child_id: i64,
) -> GatewayResult<Vec<EditableProduct>> {
    // The child's enabled product ids (legacy `pay_product_user` join leg).
    let enabled: Vec<i64> = product_users::Entity::find()
        .filter(product_users::Column::UserId.eq(child_id))
        .filter(product_users::Column::Status.eq(1))
        .all(db)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|pu| pu.pid)
        .collect();
    if enabled.is_empty() {
        return Ok(Vec::new());
    }
    // Keep only live + displayed products.
    let prods: Vec<(i64, String)> = products::Entity::find()
        .filter(products::Column::Status.eq(1))
        .filter(products::Column::Isdisplay.eq(1))
        .filter(products::Column::Id.is_in(enabled))
        .order_by_asc(products::Column::Id)
        .all(db)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    // The child's current rate overrides, keyed by product id.
    let rate_of: HashMap<i64, (i64, i64, i64, i64)> = user_rates::Entity::find()
        .filter(user_rates::Column::UserId.eq(child_id))
        .all(db)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|m| (m.channel_id, (m.rate, m.fengding, m.t0_rate, m.t0_fengding)))
        .collect();
    Ok(assemble_rate_rows(&prods, &rate_of))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(product: i64, rate: i64, t0_rate: i64) -> RateRow {
        RateRow {
            product_id: product,
            rate,
            fengding: 0,
            t0_rate,
            t0_fengding: 0,
        }
    }

    fn costs(pairs: &[(i64, i64, i64)]) -> HashMap<i64, AgentCost> {
        pairs
            .iter()
            .map(|(pid, r, t0)| {
                (
                    *pid,
                    AgentCost {
                        rate: *r,
                        t0_rate: *t0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_rate_at_or_above_cost_passes() {
        let c = costs(&[(10, 6_000, 8_000)]);
        // exactly the cost passes (strict `<`); above passes too
        assert_eq!(validate_rate_batch(&[row(10, 6_000, 8_000)], &c), None);
        assert_eq!(validate_rate_batch(&[row(10, 7_000, 9_000)], &c), None);
    }

    #[test]
    fn below_cost_rejects_t1_then_t0() {
        let c = costs(&[(10, 6_000, 8_000)]);
        // T+1 below its floor → T+1 message (checked first)
        assert_eq!(
            validate_rate_batch(&[row(10, 5_999, 8_000)], &c),
            Some(RateViolation {
                product_id: 10,
                msg: MSG_T1_BELOW_COST
            })
        );
        // T+1 fine but T+0 below → T+0 message
        assert_eq!(
            validate_rate_batch(&[row(10, 6_000, 7_999)], &c),
            Some(RateViolation {
                product_id: 10,
                msg: MSG_T0_BELOW_COST
            })
        );
    }

    #[test]
    fn a_missing_agent_cost_floors_at_zero_and_passes_everything() {
        // The §6.2 caveat: no `userrate` row for the agent → floor 0 → even a
        // submitted 0 passes (only a negative could trip it, and rates are ≥0).
        let c: HashMap<i64, AgentCost> = HashMap::new();
        assert_eq!(validate_rate_batch(&[row(99, 0, 0)], &c), None);
    }

    #[test]
    fn the_first_offending_product_in_order_wins() {
        let c = costs(&[(10, 6_000, 8_000), (20, 5_000, 5_000)]);
        // product 10 is fine, product 20 is below → 20 reported
        let rows = [row(10, 6_000, 8_000), row(20, 4_000, 5_000)];
        assert_eq!(
            validate_rate_batch(&rows, &c),
            Some(RateViolation {
                product_id: 20,
                msg: MSG_T1_BELOW_COST
            })
        );
    }

    #[test]
    fn an_empty_batch_validates_clean() {
        assert_eq!(
            validate_rate_batch(&[], &costs(&[(10, 6_000, 8_000)])),
            None
        );
    }

    #[test]
    fn below_cost_is_strict() {
        assert!(!below_cost(6_000, 6_000));
        assert!(below_cost(5_999, 6_000));
        assert!(!below_cost(0, 0));
    }

    #[test]
    fn assemble_fills_current_rates_and_defaults_missing_to_zero() {
        let prods = vec![
            (10, "支付宝".to_string()),
            (20, "微信".to_string()),
            (30, "银联".to_string()),
        ];
        let mut rate_of = HashMap::new();
        // product 10 has an override; 30 does not.
        rate_of.insert(10, (6_000i64, 500i64, 8_000i64, 600i64));
        let rows = assemble_rate_rows(&prods, &rate_of);
        assert_eq!(rows.len(), 3, "one row per opened product, in order");
        assert_eq!(rows[0].product_id, 10);
        assert_eq!(rows[0].rate, 6_000);
        assert_eq!(rows[0].t0_fengding, 600);
        // A product with no userrate row → the legacy '0.000' placeholder.
        assert_eq!(
            rows[2],
            EditableProduct {
                product_id: 30,
                name: "银联".to_string(),
                rate: 0,
                fengding: 0,
                t0_rate: 0,
                t0_fengding: 0,
            }
        );
    }
}
