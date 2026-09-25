//! The merchant-panel route pack — a JSON HTTP surface distinct from the
//! wire-compatible gateway: it authenticates with a Redis-backed session token
//! (the legacy `isLogin()` console) rather than an in-form MD5 signature, and
//! carries JSON bodies. Registered under
//! `servers[].route_packs[].name: payment-panel`.
//!
//! - `POST /panel/login`                 → issue a session token
//! - `POST /panel/logout`                → revoke it
//! - `POST /panel/register`              → §3 商户/代理自助注册（登录前公开，邀请码/激活由站点开关）
//! - `POST /panel/activate`              → §3.4 邮箱激活链接消费（登录前公开，status 0→1）
//! - `POST /panel/payout/df_pass_batch`   → §6.5 批量审核通过
//! - `POST /panel/payout/df_reject_batch` → §6.5 批量审核驳回
//! - `POST /panel/agent/save_user_rate`   → §6.2 代理给下级配费率
//! - `POST /panel/agent/user_rate_edit`    → §6.2 下级费率编辑页读侧（已开通产品 + 现费率装载）
//! - `POST /panel/agent/create_invite`     → §6.3 生成邀请码
//! - `POST /panel/agent/delete_invite`     → §6.3 删除邀请码
//! - `POST /panel/agent/save_user`         → §6.1 代理开商户（下级默认为 groupid=4 商户）
//! - `POST /panel/agent/downline_list`      → §6.4 下级会员分页列表（parentid 归属 + 过滤）
//! - `POST /panel/agent/downline_set_status` → §6.4 下级启停（仅直属下级）
//! - `POST /panel/agent/downline_report`     → §7 下级成交/分润聚合读面（childord）
//! - `POST /panel/agent/export_user`         → §6.5 下级会员导出（CSV，复用 DownlineFilter）
//! - `POST /panel/agent/downline_order_list` → §6.4 下级订单明细分页（order/childord，含今日/累计或窗口统计）
//! - `POST /panel/agent/export_order`        → §6.4 下级订单导出（CSV，status in 1,2）
//! - `POST /panel/apikey/view`             → §9 查看 APIKEY（支付密码二次验证）
//! - `POST /panel/profile/save`            → §10 编辑资料（谷歌/短信二次验证矩阵）
//! - `POST /panel/bankcard/save`           → §10 新增/编辑银行卡
//! - `POST /panel/bankcard/set_default`    → §10 设为默认卡
//! - `POST /panel/bankcard/delete`         → §10 删除银行卡
//! - `POST /panel/bankcard/list`           → §10 银行卡列表
//! - `POST /panel/password/pay/edit`       → §10 修改支付密码
//! - `POST /panel/password/login/edit`     → §10 修改登录密码
//! - `POST /panel/mobile/bind/send`        → §10 绑定手机（下发短信码）
//! - `POST /panel/mobile/bind/confirm`     → §10 绑定手机（校验码写入）
//! - `POST /panel/mobile/edit/send`        → §10 换绑手机（旧/新号下发码）
//! - `POST /panel/mobile/edit/confirm`     → §10 换绑手机（两步校验码写入）
//! - `POST /panel/google/initiate`         → §10 谷歌验证器 发起（下发待绑定 secret）
//! - `POST /panel/google/bind`             → §10 谷歌验证器 绑定（TOTP 校验后写入）
//! - `POST /panel/google/unbind`           → §10 谷歌验证器 解绑
//! - `POST /panel/attachment/list`         → §8.5 认证附件列表（含 authorized 状态）
//! - `POST /panel/attachment/upload`       → §8.5 认证附件上传（multipart，jpg/gif/png ≤2MB）
//! - `POST /panel/certification/submit`     → §8.5 提交认证（authorized=2 待审核）
//! - `POST /panel/loginrecord/list`         → §5.4 登录记录分页查询（本商户 type=0）
//! - `POST /panel/charges/link`             → §10 台卡收款码链接（Pay/Charges/index?mid）
//! - `POST /panel/charges/qrcode`           → §10 台卡收款码信息（URL + 收款人 + QR 目标路径；渲染延后）
//! - `POST /panel/charges/receiver`         → §10 保存台卡收款人（member.receiver）
//! - `POST /panel/console/main`             → §10 控制台首页聚合（今日 stat + 公告 + 登录记录）
//! - `POST /panel/console/gonggao`          → §10 公告分页列表
//! - `POST /panel/deposit/list`             → §10 保证金明细分页（本商户，含 all/已解冻/待解冻 汇总）
//! - `POST /panel/forgetpwd/send_code`      → §5 找回密码（邮箱验证码下发，登录前公开）
//! - `POST /panel/forgetpwd/reset`          → §5 找回密码（校验邮箱码并重置登录密码）

