//! Repository layer. Repos own ALL
//! query predicates: tenancy rides the [`Viewer`], never ad-hoc filters
//! in services.
//!
//! The full repo surface is the data-layer API.
#![allow(dead_code)]

pub mod access_key;
pub mod api;
pub mod audit;
pub mod config;
pub mod dict;
pub mod file;
pub mod language;
pub mod login_policy;
pub mod menu;
pub mod message;
pub mod mfa_factor;
pub mod notification_channel;
pub mod org_unit;
pub mod permission;
pub mod plan;
pub mod position;
pub mod role;
pub mod script;
pub mod task;
pub mod tenant;
pub mod user;

pub use access_key::AccessKeyRepo;
pub use api::ApiRepo;
pub use audit::AuditRepo;
pub use config::ConfigRepo;
pub use dict::DictEntryRepo;
pub use dict::DictTypeRepo;
pub use file::FileRepo;
pub use language::LanguageRepo;
pub use login_policy::LoginPolicyRepo;
pub use menu::MenuRepo;
pub use message::InternalMessageRecipientRepo;
pub use message::InternalMessageRepo;
pub use notification_channel::NotificationChannelRepo;
pub use permission::PermissionRepo;
pub use plan::PlanModuleRepo;
pub use plan::PlanQuotaRepo;
pub use plan::PlanRepo;
pub use position::PositionRepo;
pub use role::RoleRepo;
pub use script::ScriptLogRepo;
pub use script::ScriptRepo;
pub use task::TaskRepo;
pub use user::UserRepo;
