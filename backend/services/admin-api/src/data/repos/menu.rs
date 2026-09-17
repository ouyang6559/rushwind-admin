//! MenuRepo — platform-global
//! rows (menus carry no tenant column), all predicates owned here.

use sea_orm::sea_query::Condition;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::sys_menus as entity;
use crate::data::Viewer;
use crate::state::{db_err, StatusError};

repo_shell!(global MenuRepo, entity);

impl<'a> MenuRepo<'a> {
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
