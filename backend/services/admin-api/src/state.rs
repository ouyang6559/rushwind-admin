//! The shared application state: config, database pool, Redis, the JWT
//! engines (verify + mint) and the token/session store.

use std::sync::Arc;

use redis::aio::ConnectionManager;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

use crate::config::Config;
use crate::token::{TokenStore, UserTokenPayload};

pub struct AppState {
    #[allow(dead_code)] // request-time config surface (SSE/limits land with later phases)
    pub cfg: Config,
    pub db: DatabaseConnection,
    pub redis: ConnectionManager,
    /// The verification engine the auth gate rides (public key).
    pub authenticator: Arc<dyn rushwind_authn::Authenticator>,
    /// The mint engine (private key) — RS256.
    pub jwt: rushwind_authn_jwt::JwtAuthenticator,
    pub tokens: TokenStore,
    /// The SSE notification hub (`/events` subscribers).
    pub hub: crate::server::sse_server::Hub,
}

impl AppState {
    /// Connects Postgres + Redis and builds the JWT engines. `migrate`
    /// wiring (schema bootstrap) is owned by the golden-DDL pipeline; the
    /// reference DB already carries the schema via its own migration.
    pub async fn connect(
        cfg: Config,
        authenticator: Arc<dyn rushwind_authn::Authenticator>,
    ) -> Result<Self, String> {
        let mut opts = ConnectOptions::new(cfg.database_source.clone());
        opts.max_connections(25)
            .min_connections(25)
            .connect_timeout(std::time::Duration::from_secs(10));
        let db = Database::connect(opts)
            .await
            .map_err(|e| format!("postgres connect: {e}"))?;

        let client = redis::Client::open(format!(
            "redis://:{}@{}/",
            cfg.redis_password, cfg.redis_addr
        ))
        .map_err(|e| format!("redis url: {e}"))?;
        let redis_conn = client
            .get_connection_manager()
            .await
            .map_err(|e| format!("redis connect: {e}"))?;

        // The mint engine — auth.yaml private key (or env).
        let private_pem = cfg
            .jwt_private_key
            .clone()
            .ok_or("auth.yaml private_key missing (RS256 minting requires it)")?;
        let jwt = rushwind_authn_jwt::JwtAuthenticator::new(
            rushwind_authn_jwt::JwtOptions::new()
                .with_algorithm("RS256")
                .map_err(|e| format!("jwt algorithm: {e:?}"))?
                .with_rsa_private_key_from_pem(private_pem.as_bytes())
                .map_err(|e| format!("jwt private key: {e:?}"))?,
        );

        let tokens = TokenStore::new(
            redis_conn.clone(),
            cfg.access_token_expires_secs,
            cfg.refresh_token_expires_secs,
        );

        Ok(Self {
            cfg,
            db,
            redis: redis_conn,
            authenticator,
            jwt,
            tokens,
            hub: crate::server::sse_server::Hub::default(),
        })
    }
}

/// The error envelope helper: reason → the admin error table's HTTP
/// status (`StatusError::new`) and the `<Reason>` literal.
pub fn status_error(
    reason: &'static str,
    message: impl Into<String>,
) -> rushwind_http_binding::envelope::StatusError {
    let status = proto::tables::error_status("admin.service.v1", reason).unwrap_or(500);
    rushwind_http_binding::envelope::StatusError::new(status, reason, message)
}

/// The Unknown branch: 500 + empty reason (`errors.FromError`).
pub fn internal_error(message: impl Into<String>) -> rushwind_http_binding::envelope::StatusError {
    rushwind_http_binding::envelope::StatusError::new(500, "", message)
}

/// The missing-identity error for service-side operator extraction.
pub fn operator_missing() -> StatusError {
    status_error("UNAUTHORIZED", "missing identity")
}

pub type StatusError = rushwind_http_binding::envelope::StatusError;

/// The verified operator extracted from the request context claims.
pub fn operator_of(
    ctx: &rushwind_http_binding::ctx::RequestContext,
) -> Result<UserTokenPayload, StatusError> {
    ctx.claims
        .as_ref()
        .and_then(UserTokenPayload::from_claims)
        .ok_or_else(|| status_error("UNAUTHORIZED", "missing identity"))
}

/// The operator's tenant, platform (0) when no identity rides.
pub fn tenant_of(ctx: &rushwind_http_binding::ctx::RequestContext) -> u32 {
    operator_of(ctx).map(|p| p.tenant_id).unwrap_or(0)
}

/// Repository-layer DB failure → the Unknown envelope.
pub fn db_err(e: sea_orm::DbErr) -> StatusError {
    internal_error(format!("db: {e}"))
}

pub fn not_found(what: &str) -> StatusError {
    status_error("NOT_FOUND", format!("{what} not found"))
}

/// Naive local datetime → protojson Timestamp (pbjson).
pub fn naive_to_ts(value: chrono::NaiveDateTime) -> Option<pbjson_types::Timestamp> {
    use chrono::TimeZone as _;
    let utc = chrono::Utc.from_utc_datetime(&value);
    Some(pbjson_types::Timestamp {
        seconds: utc.timestamp(),
        nanos: utc.timestamp_subsec_nanos() as i32,
    })
}

/// protojson Timestamp → naive local datetime.
pub fn ts_to_naive(value: &pbjson_types::Timestamp) -> Option<chrono::NaiveDateTime> {
    use chrono::TimeZone as _;
    Some(
        chrono::Utc
            .timestamp_opt(value.seconds, value.nanos.max(0) as u32)
            .single()?
            .naive_utc(),
    )
}
