//! The unified-order channel dispatch tail (`spec/03` §2.3–2.4, §4.2) —
//! the seam that turns a signature-verified merchant request into a stored
//! order plus a browser-facing payment output:
//!
//! ```text
//! pay_bankcode → product (open)  →  product_user (assigned to merchant)
//!   → channel pick (polling weight / pinned single)   [setChannelApiControl]
//!   → channel row + registered adapter                 [支付通道不存在]
//!   → sub-account pick (merchant pin, weight)          [orderadd 57–99]
//!   → credential assembly (account ?: channel, generated notify URLs)
//!   → rate snapshot (custom_rate override, §3.2 chain) + settlement t
//!   → LedgerService::create_order                      [orderadd 170–204]
//!   → Channel::pay → PayOut (302 / QR / auto-form)
//! ```
//!
//! Legacy risk gates: the channel (CRC) and sub-account (CARC) screens of
//! `setChannelApiControl` / `orderadd` now run through
//! [`crate::risk::config::screen_subject`] whenever a gate is supplied
//! (`spec/06` §4.1-4.2) — candidates rejected as offline or rule-breaching
//! drop out of the polling pool, and a lone pinned channel answers
//! `通道:<last message>`. `risk = None` keeps the bare status-only
//! dispatch (the offline-cron-free test harnesses). The dispatch sequence
//! and every SQL-visible decision are replayed here. All decision steps are
//! pure ([`route_channel_id`], [`apply_pin`], [`pick_account`],
//! [`cred_for`], [`pricing`]) and unit-tested; the DB reads only fetch rows
//! for them.

use std::collections::HashSet;

use chrono::Utc;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::channel::{ChannelCred, ChannelRegistry, OrderCtx, PayCtx, PayOut};
use crate::data::{channel_accounts, channels, orders, product_users, products, tikuan_configs};
use crate::ledger::{LedgerService, NewOrder};
use crate::rate::{self, ChannelRate, Cycle, ResolvedRate};
use crate::risk::{config, RiskGate, RuleKind, Scope};
use crate::routing::{draw_roll, parse_weight_spec, pick_weighted, select_account_id};
use crate::state::{GatewayError, GatewayResult};

/// Legacy fail messages, verbatim (`spec/03` §2.2–2.4, §4.2).
pub(crate) const ERR_PRODUCT_CLOSED: &str = "通道关闭中,暂时无法连接!";
pub(crate) const ERR_NOT_ASSIGNED: &str = "用户未分配通道,暂时无法连接!";
pub(crate) const ERR_NO_CHANNEL: &str = "支付通道不存在";
pub(crate) const ERR_MAINTENANCE: &str = "服务器维护中,请稍后再试...";
/// The `setChannelApiControl` initial `$error_msg` (IXC:140), shown as
/// `通道:…` when every polled candidate was screened out without any rule
/// ever answering (pure offline removals keep it).
const ERR_CHANNEL_MAINT: &str = "该通道维护中，请稍后再试";

/// A signature-verified unified-order request, projected onto what the
/// dispatch and the order row need (amounts already in money units).
#[derive(Debug, Clone)]
pub struct DispatchReq {
    /// The merchant member id (wire `pay_memberid` minus 10000).
    pub user_id: i64,
    /// Wire `pay_orderid` — doubles as the platform order id (legacy
    /// orderadd stores it verbatim, `spec/02` §3.4).
    pub order_id: String,
    pub amount_units: i64,
    /// Wire `pay_bankcode` (the product id, kept as the stored string).
    pub bank_code: String,
    /// The merchant's async notify URL (stored, re-notified later).
    pub notify_url: String,
    /// The merchant's sync page URL (stored).
    pub callback_url: String,
    pub product_name: Option<String>,
    pub attach: Option<String>,
}

