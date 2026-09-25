//! Core SeaORM entities for the payment domain — the Phase-0 subset.
//! Table names are cleaned to snake_case plural (the legacy `pay_` prefix
//! is dropped); money columns are `i64` in 1/10000 元 units (see
//! [`crate::money`]); rate columns are `i64` scaled by 1e6. Full column /
//! table parity is an ongoing concern tracked by the roadmap; the payout
//! and risk tables land in Phases 4/6.
//!
//! Field names on the wire (`pay_amount`, `pay_orderid`, ...) are handled
//! at the gateway boundary; these models are the internal, modernized
//! shape.

use sea_orm::entity::prelude::*;

/// `pay_member` → merchants / agents. `mch_id` on the wire is `id + 10000`.
pub mod members {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "members")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub username: String,
        pub password: String,
        /// 1 platform super-admin; 4 merchant; 5/6/7 agent tiers.
        pub groupid: i32,
        pub salt: String,
        /// Parent agent id (0 = none) — the three-level profit tree.
        pub parentid: i64,
        /// Available balance, money units.
        pub balance: i64,
        /// T+1 blocked balance, money units.
        pub blocked_balance: i64,
        pub email: Option<String>,
        pub mobile: Option<String>,
        pub realname: Option<String>,
        /// The merchant API key (also the MD5 sign secret).
        pub apikey: Option<String>,
        /// Payment-password hash (guards APIKEY reveal, auth_type=6).
        pub pay_password: Option<String>,
        /// 0 disabled / 1 enabled.
        pub status: i32,
        /// KYC authorization state.
        pub authorized: i32,
        /// Whether the merchant may call the payout API.
        pub df_api: i32,
        // --- payout-API (§7.1) reporting + auto-review gates, off the legacy
        //     `pay_member` df_* columns; the wire handler reads all three ---
        /// Newline-separated whitelist of allowed request source base-domains
        /// (legacy `df_domain`); empty / `None` disables the referer check.
        pub df_domain: Option<String>,
        /// Newline-separated whitelist of allowed client IPs (legacy `df_ip`);
        /// empty / `None` disables the IP check.
        pub df_ip: Option<String>,
        /// Auto-review flag (legacy `df_auto_check`): non-zero makes
        /// `Dfpay::add` run `df_pass` (debit) right after filing the pending
        /// application instead of leaving it `check_status = 0`.
        pub df_auto_check: i32,
        pub google_secret_key: Option<String>,
        /// `\r\n`-separated login-IP whitelist (legacy `login_ip`); empty /
        /// `None` admits every client IP (§4.2).
        pub login_ip: Option<String>,
        /// Single-sign-on token (legacy `session_random`): the panel bumps it
        /// on every login and revokes any older session whose held version no
        /// longer matches (§4.6).
        pub session_version: Option<String>,
        // --- merchant profile columns (`saveProfile` whitelist, §10). Kept
        //     nullable so a not-yet-filled member (and every existing seed)
        //     reads `None`; the panel writes only the posted subset. ---
        /// Gender (legacy `sex`); `None` = not supplied.
        pub sex: Option<i32>,
        /// Birthday as unix seconds (legacy `birthday` int, `strtotime`).
        pub birthday: Option<i64>,
        /// ID-card number (legacy `sfznumber`).
        pub sfznumber: Option<String>,
        /// Contact QQ (legacy `qq`).
        pub qq: Option<String>,
        /// Contact address (legacy `address`).
        pub address: Option<String>,
        /// The台卡 (desktop card) payee line rendered on the收款码 image
        /// (legacy `receiver`, `varchar(255)`); `None` = not set.
        pub receiver: Option<String>,
        /// The email-activation link token (legacy `activate`,
        /// `generateUser` L796): `md5(md5(user)·md5(pw)·md5(email)·KEY)`.
        /// Always seeded at registration; only consumed by the `Activate`
        /// link when `register_need_activate` is on (§3.4, `spec/05`).
        pub activate: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_bankcard` → a merchant's settlement bank cards (`spec/05` §10). The
/// DDL column set (`addBankcard` posts `bankname/subbranch/accountname/`
/// `cardnumber/province/city/alias`); the parallel `bankcardedit` legacy field
/// set is runtime-altered / dead and intentionally not modelled. Text columns
/// are nullable to tolerate partial form posts; ownership is every target op's
/// `(id, userid)` scope.
pub mod bankcards {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bankcards")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Owning merchant uid (`userid`).
        pub userid: i64,
        pub bankname: Option<String>,
        pub subbranch: Option<String>,
        pub accountname: Option<String>,
        pub cardnumber: Option<String>,
        pub province: Option<String>,
        pub city: Option<String>,
        pub ip: Option<String>,
        pub ipaddress: Option<String>,
        pub alias: Option<String>,
        /// Default-card flag (legacy `isdefault`): 1 default / 0 normal.
        pub isdefault: i32,
        /// Last-write unix seconds (`updatetime`).
        pub updatetime: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_attachment` → a merchant's KYC evidence images (`spec/05` §8.5). The
