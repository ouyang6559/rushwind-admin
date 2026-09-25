//! Config loading — parses the embedded yaml defaults (`assets/data.yaml`,
//! compiled into the binary) with env overrides (`RUSHWIND_DATABASE_SOURCE`
//! / `RUSHWIND_REDIS_ADDR` / `RUSHWIND_REDIS_PASSWORD` /
//! `RUSHWIND_DATABASE_MIGRATE`) for out-of-container runs. The server
//! assembly document (`assets/server.yaml`) belongs to the lifecycle
//! assembler (see `main.rs`). Mirrors the admin service's loader shape,
//! minus the JWT/OSS sections which the gateway surface does not need yet.

use serde::Deserialize;

const DATA_YAML: &str = include_str!("../assets/data.yaml");

#[derive(Debug, Clone)]
pub struct Config {
    pub database_source: String,
    /// Startup gate: run pending schema migrations before serving.
    pub database_migrate: bool,
    pub redis_addr: String,
    pub redis_password: String,
    /// The public base URL used to assemble legacy callback/notify
    /// addresses (`Pay_<code>_notifyurl.html`). Overridable for host runs.
    pub site_url: String,
    /// The on-disk root for merchant KYC evidence uploads (`spec/05` §8.5).
    /// Bytes land under `<upload_root>/verifyinfo/`; the site-relative
    /// `Uploads/verifyinfo/...` record is what the DB stores. Env-overridable.
    pub upload_root: String,
    /// The T+1 thaw cron window (§6.2 T:30-35, `allowstart~allowend`,
    /// legacy default 1~5). Overridable; `end = 0` means "no restriction"
    /// per [`crate::ledger::in_thaw_window`].
    pub thaw_allow_start: i32,
    pub thaw_allow_end: i32,
    /// The registration `pay_websiteconfig` switches (§2.3 / §3). The wide
    /// `websiteconfig` table is not modeled, so — like `site_url` and the thaw
    /// window — these ride static config (yaml + env), all defaulting `false`
    /// (the websiteconfig DDL defaults): invite code not required, email
    /// activation not required, KYC not required. See [`crate::merchant::register::SiteFlags`].
    pub register_invitecode: bool,
    pub register_need_activate: bool,
    /// `websiteconfig.authorized` — when true, a new merchant is seeded
    /// `authorized = 0` (must complete KYC); when false, `authorized = 1`.
    pub register_authorized: bool,
    /// The site encryption key (legacy `C('DATA_AUTH_KEY')`) used only to
    /// derive the registration email-activation token
    /// (`generateUser` L796, `spec/05` §3.4). A fresh rewrite mints its own
    /// activation codes, so no byte-compat with the legacy PHP value is
    /// required; operators MUST set `PAYMENT_DATA_AUTH_KEY` in production and
    /// the placeholder default below is a dev-only stand-in.
    pub data_auth_key: String,
    /// `websiteconfig.df_api` — the platform-wide代付 API kill switch
    /// (`Dfpay::add` answers 代付API未开启！ while it is off). The wide
    /// `websiteconfig` table stays unmodelled, so the switch rides static
    /// config (yaml + env), defaulting `false` (the DDL default `'0'`).
    pub df_api: bool,
}

#[derive(Debug, Deserialize)]
struct DataFile {
    data: Option<DataSection>,
}

#[derive(Debug, Default, Deserialize)]
struct DataSection {
    database: Option<DatabaseSection>,
    redis: Option<RedisSection>,
    gateway: Option<GatewaySection>,
    planning: Option<PlanningSection>,
}

