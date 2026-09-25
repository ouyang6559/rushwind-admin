//! MenuRepo — platform-global
//! rows (menus carry no tenant column), all predicates owned here.

use sea_orm::sea_query::Condition;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::sys_menus as entity;
use crate::state::{db_err, not_found, StatusError};

repo_shell!(global MenuRepo, entity, "menu");
