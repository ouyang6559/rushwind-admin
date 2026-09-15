//! MenuRepo — the port of `internal/data/menu_repo.go`: platform-global
//! rows (menus carry no tenant column), all predicates owned here.

use sea_orm::sea_query::Condition;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::scope::Viewer;
use crate::data::sys_menus as entity;
use crate::state::{db_err, StatusError};

pub struct MenuRepo<'a> {
    pub db: &'a DatabaseConnection,
    pub viewer: Viewer,
}

impl<'a> MenuRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    // Platform-global table: no tenant predicate applies.
    fn condition(&self) -> Condition {
        Condition::all()
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
        req: &admin_api::proto::pagination::PagingRequest,
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
        entity::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| StatusError::new(404, "NOT_FOUND", "menu not found"))
    }

    pub async fn delete_by_id(&self, id: u32) -> Result<(), StatusError> {
        entity::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