use std::sync::Arc;

use axum::routing::post;
use rushwind_bootstrap::{BootstrapError, RouteInput, RouteSurface};

use crate::panel::handlers;
use crate::state::AppState;

/// The merchant-panel route pack.
pub fn pack(
    state: Arc<AppState>,
) -> impl Fn(serde_json::Value, RouteInput) -> Result<RouteSurface, BootstrapError> + Send + Sync + 'static
{
    move |_settings, _input| {
        let router = axum::Router::new()
            .route("/panel/login", post(handlers::login_handler))
            .route("/panel/register", post(handlers::register_submit))
            .route("/panel/activate", post(handlers::activate_handler))
            .route("/panel/logout", post(handlers::logout_handler))
            .route("/panel/payout/df_pass_batch", post(handlers::df_pass_batch))
            .route(
                "/panel/payout/df_reject_batch",
                post(handlers::df_reject_batch),
            )
            .route(
                "/panel/agent/save_user_rate",
                post(handlers::save_user_rate),
            )
            .route(
                "/panel/agent/user_rate_edit",
                post(handlers::user_rate_edit),
            )
            .route("/panel/agent/create_invite", post(handlers::create_invite))
            .route("/panel/agent/delete_invite", post(handlers::delete_invite))
            .route("/panel/agent/save_user", post(handlers::agent_save_user))
            .route("/panel/agent/downline_list", post(handlers::downline_list))
            .route(
                "/panel/agent/downline_set_status",
                post(handlers::downline_set_status),
            )
            .route(
                "/panel/agent/downline_report",
                post(handlers::downline_report),
            )
            .route(
                "/panel/agent/export_user",
                post(handlers::agent_export_user),
            )
            .route(
                "/panel/agent/downline_order_list",
                post(handlers::downline_order_list),
            )
            .route(
                "/panel/agent/export_order",
                post(handlers::agent_export_order),
            )
            .route("/panel/apikey/view", post(handlers::apikey_view))
            .route("/panel/profile/save", post(handlers::profile_save))
            .route("/panel/bankcard/save", post(handlers::bankcard_save))
            .route(
                "/panel/bankcard/set_default",
                post(handlers::bankcard_set_default),
            )
            .route("/panel/bankcard/delete", post(handlers::bankcard_delete))
            .route("/panel/bankcard/list", post(handlers::bankcard_list))
            .route(
                "/panel/password/pay/edit",
                post(handlers::pay_password_edit),
            )
            .route(
                "/panel/password/login/edit",
                post(handlers::login_password_edit),
            )
            .route("/panel/mobile/bind/send", post(handlers::mobile_bind_send))
            .route(
                "/panel/mobile/bind/confirm",
                post(handlers::mobile_bind_confirm),
            )
            .route("/panel/mobile/edit/send", post(handlers::mobile_edit_send))
            .route(
                "/panel/mobile/edit/confirm",
                post(handlers::mobile_edit_confirm),
            )
            .route("/panel/google/initiate", post(handlers::google_initiate))
            .route("/panel/google/bind", post(handlers::google_bind))
            .route("/panel/google/unbind", post(handlers::google_unbind))
            .route("/panel/attachment/list", post(handlers::attachment_list))
            .route(
                "/panel/attachment/upload",
                post(handlers::attachment_upload),
            )
            .route(
                "/panel/certification/submit",
                post(handlers::certification_submit),
            )
            .route("/panel/loginrecord/list", post(handlers::loginrecord_list))
            .route("/panel/charges/link", post(handlers::charges_link))
            .route("/panel/charges/qrcode", post(handlers::charges_qrcode))
            .route(
                "/panel/charges/receiver",
                post(handlers::charges_save_receiver),
            )
            .route("/panel/console/main", post(handlers::console_main))
            .route("/panel/console/gonggao", post(handlers::console_gonggao))
            .route(
                "/panel/deposit/list",
                post(handlers::complaints_deposit_list),
            )
            .route(
                "/panel/forgetpwd/send_code",
                post(handlers::forgetpwd_send_code),
            )
            .route("/panel/forgetpwd/reset", post(handlers::forgetpwd_reset))
            .with_state(Arc::clone(&state));
        Ok(RouteSurface::new(router))
    }
}
