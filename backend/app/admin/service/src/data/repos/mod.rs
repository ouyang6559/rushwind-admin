//! Repository layer — `internal/data/*module`
//! Repos own ALL
//! query predicates: tenancy rides the [`Viewer`], never ad-hoc filters
//! in services.
//!
//! The full repo surface is the data-layer API.
#![allow(dead_code)]

pub mod access_key_repo;
pub mod api_repo;
pub mod audit_repo;
pub mod config_repo;
pub mod dict_repo;
pub mod file_repo;
pub mod language_repo;
pub mod login_policy_repo;
pub mod menu_repo;
pub mod message_repo;
pub mod mfa_factor_repo;
pub mod notification_channel_repo;
pub mod org_unit_repo;
pub mod permission_repo;
pub mod plan_repo;
pub mod position_repo;
pub mod role_repo;
pub mod script_repo;
pub mod task_repo;
pub mod tenant_repo;
pub mod user_repo;

pub use access_key_repo::AccessKeyRepo;
pub use api_repo::ApiRepo;
pub use audit_repo::AuditRepo;
pub use config_repo::ConfigRepo;
pub use dict_repo::DictEntryRepo;
pub use dict_repo::DictTypeRepo;
pub use file_repo::FileRepo;
pub use language_repo::LanguageRepo;
pub use login_policy_repo::LoginPolicyRepo;
pub use menu_repo::MenuRepo;
pub use message_repo::InternalMessageRecipientRepo;
pub use message_repo::InternalMessageRepo;
pub use notification_channel_repo::NotificationChannelRepo;
pub use permission_repo::PermissionRepo;
pub use plan_repo::PlanModuleRepo;
pub use plan_repo::PlanQuotaRepo;
pub use plan_repo::PlanRepo;
pub use position_repo::PositionRepo;
pub use role_repo::RoleRepo;
pub use script_repo::ScriptLogRepo;
pub use script_repo::ScriptRepo;
pub use task_repo::TaskRepo;
pub use user_repo::UserRepo;
