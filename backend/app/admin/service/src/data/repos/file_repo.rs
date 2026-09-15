//! FileRepo — tenant-scoped queries with
//! all predicates owned here (never ad-hoc in services).

use sea_orm::sea_query::Condition;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::files as entity;
use crate::data::scope::Viewer;
use crate::state::{db_err, StatusError};

pub struct FileRepo<'a> {
    pub db: &'a DatabaseConnection,
    pub viewer: Viewer,
}

impl<'a> FileRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    fn condition(&self) -> Condition {
        match self.viewer.tenant_scope() {
            Some(tid) => Condition::all().add(entity::Column::TenantId.eq(tid)),
            None => Condition::all(),
        }
    }

    pub async fn list(&self) -> Result<Vec<entity::Model>, StatusError> {
        entity::Entity::find()
            .filter(self.condition())
            .all(self.db)
            .await
            .map_err(db_err)
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &gen_rust::proto::pagination::PagingRequest,
    ) -> Result<(Vec<entity::Model>, u64), StatusError> {
        use sea_orm::PaginatorTrait;
        let base = entity::Entity::find().filter(self.condition());
        let (paged, paging) = crate::paging::apply(base, req);
        let rows = paged.all(self.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            entity::Entity::find()
                .filter(self.condition())
                .count(self.db)
                .await
                .unwrap_or(0)
        };
        Ok((rows, total))
    }

    pub async fn get_by_id(&self, id: u32) -> Result<entity::Model, StatusError> {
        let mut query = entity::Entity::find_by_id(id);
        if let Some(tid) = self.viewer.tenant_scope() {
            query = entity::Entity::find_by_id(id).filter(entity::Column::TenantId.eq(tid));
        }
        query
            .one(self.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| StatusError::new(404, "NOT_FOUND", "file not found"))
    }

    pub async fn delete_by_id(&self, id: u32) -> Result<(), StatusError> {
        if self.viewer.tenant_scope().is_some() {
            entity::Entity::delete_many()
                .filter(entity::Column::TenantId.eq(self.viewer.tenant_scope().unwrap_or(0)))
                .filter(entity::Column::Id.eq(id))
                .exec(self.db)
                .await
                .map_err(db_err)?;
            return Ok(());
        }
        entity::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
