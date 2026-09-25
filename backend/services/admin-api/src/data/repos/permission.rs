//! PermissionRepo — platform-global rows with
//! all predicates owned here (never ad-hoc in services).

use sea_orm::sea_query::Condition;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::sys_permissions as entity;
use crate::state::{db_err, not_found, StatusError};

repo_shell!(global PermissionRepo, entity, "permission");
