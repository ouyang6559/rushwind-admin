//! InternalMessage repos — message repos:
//! send inserts denormalized recipients (status RECEIVED immediately);
//! delete/revoke cascade recipient rows; the inbox is the user-scoped
//! recipient list with one IN-query backfill.

use sea_orm::sea_query::Condition;
use sea_orm::{ActiveModelTrait, Set};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use crate::data::Viewer;
use crate::data::{internal_message_recipients, internal_messages};
use crate::state::{db_err, StatusError};

pub struct InternalMessageRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> InternalMessageRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    /// SendMessage's recipient fan-out: denormalized title/content rows
    /// with status RECEIVED immediately (never uses SENT).
    pub async fn insert_recipients(
        &self,
        tenant_id: u32,
        message_id: u32,
        recipient_ids: &[u32],
    ) -> Result<(), StatusError> {
        for uid in recipient_ids {
            internal_message_recipients::ActiveModel {
                tenant_id: Set(Some(tenant_id)),
                message_id: Set(Some(message_id)),
                recipient_user_id: Set(Some(*uid)),
                status: Set(Some("RECEIVED".into())),
                received_at: Set(Some(crate::data::now())),
                created_at: Set(Some(crate::data::now())),
                updated_at: Set(Some(crate::data::now())),
                ..Default::default()
            }
            .insert(self.db)
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    /// Paged tenant-scoped listing: returns (rows, total).
    pub async fn paged_list(
        &self,
        tenant_id: u32,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<internal_messages::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            internal_messages::Entity::find()
                .filter(internal_messages::Column::TenantId.eq(tenant_id))
                .order_by_desc(internal_messages::Column::CreatedAt),
            req,
        )
        .await
    }

    /// DeleteMessageWithRecipients / RevokeMessageWithRecipients.
    pub async fn delete_recipients(&self, message_id: u32) -> Result<(), StatusError> {
        internal_message_recipients::Entity::delete_many()
            .filter(internal_message_recipients::Column::MessageId.eq(message_id))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

pub struct InternalMessageRecipientRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> InternalMessageRecipientRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    /// The user inbox: own rows, newest first.
    pub async fn list_inbox(
        &self,
        user_id: u32,
    ) -> Result<Vec<internal_message_recipients::Model>, StatusError> {
        internal_message_recipients::Entity::find()
            .filter(internal_message_recipients::Column::RecipientUserId.eq(user_id))
            .order_by_desc(internal_message_recipients::Column::CreatedAt)
            .all(self.db)
            .await
            .map_err(db_err)
    }

    /// Paged inbox listing: returns (rows, total).
    pub async fn paged_inbox(
        &self,
        user_id: u32,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<internal_message_recipients::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            internal_message_recipients::Entity::find()
                .filter(internal_message_recipients::Column::RecipientUserId.eq(user_id))
                .order_by_desc(internal_message_recipients::Column::CreatedAt),
            req,
        )
        .await
    }

    /// One IN query backfills the parent messages (N+1 guard).
    pub async fn list_messages_by_ids(
        &self,
        ids: &[u32],
    ) -> Result<std::collections::HashMap<u32, internal_messages::Model>, StatusError> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        Ok(internal_messages::Entity::find()
            .filter(internal_messages::Column::Id.is_in(ids.to_vec()))
            .all(self.db)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|m| (m.id, m))
            .collect())
    }

    pub async fn mark_read(&self, user_id: u32, recipient_ids: &[u32]) -> Result<(), StatusError> {
        for id in recipient_ids {
            if let Some(row) = internal_message_recipients::Entity::find_by_id(*id)
                .filter(internal_message_recipients::Column::RecipientUserId.eq(user_id))
                .one(self.db)
                .await
                .map_err(db_err)?
            {
                let mut a: internal_message_recipients::ActiveModel = row.into();
                a.status = Set(Some("READ".into()));
                a.read_at = Set(Some(crate::data::now()));
                a.updated_at = Set(Some(crate::data::now()));
                a.update(self.db).await.map_err(db_err)?;
            }
        }
        Ok(())
    }

    pub async fn mark_status(
        &self,
        user_id: u32,
        recipient_ids: &[u32],
        status: &str,
    ) -> Result<(), StatusError> {
        for id in recipient_ids {
            if let Some(row) = internal_message_recipients::Entity::find_by_id(*id)
                .filter(internal_message_recipients::Column::RecipientUserId.eq(user_id))
                .one(self.db)
                .await
                .map_err(db_err)?
            {
                let mut a: internal_message_recipients::ActiveModel = row.into();
                a.status = Set(Some(status.to_string()));
                a.updated_at = Set(Some(crate::data::now()));
                a.update(self.db).await.map_err(db_err)?;
            }
        }
        Ok(())
    }

    pub async fn delete_from_inbox(
        &self,
        user_id: u32,
        recipient_ids: &[u32],
    ) -> Result<(), StatusError> {
        internal_message_recipients::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(internal_message_recipients::Column::RecipientUserId.eq(user_id))
                    .add(internal_message_recipients::Column::Id.is_in(recipient_ids.to_vec())),
            )
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

/// The category CRUD surface (admin-managed dictionary rows).
pub struct InternalMessageCategoryRepo<'a> {
    pub db: &'a DatabaseConnection,
}

impl<'a> InternalMessageCategoryRepo<'a> {
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }

    /// Paged tenant-scoped listing: returns (rows, total).
    pub async fn paged_list(
        &self,
        tenant_id: u32,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<crate::data::internal_message_categories::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            crate::data::internal_message_categories::Entity::find()
                .filter(crate::data::internal_message_categories::Column::TenantId.eq(tenant_id))
                .order_by_asc(crate::data::internal_message_categories::Column::SortOrder),
            req,
        )
        .await
    }

    pub async fn find(
        &self,
        id: u32,
    ) -> Result<Option<crate::data::internal_message_categories::Model>, StatusError> {
        crate::data::internal_message_categories::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn find_tenant(
        &self,
        id: u32,
        tenant_id: u32,
    ) -> Result<Option<crate::data::internal_message_categories::Model>, StatusError> {
        crate::data::internal_message_categories::Entity::find_by_id(id)
            .filter(crate::data::internal_message_categories::Column::TenantId.eq(tenant_id))
            .one(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn delete_by_id(&self, id: u32) -> Result<(), StatusError> {
        crate::data::internal_message_categories::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