/// Pick the supplier channel for one order: polling mode samples the
/// `pid:weight|…` spec (merchant override ?: product default, already
/// resolved into `spec` by the caller) over the live (status = 1) channel
/// ids; otherwise the pinned single channel wins — even a `0`, which then
/// fails the channel lookup exactly like the legacy missing-class branch
/// (`spec/03` §2.3).
pub fn route_channel_id(
    pu: &product_users::Model,
    spec: &[(i64, i64)],
    live: &HashSet<i64>,
    roll: i64,
) -> Option<i64> {
    if pu.polling == 1 && !spec.is_empty() {
        let candidates: Vec<(i64, i64)> = spec
            .iter()
            .copied()
            .filter(|(id, _)| live.contains(id))
            .collect();
        return pick_weighted(&candidates, roll);
    }
    Some(pu.channel)
}

/// Narrow the account pool by the merchant's pin list (`spec/03` §4.2
/// step 3): an enabled `user_channel_accounts` row restricts selection to
/// its listed ids; no row (or a disabled one) keeps the full pool.
pub fn apply_pin(
    accounts: Vec<channel_accounts::Model>,
    pinned: Option<&str>,
) -> Vec<channel_accounts::Model> {
    match pinned {
        Some(list) => {
            let ids: HashSet<i64> = list
                .split([',', '\n', ' '])
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            accounts
                .into_iter()
                .filter(|a| ids.contains(&a.id))
                .collect()
        }
        None => accounts,
    }
}

/// Select one sub-account: a lone candidate is taken directly, a pool is
/// sampled by weight over `(id, weight)` (the legacy `getWeight`, §4.2
/// step 3). `None` = no selectable account.
pub fn pick_account(accounts: &[channel_accounts::Model], roll: i64) -> Option<usize> {
    if accounts.len() == 1 {
        return Some(0);
    }
    let items: Vec<(i64, i32)> = accounts.iter().map(|a| (a.id, a.weight)).collect();
    let id = select_account_id(&items, roll)?;
    accounts.iter().position(|a| a.id == id)
}

/// The pricing view of the selected account: `custom_rate = 1` swaps the
/// channel's default rate/cap pair for the account's own (§4.2 step 4);
/// otherwise the channel row prices.
pub fn pricing(ch: &channels::Model, acc: &channel_accounts::Model) -> ChannelRate {
    if acc.custom_rate == 1 {
        ChannelRate {
            default_rate: acc.default_rate,
            fengding: acc.fengding,
            t0_default_rate: acc.t0_default_rate,
            t0_fengding: acc.t0_fengding,
        }
    } else {
        ChannelRate::from_model(ch)
    }
}

/// Freeze the rate pair for the order's cycle: the merchant's own fee rate
/// always rides the RAW channel defaults (§3.2 `?:` chain), while the cost
/// side reflects the account-level [`pricing`] (its `custom_rate` is the
/// platform's true cost for this order).
pub fn pricing_pair(
    cycle: Cycle,
    user: Option<&rate::UserRate>,
    ch: &channels::Model,
    acc: &channel_accounts::Model,
) -> (ResolvedRate, i64) {
    let merchant_base = ChannelRate::from_model(ch);
    let cost_base = pricing(ch, acc);
    let resolved = rate::resolve(cycle, user, &merchant_base);
    let cost = match cycle {
        Cycle::T0 => cost_base.t0_default_rate,
        Cycle::T1 => cost_base.default_rate,
    };
    (resolved, cost)
}

/// Assemble the adapter credentials: account values shadow the channel
/// ones (`?:` on stored strings, §4.2 step 4), and the upstream-facing
/// callback addresses prefer the channel's configured overrides, falling
/// back to this gateway's generated routes (§4.2 step 5).
pub fn cred_for(
    ch: &channels::Model,
    acc: &channel_accounts::Model,
    site_url: &str,
) -> ChannelCred {
    let site = site_url.trim_end_matches('/');
    let or = |a: &Option<String>, b: &Option<String>| {
        a.clone()
            .filter(|s| !s.is_empty())
            .or_else(|| b.clone().filter(|s| !s.is_empty()))
            .unwrap_or_default()
    };
    ChannelCred {
        mch_id: or(&acc.mch_id, &ch.mch_id),
        sign_key: or(&acc.sign_key, &ch.sign_key),
        app_id: or(&acc.app_id, &ch.app_id),
        app_secret: or(&acc.app_secret, &ch.app_secret),
        gateway: ch.gateway.clone().unwrap_or_default(),
        unlock_domain: ch.unlock_domain.clone().filter(|s| !s.is_empty()),
        server_return: Some(
            ch.server_return
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("{site}/notify/{}", ch.code)),
        ),
        page_return: Some(
            ch.page_return
                .clone()
                .filter(|s| !s.is_empty())
                // The rewrite's clean path (mirrors /notify/{code}); the
                // legacy `Pay_{code}_callbackurl.html` shape is served by
                // the same handler (语义差异清单).
                .unwrap_or_else(|| format!("{site}/callback/{}", ch.code)),
        ),
    }
}