/// MyISAM source table is tiny (`id/userid/filename/path`, no timestamps); a
/// merchant uploads jpg/gif/png files through `AccountController::upload`,
/// the bytes land under `Uploads/verifyinfo/` and one row is filed here. The
/// `path` column stores the site-relative record (`Uploads/verifyinfo/<name>`),
/// the same string the authorized() page renders. Ownership is `userid`.
pub mod attachments {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "attachments")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Owning merchant uid (`userid`).
        pub userid: i64,
        /// The uploader's original file name (legacy `filename`).
        pub filename: String,
        /// Site-relative stored path (legacy `path`, `Uploads/verifyinfo/...`).
        pub path: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_loginrecord` → the login audit log, shared by the front (merchant /
/// agent console, `type = 0`) and back office (`type = 1`) (`spec/05` §5.4 /
/// §10). A row is appended on each successful login; `loginaddress` is the
/// IP-geolocation string (legacy `NIpLocation` — an external lookup, left
/// `None` here) and `logindatetime` the wall clock of the attempt. The
/// merchant's `loginrecord()` page reads its own `type = 0` rows newest-first.
pub mod loginrecords {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "loginrecords")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Acting member uid (`userid`).
        pub userid: i64,
        /// Login wall clock (legacy `logindatetime` timestamp).
        pub logindatetime: chrono::NaiveDateTime,
        /// Client IP that authenticated (legacy `loginip`).
        pub loginip: String,
        /// IP geolocation `省-市` (legacy `loginaddress`); `None` = unresolved.
        pub loginaddress: Option<String>,
        /// 0 front console / 1 back office (legacy `type`).
        #[sea_orm(column_name = "type")]
        pub logintype: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_article` → platform公告 / news (`spec/05` §10 console). The merchant
/// console surfaces the visible (`status = 1`) notices whose `groupid` targets
/// it: 0 = everyone, 1 = merchants, 2 = agents. `content` is the body (kept
/// nullable); the list page shows only title / description / time.
pub mod articles {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "articles")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Category id (legacy `catid`).
        pub catid: i64,
        /// Audience group: 0 all / 1 merchant / 2 agent (legacy `groupid`).
        pub groupid: i32,
        pub title: String,
        pub content: Option<String>,
        /// Publish unix seconds (legacy `createtime`).
        pub createtime: i64,
        pub description: String,
        /// 1 visible / 0 hidden (legacy `status`).
        pub status: i32,
        /// Last-edit unix seconds (legacy `updatetime`).
        pub updatetime: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_complaints_deposit` → the per-order投诉保证金 freeze ledger
/// (`spec/05` §10). A complaint freezes `freeze_money` (money units) that
/// auto-unfreezes at `unfreeze_time` unless `is_pause`; `status` is 0 未解冻 /
/// 1 已解冻. The console sums the still-frozen (`status = 0`) balance.
pub mod complaints_deposits {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "complaints_deposits")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// System order no (legacy `pay_orderid`).
        pub pay_orderid: String,
        /// Downstream order no (legacy `out_trade_id`).
        pub out_trade_id: String,
        /// Frozen deposit, money units (legacy `freeze_money` decimal).
        pub freeze_money: i64,
        /// Planned unfreeze unix seconds (legacy `unfreeze_time`).
        pub unfreeze_time: i64,
        /// Actual unfreeze unix seconds (legacy `real_unfreeze_time`).
        pub real_unfreeze_time: i64,
        /// 1 pauses the auto-unfreeze (legacy `is_pause`).
        pub is_pause: i32,
        /// 0 未解冻 / 1 已解冻 (legacy `status`).
        pub status: i32,
        /// Record create unix seconds (legacy `create_at`).
        pub create_at: i64,
        /// Record update unix seconds (legacy `update_at`).
        pub update_at: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The complaints-deposit rule table (`pay_complaints_deposit_rule`,
/// `spec/02` §4.4): per-merchant withholding percentage + freeze duration,
/// with an `is_system` fallback row the settle path resolves when the
/// merchant has no active own row. The legacy stored `ratio` as
/// `decimal(10,2)` percent; the settle kernel (`DepositRule`) is pinned to
/// whole percent, so the column models that representation directly.
pub mod complaints_deposit_rules {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "complaints_deposit_rules")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// The merchant the rule belongs to (0/irrelevant on the system row).
        pub user_id: i64,
        /// 1 marks the platform fallback rule.
        pub is_system: i32,
        /// Withheld percentage of the arrival (whole percent, 0-100).
        pub ratio_pct: i32,
        /// Freeze duration in seconds (`unfreeze_time = now + freeze_time`).
        pub freeze_time: i64,
        /// 1 开启 / 0 关闭.
        pub status: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_sms` → the singleton SMS-channel config (`spec/05` §5). One row holds
