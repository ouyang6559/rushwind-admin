//! Rate resolution — the per-order 费率/封顶 selection the legacy
//! `PayController::huoqufeilv` performs (`spec/05-merchant-agent.md` §7.2,
//! `spec/03-pay-gateway-channel.md` §4.2 step 2).
//!
//! Priority (verbatim from the PHP `?:` chain, which treats a stored `0` as
//! "unset" and falls through):
//!
//! ```text
//! T+0: feilv = Userrate.t0_rate     ?: Channel.t0_default_rate
//!      封顶  = Userrate.t0_fengding ?: Channel.t0_fengding
//! T+1: feilv = Userrate.rate        ?: Channel.default_rate
//!      封顶  = Userrate.fengding    ?: Channel.fengding
//! ```
//!
//! There is no product/platform fallback row: the product reaches its rate
//! through its bound channel. A sub-account's `custom_rate` override is
//! applied at selection time (Phase 2), on top of the channel default.
//!
//! The 封顶 (`fengding`) is an ABSOLUTE per-transaction fee ceiling in money
//! units (`0` = no ceiling, the legacy `?: 9999999` sentinel), NOT a second
//! rate — recorded here as an intentional reading of the legacy intent vs the
//! `spec/03` `pay_amount*fengding` transcription (both live in the Phase-7
//! "语义差异清单").

use sea_orm::DatabaseConnection;

use crate::data::{channels, user_rates};
use crate::money::{cap_fee, fee_units};

/// The settlement cycle carried on the order's `t` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cycle {
    /// `t == 0` — funds land in the available balance immediately.
    T0,
    /// `t != 0` (legacy `t == 1`) — funds land in the blocked balance.
    T1,
}

impl Cycle {
    /// Reads the order `t` flag (`0` → [`Cycle::T0`], anything else → [`Cycle::T1`],
    /// matching the PHP `if ($t == 0) {..} else {..}`).
    pub fn from_t(t: i32) -> Self {
        if t == 0 {
            Cycle::T0
        } else {
            Cycle::T1
        }
    }
}

/// A merchant's own `pay_userrate` row (0 fields fall through to the channel).
#[derive(Debug, Clone, Copy, Default)]
pub struct UserRate {
    pub rate: i64,
    pub fengding: i64,
    pub t0_rate: i64,
    pub t0_fengding: i64,
}

impl UserRate {
    pub fn from_model(m: &user_rates::Model) -> Self {
        Self {
            rate: m.rate,
            fengding: m.fengding,
            t0_rate: m.t0_rate,
            t0_fengding: m.t0_fengding,
        }
    }
}

/// The channel's default rates / caps (`pay_channel`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelRate {
    pub default_rate: i64,
    pub fengding: i64,
    pub t0_default_rate: i64,
    pub t0_fengding: i64,
}

impl ChannelRate {
    pub fn from_model(m: &channels::Model) -> Self {
        Self {
            default_rate: m.default_rate,
            fengding: m.fengding,
            t0_default_rate: m.t0_default_rate,
            t0_fengding: m.t0_fengding,
        }
    }
}

/// The resolved rate snapshot for one order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRate {
    /// The applied fee rate, RATE_SCALE-scaled.
    pub feilv: i64,
    /// The applied fee ceiling in money units (`0` = none).
    pub fengding: i64,
}

/// `?:` on a stored integer — a `0` is "unset" and the fallback wins.
fn first_set(primary: i64, fallback: i64) -> i64 {
    if primary != 0 {
        primary
    } else {
        fallback
    }
}

/// Resolves the effective rate for `cycle`, preferring the merchant's own
/// `userrate` and falling back to the channel default (both may be absent, in
/// which case `user` is `None`).
pub fn resolve(cycle: Cycle, user: Option<&UserRate>, channel: &ChannelRate) -> ResolvedRate {
    let zero = UserRate::default();
    let u = user.unwrap_or(&zero);
    match cycle {
        Cycle::T0 => ResolvedRate {
            feilv: first_set(u.t0_rate, channel.t0_default_rate),
            fengding: first_set(u.t0_fengding, channel.t0_fengding),
        },
        Cycle::T1 => ResolvedRate {
            feilv: first_set(u.rate, channel.default_rate),
            fengding: first_set(u.fengding, channel.fengding),
        },
    }
}