/// Resolve the settlement `t` (`spec/02` §3.2): the merchant's enabled
/// (`systemxz = 1`) tikuan row wins, else the platform default row, else
/// `0`. Only `t1zt` participates (via [`crate::payout::config::PayoutConfig::settlement_t`]).
pub async fn settlement_t(db: &DatabaseConnection, user_id: i64) -> Result<i32, GatewayError> {
    let personal = tikuan_configs::Entity::find()
        .filter(tikuan_configs::Column::UserId.eq(user_id))
        .filter(tikuan_configs::Column::Systemxz.eq(1))
        .one(db)
        .await?;
    let system = tikuan_configs::Entity::find()
        .filter(tikuan_configs::Column::Issystem.eq(1))
        .one(db)
        .await?;
    Ok(match personal.as_ref() {
        Some(p) => crate::payout::config::PayoutConfig::from_model(p).settlement_t(),
        None => system
            .as_ref()
            .map(|s| crate::payout::config::PayoutConfig::from_model(s).settlement_t())
            .unwrap_or(0),
    })
}

/// The full dispatch: every rejection carries the legacy caller-facing
/// message ([`GatewayError::BadRequest`]); a success is the persisted
/// status-0 order row plus the adapter's browser output. A pay failure
/// leaves the order stored — the legacy stored before dispatching too, so
/// reissue/query can still complete it.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_core(
    db: &DatabaseConnection,
    ledger: &LedgerService,
    registry: &ChannelRegistry,
    site_url: &str,
    risk: Option<&RiskGate>,
    req: DispatchReq,
) -> GatewayResult<(orders::Model, PayOut)> {
    // ① product open + merchant assignment (productIsOpen, §2.3).
    let product_id: i64 = req
        .bank_code
        .parse()
        .map_err(|_| GatewayError::BadRequest(ERR_PRODUCT_CLOSED.into()))?;
    let product = products::Entity::find_by_id(product_id)
        .one(db)
        .await?
        .filter(|p| p.status == 1)
        .ok_or_else(|| GatewayError::BadRequest(ERR_PRODUCT_CLOSED.into()))?;
    let pu = product_users::Entity::find()
        .filter(product_users::Column::Pid.eq(product_id))
        .filter(product_users::Column::UserId.eq(req.user_id))
        .filter(product_users::Column::Status.eq(1))
        .one(db)
        .await?
        .ok_or_else(|| GatewayError::BadRequest(ERR_NOT_ASSIGNED.into()))?;

    // ② channel pick (setChannelApiControl, §2.3): merchant weight spec ?:
    // product default spec, sampled over live channel rows — and, with a
    // gate in hand, rows the CRC screen admits (IXC:142-166).
    let spec_src = pu
        .weight
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(product.weight.as_deref());
    let spec: Vec<(i64, i64)> = spec_src.map(parse_weight_spec).unwrap_or_default();
    let spec_ids: Vec<i64> = spec.iter().map(|(id, _)| *id).collect();
    let polled = pu.polling == 1 && !spec.is_empty();
    let now_ts = Utc::now().timestamp();
    // The legacy `$error_msg`: the last rule answer, untouched by offline
    // removals (they never call the screen in the polling loop).
    let mut last_msg: Option<String> = None;
    let live: HashSet<i64> = if polled {
        let rows = channels::Entity::find()
            .filter(channels::Column::Id.is_in(spec_ids))
            .filter(channels::Column::Status.eq(1))
            .all(db)
            .await?;
        let mut set = HashSet::new();
        for row in rows {
            let verdict = match risk {
                Some(g) => {
                    config::screen_subject(
                        g,
                        Scope::Channel,
                        row.id,
                        &config::channel_screening(&row),
                        req.amount_units,
                        now_ts,
                    )
                    .await
                }
                None => None,
            };
            match verdict {
                None => {
                    set.insert(row.id);
                }
                Some(d) => {
                    if d.rule_kind() != Some(RuleKind::Offline) {
                        last_msg = d.message();
                    }
                }
            }
        }
        set
    } else {
        HashSet::new()
    };
    let total = spec.iter().map(|(_, w)| (*w).max(0)).sum();
    let channel_id = route_channel_id(&pu, &spec, &live, draw_roll(total)).ok_or_else(|| {
        GatewayError::BadRequest(match (polled, risk) {
            (true, Some(_)) => {
                format!("通道:{}", last_msg.as_deref().unwrap_or(ERR_CHANNEL_MAINT))
            }
            _ => ERR_MAINTENANCE.to_string(),
        })
    })?;

    // ③ the channel row must exist, be open, and have a registered adapter
    // (index()'s class-file check, §2.4).
    let channel = channels::Entity::find_by_id(channel_id)
        .one(db)
        .await?
        .filter(|c| c.status == 1)
        .ok_or_else(|| GatewayError::BadRequest(ERR_NO_CHANNEL.into()))?;
    // A pinned single channel was not polled above: answer `通道:<message>`
    // straight off its CRC verdict (the legacy else-branch, IXC:186-196).
    if !polled {
        if let Some(g) = risk {
            if let Some(d) = config::screen_subject(
                g,
                Scope::Channel,
                channel.id,
                &config::channel_screening(&channel),
                req.amount_units,
                now_ts,
            )
            .await
            {
                return Err(GatewayError::BadRequest(format!(
                    "通道:{}",
                    d.message().unwrap_or_default()
                )));
            }
        }
    }
    let adapter = registry
        .get(&channel.code)
        .ok_or_else(|| GatewayError::BadRequest(ERR_NO_CHANNEL.into()))?;

    // ④ sub-account pick (§4.2 step 3): open accounts of this channel,
    // narrowed by the merchant's pin list, then single / weighted — each
    // CARC-screened on its own switches (orderadd 71-85) when a gate is
    // present; an all-screened pool answers like an empty one.
    let pool = channel_accounts::Entity::find()
        .filter(channel_accounts::Column::ChannelId.eq(channel_id))
        .filter(channel_accounts::Column::Status.eq(1))
        .all(db)
        .await?;
    let pin = crate::data::user_channel_accounts::Entity::find()
        .filter(crate::data::user_channel_accounts::Column::UserId.eq(req.user_id))
        .filter(crate::data::user_channel_accounts::Column::Status.eq(1))
        .one(db)
        .await?;
    let pool = apply_pin(pool, pin.as_ref().map(|p| p.account_ids.as_str()));
    let pool = match risk {
        Some(g) => {
            let mut keep = Vec::with_capacity(pool.len());
            for a in pool {
                let screening = config::account_screening(&a, &channel);
                if config::screen_subject(
                    g,
                    Scope::ChannelAccount,
                    a.id,
                    &screening,
                    req.amount_units,
                    now_ts,
                )
                .await
                .is_none()
                {
                    keep.push(a);
                }
            }
            keep
        }
        None => pool,
    };
    // A lone candidate skips the roll (legacy count==1); a pool is sampled.
    let roll = draw_roll(pool.iter().map(|a| (a.weight as i64).max(0)).sum());
    let idx = pick_account(&pool, roll)
        .ok_or_else(|| GatewayError::BadRequest(ERR_MAINTENANCE.into()))?;
    let account = &pool[idx];

    // ⑤ credentials + rate/t snapshot (§4.2 steps 2, 4–5).
    let cred = cred_for(&channel, account, site_url);
    let t = settlement_t(db, req.user_id).await?;
    let cycle = Cycle::from_t(t);
    let user_rate = rate::load_user_rate(db, req.user_id, channel_id).await?;
    let (resolved, cost_rate) = pricing_pair(cycle, user_rate.as_ref(), &channel, account);

    // ⑥ store the status-0 order (the admit kernel re-checks amounts inside
    // create_order and rejects with the legacy wording).
    let subject = req
        .product_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| product.name.clone());
    let new = NewOrder {
        user_id: req.user_id,
        order_id: req.order_id.clone(),
        amount_units: req.amount_units,
        rate: resolved,
        cost_rate,
        t,
        bank_code: req.bank_code.clone(),
        channel_code: channel.code.clone(),
        notify_url: req.notify_url.clone(),
        callback_url: req.callback_url.clone(),
        channel_id,
        account_id: account.id,
        sign_key: Some(cred.sign_key.clone()),
        app_id: Some(cred.app_id.clone()),
        attach: req.attach.clone(),
        product_name: Some(subject.clone()),
    };
    let order = ledger.create_order(&new).await?;

    // ⑦ hand the frozen order to the adapter; upstream faults are the
    // legacy maintenance message (the order stays status 0 for reissue).
    let ctx = OrderCtx {
        order_id: order.order_id.clone(),
        merchant_order_id: order.order_id.clone(),
        amount_units: order.amount,
        subject,
        notify_url: cred.server_return.clone().unwrap_or_default(),
        callback_url: cred.page_return.clone().unwrap_or_default(),
    };
    let payout = adapter
        .pay(&PayCtx {
            order: &ctx,
            cred: &cred,
        })
        .await
        .map_err(|e| {
            tracing::warn!(channel = %channel.code, order_id = %order.order_id, error = ?e, "upstream pay failed");
            GatewayError::BadRequest(ERR_MAINTENANCE.into())
        })?;
    Ok((order, payout))
}

