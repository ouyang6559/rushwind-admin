//! ScriptRepo / ScriptLogRepo — script repos:
//! enabled-script loading and the log purge (before-timestamp or all).

use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
};

use crate::data::Viewer;
use crate::data::{sys_script_logs, sys_scripts};
use crate::state::{db_err, StatusError};

pub struct ScriptRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> ScriptRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    pub async fn list(&self) -> Result<Vec<sys_scripts::Model>, StatusError> {
        sys_scripts::Entity::find()
            .order_by_asc(sys_scripts::Column::Id)
            .all(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn count(&self) -> u64 {
        sys_scripts::Entity::find()
            .count(self.db)
            .await
            .unwrap_or(0)
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_scripts::Model>, u64), StatusError> {
        use sea_orm::PaginatorTrait;
        let base = sys_scripts::Entity::find().order_by_asc(sys_scripts::Column::Id);
        let (paged, paging) = crate::paging::apply(base, req);
        let rows = paged.all(self.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            sys_scripts::Entity::find()
                .count(self.db)
                .await
                .unwrap_or(0)
        };
        Ok((rows, total))
    }
}

pub struct ScriptLogRepo<'a> {
    pub db: &'a DatabaseConnection,
    #[allow(dead_code)]
    pub viewer: Viewer,
}

impl<'a> ScriptLogRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    pub async fn count(&self) -> u64 {
        sys_script_logs::Entity::find()
            .count(self.db)
            .await
            .unwrap_or(0)
    }

    /// Paged listing, newest first: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_script_logs::Model>, u64), StatusError> {
        use sea_orm::PaginatorTrait;
        let base =
            sys_script_logs::Entity::find().order_by_desc(sys_script_logs::Column::CreatedAt);
        let (paged, paging) = crate::paging::apply(base, req);
        let rows = paged.all(self.db).await.map_err(db_err)?;
        let total = if paging.no_paging {
            rows.len() as u64
        } else {
            sys_script_logs::Entity::find()
                .count(self.db)
                .await
                .unwrap_or(0)
        };
        Ok((rows, total))
    }

    /// Purge: before-timestamp or everything.
    pub async fn purge(&self, before: Option<chrono::NaiveDateTime>) -> Result<u64, StatusError> {
        let mut query = sys_script_logs::Entity::delete_many();
        if let Some(cutoff) = before {
            query = query.filter(sys_script_logs::Column::CreatedAt.lt(cutoff));
        }
        let result = query.exec(self.db).await.map_err(db_err)?;
        Ok(result.rows_affected)
    }
}
