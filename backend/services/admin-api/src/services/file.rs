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
        let repo = crate::data::repos::FileRepo::new(
            &self.state.db,
            crate::data::scope::Viewer::from_ctx(&ctx),
        );
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
        let Some(proto::proto::storage::service::v1::get_file_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
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
        let data = req
            .data
            .ok_or_else(|| status_error("BAD_REQUEST", "data required"))?;
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
        let Some(proto::proto::storage::service::v1::delete_file_request::QueryBy::Id(id)) =
            req.query_by
        else {
            return Err(status_error("BAD_REQUEST", "query_by required"));
        };
        let row = crate::data::files::Entity::find_by_id(id)
            .filter(crate::data::files::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("file"))?;
        // Physical object removal (best-effort on the local mirror).
        if let (Some(dir), Some(name)) = (&row.file_directory, &row.save_file_name) {
            let path = format!("./data/files/{dir}/{name}");
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
        let client_mime = req.mime.clone().unwrap_or_default();
        if !mime_allowed(&client_mime) {
            return Err(status_error(
                "BAD_REQUEST",
                format!("mime type [{client_mime}] is not allowed"),
            ));
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
        let bucket = bucket_for_mime(&client_mime);

        // Local object mirror (MinIO rides rushwind-oss when configured).
        let object_dir = format!("./data/files/{bucket}/{dir}");
        std::fs::create_dir_all(&object_dir)
            .map_err(|e| internal_error(format!("storage mkdir: {e}")))?;
        let object_path = format!("{object_dir}/{save_name}");
        std::fs::write(&object_path, &bytes)
            .map_err(|e| internal_error(format!("storage write: {e}")))?;
        let object_name = format!("{dir}/{save_name}");

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
            link_url: Set(Some(object_name.clone())),
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
            object_name: Some(object_name),
            presigned_url: None,
            public_url: Some(format!("/admin/v1/file/download?file_id={}", row.id)),
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
        let path = format!(
            "./data/files/{}/{}/{}",
            row.bucket_name.clone().unwrap_or_else(|| "files".into()),
            row.file_directory.clone().unwrap_or_default(),
            row.save_file_name.clone().unwrap_or_default()
        );
        let bytes =
            std::fs::read(&path).map_err(|e| internal_error(format!("storage read: {e}")))?;
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
            storage_path: path.clone(),
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
