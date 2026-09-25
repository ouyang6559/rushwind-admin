//! Merchant KYC attachment upload — the port of
//! `User/AccountController::authorized / upload / certification`
//! (`spec/05` §8.5). Three primitives back the认证 page:
//!
//! - [`FileStorage`] is the on-disk store (a LocalFs rooted at a configurable
//!   directory, default `./Uploads`); `upload` writes the bytes under
//!   `<root>/verifyinfo/<uniqid>.<ext>` and files the site-relative
//!   `Uploads/verifyinfo/<uniqid>.<ext>` record via [`add`];
//! - [`list_for_user`] + [`get_authorized`] serve the `authorized()` page
//!   (the member's current认证 state plus their evidence rows);
//! - [`submit_certification`] is the `certification()` write: it sets
//!   `authorized = 2` (待审核) with NO current-state gate, faithfully matching
//!   the legacy `save(['authorized' => 2])` (an already-certified member may
//!   re-submit and drop back to pending — the legacy lets them).
//!
//! Upload constraints mirror the legacy `Upload` guard: extension in
//! jpg/gif/png and a 2 MiB (`2_097_152`) byte ceiling; the stored file name is
//! a `uniqid` — the 13 lowercase hex digits of the current microsecond, like
//! PHP's `uniqid()`. The original browser file name is preserved in the row's
//! `filename` column (as legacy does), while `path` records the generated name.

use std::path::PathBuf;

use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::data::{attachments, members};
use crate::state::{GatewayError, GatewayResult};

/// The legacy `Upload::maxSize` for认证 evidence (2 MiB, in bytes).
pub const MAX_BYTES: u64 = 2_097_152;
/// The legacy `Upload::exts` whitelist.
pub const ALLOWED_EXTS: [&str; 3] = ["jpg", "gif", "png"];
/// The on-disk subdirectory under the uploads root (`Upload::savePath`).
pub const SUBDIR: &str = "verifyinfo";
/// The site-relative prefix recorded in the `path` column (legacy concatenates
/// `'Uploads' . $info['savepath'] . $info['savename']`).
pub const RECORD_PREFIX: &str = "Uploads";

/// The `authorized` value `certification()` files (§8.5: 0 未认证 / 1 已认证 /
/// 2 待审核).
pub const AUTHORIZED_PENDING: i32 = 2;

/// Whether an extension passes the whitelist (case-insensitive, as the legacy
/// `Upload` lowercases before matching).
pub fn is_allowed_ext(ext: &str) -> bool {
    ALLOWED_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

/// A `uniqid()`-style name: the current microsecond rendered as 13 lowercase
/// hex digits. Parameterised on the clock so tests stay deterministic.
pub fn uniqid(now_micros: i64) -> String {
    // 13 hex digits hold 52 bits (up to ~year 2113 of microsecond epochs);
    // mask so a far-future clock cannot widen the field past uniqid's shape.
    format!("{:013x}", now_micros & 0xF_FFFF_FFFF_FFFF)
}

/// The LocalFs store for认证 evidence. Cheap to clone (a `PathBuf` root).
#[derive(Debug, Clone)]
pub struct FileStorage {
    root: PathBuf,
}

impl FileStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Writes `bytes` to `<root>/verifyinfo/<name>.<ext>`, creating the
    /// directory as needed, and returns the generated file name
    /// (`<name>.<ext>`) plus the site-relative record path
    /// (`Uploads/verifyinfo/<name>.<ext>`). Same-name writes overwrite
    /// (matching the legacy behaviour where a `uniqid` collision is tolerated).
    pub async fn store(
        &self,
        name: &str,
        ext: &str,
        bytes: &[u8],
    ) -> GatewayResult<(String, String)> {
        let dir = self.root.join(SUBDIR);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| GatewayError::Internal(format!("uploads mkdir: {e}")))?;
        let fname = format!("{name}.{ext}");
        let abs = dir.join(&fname);
        tokio::fs::write(&abs, bytes)
            .await
            .map_err(|e| GatewayError::Internal(format!("uploads write: {e}")))?;
        let record = format!("{RECORD_PREFIX}/{SUBDIR}/{fname}");
        Ok((fname, record))
    }
}

/// The member's current KYC `authorized` state (0/1/2); 0 when the member row
/// is missing (defensive — the panel session already guarantees existence).
pub async fn get_authorized(db: &DatabaseConnection, uid: i64) -> GatewayResult<i64> {
    let model = members::Entity::find_by_id(uid)
        .one(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(model.map(|m| m.authorized as i64).unwrap_or(0))
}

/// Lists a merchant's own evidence rows, oldest id first (the page's plain
/// ordering). Filters `userid = uid`, so one merchant never sees another's.
pub async fn list_for_user(
    db: &DatabaseConnection,
    uid: i64,
) -> GatewayResult<Vec<attachments::Model>> {
    attachments::Entity::find()
        .filter(attachments::Column::Userid.eq(uid))
        .order_by_asc(attachments::Column::Id)
        .all(db)
        .await
        .map_err(|e| GatewayError::Internal(format!("db: {e}")))
}

/// Files one attachment row (`upload`'s `M("Attachment")->add`): the
/// uploader's original `filename` plus the generated `path`. Returns the new
/// row id (the legacy `ajaxReturn($res)` value).
pub async fn add(
    db: &DatabaseConnection,
    uid: i64,
    filename: &str,
    path: &str,
) -> GatewayResult<i64> {
    let model = attachments::ActiveModel {
        userid: Set(uid),
        filename: Set(filename.to_string()),
        path: Set(path.to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(|e| GatewayError::Internal(format!("db: {e}")))?;
    Ok(model.id)
}

/// Sets the member's `authorized = 2` (待审核) — the `certification()` write.
/// Unconditional (no current-state gate), faithfully ported; returns the
/// affected-row count (Postgres counts a same-value update as 1).
pub async fn submit_certification(db: &DatabaseConnection, uid: i64) -> GatewayResult<u64> {
    let res = members::Entity::update_many()
        .col_expr(members::Column::Authorized, Expr::value(AUTHORIZED_PENDING))
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
    fn uniqid_is_thirteen_lowercase_hex() {
        let a = uniqid(1_700_000_000_000_000);
        assert_eq!(a.len(), 13);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // strictly increasing over distinct microseconds
        assert!(uniqid(2_000_000_000_000_000) > uniqid(1_000_000_000_000_000));
    }

    #[test]
    fn allowed_ext_is_case_insensitive_whitelist() {
        assert!(is_allowed_ext("jpg"));
        assert!(is_allowed_ext("JPG"));
        assert!(is_allowed_ext("Png"));
        assert!(is_allowed_ext("gif"));
        assert!(!is_allowed_ext("exe"));
        assert!(!is_allowed_ext("php"));
        assert!(!is_allowed_ext(""));
    }

    #[test]
    fn max_bytes_is_two_mebibytes() {
        assert_eq!(MAX_BYTES, 2 * 1024 * 1024);
    }
}
