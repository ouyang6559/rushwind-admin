//! FileService and FileTransferService — //! module / module. File metadata rides
//! `files`; the physical object store is MinIO (S3) when `oss.yaml` is
//! configured, else the object bytes land on the local data directory
//! (`./data/files`) with identical metadata semantics. Upload enforces
//! rules: ≤50 MiB, content-sniffed MIME whitelist,
//! `bucket = images|videos|audios|docs|files` by type, object name
//! `dir/uuid.ext`, sha256 content hash, guid v7.

use std::sync::Arc;

use sea_orm::sea_query::Condition;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::state::{
    db_err, internal_error, not_found, operator_of, status_error, AppState, StatusError,
};
use pbjson_types::Empty;
use proto::proto::pagination::PagingRequest;
use proto::proto::storage::service::v1::{
    CreateFileRequest, DeleteFileRequest, DownloadFileRequest, DownloadFileResponse, File,
    GetFileRequest, ListFileResponse, UpdateFileRequest, UploadFileRequest, UploadFileResponse,
};

/// oss.MaxUploadSize (pkg/oss/module).
const MAX_UPLOAD_SIZE: usize = 50 * 1024 * 1024;

fn provider_to_proto(s: &str) -> i32 {
    match s {
        "MINIO" => 1,
        _ => 0,
    }
}

fn file_proto(r: crate::data::files::Model) -> File {
    File {
        id: Some(r.id),
        provider: r.provider.as_deref().map(provider_to_proto),
        bucket_name: r.bucket_name,
        file_directory: r.file_directory,
        file_guid: r.file_guid,
        save_file_name: r.save_file_name,
        file_name: r.file_name,
        extension: r.extension,
        size: r.size.map(|v| v as u64),
        size_format: r.size_format,
        link_url: r.link_url,
        content_hash: r.content_hash,
        tenant_id: r.tenant_id,
        tenant_name: None,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

fn human_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2}{}", units[unit])
}

/// The MIME whitelist (pkg/oss/module:19-44): prefixes plus exact
/// doc types.
fn mime_allowed(mime: &str) -> bool {
    for prefix in ["image/", "video/", "audio/"] {
        if mime.starts_with(prefix) {
            return true;
        }
    }
    for exact in [
        "application/pdf",
        "application/msword",
        "application/zip",
        "application/x-zip-compressed",
        "application/vnd.ms-powerpoint",
        "application/vnd.ms-excel",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "text/plain",
    ] {
        if mime == exact {
            return true;
        }
    }
    false
}

fn bucket_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "images"
    } else if mime.starts_with("video/") {
        "videos"
    } else if mime.starts_with("audio/") {
        "audios"
    } else if mime.starts_with("text/")
        || mime.contains("pdf")
        || mime.contains("word")
        || mime.contains("officedocument")
        || mime.contains("powerpoint")
        || mime.contains("excel")
        || mime.contains("msword")
    {
        "docs"
    } else {
        "files"
    }
}

/// Content sniffing over the leading magic bytes — the upload path
/// trusts the bytes, not the client-declared mime, so the bucket route
/// cannot be bypassed by a relabelled payload. `None` defers to the
/// client declaration.
fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF") {
        return Some("application/pdf");
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return Some("application/zip");
    }
    if bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    if bytes.starts_with(b"ID3") || (bytes.len() > 1 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0)
    {
        return Some("audio/mpeg");
    }
    None
}

/// The signed public media URL (`/admin/v1/file/image`): HMAC-SHA256
/// over `path|expires` under the crypto key; unset key yields an empty
/// URL (the reference behavior — rich-text embedding degrades, the
/// authorized download API still works).
fn signed_media_url(crypto_key: Option<[u8; 32]>, storage_path: &str) -> String {
    let Some(key) = crypto_key else {
        return String::new();
    };
    let expires = chrono::Utc::now().timestamp() + 365 * 24 * 3600;
    let data = format!("{storage_path}|{expires}");
    use hmac::Mac as _;
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).expect("hmac accepts any key length");
    mac.update(data.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("/admin/v1/file/image?path={storage_path}&expires={expires}&sig={sig}")
}

