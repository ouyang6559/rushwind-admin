//! The admin service assembly entry: loads the embedded config defaults
//! (env overrides win), connects Postgres + Redis, builds the JWT engines
//! (RS256 verify + mint), wires the service layer, and runs the REST
//! (:7788), SSE (:7789), and task-queue transports in one lifecycle.
//! Everything lives in the library crate; this binary only composes it.

use std::sync::Arc;

use admin_api::config::Config;
use admin_api::seed;
use admin_api::server::{apalis_server, rest_server, sse_server};
use admin_api::state::AppState;
use rushwind_core::App;
use rushwind_transport::StopSignal;
use rushwind_transport_axum::AxumServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::load()?;
    let public_pem: &[u8] = match &cfg.jwt_public_key {
        Some(pem) => pem.as_bytes(),
        None => include_bytes!("../assets/jwt_public_key.pem"),
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

    // The task queue transport: apalis Postgres storage + worker,
    // registered into the same lifecycle as REST + SSE. The scheduler
    // below is the cron producer that enqueues due jobs.
    let task_server = Arc::new(apalis_server::ApalisServer::new(
        Arc::clone(&state),
        &state.cfg.database_source,
        "default",
    )?);

    // The periodic cron producer: enabled PERIODIC rows + the two system
    // crons enqueue jobs on cron match.

    let app = rest_server::build_router(Arc::clone(&state));

    // Listener addresses ride server.yaml (`server.rest.addr` /
    // `server.sse.addr`, ":7788" host-any form).
    let rest_addr = parse_addr(&state.cfg.rest_addr, 7788);
    let sse_addr = parse_addr(&state.cfg.sse_addr, 7789);

    let sse_server = sse_server::new_sse_server(Arc::clone(&state), sse_addr)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // The cron producer transport: static system crons + the DB-driven
    // PERIODIC wildcard job.
    let cron_server = apalis_server::cron_server(Arc::clone(&state), Arc::clone(&task_server));

    let server = AxumServer::new(rest_addr, app)?;
    let lifecycle = App::builder()
        .name("admin")
        .version("0.1.0")
        .server(Arc::new(server))
        .server(Arc::new(sse_server))
        .server(task_server)
        .server(Arc::new(cron_server))
        .build();
    // The lifecycle owns the OS-signal shutdown path internally; the
    // external signal here stays unfired.
    let external = StopSignal::new();
    lifecycle.run(external).await?;
    Ok(())
}

/// Parses the yaml `":7788"` host-any form (host omitted → all
/// interfaces); a bare port or missing value falls back to the default.
fn parse_addr(addr: &str, default_port: u16) -> std::net::SocketAddr {
    use std::net::SocketAddr;
    let addr = addr.trim();
    if let Some(port_text) = addr.strip_prefix(':') {
        if let Ok(port) = port_text.parse::<u16>() {
            return SocketAddr::from(([0, 0, 0, 0], port));
        }
    }
    addr.parse()
        .unwrap_or(SocketAddr::from(([0, 0, 0, 0], default_port)))
}
