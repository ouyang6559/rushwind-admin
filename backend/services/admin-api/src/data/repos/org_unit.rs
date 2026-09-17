//! OrgUnitRepo — org-unit queries: the tenant-scoped listing and row
//! lookups, the subtree collection and position guard the delete path
//! runs, and the children walk the relocation BFS takes (the
//! materialized-path maintenance itself rides these in the service).

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use sea_orm::{ConnectionTrait, DatabaseConnection};

use crate::data::sys_org_units as entity;
use crate::state::{db_err, internal_error, StatusError};

pub struct OrgUnitRepo<'a> {
    pub db: &'a DatabaseConnection,
}

impl<'a> OrgUnitRepo<'a> {
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }

    /// Paged listing within one tenant, sort_order ascending:
    /// returns (rows, total).
    pub async fn paged_list(
        &self,
        tenant_id: u32,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<entity::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            entity::Entity::find()
                .filter(entity::Column::TenantId.eq(tenant_id))
                .order_by_asc(entity::Column::SortOrder),
            req,
        )
        .await
    }

    /// The ungated row lookup (the relocation walk and the parent-path
    /// reads — tenant-blind, mirroring the reference).
    pub async fn find(&self, id: u32) -> Result<Option<entity::Model>, StatusError> {
        entity::Entity::find_by_id(id)
            .one(self.db)
            .await
            .map_err(db_err)
    }

    /// The scoped row lookup — tenant viewers see own-tenant rows only,
    /// platform/system viewers see everything.
    pub async fn find_scoped(
        &self,
        id: u32,
        scope: Option<u32>,
    ) -> Result<Option<entity::Model>, StatusError> {
        let mut query = entity::Entity::find_by_id(id);
        if let Some(tid) = scope {
            query = query.filter(entity::Column::TenantId.eq(tid));
        }
        query.one(self.db).await.map_err(db_err)
    }

    /// The direct children of one node — the relocation BFS step
    /// (tenant-blind).
    pub async fn children_ids(&self, parent_id: u32) -> Result<Vec<u32>, StatusError> {
        entity::Entity::find()
            .filter(entity::Column::ParentId.eq(parent_id))
            .all(self.db)
            .await
            .map_err(|_| internal_error("query org unit children failed"))
            .map(|rows| rows.into_iter().map(|r| r.id).collect())
    }

    /// The subtree id set under one root (root included) — the
    /// reference's recursive parent-chain walk. The scan itself is
    /// tenant-blind; the delete that consumes it carries the scope.
    pub async fn descendant_ids(&self, root: u32) -> Result<Vec<u32>, StatusError> {
        let stmt = sea_orm::Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "WITH RECURSIVE all_descendants AS (SELECT * FROM sys_org_units WHERE parent_id = ? UNION ALL SELECT p.* FROM sys_org_units p INNER JOIN all_descendants ad ON p.parent_id = ad.id) SELECT id FROM all_descendants",
            [root.into()],
        );
        let rows = self
            .db
            .query_all_raw(stmt)
            .await
            .map_err(|_| internal_error("query child orgUnits failed"))?;
        let mut ids = vec![root];
        for row in rows {
            let id: i32 = row
                .try_get("", "id")
                .map_err(|_| internal_error("query child orgUnits failed"))?;
            ids.push(id as u32);
        }
        Ok(ids)
    }

    /// Positions still anchored inside a set of org units — the delete
    /// guard counts them.
    pub async fn count_positions_in(&self, ids: &[u32]) -> Result<u64, StatusError> {
        use crate::data::sys_positions;
        use sea_orm::PaginatorTrait;
        sys_positions::Entity::find()
            .filter(sys_positions::Column::OrgUnitId.is_in(ids.to_vec()))
            .count(self.db)
            .await
            .map_err(|_| internal_error("count positions under org units failed"))
    }

    /// The scoped bulk delete — tenant viewers delete own-tenant rows
    /// only, platform/system viewers everything named. Runs inside the
    /// caller's transaction.
    pub async fn delete_ids_scoped<C>(
        &self,
        db: &C,
        ids: &[u32],
        scope: Option<u32>,
    ) -> Result<(), StatusError>
    where
        C: sea_orm::ConnectionTrait,
    {
        let mut query =
            entity::Entity::delete_many().filter(entity::Column::Id.is_in(ids.to_vec()));
        if let Some(tid) = scope {
            query = query.filter(entity::Column::TenantId.eq(tid));
        }
        query
            .exec(db)
            .await
            .map_err(|_| internal_error("delete orgUnit failed"))?;
        Ok(())
    }
}
