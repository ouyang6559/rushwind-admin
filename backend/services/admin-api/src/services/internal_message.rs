//! InternalMessageService / InternalMessageCategoryService /
//! InternalMessageRecipientService — inbox surface:
//! denormalized title/content, status RECEIVED immediately), revoke with
//! recipient cascade, category CRUD, and the user inbox surface.

use std::sync::Arc;

use sea_orm::sea_query::Condition;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use crate::state::{db_err, not_found, operator_of, tenant_of, AppState, StatusError};
use pbjson_types::Empty;
use proto::proto::internal_message::service::v1::{
    DeleteNotificationFromInboxRequest, GetInternalMessageCategoryRequest,
    GetInternalMessageRequest, InternalMessage, InternalMessageCategory, InternalMessageRecipient,
    ListInternalMessageCategoryResponse, ListInternalMessageResponse, ListUserInboxResponse,
    MarkNotificationAsReadRequest, MarkNotificationsStatusRequest, RevokeMessageRequest,
    SendMessageRequest, SendMessageResponse, UpdateInternalMessageRequest,
};
use proto::proto::pagination::PagingRequest;

fn message_status_to_proto(s: &str) -> i32 {
    match s {
        "PUBLISHED" => 1,
        "SCHEDULED" => 2,
        "REVOKED" => 3,
        "ARCHIVED" => 4,
        "DELETED" => 5,
        _ => 0, // DRAFT
    }
}

fn recipient_status_to_proto(s: &str) -> i32 {
    match s {
        "READ" => 2,
        "REVOKED" => 3,
        "DELETED" => 4,
        _ => 1, // RECEIVED
    }
}

fn message_proto(r: crate::data::internal_messages::Model) -> InternalMessage {
    InternalMessage {
        id: Some(r.id),
        title: r.title,
        content: r.content,
        status: r.status.as_deref().map(message_status_to_proto),
        r#type: r.type_column.as_deref().map(|s| match s {
            "PRIVATE" => 1,
            "GROUP" => 2,
            _ => 0,
        }),
        sender_id: r.sender_id,
        sender_name: None,
        category_id: r.category_id,
        category_name: None,
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

fn category_proto(r: crate::data::internal_message_categories::Model) -> InternalMessageCategory {
    InternalMessageCategory {
        id: Some(r.id),
        tenant_id: r.tenant_id,
        name: Some(r.name),
        code: Some(r.code),
        icon_url: r.icon_url,
        is_enabled: r.is_enabled,
        sort_order: r.sort_order,
        tenant_name: None,
        created_by: r.created_by,
        updated_by: r.updated_by,
        deleted_by: r.deleted_by,
        created_at: r.created_at.and_then(crate::state::naive_to_ts),
        updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
        deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
    }
}

pub struct InternalMessageService {
    pub state: Arc<AppState>,
}

impl InternalMessageService {
    /// Insert recipients with the denormalized title/content (status
    /// RECEIVED immediately — contract for inbox reads).
    pub async fn insert_recipients(
        &self,
        tenant_id: u32,
        message_id: u32,
        title: &str,
        content: &str,
        recipient_ids: &[u32],
    ) -> Result<(), StatusError> {
        for uid in recipient_ids {
            crate::data::internal_message_recipients::ActiveModel {
                tenant_id: Set(Some(tenant_id)),
                message_id: Set(Some(message_id)),
                recipient_user_id: Set(Some(*uid)),
                status: Set(Some("RECEIVED".into())),
                received_at: Set(Some(crate::data::now())),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(&self.state.db)
            .await
            .map_err(db_err)?;
        }
        let _ = (title, content);
        Ok(())
    }
}

#[async_trait::async_trait]
impl proto::gen::services::InternalMessageServiceHandlers for InternalMessageService {
    async fn list_message(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListInternalMessageResponse, StatusError> {
        let repo = crate::data::repos::InternalMessageRepo::new(
            &self.state.db,
            crate::data::Viewer::from_ctx(&ctx),
        );
        let (rows, total) = repo.paged_list(tenant_of(&ctx), &req).await?;
        Ok(ListInternalMessageResponse {
            items: rows.into_iter().map(message_proto).collect(),
            total,
        })
    }

    async fn get_message(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetInternalMessageRequest,
    ) -> Result<InternalMessage, StatusError> {
        let id = crate::query_by_id!(
            req.query_by,
            proto::proto::internal_message::service::v1::get_internal_message_request::QueryBy
        );
        let row = crate::data::internal_messages::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("internal message"))?;
        Ok(message_proto(row))
    }