/// Render a [`PayOut`] for the wire: real 302 for redirects (the legacy
/// `header('Location')`), HTML documents for QR / auto-form / raw bodies.
pub fn payout_response(payout: PayOut) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    match payout {
        PayOut::Redirect { url } => (StatusCode::FOUND, [(header::LOCATION, url)]).into_response(),
        other => {
            let rendered = other.render();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, rendered.content_type)],
                rendered.body,
            )
                .into_response()
        }
    }
}

/// Map a dispatch failure onto the legacy error envelope wording.
pub fn dispatch_error(e: GatewayError) -> String {
    match e {
        GatewayError::BadRequest(m) => m,
        GatewayError::Internal(m) => {
            tracing::error!(error = %m, "dispatch internal error");
            ERR_MAINTENANCE.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pu(polling: i32, channel: i64, weight: Option<&str>) -> product_users::Model {
        product_users::Model {
            id: 1,
            user_id: 10,
            pid: 903,
            polling,
            status: 1,
            channel,
            weight: weight.map(String::from),
        }
    }

    fn account(id: i64, weight: i32, custom_rate: i32) -> channel_accounts::Model {
        channel_accounts::Model {
            id,
            channel_id: 5,
            mch_id: None,
            sign_key: None,
            app_id: None,
            app_secret: None,
            title: None,
            weight,
            status: 1,
            default_rate: 0,
            fengding: 0,
            t0_default_rate: 0,
            t0_fengding: 0,
            custom_rate,
            control_status: 0,
            offline_status: 0,
            is_defined: 1,
            all_money: 0,
            unit_interval: 0,
            time_unit: "s".into(),
            unit_number: 0,
            unit_all_money: 0,
            start_time: 0,
            end_time: 0,
            min_money: 0,
            max_money: 0,
        }
    }

    fn channel() -> channels::Model {
        channels::Model {
            id: 5,
            code: "WxSm".into(),
            title: "wx".into(),
            mch_id: Some("CH-1".into()),
            sign_key: Some("ch-key".into()),
            app_id: Some("ch-app".into()),
            app_secret: None,
            gateway: None,
            page_return: None,
            server_return: None,
            default_rate: 6_000,
            fengding: 0,
            t0_default_rate: 8_000,
            t0_fengding: 0,
            status: 1,
            paytype: 1,
            unlock_domain: None,
            control_status: 0,
            offline_status: 0,
            all_money: 0,
            start_time: 0,
            end_time: 0,
            min_money: 0,
            max_money: 0,
        }
    }

    #[test]
    fn polling_samples_live_weight_bands() {
        let spec = parse_weight_spec("11:3|12:7");
        let live: HashSet<i64> = [12].into_iter().collect(); // 11 closed
                                                             // Rolls in the (dead) 11 band would be impossible; the candidate
                                                             // list is [12:7], so every in-range roll lands on 12.
        assert_eq!(
            route_channel_id(&pu(1, 0, Some("11:3|12:7")), &spec, &live, 0),
            Some(12)
        );
        // Nothing live → no channel (the caller answers 服务器维护中).
        let none: HashSet<i64> = HashSet::new();
        assert_eq!(
            route_channel_id(&pu(1, 0, Some("11:3")), &spec, &none, 0),
            None
        );
    }

    #[test]
    fn single_mode_uses_the_pinned_channel_even_zero() {
        let spec = parse_weight_spec("11:3");
        let live: HashSet<i64> = [11].into_iter().collect();
        assert_eq!(
            route_channel_id(&pu(0, 77, None), &spec, &live, 0),
            Some(77)
        );
        // polling but no spec at all → also the pinned single channel.
        assert_eq!(route_channel_id(&pu(1, 0, None), &[], &live, 0), Some(0));
    }

    #[test]
    fn pin_narrows_the_pool_and_lone_accounts_skip_the_roll() {
        let pool = vec![account(1, 5, 0), account(2, 5, 0), account(3, 5, 0)];
        let pinned = apply_pin(pool.clone(), Some("2, 3"));
        assert_eq!(pinned.iter().map(|a| a.id).collect::<Vec<_>>(), vec![2, 3]);
        // No pin row → full pool; a disabled pin is cleared upstream (None).
        assert_eq!(apply_pin(pool.clone(), None).len(), 3);
        // Lone candidate ignores the roll entirely (legacy count==1 shortcut).
        assert_eq!(pick_account(&pinned[..1], 999), Some(0));
        // Weighted: roll 5 falls in account 2's band [5..10).
        let w = vec![account(1, 5, 0), account(2, 5, 0)];
        assert_eq!(pick_account(&w, 5), Some(1));
        assert_eq!(pick_account(&[], 0), None);
    }

    #[test]
    fn custom_rate_swaps_the_pricing_base() {
        let ch = channel();
        let plain = account(1, 1, 0);
        assert_eq!(pricing(&ch, &plain).t0_default_rate, 8_000);
        let mut custom = account(2, 1, 1);
        custom.t0_default_rate = 4_000;
        custom.default_rate = 5_000;
        assert_eq!(pricing(&ch, &custom).t0_default_rate, 4_000);

        // The merchant fee rate still rides the RAW channel defaults while
        // the cost follows the custom override.
        let (resolved, cost) = pricing_pair(Cycle::T0, None, &ch, &custom);
        assert_eq!(resolved.feilv, 8_000);
        assert_eq!(cost, 4_000);
    }

    #[test]
    fn creds_shadow_channel_with_account_and_generate_routes() {
        let ch = channel();
        let mut acc = account(1, 1, 0);
        acc.sign_key = Some("acc-key".into());
        acc.mch_id = Some(String::new()); // empty → falls through
        let cred = cred_for(&ch, &acc, "https://pay.example/");
        assert_eq!(cred.sign_key, "acc-key");
        assert_eq!(cred.mch_id, "CH-1"); // channel value via the `?:` chain
        assert_eq!(
            cred.server_return.as_deref(),
            Some("https://pay.example/notify/WxSm")
        );
        assert_eq!(
            cred.page_return.as_deref(),
            Some("https://pay.example/callback/WxSm")
        );

        acc.sign_key = None;
        let cred = cred_for(&ch, &acc, "http://x");
        assert_eq!(cred.sign_key, "ch-key");
        assert_eq!(cred.app_id, "ch-app");
    }
}
