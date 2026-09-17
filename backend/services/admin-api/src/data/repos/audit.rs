//! AuditRepo — audit-log repos (login, api,
//! operation, data-access, permission, policy-evaluation): newest-first
//! listing with paging over the six append-only tables.

use sea_orm::{DatabaseConnection, EntityTrait, PaginatorTrait, QueryOrder, QuerySelect};

use crate::paging as admin_paging;
use proto::proto::pagination::PagingRequest;

use crate::data::{
    sys_api_audit_logs, sys_data_access_audit_logs, sys_login_audit_logs, sys_operation_audit_logs,
    sys_permission_audit_logs, sys_policy_evaluation_logs,
};
use crate::state::{db_err, StatusError};

pub struct AuditRepo<'a> {
    pub db: &'a DatabaseConnection,
}

macro_rules! audit_suite {
    ($list:ident, $count:ident, $entity:ident) => {
        pub async fn $list(
            &self,
            limit: u64,
            offset: u64,
        ) -> Result<Vec<$entity::Model>, StatusError> {
            $entity::Entity::find()
                .order_by_desc($entity::Column::CreatedAt)
                .offset(offset)
                .limit(limit)
                .all(self.db)
                .await
                .map_err(db_err)
        }

        pub async fn $count(&self) -> u64 {
            $entity::Entity::find().count(self.db).await.unwrap_or(0)
        }
    };
}

impl<'a> AuditRepo<'a> {
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }

    audit_suite!(list_login, count_login, sys_login_audit_logs);
    audit_suite!(list_api, count_api, sys_api_audit_logs);
    audit_suite!(list_operation, count_operation, sys_operation_audit_logs);
    audit_suite!(
        list_data_access,
        count_data_access,
        sys_data_access_audit_logs
    );
    audit_suite!(list_permission, count_permission, sys_permission_audit_logs);
    audit_suite!(
        list_policy_evaluation,
        count_policy_evaluation,
        sys_policy_evaluation_logs
    );
}

macro_rules! audit_paged {
    ($paged:ident, $entity:ident) => {
        pub async fn $paged(
            &self,
            req: &PagingRequest,
        ) -> Result<(Vec<$entity::Model>, u64), StatusError> {
            admin_paging::fetch_paged(
                self.db,
                $entity::Entity::find().order_by_desc($entity::Column::CreatedAt),
                req,
            )
            .await
        }
    };
}

impl<'a> AuditRepo<'a> {
    audit_paged!(paged_login, sys_login_audit_logs);
    audit_paged!(paged_api, sys_api_audit_logs);
    audit_paged!(paged_operation, sys_operation_audit_logs);
    audit_paged!(paged_data_access, sys_data_access_audit_logs);
    audit_paged!(paged_permission, sys_permission_audit_logs);
    audit_paged!(paged_policy_evaluation, sys_policy_evaluation_logs);
}
