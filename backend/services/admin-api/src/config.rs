//! Config loading — parses the embedded yaml defaults
//! (`assets/data.yaml`, `assets/auth.yaml`, `assets/oss.yaml`,
//! compiled into the binary) with the same env overrides
//! (`RUSHWIND_AUTH_JWT_*`,
//! plus `RUSHWIND_DATABASE_SOURCE` / `RUSHWIND_REDIS_ADDR` / `RUSHWIND_REDIS_PASSWORD`
//! for out-of-container runs). The server assembly document
//! (`assets/server.yaml`) belongs to the lifecycle assembler.

use rushwind_bootstrap::DurationWire;
use serde::Deserialize;

const DATA_YAML: &str = include_str!("../assets/data.yaml");
const AUTH_YAML: &str = include_str!("../assets/auth.yaml");
const OSS_YAML: &str = include_str!("../assets/oss.yaml");

#[derive(Debug, Clone)]
pub struct Config {
    pub database_source: String,
    /// Startup gate: run pending schema migrations before seeding.
    pub database_migrate: bool,
    pub redis_addr: String,
    pub redis_password: String,
    /// RS256 private key PEM (minting) — from auth.yaml or the env override.
    pub jwt_private_key: Option<String>,
    /// RS256 public key PEM (verification) — falls back to the vendored
    /// jwt_public_key.pem when auth.yaml carries none.
    pub jwt_public_key: Option<String>,
    pub access_token_expires_secs: i64,
    pub refresh_token_expires_secs: i64,
    #[allow(dead_code)]
    pub oss: Option<OssConfig>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // object-storage section, wired with the file services
pub struct OssConfig {
    pub endpoint: String,
    pub upload_host: String,
    pub download_host: String,
    pub access_key: String,
    pub secret_key: String,
    pub use_ssl: bool,
}

#[derive(Debug, Deserialize)]
struct DataFile {
    data: Option<DataSection>,
}

#[derive(Debug, Default, Deserialize)]
struct DataSection {
    database: Option<DatabaseSection>,
    redis: Option<RedisSection>,
}

#[derive(Debug, Default, Deserialize)]
struct DatabaseSection {
    #[serde(default)]
    #[allow(dead_code)] // auth.yaml parity; the yaml carries the connection
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

#[derive(Debug, Deserialize)]
struct AuthFile {
    authn: Option<AuthnSection>,
}

#[derive(Debug, Deserialize)]
struct AuthnSection {
    jwt: Option<JwtSection>,
}

#[derive(Debug, Deserialize)]
struct JwtSection {
    #[serde(default)]
    #[allow(dead_code)] // taken verbatim; RS256 params come from the dedicated keys
    method: String,
    #[serde(default)]
    #[allow(dead_code)]
    key: String,
    #[serde(default)]
    private_key: String,
    #[serde(default)]
    public_key: String,
    #[serde(default)]
    access_token_expires: Option<DurationWire>,
    #[serde(default)]
    refresh_token_expires: Option<DurationWire>,
}

#[derive(Debug, Deserialize)]
struct OssFile {
    oss: Option<OssSection>,
}

#[derive(Debug, Deserialize)]
struct OssSection {
    #[serde(default)]
    minio: Option<MinioSection>,
}

#[derive(Debug, Default, Deserialize)]
struct MinioSection {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    upload: String,
    #[serde(default)]
    download: String,
    #[serde(default)]
    access_key: String,
    #[serde(default)]
    secret_key: String,
    #[serde(default)]
    use_ssl: bool,
}

impl Config {
    /// Loads the vendored yaml files with env overrides applied.
    pub fn load() -> Result<Self, String> {
        let data: DataFile =
            serde_yaml::from_str(DATA_YAML).map_err(|e| format!("parse data.yaml: {e}"))?;
        let auth: AuthFile =
            serde_yaml::from_str(AUTH_YAML).map_err(|e| format!("parse auth.yaml: {e}"))?;
        let oss: OssFile = serde_yaml::from_str(OSS_YAML).unwrap_or(OssFile { oss: None });

        let data_section = data.data.unwrap_or_default();
        let database = data_section.database.unwrap_or_default();
        let redis_section = data_section.redis.unwrap_or_default();
        let jwt = auth.authn.and_then(|a| a.jwt).unwrap_or(JwtSection {
            method: "HS256".into(),
            key: String::new(),
            private_key: String::new(),
            public_key: String::new(),
            access_token_expires: None,
            refresh_token_expires: None,
        });

        // The code defaults : access 15 min,
        // refresh 7 days.
        let access_token_expires_secs = std::env::var("RUSHWIND_ACCESS_TOKEN_EXPIRES_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                jwt.access_token_expires
                    .as_ref()
                    .map(|d| d.0.as_secs() as i64)
            })
            .unwrap_or(900);
        let refresh_token_expires_secs = std::env::var("RUSHWIND_REFRESH_TOKEN_EXPIRES_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                jwt.refresh_token_expires
                    .as_ref()
                    .map(|d| d.0.as_secs() as i64)
            })
            .unwrap_or(7 * 24 * 3600);

        let minio = oss.oss.and_then(|o| o.minio).unwrap_or_default();

        Ok(Config {
            database_source: std::env::var("RUSHWIND_DATABASE_SOURCE").unwrap_or(database.source),
            database_migrate: std::env::var("RUSHWIND_DATABASE_MIGRATE")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(database.migrate),
            redis_addr: std::env::var("RUSHWIND_REDIS_ADDR").unwrap_or(redis_section.addr),
            redis_password: std::env::var("RUSHWIND_REDIS_PASSWORD")
                .unwrap_or(redis_section.password),
            jwt_private_key: std::env::var("RUSHWIND_AUTH_JWT_PRIVATE_KEY")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| non_empty(jwt.private_key.clone())),
            jwt_public_key: std::env::var("RUSHWIND_AUTH_JWT_PUBLIC_KEY")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| non_empty(jwt.public_key.clone())),
            access_token_expires_secs,
            refresh_token_expires_secs,
            oss: (!minio.endpoint.is_empty()).then_some(OssConfig {
                endpoint: minio.endpoint,
                upload_host: minio.upload,
                download_host: minio.download,
                access_key: minio.access_key,
                secret_key: minio.secret_key,
                use_ssl: minio.use_ssl,
            }),
        })
    }
}

fn non_empty(v: String) -> Option<String> {
    (!v.is_empty()).then_some(v)
}
