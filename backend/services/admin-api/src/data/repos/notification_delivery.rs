//! NotificationDeliveryRepo — platform-global delivery ledger (read-only view).

use sea_orm::sea_query::Condition;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::sys_notification_deliveries as entity;
use crate::state::{db_err, not_found, StatusError};

repo_shell!(global NotificationDeliveryRepo, entity, "notification delivery");

impl<'a> NotificationDeliveryRepo<'a> {
    pub async fn insert(&self, row: entity::ActiveModel) -> Result<entity::Model, StatusError> {
        ActiveModelTrait::insert(row, self.db).await.map_err(db_err)
    }
}