/// the provider switch + credentials: `is_open` gates every dispatch (the
/// legacy `sendSMS` returns early when off), `sms_channel` selects the adapter
/// (`aliyun` | `smsbao`), and the credential columns carry the per-provider
/// knobs (`app_key` / `app_secret` / `sign_name` for aliyun, `smsbao_user` /
/// `smsbao_pass` for 短信宝). [`crate::merchant`]/[`crate::sms`] read it to
/// derive the effective send decision; secrets are stored as the legacy does,
/// never seeded with live values by the rewrite's own tests.
pub mod sms_configs {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sms_configs")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub app_key: Option<String>,
        pub app_secret: Option<String>,
        /// Provider signature / 【签名】 prepended to the message.
        pub sign_name: Option<String>,
        /// 0 closed / 1 open (legacy `is_open`).
        pub is_open: i32,
        /// Optional admin copy-to mobile (legacy `admin_mobile`).
        pub admin_mobile: Option<String>,
        /// Whether admin copy-to is on (legacy `is_receive`).
        pub is_receive: i32,
        /// `aliyun` | `smsbao` (legacy `sms_channel`, default `aliyun`).
        pub sms_channel: String,
        pub smsbao_user: String,
        pub smsbao_pass: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_sms_template` → the per-scene message template
/// (`getSmsTemplateCode($callIndex)`, `spec/05` §5). `call_index` is the scene
/// string the caller passes (`bindMobile`, `editPassword`, …); `template_code`
/// is the aliyun template id and `template_content` the rendered body with a
/// `${code}` placeholder. 短信宝 ignores the template and composes the body in
/// code (see [`crate::sms`]).
pub mod sms_templates {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sms_templates")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub title: String,
        pub template_code: Option<String>,
        /// Scene key the caller looks the template up by (legacy `call_index`).
        pub call_index: Option<String>,
        pub template_content: Option<String>,
        /// Create unix seconds (legacy `ctime`).
        pub ctime: Option<i64>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_user_code` → the email verification-code ledger for the找回密码 flow
/// (`sendUserCode` / `forgetpwd`, `spec/05` §5 / §10). NOTE: the legacy找回
/// password rides an EMAIL code (not SMS): a 5-digit code is mailed and one row
/// is filed here (`type = 0` = 找回密码); `forgetpwd` matches an unexpired,
/// unconsumed row (`status = 0`, `endtime > now`) on username + email + code,
/// resets the password, then flips `status = 1` (consumed) + `uptime`.
pub mod user_codes {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "user_codes")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Purpose discriminator (legacy `type`): 0 = 找回密码.
        pub r#type: i32,
        /// The emailed code (legacy `code`, a 5-digit string).
        pub code: Option<String>,
        pub username: Option<String>,
        pub email: Option<String>,
        /// Optional mobile target (unused by找回密码; kept for the ledger).
        pub mobile: Option<String>,
        /// 0 unconsumed / 1 consumed (legacy `status`).
        pub status: i32,
        /// Issue unix seconds (legacy `ctime`).
        pub ctime: Option<i64>,
        /// Consume unix seconds (legacy `uptime`), set when `status → 1`.
        pub uptime: Option<i64>,
        /// Expiry unix seconds (legacy `endtime` = ctime + 600).
        pub endtime: Option<i64>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_order` → orders. Idempotency key `order_id` carries a unique index
/// (added by the migration).
pub mod orders {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "orders")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Wire `pay_memberid` (mch id, `= user id + 10000`).
        pub mch_id: String,
        /// Wire `pay_orderid` — merchant out order no, unique.
        pub order_id: String,
        pub amount: i64,
        pub poundage: i64,
        pub actual_amount: i64,
        pub cost: i64,
        pub apply_date: i64,
        pub success_date: Option<i64>,
        /// Wire `pay_bankcode` (the selected product id).
        pub bank_code: String,
        pub notify_url: String,
        pub callback_url: String,
        /// 0 unpaid / 1 paid-unreturned / 2 paid-returned.
        pub status: i32,
        /// Channel code of the settled account (e.g. "WxSm").
        pub channel_code: Option<String>,
        /// Upstream trade no.
        pub out_trade_id: Option<String>,
        /// Reissue counter (optimistic lock).
        pub num: i32,
        pub last_reissue_time: i64,
        /// The signing secret snapshot (orderadd `key`).
        pub sign_key: Option<String>,
        /// The sub-account appid snapshot (orderadd `account`).
        pub account: Option<String>,
        pub user_id: i64,
        pub channel_id: i64,
        pub account_id: i64,
        /// Settlement cycle: 0 T0 / 1 T+1.
        pub t: i32,
        /// 0 / 1 frozen.
        pub lock_status: i32,
        pub attach: Option<String>,
        pub product_name: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_moneychange` → the immutable fund-flow ledger.
pub mod money_changes {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "money_changes")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// Balance before, money units.
        pub y_money: i64,
        /// Delta, money units.
        pub money: i64,
        /// Balance after, money units.
        pub g_money: i64,
        pub datetime: chrono::NaiveDateTime,
        /// Flow type (lx): 1 in / 8 adjust / 9 agent-split / 11 reject /
        /// 13 deposit-unfreeze / 17 fee-refund (see `spec/00` §6).
        pub lx: i32,
        pub trans_id: Option<String>,
        pub order_id: Option<String>,
        pub content: Option<String>,
        /// Idempotency key for the flow (unique index; new in the rewrite).
        pub request_id: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_channel` → the upstream channel definition.
pub mod channels {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "channels")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// The adapter code (class name in PHP), e.g. "WxSm".
        pub code: String,
        pub title: String,
        pub mch_id: Option<String>,
        pub sign_key: Option<String>,
        pub app_id: Option<String>,
        pub app_secret: Option<String>,
        pub gateway: Option<String>,
        pub page_return: Option<String>,
        pub server_return: Option<String>,
        /// T+1 default rate (1e6 scaled) and cap (money units).
        pub default_rate: i64,
        pub fengding: i64,
        /// T+0 default rate and cap.
        pub t0_default_rate: i64,
        pub t0_fengding: i64,
        pub status: i32,
        pub paytype: i32,
        pub unlock_domain: Option<String>,
        pub control_status: i32,
        pub offline_status: i32,
        /// Same-day upstream cap, money units (`all_money`; 0 = unlimited,
        /// `spec/06` §2.1 — the day-cap the post-settle counter trips on).
        pub all_money: i64,
        /// Trading window hours (`start_time`/`end_time`; 0 = unlimited,
        /// `spec/06` §2.1 / RC:53-59).
        pub start_time: i32,
        pub end_time: i32,
        /// Per-transaction bounds, money units (0 = unbounded, §3.4).
        pub min_money: i64,
        pub max_money: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_channel_account` → per-channel sub-accounts (weighted).
pub mod channel_accounts {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "channel_accounts")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub channel_id: i64,
        pub mch_id: Option<String>,
        pub sign_key: Option<String>,
        pub app_id: Option<String>,
        pub app_secret: Option<String>,
        pub title: Option<String>,
        /// Routing weight (getWeight).
        pub weight: i32,
        pub status: i32,
        pub default_rate: i64,
        pub fengding: i64,
        pub t0_default_rate: i64,
        pub t0_fengding: i64,
        pub custom_rate: i32,
        pub control_status: i32,
        pub offline_status: i32,
        /// 1 = own risk rules, 0 = inherit the channel's (`spec/06` §2.2,
        /// legacy DDL default 0).
        pub is_defined: i32,
        /// Same-day cap, money units (0 = unlimited).
        pub all_money: i64,
        /// Unit-time throttle: `interval × time_unit` ('s'/'i'/'h'/'d'),
        /// `unit_number` max trades, `unit_all_money` max amount (0 = the
        /// dimension is off; `spec/06` §3.6).
        pub unit_interval: i32,
        pub time_unit: String,
        pub unit_number: i64,
        pub unit_all_money: i64,
        /// Own trading window hours / per-transaction bounds, money units
        /// (inherited from the channel when `is_defined = 0`; §2.2).
        pub start_time: i32,
        pub end_time: i32,
        pub min_money: i64,
        pub max_money: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_product` → the merchant-facing 产品/通道 (a product binds a concrete
/// supplier `channel`). The gateway routes `pay_bankcode` → product → channel
/// (`spec/05` §7.1).
pub mod products {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "products")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub name: String,
        /// The channel adapter code (matches `channels.code`).
        pub code: String,
        /// 0 single / 1 polling (weighted across bound accounts).
        pub polling: i32,
        /// Pay type (1..14, `paytype.php`).
        pub paytype: i32,
        pub status: i32,
        /// Show on the merchant cashier (1 yes / 0 no).
        pub isdisplay: i32,
        /// The bound supplier channel id (`channels.id`).
        pub channel: i64,
        /// Platform default weight spec (`pid:weight|…`).
        pub weight: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_product_user` → per-merchant enablement/assembly of a product
/// (polling mode, chosen channel, weight override).
pub mod product_users {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "product_users")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// The product id.
        pub pid: i64,
        pub polling: i32,
        /// 0 closed / 1 open.
        pub status: i32,
        /// A pinned single channel id (0 = auto / weighted).
        pub channel: i64,
        /// Merchant weight spec (`pid:weight|…`).
        pub weight: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_user_channel_account` → a merchant's pin of specific channel
/// sub-accounts (the `custom_rate` / account-narrowing path, `spec/03` §4.2
/// step 3).
pub mod user_channel_accounts {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "user_channel_accounts")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// The pinned account ids (comma/newline separated).
        pub account_ids: String,
        /// Whether account pinning is on (1) or the pool is open (0).
        pub status: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_userrate` → per-merchant, per-channel rate override.
pub mod user_rates {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "user_rates")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        pub channel_id: i64,
        pub rate: i64,
        pub fengding: i64,
        pub t0_rate: i64,
        pub t0_fengding: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_invitecode` → agent-issued registration invite codes (`spec/05`
/// §6.3). Two PARALLEL state columns are kept faithful to the legacy:
/// `status` (0 disabled / 1 unused / 2 used) is the lifecycle gate the
/// register path reads and consumes, while `inviteconfigzt` is the
/// secondary display flag the agent form writes and `getinviteconfigzt`
/// renders — the register path never touches it (§6.3 quirk).
pub mod invite_codes {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "invite_codes")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// The 4-char `[a-z0-9]` code (`random_str(C('INVITECODE'))`), unique.
        pub invitecode: String,
        /// Creator: the agent uid (`fmusernameid`), or a platform admin uid
        /// when `is_admin = 1`. An admin-owned code never becomes a parent.
        pub fmusernameid: i64,
        /// The registrant that consumed it (`syusernameid`), 0 = unused.
        pub syusernameid: i64,
        /// The group the invite admits (`regtype`): 4 merchant / 5-7 agent
        /// tier; drives the registrant's `groupid` (§3.1 `generateUser`).
        pub regtype: i32,
        /// Creation unix seconds (`fbdatetime`).
        pub fbdatetime: i64,
        /// Expiry unix seconds (`yxdatetime`); register-valid while `>= now`.
        pub yxdatetime: i64,
        /// Consumption unix seconds (`sydatetime`), 0 = unused.
        pub sydatetime: i64,
        /// Lifecycle gate register keys on: 0 disabled / 1 unused / 2 used.
        pub status: i32,
        /// Display flag (`inviteconfigzt`): 0 禁用 / 1 可过期 / 2 已使用.
        pub inviteconfigzt: i32,
        /// 1 = minted by the platform back-office (`is_admin`): undeletable
        /// by an agent and does not parent the registrant.
        pub is_admin: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_user_riskcontrol_config` → the merchant-screening rule row
/// (`spec/06` §2.3, URC). Exactly one row per merchant (unique `user_id`)
/// plus a single `is_system = 1` platform fallback; `status = 0` rows are
/// invisible to [`crate::risk::config::merchant_config`]. Money bounds are
/// money units; `add_time`/`edit_time` are display-only and unmodeled.
pub mod user_riskcontrol_configs {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "user_riskcontrol_configs")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// Per-transaction bounds, money units (0 = unbounded).
        pub min_money: i64,
        pub max_money: i64,
        /// Same-day cap, money units (0 = unlimited).
        pub all_money: i64,
        /// Trading window hours (0 = unlimited).
        pub start_time: i32,
        pub end_time: i32,
        /// Unit-time throttle (same shape as `channel_accounts`).
        pub unit_interval: i32,
        pub time_unit: String,
        pub unit_number: i64,
        pub unit_all_money: i64,
        /// 1 = the platform-wide fallback row (`is_system`).
        pub is_system: i32,
        /// 1 = enabled; a `status = 0` row is never served.
        pub status: i32,
        /// 封禁域名 whitelist, `\r\n`-separated hosts (empty = allow any,
        /// URC::controlDomain).
        pub domain: String,
        /// 1 = the row is the merchant's own rule, 0 = defer to the
        /// platform row (URCC::findConfigInfo).
        pub system_xz: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_tikuanconfig` → per-merchant settlement / withdrawal configuration.
/// The `issystem = 1`, `user_id = 0` row is the platform default; a merchant
/// may carry an overriding row (`systemxz = 1`). Money bounds are money
/// units (1/10000 元); `sx_rate` is a RATE_SCALE fraction (2% → 20_000).
pub mod tikuan_configs {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Default, DeriveEntityModel)]
    #[sea_orm(table_name = "tikuan_configs")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// The owning merchant (0 on the platform default row).
        pub user_id: i64,
        /// T+1 enabled: `0` → T+0 pricing, else the settlement interval tag
        /// (`1` T+1, `7` weekly-Monday, `30` monthly-day-1).
        pub t1zt: i32,
        /// Withdraw settings enabled (the user-rule gate).
        pub tkzt: i32,
        /// 0 system rule / 1 user rule.
        pub systemxz: i32,
        /// 1 = the platform default row.
        pub issystem: i32,
        /// Single-transaction minimum withdrawal, money units.
        pub tkzx_money: i64,
        /// Single-transaction maximum withdrawal, money units.
        pub tkzd_money: i64,
        /// Same-day total withdrawal cap, money units (0 = unlimited).
        pub dayzd_money: i64,
        /// Same-day withdrawal count cap (0 = unlimited).
        pub dayzd_num: i64,
        /// Allowed withdrawal window start hour (0-based).
        pub allow_start: i32,
        /// Allowed withdrawal window end hour (`0` = no window limit).
        pub allow_end: i32,
        /// Per-card same-day cap, money units (`0` = disabled).
        pub daycardzd_money: i64,
        /// Fee type: `1` per-transaction fixed, `0` percentage.
        pub tk_type: i32,
        /// Percentage fee rate, RATE_SCALE-scaled (used when `tk_type = 0`).
        pub sx_rate: i64,
        /// Fixed fee per transaction, money units (used when `tk_type = 1`).
        pub sxf_fixed: i64,
        /// Fee deduction mode: `0` from arrival, `1` from balance.
        pub tk_charge_type: i32,
        // --- 自动代付 CLI parameters (`spec/04` §10.1, only the `issystem = 1`
        //     row is read; the money columns are money units, the legacy's 元
        //     decimals scaled by MONEY_SCALE on import) ---
        /// Auto-payout master switch (legacy `auto_df_switch`): `0` = off, the
        /// whole §10.1 sweep is a no-op.
        pub auto_df_switch: i32,
        /// Per-order arrival ceiling (legacy `auto_df_maxmoney`), money units
        /// (`0` = no ceiling); orders above it never enter the auto queue.
        pub auto_df_maxmoney: i64,
        /// Daily-run window start (legacy `auto_df_stime`), a `HH:MM` clock
        /// string; empty = unrestricted (§10.1 gate, see
        /// [`crate::payout::auto_df::in_window`]).
        pub auto_df_stime: String,
        /// Daily-run window end (legacy `auto_df_etime`), a `HH:MM` string;
        /// inclusive through that minute (the legacy's `+59s`), empty = open.
        pub auto_df_etime: String,
        /// Per-merchant same-day auto-payout count cap (legacy
        /// `auto_df_max_count`): `0` = unlimited.
        pub auto_df_max_count: i64,
        /// Per-merchant same-day auto-payout amount cap, money units (legacy
        /// `auto_df_max_sum`, compared against `SUM(tkmoney)` where
        /// `is_auto = 1`): `0` = unlimited.
        pub auto_df_max_sum: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_tikuanholiday` → platform withdrawal blackout days. `datetime` holds
/// the day's UTC midnight unix seconds; a withdrawal on the matching `Ymd`
/// is rejected (§2.4).
pub mod tikuan_holidays {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "tikuan_holidays")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// The holiday's midnight unix timestamp (seconds).
        pub datetime: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_blockedlog` → T+1 frozen amounts awaiting scheduled thaw.
pub mod blocked_logs {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "blocked_logs")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        pub order_id: Option<String>,
        pub amount: i64,
        /// 0 blocked / 1 thawed.
        pub status: i32,
        pub thaw_time: i64,
        pub create_time: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_reconciliation` → the merchant×day statement snapshot
/// (`spec/02` §8). One row per merchant per day (the unique index rides
/// the migration); money columns are 元 decimals on the legacy,
/// i64 money units here, `date` is the plain statement day.
pub mod reconciliations {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "reconciliations")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// All orders created that day (any status).
        pub order_total_count: i64,
        /// Orders created that day at status 1/2 (count rides the CREATE
        /// day; the amount below rides the SUCCESS day — the legacy
        /// mixed windows, reproduced on purpose).
        pub order_success_count: i64,
        /// Orders created that day still unpaid.
        pub order_fail_count: i64,
        /// Sum actual_amount over orders created that day (any status).
        pub order_total_amount: i64,
        /// Sum actual_amount of settled orders whose success landed that day.
        pub order_success_amount: i64,
        /// Sum actual_amount of that day's created-but-unpaid orders.
        pub order_success0_amount: i64,
        /// Sum poundage of settled orders whose success landed that day.
        pub order_poundage_amount: i64,
        pub date: Date,
        pub ctime: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// `pay_pay_for_another` → the payout-channel (代付通道) configuration table
/// (`spec/04` §8 / §10, `spec/01` §3.4). One row is a live upstream the
/// execution queue submits through and re-queries against: the adapter
/// `code`, the endpoints (`exec_gateway` / `query_gateway`), the signing
/// secrets, and the cost basis. Previously hand-built into a
/// [`crate::payout::exec::PayoutChannelCfg`] per drive; now the queue reads
/// the row by the order's `df_channel_id` (§10.2) or resolves the default /
/// an operator-picked channel for a submit (§8.1 / §10.1).
///
/// `cost_rate` carries the SAME dual meaning as on
/// [`crate::payout::exec::PayoutChannelCfg`], discriminated by `rate_type`:
/// a RATE_SCALE-scaled fraction when proportional (`1`), else a fixed money
/// units cost (`0`) — the modernized (integer) stand-in for the legacy
/// `decimal(10,4)`, consistent with the `channels` / `tikuan_configs` rate
/// columns. The `public_key` / `private_key` / `app_id` columns ride the
/// table for the RSA / certificate channels not yet ported.
pub mod payout_channels {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Default, DeriveEntityModel)]
    #[sea_orm(table_name = "payout_channels")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// The adapter code (matches a registered [`crate::payout::exec::PayoutExec`]).
        pub code: String,
        /// Display title (legacy `title`; surfaced as the cfg `name`).
        pub title: String,
        pub mch_id: Option<String>,
        pub app_id: Option<String>,
        /// The trade password (Yibao `advPasswordMd5`); legacy `appsecret`.
        pub app_secret: Option<String>,
        /// The MD5 signing secret; legacy `signkey`.
        pub sign_key: Option<String>,
        pub public_key: Option<String>,
        pub private_key: Option<String>,
        /// Submit endpoint; legacy `exec_gateway`.
        pub exec_gateway: Option<String>,
        /// Query endpoint; legacy `query_gateway`.
        pub query_gateway: Option<String>,
        /// Upstream async-notify url (legacy `serverreturn`).
        pub server_return: Option<String>,
        /// Anti-block domain (legacy `unlockdomain`).
        pub unlock_domain: Option<String>,
        /// Last edit time (unix seconds; legacy `updatetime`).
        pub update_time: i64,
        /// 1 enabled / 0 disabled — a disabled channel is never a submit
        /// target but stays queryable for in-flight orders (§10.2).
        pub status: i32,
        /// The auto-submit default (`status=1 AND is_default=1`, §10.1).
        pub is_default: i32,
        /// Cost basis, rate_type-discriminated (see module doc).
        pub cost_rate: i64,
        /// 0 fixed per-transaction / 1 proportional to the arrival amount.
        pub rate_type: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The unified payout order (`spec/04` §13.1) — one table converging the