#[derive(Debug, Default, Deserialize)]
struct DatabaseSection {
    #[serde(default)]
    #[allow(dead_code)] // taken verbatim; the driver is fixed to postgres here
    driver: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    migrate: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RedisSection {
    #[serde(default)]
    addr: String,
    #[serde(default)]
    password: String,
}

#[derive(Debug, Default, Deserialize)]
struct GatewaySection {
    #[serde(default)]
    site_url: String,
    #[serde(default = "default_upload_root")]
    upload_root: String,
    #[serde(default)]
    register_invitecode: bool,
    #[serde(default)]
    register_need_activate: bool,
    #[serde(default)]
    register_authorized: bool,
    #[serde(default = "default_data_auth_key")]
    data_auth_key: String,
    #[serde(default)]
    df_api: bool,
}

/// The fallback site key when neither yaml nor env supplies one. A dev-only
/// placeholder: production must set `PAYMENT_DATA_AUTH_KEY`.
fn default_data_auth_key() -> String {
    "payment-dev-data-auth-key".to_string()
}

/// The fallback uploads root when neither the yaml nor the env sets one.
fn default_upload_root() -> String {
    "./Uploads".to_string()
}

#[derive(Debug, Default, Deserialize)]
struct PlanningSection {
    #[serde(default)]
    thaw_allow_start: Option<i32>,
    #[serde(default)]
    thaw_allow_end: Option<i32>,
}

impl Config {
    /// Loads the vendored yaml with env overrides applied.
    pub fn load() -> Result<Self, String> {
        let data: DataFile =
            serde_yaml::from_str(DATA_YAML).map_err(|e| format!("parse data.yaml: {e}"))?;

        let section = data.data.unwrap_or_default();
        let database = section.database.unwrap_or_default();
        let redis = section.redis.unwrap_or_default();
        let gateway = section.gateway.unwrap_or_default();
        let planning = section.planning.unwrap_or_default();

        Ok(Config {
            database_source: env_string("RUSHWIND_DATABASE_SOURCE", database.source),
            database_migrate: env_bool("RUSHWIND_DATABASE_MIGRATE", database.migrate),
            redis_addr: env_string("RUSHWIND_REDIS_ADDR", redis.addr),
            redis_password: env_string("RUSHWIND_REDIS_PASSWORD", redis.password),
            site_url: env_string("PAYMENT_SITE_URL", gateway.site_url),
            upload_root: env_string("PAYMENT_UPLOADS_ROOT", gateway.upload_root),
            // Legacy defaults: PLANNING allowstart=1 / allowend=5 (§6.2).
            thaw_allow_start: env_i32("PAYMENT_THAW_ALLOW_START", planning.thaw_allow_start, 1),
            thaw_allow_end: env_i32("PAYMENT_THAW_ALLOW_END", planning.thaw_allow_end, 5),
            // websiteconfig registration switches, config-sourced (§2.3/§3),
            // defaulting off (the DDL defaults).
            register_invitecode: env_bool(
                "PAYMENT_REGISTER_INVITECODE",
                gateway.register_invitecode,
            ),
            register_need_activate: env_bool(
                "PAYMENT_REGISTER_NEED_ACTIVATE",
                gateway.register_need_activate,
            ),
            register_authorized: env_bool(
                "PAYMENT_REGISTER_AUTHORIZED",
                gateway.register_authorized,
            ),
            data_auth_key: env_string("PAYMENT_DATA_AUTH_KEY", gateway.data_auth_key),
            // websiteconfig.df_api — the payout-API master switch, off by
            // default (the DDL default '0').
            df_api: env_bool("PAYMENT_DF_API", gateway.df_api),
        })
    }
}

fn env_string(name: &str, yaml: String) -> String {
    std::env::var(name).unwrap_or(yaml)
}

fn env_bool(name: &str, yaml: bool) -> bool {
    std::env::var(name)
        .map(|v| v == "true" || v == "1")
        .unwrap_or(yaml)
}

fn env_i32(name: &str, yaml: Option<i32>, fallback: i32) -> i32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .or(yaml)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::{env_bool, env_i32};
    use std::env::{remove_var, set_var};

    const BOOL: &str = "PAYMENT_TEST_ENV_BOOL";
    const NUM: &str = "PAYMENT_TEST_ENV_NUM";

    #[test]
    fn env_bool_takes_true_or_1_else_yaml() {
        set_var(BOOL, "true");
        assert!(env_bool(BOOL, false));
        set_var(BOOL, "1");
        assert!(env_bool(BOOL, false));
        set_var(BOOL, "no");
        assert!(!env_bool(BOOL, false));
        remove_var(BOOL);
        assert!(!env_bool(BOOL, false));
        assert!(env_bool(BOOL, true));
    }

    #[test]
    fn env_i32_prefers_env_then_yaml_then_fallback() {
        set_var(NUM, "7");
        assert_eq!(env_i32(NUM, Some(3), 1), 7);
        remove_var(NUM);
        assert_eq!(env_i32(NUM, Some(3), 1), 3);
        assert_eq!(env_i32(NUM, None, 1), 1);
    }
}
