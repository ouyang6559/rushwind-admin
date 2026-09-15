//! Repository layer. Repos own ALL
//! query predicates: tenancy rides the [`Viewer`], never ad-hoc filters
//! in services.
//!
//! The full repo surface is the data-layer API.

mod access_key;
mod api;
mod audit;
mod config;
mod dict;
mod file;
mod language;
mod login_policy;
mod menu;
mod message;
mod mfa_factor;
mod notification_channel;
mod org_unit;
mod permission;
mod plan;
mod position;
mod role;
mod script;
mod task;
mod tenant;
mod user;

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
