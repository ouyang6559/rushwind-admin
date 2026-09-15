//! The service layer — one module per proto service, each implementing
//! its generated Handlers trait against the shared [`crate::state::AppState`]
//! (the reference internal/service/*).

pub mod access_key_service;
pub mod admin_portal_service;
pub mod authentication_service;
pub mod config_service;
pub mod dict_entry_service;
pub mod dict_type_service;
pub mod language_service;
pub mod login_policy_service;
pub mod mfa_service;
pub mod permission_group_service;
pub mod role_service;
pub mod user_profile_service;
pub mod user_service;

pub use access_key_service::AccessKeyService;
pub use admin_portal_service::AdminPortalService;
pub use authentication_service::AuthenticationService;
pub use config_service::ConfigService;
pub use dict_entry_service::DictEntryService;
pub use dict_type_service::DictTypeService;
pub use language_service::LanguageService;
pub use login_policy_service::LoginPolicyService;
pub use mfa_service::MfaService;
pub use permission_group_service::PermissionGroupService;
pub use role_service::RoleService;
pub use user_profile_service::UserProfileService;
pub use user_service::UserService;
