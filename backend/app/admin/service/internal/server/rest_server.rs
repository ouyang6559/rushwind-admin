//! The REST server assembly — //! `internal/server/module`: mounts the full route surface with
//! null placeholder services, splits the auth-free public subtree from
//! the gated one (`AddWhiteList` set), composes the
//! per-route layer stack, merges the two, and applies the HTTP edge.
//!
//! Per-route layer composition (the `wrap` closure below): the framework
//! bind layer — body and query binding — outermost on EVERY route, and
//! the auth gate (pkg/middleware-auth) composed INSIDE it on gated routes
//! only. That order is the wire contract: codec/binding failures answer
//! 400 ahead of any 401.
//!
//! The authorization engine wires here too lands with the
//! storage phase; until then the gate is the protected subtree's only
//! defense.
//!
//! The CORS policy and request budget are mirrored verbatim from the
//! reference server.yaml `rest` block, through the gorilla-compatible
//! CORS layer (see rushwind_http::cors_compat — wires
//! gorilla/handlers there, whose emission rules tower-http does not
//! reproduce).

use std::sync::Arc;

use axum::http::header;
use axum::routing::MethodRouter;

use crate::service::{
    AccessKeyService, AdminPortalService, ApiAuditLogService, ApiService, AuthenticationService,
    ConfigService, DashboardService, DataAccessAuditLogService, DictEntryService, DictTypeService,
    FileService, FileTransferService, InternalMessageCategoryService,
    InternalMessageRecipientService, InternalMessageService, LanguageService, LoginAuditLogService,
    LoginPolicyService, MenuService, MfaService, NotificationChannelService, OnlineSessionService,
    OperationAuditLogService, OrgUnitService, PermissionAuditLogService, PermissionGroupService,
    PermissionService, PlanModuleService, PlanQuotaService, PlanService,
    PolicyEvaluationLogService, PositionService, RedisCacheMonitorService, RoleService,
    ScriptLogService, ScriptService, ServerMonitorService, TaskService, TenantService,
    UserProfileService, UserService,
};
use crate::state::AppState;
use gen_rust::pool;
use middleware_auth::auth_gate;
use rushwind_http::{CorsOptions, HttpEdge};
use rushwind_http_binding::bindgate::bind_run;
use rushwind_http_binding::wire::RouteWire;

/// The deployment's registered codec subtypes — the packages
/// its binary imports (the compatibility spec §2.1 register).
const REGISTERED_SUBTYPES: &[&str] = &["json", "proto", "x-www-form-urlencoded"];