impl ResolvedRate {
    /// The order's 手续费 (fee) in money units: `round(amount*feilv)` capped at
    /// `fengding` (see [`crate::money::cap_fee`]; a `0` cap is no cap).
    pub fn poundage(&self, amount_units: i64) -> i64 {
        cap_fee(fee_units(amount_units, self.feilv), self.fengding)
    }

    /// The 实际到账 (actual amount credited) = `amount - fee`
    /// (`spec/03` §4.2 step 6, L157).
    pub fn actual_amount(&self, amount_units: i64) -> i64 {
        amount_units - self.poundage(amount_units)
    }
}

/// Loads the merchant's `userrate` for a channel, if any (used by the
/// gateway/order-add path in Phase 3; exposed here so the identity/rate
/// surface is complete).
pub async fn load_user_rate(
    db: &DatabaseConnection,
    user_id: i64,
    channel_id: i64,
) -> Result<Option<UserRate>, sea_orm::DbErr> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let row = user_rates::Entity::find()
        .filter(user_rates::Column::UserId.eq(user_id))
        .filter(user_rates::Column::ChannelId.eq(channel_id))
        .one(db)
        .await?;
    Ok(row.as_ref().map(UserRate::from_model))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch() -> ChannelRate {
        ChannelRate {
            default_rate: 6_000, // 0.6% (T+1)
            fengding: 50_000,    // 5元 cap (money units)
            t0_default_rate: 8_000,
            t0_fengding: 0, // no T0 cap
        }
    }

    #[test]
    fn falls_back_to_channel_when_no_user_rate() {
        let r = resolve(Cycle::T1, None, &ch());
        assert_eq!(
            r,
            ResolvedRate {
                feilv: 6_000,
                fengding: 50_000
            }
        );
    }

    #[test]
    fn user_rate_overrides_channel_per_cycle() {
        let u = UserRate {
            rate: 5_000,
            fengding: 0,
            t0_rate: 7_000,
            t0_fengding: 0,
        };
        // T+1 uses `rate` (5_000) but its fengding==0 falls back to channel 50_000
        assert_eq!(
            resolve(Cycle::T1, Some(&u), &ch()),
            ResolvedRate {
                feilv: 5_000,
                fengding: 50_000
            }
        );
        // T+0 uses `t0_rate` (7_000); both caps 0 → channel t0_fengding 0 → no cap
        assert_eq!(
            resolve(Cycle::T0, Some(&u), &ch()),
            ResolvedRate {
                feilv: 7_000,
                fengding: 0
            }
        );
    }

    #[test]
    fn zero_stored_user_field_is_treated_as_unset() {
        // PHP `0 ?: channel` → channel wins.
        let u = UserRate {
            rate: 0,
            fengding: 0,
            t0_rate: 0,
            t0_fengding: 0,
        };
        assert_eq!(resolve(Cycle::T1, Some(&u), &ch()).feilv, 6_000);
    }

    #[test]
    fn poundage_applies_rate_then_cap() {
        // 100元 (1_000_000 units) * 0.6% = 0.60元 (6_000 units), under the 5元 cap.
        let r = ResolvedRate {
            feilv: 6_000,
            fengding: 50_000,
        };
        assert_eq!(r.poundage(1_000_000), 6_000);
        assert_eq!(r.actual_amount(1_000_000), 994_000);

        // A large amount where the percentage fee exceeds the 5元 cap → capped.
        // 10000元 * 0.6% = 60元, cap 5元 (50_000 units) → fee = 5元.
        assert_eq!(r.poundage(100_000_000), 50_000);

        // Zero cap = no ceiling.
        let uncapped = ResolvedRate {
            feilv: 6_000,
            fengding: 0,
        };
        assert_eq!(uncapped.poundage(100_000_000), 600_000);
    }

    #[test]
    fn cycle_reads_t_flag() {
        assert_eq!(Cycle::from_t(0), Cycle::T0);
        assert_eq!(Cycle::from_t(1), Cycle::T1);
        assert_eq!(Cycle::from_t(2), Cycle::T1);
    }
}
