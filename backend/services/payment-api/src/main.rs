//! The payment service assembly entry: loads the embedded config
//! defaults (env overrides win), applies the golden schema migration,
//! connects Postgres + Redis and builds the core services (ledger,
//! channel registry), then hands the hand-written gateway route surface
//! plus the task-queue worker and the cron producer (T+1 thaw) to the
//! config-driven lifecycle assembler (the embedded server document, one
//! `App`, one shutdown path). Everything lives in the library crate;
//! this binary only composes it.

use std::sync::Arc;

use payment_api::config::Config;
use payment_api::migration;
use payment_api::server::{gateway, panel, worker};
use payment_api::state::AppState;
use rushwind_bootstrap::Bootstrap;
use rushwind_transport::StopSignal;

/// The server assembly document, compiled into the binary.
const SERVER_YAML: &str = include_str!("../assets/server.yaml");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::load()?;

    // The golden DDL migration must land before the state builds, since
    // the channel-registry load reads the migrated tables.
    if cfg.database_migrate {
        migration::run(&cfg).await?;
    }

    let state = Arc::new(AppState::connect(cfg).await?);

    // The shared task-queue storage handle: the worker transport claims
    // and executes; the cron producer pushes through.
    let task_queue = Arc::new(worker::TaskQueue::new(
        &state.cfg.database_source,
        "payment",
    )?);

    let mut assembler = Bootstrap::from_yaml_str(SERVER_YAML)?
        .route_pack("payment-gateway", gateway::pack(Arc::clone(&state)))
        .route_pack("payment-panel", panel::pack(Arc::clone(&state)));
    for (name, job) in worker::cron_jobs(Arc::clone(&state), Arc::clone(&task_queue)) {
        assembler = assembler.cron_job(name, job);
    }
    let assembler =
        assembler.server_factory("payment-tasks", worker::worker_factory(state, task_queue));

    let booted = assembler.build().await?;
    // The lifecycle owns the OS-signal shutdown path internally; the
    // external signal here stays unfired.
    let external = StopSignal::new();
    booted.app.run(external).await?;
    Ok(())
}