/// The signature image proxy — a public route whose credential is the
/// HMAC: verify expiry then signature, stream the object from the
/// bucket the path names.
pub async fn image_proxy(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    params: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let bad = |code: u16, text: &'static str| async move {
        (
            axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
            text,
        )
            .into_response()
    };
    let path = params.get("path").cloned().unwrap_or_default();
    let expires = params.get("expires").cloned().unwrap_or_default();
    let sig = params.get("sig").cloned().unwrap_or_default();
    if path.is_empty() || expires.is_empty() || sig.is_empty() {
        return bad(400, "missing parameters").await;
    }
    let Ok(expires_at) = expires.parse::<i64>() else {
        return bad(403, "url expired").await;
    };
    if chrono::Utc::now().timestamp() > expires_at {
        return bad(403, "url expired").await;
    }
    let Some(key) = crate::crypto::crypto_key() else {
        return bad(403, "invalid signature").await;
    };
    use hmac::Mac as _;
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).expect("hmac accepts any key length");
    mac.update(format!("{path}|{expires}").as_bytes());
    if hex::encode(mac.finalize().into_bytes()) != sig {
        return bad(403, "invalid signature").await;
    }

    // "/bucket/object" — the bucket routes to its engine.
    let media = path.trim_start_matches('/');
    let Some((bucket, object)) = media.split_once('/') else {
        return bad(400, "invalid path").await;
    };
    let Some(oss) = state.oss.as_ref().and_then(|o| o.storage(bucket)) else {
        return bad(404, "not found").await;
    };
    match oss.get(object).await {
        Ok(bytes) => {
            let ext = object.rsplit('.').next().unwrap_or("");
            let mime = match ext {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "application/octet-stream",
            };
            ([(axum::http::header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(_) => bad(404, "not found").await,
    }
}

pub struct FileService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl proto::gen::services::FileServiceHandlers for FileService {
    async fn list(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListFileResponse, StatusError> {
        let repo =
            crate::data::repos::FileRepo::new(&self.state.db, crate::data::Viewer::from_ctx(&ctx));
        let (rows, total) = repo.paged_list(&req).await?;
        Ok(ListFileResponse {
            items: rows.into_iter().map(file_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetFileRequest,
    ) -> Result<File, StatusError> {
        let id = crate::query_by_id!(
            req.query_by,
            proto::proto::storage::service::v1::get_file_request::QueryBy
        );
        let row = crate::data::files::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("file"))?;
        Ok(file_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: CreateFileRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = crate::state::require_data(req.data)?;
        crate::data::files::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            provider: Set(Some("MINIO".into())),
            bucket_name: Set(data.bucket_name),
            file_directory: Set(data.file_directory),
            file_guid: Set(data.file_guid),
            save_file_name: Set(data.save_file_name),
            file_name: Set(data.file_name),
            extension: Set(data.extension),
            size: Set(data.size.map(|v| v as i64)),
            size_format: Set(data.size_format),
            link_url: Set(data.link_url),
            content_hash: Set(data.content_hash),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn update(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UpdateFileRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::files::Entity::find_by_id(req.id)
            .filter(crate::data::files::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("file"))?;
        let mut a: crate::data::files::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.file_name {
                a.file_name = Set(Some(v.clone()));
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn delete(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteFileRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let id = crate::query_by_id!(
            req.query_by,
            proto::proto::storage::service::v1::delete_file_request::QueryBy
        );
        let row = crate::data::files::Entity::find_by_id(id)
            .filter(crate::data::files::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("file"))?;
        // Physical object removal (best-effort on either store).
        let bucket = row.bucket_name.clone().unwrap_or_else(|| "files".into());
        let mut object_key = row.file_directory.clone().unwrap_or_default();
        if !object_key.is_empty() {
            object_key.push('/');
        }
        object_key.push_str(row.save_file_name.as_deref().unwrap_or_default());
        if let Some(oss) = self.state.oss.as_ref().and_then(|o| o.storage(&bucket)) {
            let _ = oss.delete(&object_key).await;
        } else {
            let path = format!("./data/files/{bucket}/{object_key}");
            let _ = std::fs::remove_file(path);
        }
        crate::data::files::Entity::delete_by_id(row.id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}

pub struct FileTransferService {
    pub state: Arc<AppState>,
}

impl FileTransferService {
    /// directUploadFile: validate → sniff → store → record.
    async fn upload(
        &self,
        ctx: &rushwind_http_binding::ctx::RequestContext,
        req: &UploadFileRequest,
    ) -> Result<UploadFileResponse, StatusError> {
        let payload = operator_of(ctx)?;
        let storage_object = req
            .storage_object
            .as_ref()
            .ok_or_else(|| status_error("BAD_REQUEST", "storage object required"))?;
        let bytes = match &req.source {
            Some(proto::proto::storage::service::v1::upload_file_request::Source::File(bytes)) => {
                bytes.clone()
            }
            _ => {
                return Err(status_error(
                    "BAD_REQUEST",
                    "file content required (presigned flow disabled)",
                ))
            }
        };
        if bytes.len() > MAX_UPLOAD_SIZE {
            return Err(status_error(
                "BAD_REQUEST",
                "file exceeds the 50MiB upload limit",
            ));
        }
        let mut mime = req.mime.clone().unwrap_or_default();
        if mime.is_empty() {
            return Err(status_error("BAD_REQUEST", "unknown mime type"));
        }
        if req
            .source_file_name
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true)
        {
            return Err(status_error("BAD_REQUEST", "unknown source file name"));
        }
        if !mime_allowed(&mime) {
            return Err(status_error(
                "BAD_REQUEST",
                format!("mime type [{mime}] is not allowed"),
            ));
        }
        // The bytes decide the type, not the client declaration — the
        // sniffed type overrides so the bucket route cannot be bypassed
        // by a relabelled payload.
        if let Some(sniffed) = sniff_mime(&bytes) {
            mime = sniffed.to_string();
        }
        let dir = storage_object
            .file_directory
            .clone()
            .filter(|d| !d.is_empty() && !d.starts_with('/') && !d.contains(".."))
            .unwrap_or_else(|| "uploads".into());
        // Content hash (sha256) — the dedup/fingerprint.
        let hash = crate::crypto::sha256_hex(&bytes);
        let ext = req
            .source_file_name
            .as_deref()
            .and_then(|n| n.rsplit('.').next())
            .map(|e| e.to_string())
            .unwrap_or_else(|| "bin".into());
        let guid = uuid::Uuid::now_v7().simple().to_string();
        let save_name = format!("{guid}.{ext}");
        let bucket = bucket_for_mime(&mime);
        let object_name = format!("{dir}/{save_name}");

        // The object store: MinIO when configured (the URL shapes the
        // reference pins — link carries the download host), else the
        // local mirror with identical metadata semantics.
        let (link_url, public_url) = match self.state.oss.as_ref() {
            Some(oss) => {
                let storage = oss
                    .storage(bucket)
                    .ok_or_else(|| internal_error("storage engine missing"))?;
                storage
                    .put(&object_name, &bytes, Some(mime.as_str()))
                    .await
                    .map_err(|e| internal_error(format!("storage put: {e}")))?;
                let download_url = format!(
                    "{}/{bucket}/{object_name}",
                    oss.download_host.trim_end_matches('/')
                );
                let storage_path = format!("/{bucket}/{object_name}");
                let public_url = signed_media_url(crate::crypto::crypto_key(), &storage_path);
                (download_url, public_url)
            }
            None => {
                let object_dir = format!("./data/files/{bucket}/{dir}");
                std::fs::create_dir_all(&object_dir)
                    .map_err(|e| internal_error(format!("storage mkdir: {e}")))?;
                let object_path = format!("{object_dir}/{save_name}");
                std::fs::write(&object_path, &bytes)
                    .map_err(|e| internal_error(format!("storage write: {e}")))?;
                (object_name.clone(), String::new())
            }
        };

        let row = crate::data::files::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            provider: Set(Some("MINIO".into())),
            bucket_name: Set(Some(bucket.into())),
            file_directory: Set(Some(dir)),
            file_guid: Set(Some(guid)),
            save_file_name: Set(Some(save_name)),
            file_name: Set(req.source_file_name.clone()),
            extension: Set(Some(ext)),
            size: Set(Some(bytes.len() as i64)),
            size_format: Set(Some(human_size(bytes.len() as u64))),
            link_url: Set(Some(link_url)),
            content_hash: Set(Some(hash)),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .map_err(db_err)?;

        Ok(UploadFileResponse {
            object_name: Some(row.link_url.clone().unwrap_or_default()),
            presigned_url: None,
            public_url: Some(public_url),
        })
    }
}

#[async_trait::async_trait]
impl proto::gen::services::FileTransferServiceHandlers for FileTransferService {
    async fn download_file(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: DownloadFileRequest,
    ) -> Result<DownloadFileResponse, StatusError> {
        // Selector: file_id (direct record), storage_object (bucket+key
        // download_url: local mirror resolution only;
        // only same-origin local mirrors resolve here).
        let row = match &req.selector {
            Some(proto::proto::storage::service::v1::download_file_request::Selector::FileId(
                id,
            )) => crate::data::files::Entity::find_by_id(*id)
                .one(&self.state.db)
                .await
                .map_err(db_err)?,
            Some(
                proto::proto::storage::service::v1::download_file_request::Selector::StorageObject(
                    obj,
                ),
            ) => crate::data::files::Entity::find()
                .filter(
                    Condition::all()
                        .add(
                            crate::data::files::Column::BucketName
                                .eq(obj.bucket_name.clone().unwrap_or_default()),
                        )
                        .add(
                            crate::data::files::Column::LinkUrl
                                .eq(obj.object_name.clone().unwrap_or_default()),
                        ),
                )
                .one(&self.state.db)
                .await
                .map_err(db_err)?,
            Some(
                proto::proto::storage::service::v1::download_file_request::Selector::DownloadUrl(_),
            ) => {
                return Err(status_error(
                    "UNIMPLEMENTED",
                    "remote url download not wired",
                ));
            }
            None => None,
        }
        .ok_or_else(|| not_found("file"))?;
        let bucket = row.bucket_name.clone().unwrap_or_else(|| "files".into());
        let mut object_key = row.file_directory.clone().unwrap_or_default();
        if !object_key.is_empty() {
            object_key.push('/');
        }
        object_key.push_str(row.save_file_name.as_deref().unwrap_or_default());
        let (bytes, storage_path) = match self.state.oss.as_ref() {
            Some(oss) => {
                let storage = oss
                    .storage(&bucket)
                    .ok_or_else(|| internal_error("storage engine missing"))?;
                let bytes = storage
                    .get(&object_key)
                    .await
                    .map_err(|e| internal_error(format!("storage get: {e}")))?;
                (bytes, format!("/{bucket}/{object_key}"))
            }
            None => {
                let path = format!("./data/files/{bucket}/{object_key}");
                let bytes = std::fs::read(&path)
                    .map_err(|e| internal_error(format!("storage read: {e}")))?;
                (bytes, path)
            }
        };
        let mime = match row.extension.as_deref() {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("pdf") => "application/pdf",
            Some("txt") => "text/plain",
            _ => "application/octet-stream",
        };
        Ok(DownloadFileResponse {
            source_file_name: row.file_name.clone().unwrap_or_default(),
            mime: mime.into(),
            size: bytes.len() as i64,
            checksum: row.content_hash.clone().unwrap_or_default(),
            storage_path,
            updated_at: row.updated_at.and_then(crate::state::naive_to_ts),
            content: Some(
                proto::proto::storage::service::v1::download_file_response::Content::File(bytes),
            ),
        })
    }

    async fn put_upload_file(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UploadFileRequest,
    ) -> Result<UploadFileResponse, StatusError> {
        self.upload(&ctx, &req).await
    }

    async fn post_upload_file(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UploadFileRequest,
    ) -> Result<UploadFileResponse, StatusError> {
        self.upload(&ctx, &req).await
    }
}
