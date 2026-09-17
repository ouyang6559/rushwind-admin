//! DictTypeRepo / DictEntryRepo — tenant-scoped dictionary trees with cascade
//! deletes and the ListByTypeCode walk (enabled entries by sort_order).

use sea_orm::sea_query::Condition;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use crate::data::Viewer;
use crate::data::{sys_dict_entries, sys_dict_types};
use crate::state::{db_err, StatusError};

pub struct DictTypeRepo<'a> {
    pub db: &'a DatabaseConnection,
    pub viewer: Viewer,
}

impl<'a> DictTypeRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    fn condition(&self) -> Condition {
        match self.viewer.tenant_scope() {
            Some(tid) => Condition::all().add(sys_dict_types::Column::TenantId.eq(tid)),
            None => Condition::all(),
        }
    }

    pub async fn list(&self) -> Result<Vec<sys_dict_types::Model>, StatusError> {
        sys_dict_types::Entity::find()
            .filter(self.condition())
            .order_by_asc(sys_dict_types::Column::SortOrder)
            .all(self.db)
            .await
            .map_err(db_err)
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_dict_types::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            sys_dict_types::Entity::find()
                .filter(self.condition())
                .order_by_asc(sys_dict_types::Column::SortOrder),
            req,
        )
        .await
    }

    pub async fn get_by_code(
        &self,
        code: &str,
    ) -> Result<Option<sys_dict_types::Model>, StatusError> {
        sys_dict_types::Entity::find()
            .filter(
                self.condition()
                    .add(sys_dict_types::Column::TypeCode.eq(code)),
            )
            .one(self.db)
            .await
            .map_err(db_err)
    }

    /// Delete cascades the type's entries (Delete = BatchDelete semantics).
    pub async fn delete_cascade(&self, ids: &[u32]) -> Result<(), StatusError> {
        sys_dict_entries::Entity::delete_many()
            .filter(sys_dict_entries::Column::TypeId.is_in(ids.to_vec()))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        sys_dict_types::Entity::delete_many()
            .filter(sys_dict_types::Column::Id.is_in(ids.to_vec()))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

pub struct DictEntryRepo<'a> {
    pub db: &'a DatabaseConnection,
    pub viewer: Viewer,
}

impl<'a> DictEntryRepo<'a> {
    pub fn new(db: &'a DatabaseConnection, viewer: Viewer) -> Self {
        Self { db, viewer }
    }

    fn condition(&self) -> Condition {
        match self.viewer.tenant_scope() {
            Some(tid) => Condition::all().add(sys_dict_entries::Column::TenantId.eq(tid)),
            None => Condition::all(),
        }
    }

    /// Paged listing over the PagingRequest contract: returns (rows, total).
    pub async fn paged_list(
        &self,
        req: &proto::proto::pagination::PagingRequest,
    ) -> Result<(Vec<sys_dict_entries::Model>, u64), StatusError> {
        crate::paging::fetch_paged(
            self.db,
            sys_dict_entries::Entity::find()
                .filter(self.condition())
                .order_by_asc(sys_dict_entries::Column::SortOrder),
            req,
        )
        .await
    }

    /// The ListByTypeCode walk: enabled
    /// entries under the type, sort_order ascending.
    pub async fn list_by_type_code(
        &self,
        tenant_id: u32,
        type_code: &str,
    ) -> Result<Vec<sys_dict_entries::Model>, StatusError> {
        let type_row = sys_dict_types::Entity::find()
            .filter(
                Condition::all()
                    .add(sys_dict_types::Column::TenantId.eq(tenant_id))
                    .add(sys_dict_types::Column::TypeCode.eq(type_code)),
            )
            .one(self.db)
            .await
            .map_err(db_err)?;
        let Some(type_row) = type_row else {
            return Ok(Vec::new());
        };
        sys_dict_entries::Entity::find()
            .filter(
                Condition::all()
                    .add(sys_dict_entries::Column::TenantId.eq(tenant_id))
                    .add(sys_dict_entries::Column::TypeId.eq(type_row.id))
                    .add(sys_dict_entries::Column::IsEnabled.eq(true)),
            )
            .order_by_asc(sys_dict_entries::Column::SortOrder)
            .all(self.db)
            .await
            .map_err(db_err)
    }

    pub async fn delete_with_i18n(&self, ids: &[u32]) -> Result<(), StatusError> {
        for id in ids {
            crate::data::sys_dict_entry_i18n::Entity::delete_many()
                .filter(crate::data::sys_dict_entry_i18n::Column::EntryId.eq(*id))
                .exec(self.db)
                .await
                .map_err(db_err)?;
        }
        sys_dict_entries::Entity::delete_many()
            .filter(sys_dict_entries::Column::Id.is_in(ids.to_vec()))
            .exec(self.db)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
