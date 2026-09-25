//! The payout-channel store (`spec/04` §8 / §10): the read half that turns a
//! `payout_channels` (`pay_pay_for_another`) row into the
//! [`PayoutChannelCfg`] the [`super::exec`] sweeps drive, and the back-office
//! CRUD that maintains those rows.
//!
//! Before this slice the queue's channel endpoints + secrets were hand-built
//! into a [`PayoutChannelCfg`] by every caller. The legacy never did that —
//! `AutodfController::doDf` submitted through the `status=1 AND is_default=1`
//! row, `IndexController` through `findPaymentType(id)` (`status=1`, the
//! operator's pick), and the query loop read `pay_for_another WHERE id =
//! df_id` with **no status filter** (a since-disabled channel must still
//! settle its in-flight orders). [`PayoutChannelRepo`] reproduces exactly
//! those access paths, so the sweeps resolve config straight from the library.
//!
//! The cost basis is carried verbatim: the `payout_channels.cost_rate` column
//! already stores the [`PayoutChannelCfg`] representation (a RATE_SCALE
//! fraction when `rate_type = 1`, a money-units fixed cost when `0`), so
//! [`to_cfg`] is a plain field map — the decimal(10,4) → integer scaling is a
//! data-migration concern, not a per-read one.

use std::collections::BTreeMap;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::data::payout_channels;
use crate::state::{GatewayError, GatewayResult};

use super::exec::PayoutChannelCfg;

/// Whether a channel row is enabled for submit.
pub const STATUS_ENABLED: i32 = 1;
/// Proportional cost basis (`money × cost_rate`); `0` is a fixed per-txn cost.
pub const RATE_TYPE_PROPORTIONAL: i32 = 1;

/// Projects a stored row onto the [`PayoutChannelCfg`] the adapters and
/// [`super::exec::ExecAttribution`] consume. Pure so the endpoint / secret /
/// cost mapping is pinned offline.
pub fn to_cfg(m: &payout_channels::Model) -> PayoutChannelCfg {
    PayoutChannelCfg {
        id: m.id,
        code: m.code.clone(),
        name: m.title.clone(),
        mch_id: m.mch_id.clone(),
        rate_type: m.rate_type,
        cost_rate: m.cost_rate,
        exec_gateway: m.exec_gateway.clone().unwrap_or_default(),
        query_gateway: m.query_gateway.clone().unwrap_or_default(),
        sign_key: m.sign_key.clone().unwrap_or_default(),
        app_secret: m.app_secret.clone().unwrap_or_default(),
    }
}

/// The create payload (a full channel row minus `id` / `update_time`).
/// Money-bearing `cost_rate` rides the [`PayoutChannelCfg`] representation
/// described in the module doc.
#[derive(Debug, Clone)]
pub struct NewPayoutChannel {
    pub code: String,
    pub title: String,
    pub mch_id: Option<String>,
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    pub sign_key: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    pub exec_gateway: Option<String>,
    pub query_gateway: Option<String>,
    pub server_return: Option<String>,
    pub unlock_domain: Option<String>,
    pub cost_rate: i64,
    pub rate_type: i32,
    /// 1 enabled / 0 disabled.
    pub status: i32,
    /// 1 marks the auto-submit default.
    pub is_default: i32,
}

impl NewPayoutChannel {
    fn validate(&self) -> Result<(), GatewayError> {
        if self.code.trim().is_empty() {
            return Err(GatewayError::BadRequest("代付渠道代码不能为空".into()));
        }
        if self.title.trim().is_empty() {
            return Err(GatewayError::BadRequest("代付渠道名称不能为空".into()));
        }
        check_flags(self.status, self.rate_type)?;
        if self.cost_rate < 0 {
            return Err(GatewayError::BadRequest("成本费率不能为负".into()));
        }
        Ok(())
    }
}

/// The mutable slice of a channel row; a `None` field is left untouched.
#[derive(Debug, Clone, Default)]
pub struct UpdatePayoutChannel {
    pub code: Option<String>,
    pub title: Option<String>,
    pub mch_id: Option<String>,
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    pub sign_key: Option<String>,
    pub public_key: Option<String>,
    pub private_key: Option<String>,
    pub exec_gateway: Option<String>,
    pub query_gateway: Option<String>,
    pub server_return: Option<String>,
    pub unlock_domain: Option<String>,
    pub cost_rate: Option<i64>,
    pub rate_type: Option<i32>,
    pub status: Option<i32>,
    pub is_default: Option<i32>,
    pub update_time: i64,
}

