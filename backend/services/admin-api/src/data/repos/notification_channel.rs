//! NotificationChannelRepo — platform-global rows with
//! all predicates owned here (never ad-hoc in services).

use sea_orm::sea_query::Condition;
use sea_orm::{DatabaseConnection, EntityTrait, QueryFilter};

use crate::data::sys_notification_channels as entity;
use crate::state::{db_err, not_found, StatusError};

repo_shell!(global NotificationChannelRepo, entity, "notification channel");
