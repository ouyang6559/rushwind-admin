//! Merchant台卡 / 收款码 (`spec/05` §10) — the port of
//! `User/AccountController::link / qrcode / saveReceiver`. The收款码 is a
//! plain pointer at the hosted cash-page `Pay/Charges/index?mid=<mch>` where
//! `<mch> = uid + 10000` (the wire merchant number, [`crate::merchant::mch_id_of`]);
//! [`charges_url`] builds it from the configured site base, exactly as the
//! legacy `link()` page does.
//!
//! [`save_receiver`] persists the台卡 payee line (`member.receiver`) that the
//! legacy composites onto the QR background. The legacy `saveReceiver` saved an
//! arbitrary `I('request.p')` array straight onto the member (a mass-assignment
//! hazard); this port narrows the write to the single `receiver` column the page
//! actually edits — the safer, intent-preserving subset.
//!
//! QR-IMAGE RENDERING is intentionally out of scope: `qrcode`'s `\QRcode::png`
//! plus Intervention-Image background compositing (and `downQrcode`'s PNG
//! download) ride a raster pipeline this service does not carry.
//! [`qr_target_path`] reports the site-relative path a rendered card image
//! would live at, so the frontend or a future renderer can key off one name.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::members;
use crate::merchant::mch_id_of;
use crate::state::{GatewayError, GatewayResult};

/// The hosted cash-page route the收款码 points at (legacy `U('Pay/Charges/index')`).
pub const CHARGES_ROUTE: &str = "Pay/Charges/index";

/// Builds the收款码 URL for a merchant: `<site>/Pay/Charges/index?mid=<uid+10000>`.
/// A trailing slash on `site_url` is trimmed so the join is exact.
pub fn charges_url(site_url: &str, uid: i64) -> String {
    let site = site_url.trim_end_matches('/');
    format!("{site}/{CHARGES_ROUTE}?mid={}", mch_id_of(uid))
}

/// The site-relative path a rendered台卡 QR image would live at
/// (`Uploads/charges/<mch>.png`), matching the legacy `qrcode` / `downQrcode`
/// file name. Rendering itself is a documented seam.
pub fn qr_target_path(uid: i64) -> String {
    format!("Uploads/charges/{}.png", mch_id_of(uid))
}

/// Reads the member's台卡 payee line (`receiver`); `None` when unset / missing.
pub async fn get_receiver(db: &DatabaseConnection, uid: i64) -> GatewayResult<Option<String>> {
    let model = members::Entity::find_by_id(uid)
        .one(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(model.and_then(|m| m.receiver))
}

/// Writes the台卡 payee line (`saveReceiver`, narrowed to `receiver` only).
/// Returns the affected-row count (Postgres counts a same-value update as 1).
pub async fn save_receiver(
    db: &DatabaseConnection,
    uid: i64,
    receiver: &str,
) -> GatewayResult<u64> {
    let res = members::Entity::update_many()
        .col_expr(
            members::Column::Receiver,
            Expr::value(Some(receiver.to_string())),
        )
        .filter(members::Column::Id.eq(uid))
        .exec(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(res.rows_affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_url_offsets_mch_and_trims_trailing_slash() {
        assert_eq!(
            charges_url("https://pay.example.com", 1001),
            "https://pay.example.com/Pay/Charges/index?mid=11001"
        );
        // a trailing slash must not double up
        assert_eq!(
            charges_url("https://pay.example.com/", 7),
            "https://pay.example.com/Pay/Charges/index?mid=10007"
        );
    }

    #[test]
    fn qr_target_path_uses_mch_number() {
        assert_eq!(qr_target_path(180772223), "Uploads/charges/180782223.png");
    }
}