impl UpdatePayoutChannel {
    fn validate(&self) -> Result<(), GatewayError> {
        if let Some(c) = &self.code {
            if c.trim().is_empty() {
                return Err(GatewayError::BadRequest("代付渠道代码不能为空".into()));
            }
        }
        if let Some(t) = &self.title {
            if t.trim().is_empty() {
                return Err(GatewayError::BadRequest("代付渠道名称不能为空".into()));
            }
        }
        if let Some(rt) = self.rate_type {
            if rt != 0 && rt != RATE_TYPE_PROPORTIONAL {
                return Err(GatewayError::BadRequest("费率类型错误".into()));
            }
        }
        if let Some(st) = self.status {
            if st != 0 && st != STATUS_ENABLED {
                return Err(GatewayError::BadRequest("状态错误".into()));
            }
        }
        if self.cost_rate.is_some_and(|c| c < 0) {
            return Err(GatewayError::BadRequest("成本费率不能为负".into()));
        }
        Ok(())
    }
}

fn check_flags(status: i32, rate_type: i32) -> Result<(), GatewayError> {
    if status != 0 && status != STATUS_ENABLED {
        return Err(GatewayError::BadRequest("状态错误".into()));
    }
    if rate_type != 0 && rate_type != RATE_TYPE_PROPORTIONAL {
        return Err(GatewayError::BadRequest("费率类型错误".into()));
    }
    Ok(())
}

/// Reads and maintains `payout_channels`. Generic over the connection (like
/// [`super::config::PayoutConfigRepo`]) so the [`super::PayoutService`] sweeps
/// reuse their own pool and the back-office CRUD can run on a transaction.
pub struct PayoutChannelRepo<'a, C: ConnectionTrait> {
    db: &'a C,
}

impl<'a, C: ConnectionTrait> PayoutChannelRepo<'a, C> {
    pub fn new(db: &'a C) -> Self {
        Self { db }
    }

    // --- resolution (the sweep access paths) --------------------------------

    /// Loads a row by primary key, whatever its status.
    pub async fn by_id(&self, id: i64) -> GatewayResult<Option<payout_channels::Model>> {
        Ok(payout_channels::Entity::find_by_id(id).one(self.db).await?)
    }

    /// The cfg for a channel id with **no status filter** — the §10.2 query
    /// sweep (`pay_for_another WHERE id = df_id`), so an order already in
    /// flight still settles after its channel is taken offline.
    pub async fn cfg_by_id(&self, id: i64) -> GatewayResult<Option<PayoutChannelCfg>> {
        Ok(self.by_id(id).await?.map(|m| to_cfg(&m)))
    }

    /// The cfg for an operator-picked channel that must be enabled — the §8.1
    /// manual-submit `findPaymentType(id)` (`status = 1`).
    pub async fn cfg_enabled_by_id(&self, id: i64) -> GatewayResult<Option<PayoutChannelCfg>> {
        Ok(payout_channels::Entity::find()
            .filter(payout_channels::Column::Id.eq(id))
            .filter(payout_channels::Column::Status.eq(STATUS_ENABLED))
            .one(self.db)
            .await?
            .map(|m| to_cfg(&m)))
    }

    /// The auto-submit default — `status = 1 AND is_default = 1`
    /// (`AutodfController::doDf`'s `$channel`).
    pub async fn default_enabled_cfg(&self) -> GatewayResult<Option<PayoutChannelCfg>> {
        Ok(payout_channels::Entity::find()
            .filter(payout_channels::Column::Status.eq(STATUS_ENABLED))
            .filter(payout_channels::Column::IsDefault.eq(1))
            .order_by_asc(payout_channels::Column::Id)
            .one(self.db)
            .await?
            .map(|m| to_cfg(&m)))
    }

