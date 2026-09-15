//! TenantRepo — the port of `internal/data/tenant_repo.go`: tenants are
//! platform-global (no tenant column); the exists gate is code OR name
//! (tenant_repo.go:325-350); usage counts ride the tenant-scoped tables.

use sea_orm::sea_query::Condition;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter};

use crate::data::scope::Viewer;
use crate::data::sys_tenants as tenants;
use crate::data::sys_users as users;
use crate::state::{db_err, StatusError};

pub struct TenantRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> TenantRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    pub async fn get_by_id(&self, id: u32) -> Result<Option<tenants::Model>, StatusError> {
        tenants::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn get_by_code(&self, code: &str) -> Result<Option<tenants::Model>, StatusError> {
        tenants::Entity::find()
            .filter(tenants::Column::Code.eq(code))
            .one(self.db)
            .await
            .map_err(db_err)
    }

    /// The OR-semantics exists probe (code OR name).
    pub async fn exists_by_code_or_name(
        &self,
        code: &str,
        name: &str,
    ) -> Result<bool, StatusError> {
        let mut query = tenants::Entity::find();
        if !code.is_empty() && !name.is_empty() {
            query = query.filter(
                Condition::any()
                    .add(tenants::Column::Code.eq(code))
                    .add(tenants::Column::Name.eq(name)),
            );
        } else if !code.is_empty() {
            query = query.filter(tenants::Column::Code.eq(code));
        } else if !name.is_empty() {
            query = query.filter(tenants::Column::Name.eq(name));
        } else {
            return Ok(false);
        }
        Ok(query.one(self.db).await.map_err(db_err)?.is_some())
    }

    pub async fn count(&self) -> u64 {
        tenants::Entity::find().count(self.db).await.unwrap_or(0)
    }

    pub async fn user_count(&self, tenant_id: u32) -> u64 {
        users::Entity::find()
            .filter(users::Column::TenantId.eq(tenant_id))
            .count(self.db)
            .await
            .unwrap_or(0)
    }
}