/// Builds the mounted router. `state` carries the verification engine
/// and the server-side session store; the assembly order mirrors
/// module.
pub fn build_router(state: Arc<AppState>) -> axum::Router {
    let descriptor_pool = pool();
    let authenticator = Arc::clone(&state.authenticator);
    let checker = Arc::new(crate::RedisTokenChecker(state.tokens.clone()))
        as Arc<dyn middleware_auth::AccessTokenChecker + 'static>;

    // The per-route layer composition (see the module docs): bind layer
    // outermost always, auth gate inside it on gated routes.
    let wrap = |mr: MethodRouter, wire: &RouteWire, gated: bool| -> MethodRouter {
        let mut out = mr;
        if gated {
            let auth = Arc::clone(&authenticator);
            let checker = Arc::clone(&checker);
            let gate = axum::middleware::from_fn(move |req, next| {
                let auth = Arc::clone(&auth);
                let checker = Arc::clone(&checker);
                async move { auth_gate(auth, checker, req, next).await }
            });
            out = out.layer(gate);
        }
        let input_fq = wire.input_fq;
        let body_star = wire.body_star;
        let bind = axum::middleware::from_fn(move |req, next| {
            let (fq, body) = (input_fq, body_star);
            async move { bind_run(descriptor_pool, fq, body, REGISTERED_SUBTYPES, req, next).await }
        });
        out = out.layer(bind);
        out
    };

    // The full mounted surface, placeholder services behind it. Every
    // mount call threads both routers; the generator's auth-free table
    // classifies each of its route bindings.
    let mut router_pub = axum::Router::new();
    let mut router_gate = axum::Router::new();
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_access_key_service(
        router_pub,
        router_gate,
        Arc::new(AccessKeyService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_admin_portal_service(
        router_pub,
        router_gate,
        Arc::new(AdminPortalService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_api_audit_log_service(
        router_pub,
        router_gate,
        Arc::new(ApiAuditLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_api_service(
        router_pub,
        router_gate,
        Arc::new(ApiService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_authentication_service(
        router_pub,
        router_gate,
        Arc::new(AuthenticationService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_config_service(
        router_pub,
        router_gate,
        Arc::new(ConfigService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_dashboard_service(
        router_pub,
        router_gate,
        Arc::new(DashboardService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_data_access_audit_log_service(
        router_pub,
        router_gate,
        Arc::new(DataAccessAuditLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_dict_entry_service(
        router_pub,
        router_gate,
        Arc::new(DictEntryService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_dict_type_service(
        router_pub,
        router_gate,
        Arc::new(DictTypeService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_file_service(
        router_pub,
        router_gate,
        Arc::new(FileService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_file_transfer_service(
        router_pub,
        router_gate,
        Arc::new(FileTransferService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_internal_message_category_service(
        router_pub,
        router_gate,
        Arc::new(InternalMessageCategoryService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_internal_message_recipient_service(
        router_pub,
        router_gate,
        Arc::new(InternalMessageRecipientService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_internal_message_service(
        router_pub,
        router_gate,
        Arc::new(InternalMessageService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_language_service(
        router_pub,
        router_gate,
        Arc::new(LanguageService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_login_audit_log_service(
        router_pub,
        router_gate,
        Arc::new(LoginAuditLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_login_policy_service(
        router_pub,
        router_gate,
        Arc::new(LoginPolicyService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_menu_service(
        router_pub,
        router_gate,
        Arc::new(MenuService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_mfa_service(
        router_pub,
        router_gate,
        Arc::new(MfaService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_notification_channel_service(
        router_pub,
        router_gate,
        Arc::new(NotificationChannelService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_online_session_service(
        router_pub,
        router_gate,
        Arc::new(OnlineSessionService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_operation_audit_log_service(
        router_pub,
        router_gate,
        Arc::new(OperationAuditLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_org_unit_service(
        router_pub,
        router_gate,
        Arc::new(OrgUnitService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_permission_audit_log_service(
        router_pub,
        router_gate,
        Arc::new(PermissionAuditLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_permission_group_service(
        router_pub,
        router_gate,
        Arc::new(PermissionGroupService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_permission_service(
        router_pub,
        router_gate,
        Arc::new(PermissionService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_plan_module_service(
        router_pub,
        router_gate,
        Arc::new(PlanModuleService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_plan_quota_service(
        router_pub,
        router_gate,
        Arc::new(PlanQuotaService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_plan_service(
        router_pub,
        router_gate,
        Arc::new(PlanService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_policy_evaluation_log_service(
        router_pub,
        router_gate,
        Arc::new(PolicyEvaluationLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_position_service(
        router_pub,
        router_gate,
        Arc::new(PositionService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_redis_cache_monitor_service(
        router_pub,
        router_gate,
        Arc::new(RedisCacheMonitorService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_role_service(
        router_pub,
        router_gate,
        Arc::new(RoleService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_script_log_service(
        router_pub,
        router_gate,
        Arc::new(ScriptLogService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_script_service(
        router_pub,
        router_gate,
        Arc::new(ScriptService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_server_monitor_service(
        router_pub,
        router_gate,
        Arc::new(ServerMonitorService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_task_service(
        router_pub,
        router_gate,
        Arc::new(TaskService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_tenant_service(
        router_pub,
        router_gate,
        Arc::new(TenantService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_user_profile_service(
        router_pub,
        router_gate,
        Arc::new(UserProfileService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = gen_rust::gen::mounts::mount_user_service(
        router_pub,
        router_gate,
        Arc::new(UserService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    let mut app = router_pub.merge(router_gate);

    // The docs surface (Swagger UI / Redoc / raw spec), switched by
    // server.rest.enable_swagger / enable_redoc.
    app = app.merge(crate::docs_server::router(
        state.cfg.enable_swagger,
        state.cfg.enable_redoc,
    ));

    // The audit-write layer:
    // post-handler persistence into the audit tables, outermost so it
    // sees final status codes.
    let app = app.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&state),
        crate::audit::layer,
    ));

    // The CORS policy and request budget, mirrored verbatim
    // from server.yaml's rest block: credentialed responses (the
    // refresh-token cookie), the six methods, the six headers, the three
    // frontend domains plus the local dev ports.
    // The CORS policy and request budget ride server.yaml's rest block
    // (parsed in config.rs) — never hardcoded.
    let mut cors = CorsOptions::default().with_allow_credentials(state.cfg.cors_allow_credentials);
    for method in &state.cfg.cors_methods {
        cors = cors.with_allow_method(method.as_str());
    }
    for header_name in &state.cfg.cors_headers {
        cors = cors.with_allow_header(header_name.as_str());
    }
    for origin in &state.cfg.cors_origins {
        cors = cors.with_allow_origin(origin.as_str());
    }
    HttpEdge::new()
        .with_cors_compat(cors)
        .with_timeout(std::time::Duration::from_secs(
            state.cfg.rest_timeout_secs.max(1),
        ))
        .wrap(app)
}