    /// Batch-loads the cfgs for a set of channel ids (no status filter), the
    /// §10.2 query sweep resolving every in-flight order's `df_channel_id` in
    /// one read. Ids with no row are simply absent (the sweep counts those
    /// `no_adapter`).
    pub async fn cfgs_for_ids(
        &self,
        ids: &[i64],
    ) -> GatewayResult<BTreeMap<i64, PayoutChannelCfg>> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = payout_channels::Entity::find()
            .filter(payout_channels::Column::Id.is_in(ids.iter().copied()))
            .all(self.db)
            .await?;
        Ok(rows.iter().map(|m| (m.id, to_cfg(m))).collect())
    }

    /// The enabled channels, for the merchant panel's 可用代付渠道 dropdown
    /// (§6.2 / §6.3, the legacy `pay_for_another where status=1`).
    pub async fn list_enabled(&self) -> GatewayResult<Vec<payout_channels::Model>> {
        Ok(payout_channels::Entity::find()
            .filter(payout_channels::Column::Status.eq(STATUS_ENABLED))
            .order_by_desc(payout_channels::Column::IsDefault)
            .order_by_asc(payout_channels::Column::Id)
            .all(self.db)
            .await?)
    }

    // --- back-office CRUD ---------------------------------------------------

    /// Inserts a new channel; the DB assigns `id`, `update_time` is stamped
    /// now. Rejects a blank code / title or a bad flag.
    pub async fn create(
        &self,
        in_: NewPayoutChannel,
        now_ts: i64,
    ) -> GatewayResult<payout_channels::Model> {
        in_.validate()?;
        let am = payout_channels::ActiveModel {
            code: Set(in_.code.trim().to_string()),
            title: Set(in_.title.trim().to_string()),
            mch_id: Set(in_.mch_id),
            app_id: Set(in_.app_id),
            app_secret: Set(in_.app_secret),
            sign_key: Set(in_.sign_key),
            public_key: Set(in_.public_key),
            private_key: Set(in_.private_key),
            exec_gateway: Set(in_.exec_gateway),
            query_gateway: Set(in_.query_gateway),
            server_return: Set(in_.server_return),
            unlock_domain: Set(in_.unlock_domain),
            update_time: Set(now_ts),
            status: Set(in_.status),
            is_default: Set(in_.is_default),
            cost_rate: Set(in_.cost_rate),
            rate_type: Set(in_.rate_type),
            ..Default::default()
        };
        Ok(am.insert(self.db).await?)
    }

    /// Applies a partial update to a live row, stamping `update_time`. Returns
    /// `None` when the id is unknown.
    pub async fn update(
        &self,
        id: i64,
        patch: UpdatePayoutChannel,
    ) -> GatewayResult<Option<payout_channels::Model>> {
        patch.validate()?;
        let Some(model) = self.by_id(id).await? else {
            return Ok(None);
        };
        let mut am: payout_channels::ActiveModel = model.into();
        if let Some(v) = patch.code {
            am.code = Set(v.trim().to_string());
        }
        if let Some(v) = patch.title {
            am.title = Set(v.trim().to_string());
        }
        if let Some(v) = patch.mch_id {
            am.mch_id = Set(Some(v));
        }
        if let Some(v) = patch.app_id {
            am.app_id = Set(Some(v));
        }
        if let Some(v) = patch.app_secret {
            am.app_secret = Set(Some(v));
        }
        if let Some(v) = patch.sign_key {
            am.sign_key = Set(Some(v));
        }
        if let Some(v) = patch.public_key {
            am.public_key = Set(Some(v));
        }
        if let Some(v) = patch.private_key {
            am.private_key = Set(Some(v));
        }
        if let Some(v) = patch.exec_gateway {
            am.exec_gateway = Set(Some(v));
        }
        if let Some(v) = patch.query_gateway {
            am.query_gateway = Set(Some(v));
        }
        if let Some(v) = patch.server_return {
            am.server_return = Set(Some(v));
        }
        if let Some(v) = patch.unlock_domain {
            am.unlock_domain = Set(Some(v));
        }
        if let Some(v) = patch.cost_rate {
            am.cost_rate = Set(v);
        }
        if let Some(v) = patch.rate_type {
            am.rate_type = Set(v);
        }
        if let Some(v) = patch.status {
            am.status = Set(v);
        }
        if let Some(v) = patch.is_default {
            am.is_default = Set(v);
        }
        am.update_time = Set(patch.update_time);
        Ok(Some(am.update(self.db).await?))
    }

    /// Enables (`true`) / disables (`false`) a channel. `None` = unknown id.
    pub async fn set_status(
        &self,
        id: i64,
        enabled: bool,
        now_ts: i64,
    ) -> GatewayResult<Option<payout_channels::Model>> {
        let Some(model) = self.by_id(id).await? else {
            return Ok(None);
        };
        let mut am: payout_channels::ActiveModel = model.into();
        am.status = Set(if enabled { STATUS_ENABLED } else { 0 });
        am.update_time = Set(now_ts);
        Ok(Some(am.update(self.db).await?))
    }

    /// Makes one channel the sole auto-submit default (clearing any other),
    /// matching the single-`is_default` invariant the auto sweep assumes.
    /// `None` = unknown id.
    pub async fn set_default(
        &self,
        id: i64,
        now_ts: i64,
    ) -> GatewayResult<Option<payout_channels::Model>> {
        let Some(model) = self.by_id(id).await? else {
            return Ok(None);
        };
        payout_channels::Entity::update_many()
            .col_expr(payout_channels::Column::IsDefault, Expr::value(0i32))
            .filter(payout_channels::Column::IsDefault.eq(1))
            .filter(payout_channels::Column::Id.ne(id))
            .exec(self.db)
            .await?;
        let mut am: payout_channels::ActiveModel = model.into();
        am.is_default = Set(1);
        am.update_time = Set(now_ts);
        Ok(Some(am.update(self.db).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> payout_channels::Model {
        payout_channels::Model {
            id: 21,
            code: "yibao".into(),
            title: "易宝代付".into(),
            mch_id: Some("MCH21".into()),
            app_secret: Some("sec".into()),
            sign_key: Some("key".into()),
            exec_gateway: Some("https://x/exec".into()),
            query_gateway: None,
            cost_rate: 20_000,
            rate_type: 1,
            status: 1,
            is_default: 1,
            ..Default::default()
        }
    }

    #[test]
    fn to_cfg_maps_fields_and_blanks_missing_endpoints() {
        let cfg = to_cfg(&row());
        assert_eq!(cfg.id, 21);
        assert_eq!(cfg.code, "yibao");
        assert_eq!(cfg.name, "易宝代付");
        assert_eq!(cfg.mch_id.as_deref(), Some("MCH21"));
        assert_eq!(cfg.cost_rate, 20_000);
        assert_eq!(cfg.rate_type, 1);
        assert_eq!(cfg.exec_gateway, "https://x/exec");
        // A NULL query_gateway becomes an empty string on the cfg (the
        // adapter treats it as unconfigured), never a panic.
        assert_eq!(cfg.query_gateway, "");
        assert_eq!(cfg.sign_key, "key");
        assert_eq!(cfg.app_secret, "sec");
    }

    #[test]
    fn create_rejects_blank_code_and_bad_flags() {
        let base = NewPayoutChannel {
            code: "  ".into(),
            title: "t".into(),
            mch_id: None,
            app_id: None,
            app_secret: None,
            sign_key: None,
            public_key: None,
            private_key: None,
            exec_gateway: None,
            query_gateway: None,
            server_return: None,
            unlock_domain: None,
            cost_rate: 0,
            rate_type: 0,
            status: 1,
            is_default: 0,
        };
        assert!(base.validate().is_err(), "blank code rejected");

        let ok = NewPayoutChannel {
            code: "mgzf".into(),
            ..base.clone()
        };
        assert!(ok.validate().is_ok());

        let bad_status = NewPayoutChannel {
            status: 7,
            ..ok.clone()
        };
        assert!(bad_status.validate().is_err());

        let bad_rate = NewPayoutChannel {
            rate_type: 5,
            ..ok.clone()
        };
        assert!(bad_rate.validate().is_err());

        let neg_cost = NewPayoutChannel {
            cost_rate: -1,
            ..ok
        };
        assert!(neg_cost.validate().is_err());
    }

    #[test]
    fn update_validates_only_present_fields() {
        // A patch that touches neither code nor flags validates even though
        // it leaves those untouched (None means "do not change").
        let patch = UpdatePayoutChannel {
            exec_gateway: Some("https://new".into()),
            ..Default::default()
        };
        assert!(patch.validate().is_ok());
        let blank = UpdatePayoutChannel {
            code: Some("   ".into()),
            ..Default::default()
        };
        assert!(blank.validate().is_err());
        let neg = UpdatePayoutChannel {
            cost_rate: Some(-1),
            ..Default::default()
        };
        assert!(neg.validate().is_err());
    }
}