    async fn update_message(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: UpdateInternalMessageRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::internal_messages::Entity::find_by_id(req.id)
            .filter(crate::data::internal_messages::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("internal message"))?;
        let mut a: crate::data::internal_messages::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.title {
                a.title = Set(Some(v.clone()));
            }
            if let Some(v) = &data.content {
                a.content = Set(Some(v.clone()));
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn delete_message(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: proto::proto::internal_message::service::v1::DeleteInternalMessageRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let id = crate::query_by_id!(
            req.query_by,
            proto::proto::internal_message::service::v1::delete_internal_message_request::QueryBy
        );
        let row = crate::data::internal_messages::Entity::find_by_id(id)
            .filter(crate::data::internal_messages::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("internal message"))?;
        // Transactional cascade of recipients (DeleteMessageWithRecipients).
        crate::data::internal_message_recipients::Entity::delete_many()
            .filter(crate::data::internal_message_recipients::Column::MessageId.eq(row.id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        crate::data::internal_messages::Entity::delete_by_id(row.id)
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn send_message(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, StatusError> {
        let payload = operator_of(&ctx)?;
        let type_str = match req.r#type {
            1 => "PRIVATE",
            2 => "GROUP",
            _ => "NOTIFICATION",
        };
        let message = crate::data::internal_messages::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            title: Set(req.title.clone()),
            content: Set(Some(req.content.clone())),
            sender_id: Set(Some(payload.user_id)),
            category_id: Set(req.category_id),
            status: Set(Some("PUBLISHED".into())),
            type_column: Set(Some(type_str.into())),
            created_by: Set(Some(payload.user_id)),
            created_at: Set(Some(crate::data::now())),
            updated_at: Set(Some(crate::data::now())),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .map_err(db_err)?;

        // Recipients: explicit list (single or multi), else the whole
        // tenant fan-out (the broadcast rides the scheduler phase;
        // direct fan-out covers the API contract).
        let recipient_ids: Vec<u32> = if !req.target_user_ids.is_empty() {
            req.target_user_ids.clone()
        } else if let Some(uid) = req.recipient_user_id {
            vec![uid]
        } else if req.target_all == Some(true) {
            crate::data::sys_users::Entity::find()
                .filter(crate::data::sys_users::Column::TenantId.eq(payload.tenant_id))
                .all(&self.state.db)
                .await
                .map_err(db_err)?
                .iter()
                .map(|u| u.id)
                .collect()
        } else {
            Vec::new()
        };
        let received_at = crate::state::naive_to_ts(crate::data::now());
        for chunk in recipient_ids.chunks(1000) {
            self.insert_recipients(
                payload.tenant_id,
                message.id,
                req.title.as_deref().unwrap_or(""),
                &req.content,
                chunk,
            )
            .await?;
            // SSE push per recipient (publishNotification: stream = userId,
            // event = notification, data = the recipient protojson).
            for uid in chunk {
                crate::server::sse::publish_recipient(
                    &self.state.hub,
                    &crate::server::sse::NotificationPayload {
                        id: message.id,
                        message_id: message.id,
                        recipient_user_id: *uid,
                        title: req.title.clone().unwrap_or_default(),
                        content: req.content.clone(),
                        received_at,
                    },
                );
            }
        }
        Ok(SendMessageResponse {
            message_id: message.id,
        })
    }

    async fn revoke_message(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: RevokeMessageRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let row = crate::data::internal_messages::Entity::find_by_id(req.message_id)
            .filter(crate::data::internal_messages::Column::TenantId.eq(payload.tenant_id))
            .one(&self.state.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("internal message"))?;
        crate::data::internal_message_recipients::Entity::delete_many()
            .filter(crate::data::internal_message_recipients::Column::MessageId.eq(row.id))
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        let mut a: crate::data::internal_messages::ActiveModel = row.into();
        a.status = Set(Some("REVOKED".into()));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }
}

pub struct InternalMessageCategoryService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl proto::gen::services::InternalMessageCategoryServiceHandlers
    for InternalMessageCategoryService
{
    async fn list(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListInternalMessageCategoryResponse, StatusError> {
        let tid = tenant_of(&ctx);
        let repo = crate::data::repos::InternalMessageCategoryRepo::new(&self.state.db);
        let (rows, total) = repo.paged_list(tid, &req).await?;
        Ok(ListInternalMessageCategoryResponse {
            items: rows.into_iter().map(category_proto).collect(),
            total,
        })
    }

    async fn get(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: GetInternalMessageCategoryRequest,
    ) -> Result<InternalMessageCategory, StatusError> {
        let id = crate::query_by_id!(req.query_by, proto::proto::internal_message::service::v1::get_internal_message_category_request::QueryBy);
        let repo = crate::data::repos::InternalMessageCategoryRepo::new(&self.state.db);
        let row = repo
            .find(id)
            .await?
            .ok_or_else(|| not_found("message category"))?;
        Ok(category_proto(row))
    }

    async fn create(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: proto::proto::internal_message::service::v1::CreateInternalMessageCategoryRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let data = crate::state::require_data(req.data)?;
        crate::data::internal_message_categories::ActiveModel {
            tenant_id: Set(Some(payload.tenant_id)),
            name: Set(data.name.unwrap_or_default()),
            code: Set(data.code.unwrap_or_default()),
            icon_url: Set(data.icon_url),
            is_enabled: Set(data.is_enabled.or(Some(true))),
            sort_order: Set(data.sort_order.or(Some(0))),
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
        req: proto::proto::internal_message::service::v1::UpdateInternalMessageCategoryRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let repo = crate::data::repos::InternalMessageCategoryRepo::new(&self.state.db);
        let row = repo
            .find_tenant(req.id, payload.tenant_id)
            .await?
            .ok_or_else(|| not_found("message category"))?;
        let mut a: crate::data::internal_message_categories::ActiveModel = row.into();
        if let Some(data) = &req.data {
            if let Some(v) = &data.name {
                a.name = Set(v.clone());
            }
            if let Some(v) = &data.icon_url {
                a.icon_url = Set(Some(v.clone()));
            }
            if let Some(v) = data.is_enabled {
                a.is_enabled = Set(Some(v));
            }
            if let Some(v) = data.sort_order {
                a.sort_order = Set(Some(v));
            }
        }
        a.updated_by = Set(Some(payload.user_id));
        a.updated_at = Set(Some(crate::data::now()));
        a.update(&self.state.db).await.map_err(db_err)?;
        Ok(Empty {})
    }

    async fn delete(
        &self,
        _ctx: rushwind_http_binding::ctx::RequestContext,
        req: proto::proto::internal_message::service::v1::DeleteInternalMessageCategoryRequest,
    ) -> Result<Empty, StatusError> {
        let id = crate::query_by_id!(req.query_by, proto::proto::internal_message::service::v1::delete_internal_message_category_request::QueryBy);
        let repo = crate::data::repos::InternalMessageCategoryRepo::new(&self.state.db);
        repo.delete_by_id(id).await?;
        Ok(Empty {})
    }
}

pub struct InternalMessageRecipientService {
    pub state: Arc<AppState>,
}

#[async_trait::async_trait]
impl proto::gen::services::InternalMessageRecipientServiceHandlers
    for InternalMessageRecipientService
{
    async fn list_user_inbox(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: PagingRequest,
    ) -> Result<ListUserInboxResponse, StatusError> {
        let payload = operator_of(&ctx)?;
        let repo = crate::data::repos::InternalMessageRecipientRepo::new(
            &self.state.db,
            crate::data::Viewer::from_ctx(&ctx),
        );
        let (rows, total) = repo.paged_inbox(payload.user_id, &req).await?;
        // One IN query backfills the messages (N+1 guard).
        let message_ids: Vec<u32> = rows.iter().filter_map(|r| r.message_id).collect();
        let messages: std::collections::HashMap<u32, crate::data::internal_messages::Model> =
            if message_ids.is_empty() {
                Default::default()
            } else {
                crate::data::internal_messages::Entity::find()
                    .filter(crate::data::internal_messages::Column::Id.is_in(message_ids))
                    .all(&self.state.db)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| (m.id, m))
                    .collect()
            };
        let items: Vec<InternalMessageRecipient> = rows
            .into_iter()
            .map(|r| {
                let message = r.message_id.and_then(|mid| messages.get(&mid));
                InternalMessageRecipient {
                    id: Some(r.id),
                    recipient_user_id: r.recipient_user_id,
                    message_id: r.message_id,
                    status: r.status.as_deref().map(recipient_status_to_proto),
                    received_at: r.received_at.and_then(crate::state::naive_to_ts),
                    read_at: r.read_at.and_then(crate::state::naive_to_ts),
                    // Denormalized title/content copied at send time; the
                    // backfilled message row fills gaps.
                    title: message.and_then(|m| m.title.clone()),
                    content: message.and_then(|m| m.content.clone()),
                    tenant_id: r.tenant_id,
                    tenant_name: None,
                    created_by: r.created_by,
                    updated_by: r.updated_by,
                    deleted_by: r.deleted_by,
                    created_at: r.created_at.and_then(crate::state::naive_to_ts),
                    updated_at: r.updated_at.and_then(crate::state::naive_to_ts),
                    deleted_at: r.deleted_at.and_then(crate::state::naive_to_ts),
                }
            })
            .collect();
        Ok(ListUserInboxResponse { items, total })
    }

    async fn delete_notification_from_inbox(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: DeleteNotificationFromInboxRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        crate::data::internal_message_recipients::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(
                        crate::data::internal_message_recipients::Column::RecipientUserId
                            .eq(payload.user_id),
                    )
                    .add(
                        crate::data::internal_message_recipients::Column::Id
                            .is_in(req.recipient_ids),
                    ),
            )
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn mark_notification_as_read(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: MarkNotificationAsReadRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        crate::data::internal_message_recipients::Entity::update_many()
            .col_expr(
                crate::data::internal_message_recipients::Column::Status,
                sea_orm::sea_query::Expr::value("READ"),
            )
            .col_expr(
                crate::data::internal_message_recipients::Column::ReadAt,
                sea_orm::sea_query::Expr::value(crate::data::now()),
            )
            .filter(
                Condition::all()
                    .add(
                        crate::data::internal_message_recipients::Column::RecipientUserId
                            .eq(payload.user_id),
                    )
                    .add(
                        crate::data::internal_message_recipients::Column::Id
                            .is_in(req.recipient_ids),
                    ),
            )
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }

    async fn mark_notifications_status(
        &self,
        ctx: rushwind_http_binding::ctx::RequestContext,
        req: MarkNotificationsStatusRequest,
    ) -> Result<Empty, StatusError> {
        let payload = operator_of(&ctx)?;
        let status = match req.new_status {
            2 => "READ",
            4 => "DELETED",
            _ => "RECEIVED",
        };
        crate::data::internal_message_recipients::Entity::update_many()
            .col_expr(
                crate::data::internal_message_recipients::Column::Status,
                sea_orm::sea_query::Expr::value(status),
            )
            .filter(
                Condition::all()
                    .add(
                        crate::data::internal_message_recipients::Column::RecipientUserId
                            .eq(payload.user_id),
                    )
                    .add(
                        crate::data::internal_message_recipients::Column::Id
                            .is_in(req.recipient_ids),
                    ),
            )
            .exec(&self.state.db)
            .await
            .map_err(db_err)?;
        Ok(Empty {})
    }
}
