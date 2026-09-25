//! Redis-backed risk counters — the modernization of the legacy per-row
//! accumulator columns `paying_money` / `unit_paying_number` /
//! `unit_paying_amount` / `unit_frist_paying_time` (`spec/06` §5, target §10.3).
//!
//! Two windowed buckets replace the read-modify-write row columns:
//! - a **same-day** amount keyed by the UTC date (`risk:daily:<scope>:<id>:<Ymd>`)
//!   that self-separates across midnight and carries a TTL to end-of-day — so
//!   the legacy cron reset race (§9.1) and the wrong-column reset bug (§6) both
//!   disappear; and
//! - a **fixed unit-time bucket** (`risk:unit:<scope>:<id>:<c|a>:<bucket>`),
//!   where `bucket = now / window_secs`, so a new window is a new key.
//!
//! The key/date/bucket derivation is pure and tested; the reads and writes go
//! through Redis and fail *open* (a miss reads `0`), mirroring the rest of the
//! crate. The engine [`super::rules::evaluate`] turns a [`super::rules::Counters`]
//! snapshot built here into a decision.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;

use crate::risk::rules::{Counters, UnitRule};

/// The subject a counter bucket belongs to (`spec/06` §1: the three risk
/// levels), keyed under distinct namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Channel,
    ChannelAccount,
    Merchant,
}

impl Scope {
    /// The short key namespace.
    pub fn tag(self) -> &'static str {
        match self {
            Scope::Channel => "channel",
            Scope::ChannelAccount => "account",
            Scope::Merchant => "member",
        }
    }
}

/// UTC `Ymd` (e.g. `19700101`) from unix seconds — pure civil-from-days.
pub fn ymd(now_ts: i64) -> String {
    let (y, m, d) = civil_from_days(now_ts.div_euclid(86400));
    format!("{y:04}{m:02}{d:02}")
}

/// Seconds remaining until the next UTC midnight (always in `1..=86400`).
pub fn secs_to_end_of_day(now_ts: i64) -> i64 {
    86400 - now_ts.rem_euclid(86400)
}

/// The fixed-window bucket index (`now / window_secs`); `0` when disabled.
pub fn unit_bucket(now_ts: i64, window_secs: i64) -> i64 {
    if window_secs <= 0 {
        0
    } else {
        now_ts.div_euclid(window_secs)
    }
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 → proleptic
/// Gregorian `(year, month, day)`), integer-only and exact for our range.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn daily_key(scope: Scope, id: i64, now_ts: i64) -> String {
    format!("risk:daily:{}:{id}:{}", scope.tag(), ymd(now_ts))
}

fn unit_count_key(scope: Scope, id: i64, now_ts: i64, window_secs: i64) -> String {
    format!(
        "risk:unit:{}:{id}:c:{}",
        scope.tag(),
        unit_bucket(now_ts, window_secs)
    )
}

fn unit_amount_key(scope: Scope, id: i64, now_ts: i64, window_secs: i64) -> String {
    format!(
        "risk:unit:{}:{id}:a:{}",
        scope.tag(),
        unit_bucket(now_ts, window_secs)
    )
}

fn offline_key(scope: Scope, id: i64) -> String {
    format!("risk:offline:{}:{id}", scope.tag())
}

/// The Redis counter store (cheap to clone; shares the connection manager).
#[derive(Clone)]
pub struct RiskCounters {
    redis: ConnectionManager,
}

