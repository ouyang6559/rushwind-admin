//! Money representation. The legacy schema stores amounts as
//! `decimal(15,4)` (元, four fractional digits). The rewrite stores every
//! money column as a lossless integer in units of 1/10000 元 (「百厘」,
//! `MONEY_SCALE = 10_000`), matching `spec/02-funds-order.md` §11.4
//! (`balance` migration = `balance * 10000`). Fee/rate percentages are
//! stored separately, scaled by 1e6 (see [`RATE_SCALE`]).
//!
//! Intermediate math widens to `i128` and rounds half-up to 2 decimal
//! places of 元 (i.e. to `CENT * 100` sub-units) exactly where the PHP
//! `round($x, 2)` did — see [`fee_units`].

/// Money unit: 1 unit == 1/10000 元 (lossless vs `decimal(15,4)`).
pub const MONEY_SCALE: i64 = 10_000;
/// Rate unit: percentages scaled by 1e6 (e.g. 0.6% == 6_000).
pub const RATE_SCALE: i64 = 1_000_000;

/// Parses a legacy decimal string amount (元) into money units
/// (1/10000 元). Rejects anything that is not a plain `[-]d+(.d+)?` so the
/// gateway never silently mis-scales a malformed `pay_amount`.
pub fn parse_yuan_to_units(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    // Keep the first 4 fractional digits (the legacy precision), right-padded.
    let mut frac = String::from(frac_part);
    while frac.len() < 4 {
        frac.push('0');
    }
    frac.truncate(4);
    let int: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    let frac_units: i64 = frac.parse().ok()?;
    let total = int.checked_mul(MONEY_SCALE)?.checked_add(frac_units)?;
    Some(if neg { -total } else { total })
}

/// Parses a legacy decimal rate string (a bare fraction, e.g. `0.0060` ==
/// 0.6%) into a [`RATE_SCALE`]-scaled integer (→ 6_000). Keeps the first 6
/// fractional digits (right-padded, extra truncated), mirroring the legacy
/// `decimal` rate columns; rejects anything that is not a plain `d+(.d+)?`.
/// A rate is never negative, so any leading sign / `-` is refused.
pub fn parse_rate_to_scaled(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let body = s.strip_prefix('+').unwrap_or(s);
    if body.contains('-') {
        return None;
    }
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    // Keep the first 6 fractional digits (the rate scale), right-padded.
    let mut frac = String::from(frac_part);
    while frac.len() < 6 {
        frac.push('0');
    }
    frac.truncate(6);
    let int: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    let frac_units: i64 = frac.parse().ok()?;
    int.checked_mul(RATE_SCALE)?.checked_add(frac_units)
}

/// Renders money units back to a legacy 4-decimal 元 string (for wire
/// responses such as the order-query body).
pub fn units_to_yuan(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    let yuan = abs / MONEY_SCALE as u64;
    let frac = abs % MONEY_SCALE as u64;
    format!("{sign}{yuan}.{frac:04}")
}

/// Renders money units to a 2-decimal 元 string for upstream channels that
/// price in cents (易支付 `money`, etc.), rounding half-up on the 3rd/4th
/// fractional digits (PHP `sprintf('%.2f', ..)`).
pub fn units_to_yuan_2dp(units: i64) -> String {
    // cents = round(units / 100) with half-up; 1 元 == 10_000 units == 100 cents.
    let cents = div_round_half_up(units as i128, 100);
    let sign = if cents < 0 { "-" } else { "" };
    let ac = cents.unsigned_abs();
    format!("{sign}{}.{:02}", ac / 100, ac % 100)
}

/// Money units → an integer 分 (cent) count, rounding half-up (legacy
/// `round(pay_amount,4) * 100`, the 分-denominated channels such as `Rzfkj`).
pub fn units_to_fen(units: i64) -> i64 {
    div_round_half_up(units as i128, 100) as i64
}

/// Fee = `round(amount * rate, 2元)` in money units, i.e. computed on the
/// widened `i128` product and rounded half-up to cents (2 dp of 元).
/// `amount` and the return are money units; `rate` is RATE_SCALE-scaled.
pub fn fee_units(amount_units: i64, rate_scaled: i64) -> i64 {
    // amount_units (1e-4元) * rate/1e6 → 1e-10元, then re-scale to 1e-4元
    // rounded to whole 元-cents (0.01元 == 100 units).
    let product = (amount_units as i128) * (rate_scaled as i128); // 1e-10 元
                                                                  // 元 = product / 1e10; cents(0.01元) = round(元 * 100) = round(product / 1e8).
    let cents = div_round_half_up(product, 100_000_000);
    (cents * 100) as i64 // 0.01元 == 100 money-units
}