/// legacy `pay_tklist` (merchant settlement), `pay_wttklist` (entrusted /
/// batch) and `pay_df_api_order` (downstream payout API) rows, discriminated
/// by `source`. All money columns are i64 money units (the legacy's 元
/// decimals are the §12 quantisation defect the rewrite closes);
/// `order_no` is unique and `(user_id, out_trade_no)` carries a partial
/// unique index (§13.4 idempotency, replacing the racy find-then-add).
/// The `df_*` execution-queue columns are pre-built for the channel runner
/// (a later slice); the review flow rides `check_status` / `reject_reason`.
pub mod payout_orders {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Default, DeriveEntityModel)]
    #[sea_orm(table_name = "payout_orders")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Platform order no (legacy `getOrderId`), unique.
        pub order_no: String,
        /// Downstream merchant order no (df API); NULL for own withdrawals.
        pub out_trade_no: Option<String>,
        /// The owning merchant (`members.id`).
        pub user_id: i64,
        /// 1 settlement (tklist) / 2 entrusted (wttklist) / 3 payout API.
        pub source: i16,
        /// Main status: 0 pending / 1 processing / 2 success / 3 rejected /
        /// 4 unconfirmed (channel failure awaiting query, §8.3 trap).
        pub status: i16,
        /// API review sub-status (df rows only): 0 pending / 1 approved /
        /// 2 rejected; NULL for non-API sources.
        pub check_status: Option<i16>,
        /// Settlement cycle recorded on the order (0 / 1 / 7 / 30).
        pub t: i32,
        /// Requested withdrawal principal, money units.
        pub tkmoney: i64,
        /// Fee, money units.
        pub sxfmoney: i64,
        /// Arrival amount, money units (`= tkmoney` when charge type 1).
        pub money: i64,
        /// Fee deduction mode: 0 from arrival / 1 from balance.
        pub charge_type: i32,
        // --- payee bank snapshot (legacy tklist columns) ---
        pub bankname: Option<String>,
        pub subbranch: Option<String>,
        pub accountname: Option<String>,
        pub cardnumber: Option<String>,
        pub province: Option<String>,
        pub city: Option<String>,
        /// Channel-specific extension JSON (legacy `additional` / `extends`).
        pub additional: Option<String>,
        // --- execution-queue columns (§8 / §10, wired by `payout::exec`) ---
        pub df_channel_id: Option<i64>,
        pub df_code: Option<String>,
        pub df_name: Option<String>,
        /// Upstream merchant id of the payout channel (`channel_mch_id`).
        pub channel_mch_id: Option<String>,
        /// Channel cost booked on submit, money units (`cost`).
        pub cost: i64,
        /// The channel cost rate snapshot, RATE_SCALE-scaled (`cost_rate`).
        pub cost_rate: i64,
        /// Cost basis: `1` proportional (money × cost_rate) / `0` fixed.
        pub rate_type: i32,
        /// 0 free / 1 claimed by an executor.
        pub df_lock: i32,
        /// Last submit timestamp (unix seconds, 0 = never).
        pub last_submit_time: i64,
        /// Auto-submit attempt counter (the `< 5` retry valve, §10.1).
        pub auto_submit_try: i32,
        /// Auto-query attempt counter (`auto_query_num`, §10.2).
        pub auto_query_num: i32,
        /// 1 when booked by the自动代付 CLI (§10.1).
        pub is_auto: i32,
        /// Rejection reason (review flow).
        pub reject_reason: Option<String>,
        /// Operator memo (the channel `msg` written by `handle`).
        pub memo: Option<String>,
        // --- timestamps ---
        pub created_at: chrono::NaiveDateTime,
        pub review_time: Option<i64>,
        /// Settle time `cldatetime` (unix seconds, stamped on success).
        pub settled_at: Option<i64>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The merchant outbound-notify audit trail (`spec/02` §4.6). The legacy's