impl RiskCounters {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }

    async fn get_i64(&self, key: &str) -> i64 {
        let mut conn = self.redis.clone();
        conn.get::<_, Option<i64>>(key)
            .await
            .unwrap_or(None)
            .unwrap_or(0)
    }

    /// Builds the [`Counters`] snapshot the engine reads: the day total and,
    /// when a unit rule is configured, the current bucket's count/amount.
    pub async fn snapshot(
        &self,
        scope: Scope,
        id: i64,
        unit: Option<UnitRule>,
        now_ts: i64,
    ) -> Counters {
        let daily_amount = self.get_i64(&daily_key(scope, id, now_ts)).await;
        let mut c = Counters {
            daily_amount,
            ..Default::default()
        };
        if let Some(u) = unit.filter(|u| u.window_secs > 0) {
            let count = self
                .get_i64(&unit_count_key(scope, id, now_ts, u.window_secs))
                .await;
            let amount = self
                .get_i64(&unit_amount_key(scope, id, now_ts, u.window_secs))
                .await;
            c.window_open = count > 0 || amount > 0;
            c.window_count = count;
            c.window_amount = amount;
        }
        c
    }

    /// Records one settled trade's day accumulation (`saveOfflineStatus`'s
    /// `paying_money += pay_amount`, but atomic): `+amount` into the day
    /// bucket, returning the RUNNING TOTAL — the `>= cap` offline trip wire
    /// reads it, so the legacy's read-modify-write lost-update (`spec/06`
    /// §9.3) cannot happen. A Redis failure returns `None` and the caller
    /// fails open (no trip), mirroring [`Self::snapshot`]'s fail-open reads.
    pub async fn incr_daily(&self, scope: Scope, id: i64, amount: i64, now_ts: i64) -> Option<i64> {
        let mut conn = self.redis.clone();
        let dk = daily_key(scope, id, now_ts);
        let after: redis::RedisResult<i64> = conn.incr(&dk, amount).await;
        let after = after.ok()?;
        let _: redis::RedisResult<i64> = conn.expire(&dk, secs_to_end_of_day(now_ts)).await;
        Some(after)
    }

    /// The unit-window accumulation (`unit_paying_number+1` /
    /// `unit_paying_amount+=…`, P:401-403): one trade into the current
    /// fixed window's count and amount buckets, each TTL-bounded by the
    /// window itself.
    pub async fn incr_unit(&self, scope: Scope, id: i64, unit: UnitRule, amount: i64, now_ts: i64) {
        if unit.window_secs <= 0 {
            return; // throttle off — nothing to count into
        }
        let mut conn = self.redis.clone();
        let ck = unit_count_key(scope, id, now_ts, unit.window_secs);
        let ak = unit_amount_key(scope, id, now_ts, unit.window_secs);
        let _: redis::RedisResult<i64> = conn.incr(&ck, 1).await;
        let _: redis::RedisResult<i64> = conn.expire(&ck, unit.window_secs).await;
        let _: redis::RedisResult<i64> = conn.incr(&ak, amount).await;
        let _: redis::RedisResult<i64> = conn.expire(&ak, unit.window_secs).await;
    }

    /// Whether a subject carries an offline marker (replaces
    /// `offline_status=0`, restored by the marker's TTL, `spec/06` §10.3).
    pub async fn is_offline(&self, scope: Scope, id: i64) -> bool {
        self.get_i64(&offline_key(scope, id)).await > 0
    }

    /// Sets the offline marker for `ttl_secs` (the day-cap trip wires this).
    pub async fn set_offline(&self, scope: Scope, id: i64, ttl_secs: i64) {
        let mut conn = self.redis.clone();
        let _: redis::RedisResult<()> = conn
            .set_ex::<_, _, ()>(offline_key(scope, id), 1, ttl_secs.max(1) as u64)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn ymd_matches_known_epoch_dates() {
        assert_eq!(ymd(0), "19700101");
        assert_eq!(ymd(DAY), "19700102");
        // 2000-03-01 = 10992 days after epoch
        assert_eq!(ymd(1_000_000_000), "20010909");
    }

    #[test]
    fn secs_to_end_of_day_counts_down() {
        assert_eq!(secs_to_end_of_day(0), DAY);
        assert_eq!(secs_to_end_of_day(10), DAY - 10);
        assert_eq!(secs_to_end_of_day(DAY - 1), 1);
        assert_eq!(secs_to_end_of_day(DAY), DAY); // next midnight resets
    }

    #[test]
    fn unit_bucket_is_window_aligned() {
        assert_eq!(unit_bucket(0, 60), 0);
        assert_eq!(unit_bucket(59, 60), 0);
        assert_eq!(unit_bucket(60, 60), 1);
        assert_eq!(unit_bucket(125, 60), 2);
        assert_eq!(unit_bucket(125, 0), 0); // disabled
    }

    #[test]
    fn keys_are_scoped_and_namespaced() {
        assert_eq!(
            daily_key(Scope::Merchant, 42, 0),
            "risk:daily:member:42:19700101"
        );
        assert_eq!(
            unit_count_key(Scope::ChannelAccount, 7, 120, 60),
            "risk:unit:account:7:c:2"
        );
        assert_eq!(
            unit_amount_key(Scope::Channel, 7, 120, 60),
            "risk:unit:channel:7:a:2"
        );
        assert_eq!(offline_key(Scope::Channel, 3), "risk:offline:channel:3");
    }
}
