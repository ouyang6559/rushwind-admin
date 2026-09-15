//! The REST server assembly — the port of the reference
//! `internal/server/rest_server.go`: mounts the full route surface with
//! null placeholder services, splits the auth-free public subtree from
//! the gated one (the reference `AddWhiteList` set), composes the
//! per-route layer stack, merges the two, and applies the HTTP edge.
//!
//! Per-route layer composition (the `wrap` closure below): the framework
//! bind layer — body and query binding — outermost on EVERY route, and
//! the auth gate (pkg/middleware-auth) composed INSIDE it on gated routes
//! only. That order reproduces the reference's: its generated handlers
//! run `ctx.Bind`/`ctx.BindQuery` before `ctx.Middleware` executes the
//! auth chain, so codec/binding failures answer 400 ahead of any 401.
//!
//! The authorization engine the reference wires here too lands with the
//! storage phase; until then the gate is the protected subtree's only
//! defense.
//!
//! The CORS policy and request budget are mirrored verbatim from the
//! reference server.yaml `rest` block, through the gorilla-compatible
//! CORS layer (see rushwind_http::cors_compat — the reference wires
//! gorilla/handlers there, whose emission rules tower-http does not
//! reproduce).

use std::sync::Arc;

use axum::routing::MethodRouter;

use crate::service::{
    AccessKeyService, AdminPortalService, AuthenticationService, ConfigService, DictEntryService,
    DictTypeService, LanguageService, LoginPolicyService, MfaService, PermissionGroupService,
    RoleService, UserProfileService, UserService,
};
use crate::state::AppState;
use admin_api::pool;
use middleware_auth::auth_gate;
use rushwind_http::{CorsOptions, HttpEdge};
use rushwind_http_binding::bindgate::bind_run;
use rushwind_http_binding::wire::RouteWire;

/// The reference deployment's registered codec subtypes — the packages
/// its binary imports (the compatibility spec §2.1 register).
const REGISTERED_SUBTYPES: &[&str] = &["json", "proto", "x-www-form-urlencoded"];

/// Builds the mounted router. `state` carries the verification engine
/// and the server-side session store; the assembly order mirrors
/// rest_server.go.
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
    (router_pub, router_gate) = admin_api::gen::mounts::mount_access_key_service(
        router_pub,
        router_gate,
        Arc::new(AccessKeyService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_admin_portal_service(
        router_pub,
        router_gate,
        Arc::new(AdminPortalService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_api_audit_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_api_audit_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_api_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_api_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_authentication_service(
        router_pub,
        router_gate,
        Arc::new(AuthenticationService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_config_service(
        router_pub,
        router_gate,
        Arc::new(ConfigService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_dashboard_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_dashboard_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_data_access_audit_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_data_access_audit_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_dict_entry_service(
        router_pub,
        router_gate,
        Arc::new(DictEntryService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_dict_type_service(
        router_pub,
        router_gate,
        Arc::new(DictTypeService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_file_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_file_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_file_transfer_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_file_transfer_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_internal_message_category_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_internal_message_category_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_internal_message_recipient_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_internal_message_recipient_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_internal_message_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_internal_message_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_language_service(
        router_pub,
        router_gate,
        Arc::new(LanguageService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_login_audit_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_login_audit_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_login_policy_service(
        router_pub,
        router_gate,
        Arc::new(LoginPolicyService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_menu_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_menu_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_mfa_service(
        router_pub,
        router_gate,
        Arc::new(MfaService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_notification_channel_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_notification_channel_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_online_session_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_online_session_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_operation_audit_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_operation_audit_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_org_unit_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_org_unit_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_permission_audit_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_permission_audit_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_permission_group_service(
        router_pub,
        router_gate,
        Arc::new(PermissionGroupService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_permission_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_permission_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_plan_module_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_plan_module_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_plan_quota_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_plan_quota_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_plan_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_plan_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_policy_evaluation_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_policy_evaluation_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_position_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_position_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_redis_cache_monitor_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_redis_cache_monitor_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_role_service(
        router_pub,
        router_gate,
        Arc::new(RoleService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_script_log_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_script_log_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_script_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_script_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_server_monitor_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_server_monitor_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_task_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_task_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_tenant_service(
        router_pub,
        router_gate,
        admin_api::gen::nulls::null_tenant_service(),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_user_profile_service(
        router_pub,
        router_gate,
        Arc::new(UserProfileService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    (router_pub, router_gate) = admin_api::gen::mounts::mount_user_service(
        router_pub,
        router_gate,
        Arc::new(UserService {
            state: Arc::clone(&state),
        }),
        &wrap,
    );
    let app = router_pub.merge(router_gate);

    // The reference's CORS policy and request budget, mirrored verbatim
    // from server.yaml's rest block: credentialed responses (the
    // refresh-token cookie), the six methods, the six headers, the three
    // frontend domains plus the local dev ports.
    let cors = CorsOptions::default()
        .with_allow_credentials(true)
        .with_allow_method("GET")
        .with_allow_method("POST")
        .with_allow_method("PUT")
        .with_allow_method("DELETE")
        .with_allow_method("HEAD")
        .with_allow_method("OPTIONS")
        .with_allow_header("X-Requested-With")
        .with_allow_header("X-Request-ID")
        .with_allow_header("Content-Type")
        .with_allow_header("Authorization")
        .with_allow_header("X-Captcha-Id")
        .with_allow_header("X-Captcha-Value")
        .with_allow_origin("https://vben.admin.gowind.cloud")
        .with_allow_origin("https://ele.admin.gowind.cloud")
        .with_allow_origin("https://react.admin.gowind.cloud")
        .with_allow_origin("http://localhost:5666")
        .with_allow_origin("http://localhost:5777")
        .with_allow_origin("http://localhost:5888")
        .with_allow_origin("http://localhost:5667")
        .with_allow_origin("http://localhost:5778");
    HttpEdge::new()
        .with_cors_compat(cors)
        .with_timeout(std::time::Duration::from_secs(10))
        .wrap(app)
}