/// `log_server_notify` (P:447, Common function.php:1349) appended a FILE line
/// per attempt under `Data/server_notify/` — the `pay_paylog` DDL sat beside
/// it unwritten. The rewrite models the same append-per-attempt ledger as a
/// table: every notify POST (settled, duplicate or reissue retry, reachable
/// or not) gets its row, so the §7.1 reissue history is queryable instead of
/// grep-ing daily files. No unique key — retries are the point.
pub mod notify_logs {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "notify_logs")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        /// Platform order no (legacy `pay_orderid`).
        pub order_id: String,
        /// The merchant URL the POST rode to.
        pub notify_url: String,
        /// The reply body as sent (`k=v&…`, values unencoded for readability
        /// — the legacy logged curl's urlencoded bytes).
        pub notify_str: String,
        /// Upstream HTTP status; `0` = transport failure.
        pub http_code: i32,
        /// The merchant's reply body (the transport error text on failure).
        pub contents: String,
        /// 1 when the reply carried the legacy `ok` substring (§4.2 step 3).
        pub acked: i32,
        /// Attempt time, unix seconds.
        pub create_time: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// The manual-reversal ledger (`pay_redo_order`, `spec/02` §6.5): the legacy
/// shipped only the READ side (the statistics sum `type=1`/`type=2` rows into
/// the merchant income formula) with NO write path — `spec/02` §11 mandates
/// the rewrite build one ([`crate::ledger::LedgerService::redo_balance`]).
/// One row per reversal, next to the balance move + `money_changes` flow it
/// drives in the same transaction.
pub mod redo_orders {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "redo_orders")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub user_id: i64,
        /// Operating admin (`admin_id`; 0 until the Phase-7 surface carries
        /// real operator identities).
        pub admin_id: i64,
        /// Reversal amount, money units.
        pub money: i64,
        /// 1 增加 / 2 减少 (legacy `type` — renamed off the SQL keyword).
        pub redo_type: i32,
        /// Operator remark.
        pub remark: String,
        /// The reversal period the row counts into (legacy `date` datetime).
        pub date: chrono::NaiveDateTime,
        /// Operation time, unix seconds (legacy `ctime`).
        pub ctime: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
