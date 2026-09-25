//! The shared application state: config, the Postgres pool and the Redis
//! connection. The JWT engines and the auth gate ride the back-office
//! surface (Phase 7); the merchant gateway authenticates by MD5 signature
//! and needs neither, so they stay out of this Phase-0 core.

use std::sync::Arc;

use redis::aio::ConnectionManager;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

use crate::channel::ChannelRegistry;
use crate::config::Config;
use crate::gateway::notify::MerchantNotifier;
use crate::ledger::LedgerService;
use crate::merchant::attachment::FileStorage;
use crate::payout::{PayoutRegistry, PayoutService};
use crate::risk::RiskGate;

pub struct AppState {
    pub cfg: Config,
    pub db: DatabaseConnection,
    pub redis: ConnectionManager,
    /// The single atomic accounting service (order/settle/freeze/profit/
    /// thaw/redo) — see `spec/02-funds-order.md` §11.
    pub ledger: Arc<LedgerService>,
    /// The config-driven channel adapter registry — see `spec/03` §Channel.
    pub channels: Arc<ChannelRegistry>,
    /// The merchant outbound notify sender (`spec/02` §4.6) — spawned after
    /// each handled upstream callback, advances 1→2 on an `ok` reply.
    pub notifier: Arc<MerchantNotifier>,
    /// The risk gate (`spec/06`): screens orders pre-settle and feeds the
    /// day/unit buckets + offline trips post-settle.
    pub risk: Arc<RiskGate>,
    /// The payout / withdrawal service (`spec/04`) — the settlement booking,
    /// the df-API review machine and the execution queue, mounted on the
    /// merchant-panel surface.
    pub payout: Arc<PayoutService>,
    /// The payout-channel adapter registry the execution-queue sweeps drive
    /// (`spec/04` §9), pre-built with the built-in live adapters (MGZF /
    /// Yibao) so the cron worker submits without per-run wiring.
    pub payout_registry: Arc<PayoutRegistry>,
    /// The on-disk store for merchant KYC evidence uploads (`spec/05` §8.5),
    /// rooted at the configured `upload_root`.
    pub uploads: FileStorage,
}

impl AppState {
    /// Connects Postgres + Redis and builds the core services. Schema
    /// bootstrap is owned by [`crate::migration`]; the database is
    /// expected to carry the migrated tables at startup.
    pub async fn connect(cfg: Config) -> Result<Self, String> {
        let mut opts = ConnectOptions::new(cfg.database_source.clone());
        opts.max_connections(25)
            .min_connections(5)
            .connect_timeout(std::time::Duration::from_secs(10));
        let db = Database::connect(opts)
            .await
            .map_err(|e| format!("postgres connect: {e}"))?;

        let url = if cfg.redis_password.is_empty() {
            format!("redis://{}/", cfg.redis_addr)
        } else {
            format!("redis://:{}@{}/", cfg.redis_password, cfg.redis_addr)
        };
        let client = redis::Client::open(url).map_err(|e| format!("redis url: {e}"))?;
        let redis = client
            .get_connection_manager()
            .await
            .map_err(|e| format!("redis connect: {e}"))?;

        let ledger = Arc::new(LedgerService::new(db.clone()));
        let channels = Arc::new(ChannelRegistry::load(&db).await?);
        let notifier = Arc::new(MerchantNotifier::new(db.clone(), ledger.clone()));
        let risk = Arc::new(RiskGate::new(redis.clone()));
        let payout = Arc::new(PayoutService::new(db.clone()));
        let payout_registry = Arc::new(PayoutRegistry::with_known_adapters());
        let uploads = FileStorage::new(cfg.upload_root.clone());

        Ok(Self {
            cfg,
            db,
            redis,
            ledger,
            channels,
            notifier,
            risk,
            payout,
            payout_registry,
            uploads,
        })
    }
}

/// The repository-layer DB failure → the internal-error envelope.
pub fn db_err(e: sea_orm::DbErr) -> GatewayError {
    GatewayError::Internal(format!("db: {e}"))
}

/// The gateway's error type. Wire responses render this as the legacy
/// `{"status":"error","msg":...,"data":{...}}` body (Phase 3 fills the
/// precise per-endpoint messages).
#[derive(Debug)]
pub enum GatewayError {
    /// A caller-facing rejection carrying the exact legacy message.
    BadRequest(String),
    /// An unexpected internal failure.
    Internal(String),
}

impl GatewayError {
    /// The carried message, whatever the variant — for batch tallies and
    /// per-row failure strings that surface the legacy text.
    pub fn message(&self) -> &str {
        match self {
            GatewayError::BadRequest(m) | GatewayError::Internal(m) => m,
        }
    }
}

/// DB failures are never caller-facing — they surface as the internal-error
/// envelope (same shaping as [`db_err`], as a `?`-friendly conversion).
impl From<sea_orm::DbErr> for GatewayError {
    fn from(e: sea_orm::DbErr) -> Self {
        db_err(e)
    }
}

pub type GatewayResult<T> = Result<T, GatewayError>;
