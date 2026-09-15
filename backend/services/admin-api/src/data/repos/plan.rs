//! Plan / PlanModule / PlanQuota repos — //! plan repos: platform-global subscription-plan catalog rows.

use sea_orm::sea_query::Condition;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use crate::data::scope::Viewer;
use crate::data::{sys_plan_modules, sys_plan_quotas, sys_plans};
use crate::state::{db_err, StatusError};

pub struct PlanRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> PlanRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    // Platform-global table: no tenant predicate applies.
    fn condition(&self) -> Condition {
        Condition::all()
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_plans::Model>, u64), StatusError> {
        use sea_orm::PaginatorTrait;
        let base = sys_plans::Entity::find()
            .filter(self.condition())
            .order_by_asc(sys_plans::Column::Id);
        let (paged, paging) = crate::paging::apply(base, req);
        let rows = paged.all(self.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            sys_plans::Entity::find()
                .filter(self.condition())
                .count(self.db)
                .await
                .unwrap_or(0)
        };
        Ok((rows, total))
    }

    pub async fn get_by_id(&self, id: u32) -> Result<sys_plans::Model, StatusError> {
        sys_plans::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| StatusError::new(404, "NOT_FOUND", "plan not found"))
    }

    pub async fn delete_cascade(&self, id: u32) -> Result<(), StatusError> {
        sys_plan_modules::Entity::delete_many()
            .filter(sys_plan_modules::Column::PlanId.eq(id))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        sys_plan_quotas::Entity::delete_many()
            .filter(sys_plan_quotas::Column::PlanId.eq(id))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        sys_plans::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

pub struct PlanModuleRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> PlanModuleRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    // Platform-global table: no tenant predicate applies.
    fn condition(&self) -> Condition {
        Condition::all()
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_plan_modules::Model>, u64), StatusError> {
        use sea_orm::PaginatorTrait;
        let base = sys_plan_modules::Entity::find()
            .filter(self.condition())
            .order_by_asc(sys_plan_modules::Column::Id);
        let (paged, paging) = crate::paging::apply(base, req);
        let rows = paged.all(self.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            sys_plan_modules::Entity::find()
                .filter(self.condition())
                .count(self.db)
                .await
                .unwrap_or(0)
        };
        Ok((rows, total))
    }

    pub async fn get_by_id(&self, id: u32) -> Result<sys_plan_modules::Model, StatusError> {
        sys_plan_modules::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| StatusError::new(404, "NOT_FOUND", "plan module not found"))
    }

    pub async fn delete_by_id(&self, id: u32) -> Result<(), StatusError> {
        sys_plan_modules::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

pub struct PlanQuotaRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> PlanQuotaRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    // Platform-global table: no tenant predicate applies.
    fn condition(&self) -> Condition {
        Condition::all()
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_plan_quotas::Model>, u64), StatusError> {
        use sea_orm::PaginatorTrait;
        let base = sys_plan_quotas::Entity::find()
            .filter(self.condition())
            .order_by_asc(sys_plan_quotas::Column::Id);
        let (paged, paging) = crate::paging::apply(base, req);
        let rows = paged.all(self.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            sys_plan_quotas::Entity::find()
                .filter(self.condition())
                .count(self.db)
                .await
                .unwrap_or(0)
        };
        Ok((rows, total))
    }

    pub async fn get_by_id(&self, id: u32) -> Result<sys_plan_quotas::Model, StatusError> {
        sys_plan_quotas::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)?
            .ok_or_else(|| StatusError::new(404, "NOT_FOUND", "plan quota not found"))
    }

    pub async fn delete_by_id(&self, id: u32) -> Result<(), StatusError> {
        sys_plan_quotas::Entity::delete_by_id(id)
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
