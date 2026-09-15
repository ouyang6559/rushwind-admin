//! The admin service entry: loads the vendored reference configs, connects
//! Postgres + Redis, builds the JWT engines (RS256 verify + mint), wires
//! the real service layer, and serves REST :7788. SSE (:7789) lands with
//! the notification phase.

#[path = "../../internal/captcha.rs"]
mod captcha;
#[path = "../../internal/config.rs"]
mod config;
#[path = "../../internal/crypto.rs"]
mod crypto;
#[path = "../../internal/data/mod.rs"]
mod data;
#[path = "../../internal/paging.rs"]
mod paging;
#[path = "../../internal/policy.rs"]
mod policy;
#[path = "../../internal/ratelimit.rs"]
mod ratelimit;
#[path = "../../internal/server/rest_server.rs"]
mod rest_server;
#[path = "../../internal/seed.rs"]
mod seed;
#[path = "../../internal/service/mod.rs"]
mod service;
#[path = "../../internal/state.rs"]
mod state;
#[path = "../../internal/token.rs"]
mod token;

use std::sync::Arc;

use rushwind_core::App;
use rushwind_transport::StopSignal;
use rushwind_transport_axum::AxumServer;

use state::AppState;
use token::TokenStore;

/// The adapter wiring the service-side Redis session store into the
/// middleware's server-side check stage.
struct RedisTokenChecker(TokenStore);

#[async_trait::async_trait]
impl middleware_auth::AccessTokenChecker for RedisTokenChecker {
    async fn is_valid_access_token(&self, uid: u32, jti: &str, token: &str) -> bool {
        self.0.is_valid_access_token(uid, jti, token).await
    }
    async fn is_blocked_access_token(&self, jti: &str) -> bool {
        self.0.is_blocked_access_token(jti).await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config::Config::load()?;
    let public_pem: &[u8] = match &cfg.jwt_public_key {
        Some(pem) => pem.as_bytes(),
        None => include_bytes!("../../configs/jwt_public_key.pem"),
    };
    let verifier = rushwind_authn_jwt::JwtAuthenticator::new(
        rushwind_authn_jwt::JwtOptions::new()
            .with_algorithm("RS256")
            .map_err(|e| format!("jwt algorithm: {e:?}"))?
            .with_rsa_public_key_from_pem(public_pem)
            .map_err(|e| format!("jwt public key: {e:?}"))?,
    );
    let authenticator: Arc<dyn rushwind_authn::Authenticator> = Arc::new(verifier);

    let state = Arc::new(AppState::connect(cfg, Arc::clone(&authenticator)).await?);
    seed::run(&state).await;

    let app = rest_server::build_router(state);

    // The reference rest.addr ":7788" — every interface.
    let server = AxumServer::new(std::net::SocketAddr::from(([0, 0, 0, 0], 7788)), app)?;
    let lifecycle = App::builder()
        .name("admin")
        .version("0.1.0")
        .server(Arc::new(server))
        .build();
    // The lifecycle owns the OS-signal shutdown path internally; the
    // external signal here stays unfired.
    let external = StopSignal::new();
    lifecycle.run(external).await?;
    Ok(())
}