/// `amount_units * (rate_scaled / RATE_SCALE)`, rounded half-up to whole
/// money units — i.e. the full 1e-4 元 precision, WITHOUT the 2-dp cent
/// clamping [`fee_units`] applies. This is the payout-fee form (the legacy
/// `bcdiv(bcmul(tkmoney, sxfrate, 4), 100, 4)` keeps 4 decimals), where
/// `sxfrate` percent is passed as a RATE_SCALE fraction (2% → 20_000).
pub fn scale_units(amount_units: i64, rate_scaled: i64) -> i64 {
    let product = (amount_units as i128) * (rate_scaled as i128); // 1e-10 元
    div_round_half_up(product, RATE_SCALE as i128) as i64 // → 1e-4 元 units
}

/// Caps a fee at `fengding` (both money units): the legacy
/// `min(amount*rate, amount*fengding)`; `fengding == 0` means "no cap"
/// upstream (mapped to a sentinel), so a zero cap never truncates here.
pub fn cap_fee(fee: i64, cap: i64) -> i64 {
    if cap > 0 && fee > cap {
        cap
    } else {
        fee
    }
}

/// Integer division rounding the remainder half-up (PHP `round` tie-break).
fn div_round_half_up(n: i128, d: i128) -> i128 {
    let q = n / d;
    let r = n % d;
    if r.abs() * 2 >= d.abs() {
        q + if n >= 0 { 1 } else { -1 }
    } else {
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yuan_to_units_matches_legacy_scale() {
        assert_eq!(parse_yuan_to_units("100"), Some(1_000_000));
        assert_eq!(parse_yuan_to_units("100.0000"), Some(1_000_000));
        assert_eq!(parse_yuan_to_units("0.01"), Some(100));
        assert_eq!(parse_yuan_to_units("12.3456"), Some(123_456));
        assert_eq!(parse_yuan_to_units("12.34567"), Some(123_456)); // truncate 5th
        assert_eq!(parse_yuan_to_units("-1.5"), Some(-15_000));
        assert_eq!(parse_yuan_to_units("abc"), None);
        assert_eq!(parse_yuan_to_units(""), None);
    }

    #[test]
    fn units_to_yuan_roundtrip() {
        assert_eq!(units_to_yuan(1_000_000), "100.0000");
        assert_eq!(units_to_yuan(123_456), "12.3456");
        assert_eq!(units_to_yuan(-15_000), "-1.5000");
    }

    #[test]
    fn units_to_yuan_2dp_rounds_half_up() {
        assert_eq!(units_to_yuan_2dp(1_000_000), "100.00");
        assert_eq!(units_to_yuan_2dp(123_456), "12.35"); // .3456 → .35
        assert_eq!(units_to_yuan_2dp(123_400), "12.34");
        assert_eq!(units_to_yuan_2dp(50), "0.01"); // 0.005 → 0.01 half-up
    }

    #[test]
    fn fee_matches_manual_half_up() {
        // 100元 * 0.6% = 0.60元 == 6000 units.
        assert_eq!(fee_units(1_000_000, 6_000), 6_000);
        // 33.33元 * 1% = 0.3333元 → round(0.33,2)=0.33元 == 3300 units.
        assert_eq!(fee_units(333_300, 10_000), 3_300);
    }

    #[test]
    fn scale_units_keeps_full_money_precision() {
        // 100元 (1_000_000 units) at 2% (20_000 scaled) = 2元 (20_000 units).
        assert_eq!(scale_units(1_000_000, 20_000), 20_000);
        // 33.3333元 * 1.5% = 0.499999...5元 → 4999.995 units → round half-up 5000.
        assert_eq!(scale_units(333_333, 15_000), 5_000);
        // unlike fee_units, no clamp to whole cents: 0.0001元 * 100% = 1 unit.
        assert_eq!(scale_units(1, 1_000_000), 1);
    }

    #[test]
    fn cap_zero_is_no_cap() {
        assert_eq!(cap_fee(5_000, 0), 5_000);
        assert_eq!(cap_fee(5_000, 3_000), 3_000);
    }

    #[test]
    fn parse_rate_scales_a_decimal_fraction() {
        // 0.6% == fraction 0.006 → 6_000 (RATE_SCALE 1e6).
        assert_eq!(parse_rate_to_scaled("0.006"), Some(6_000));
        assert_eq!(parse_rate_to_scaled("0.0060"), Some(6_000));
        assert_eq!(parse_rate_to_scaled("0.6"), Some(600_000)); // 60%
        assert_eq!(parse_rate_to_scaled("1"), Some(1_000_000)); // 100%
        assert_eq!(parse_rate_to_scaled("0"), Some(0));
        // a 7th fractional digit truncates (not rounds), matching the column
        assert_eq!(parse_rate_to_scaled("0.0000009"), Some(0));
        assert_eq!(parse_rate_to_scaled(" 0.0075 "), Some(7_500));
    }

    #[test]
    fn parse_rate_rejects_junk_and_signs() {
        assert_eq!(parse_rate_to_scaled(""), None);
        assert_eq!(parse_rate_to_scaled("abc"), None);
        assert_eq!(
            parse_rate_to_scaled("-0.1"),
            None,
            "a rate is never negative"
        );
        assert_eq!(parse_rate_to_scaled("1.2.3"), None);
        assert_eq!(parse_rate_to_scaled("+0.5"), Some(500_000));
    }
}
