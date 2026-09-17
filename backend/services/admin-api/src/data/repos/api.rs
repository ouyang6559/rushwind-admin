//! ApiRepo — platform-global rows with
//! all predicates owned here (never ad-hoc in services).

use sea_orm::sea_query::Condition;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::sys_apis as entity;
use crate::state::{db_err, StatusError};

repo_shell!(global ApiRepo, entity);

impl<'a> ApiRepo<'a> {
    pub async fn get_by_id(&self, id: u32) -> Result<entity::Model, StatusError> {
        let query = entity::Entity::find_by_id(id);
        query
            .one(self.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| StatusError::new(404, "NOT_FOUND", "api not found"))
    }

    pub async fn delete_by_id(&self, id: u32) -> Result<(), StatusError> {
        entity::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
