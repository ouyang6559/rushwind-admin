//! Config loading — parses the embedded yaml defaults
//! (`assets/data.yaml`, `assets/auth.yaml`, `assets/oss.yaml`,
//! `assets/server.yaml`, compiled into the binary) with the same env
//! overrides honors (`RUSHWIND_AUTH_JWT_*`,
//! plus `RUSHWIND_DATABASE_SOURCE` / `RUSHWIND_REDIS_ADDR` / `RUSHWIND_REDIS_PASSWORD`
//! for out-of-container runs).

use serde::Deserialize;

const DATA_YAML: &str = include_str!("../assets/data.yaml");
const AUTH_YAML: &str = include_str!("../assets/auth.yaml");
const OSS_YAML: &str = include_str!("../assets/oss.yaml");
const SERVER_YAML: &str = include_str!("../assets/server.yaml");

#[derive(Debug, Clone)]
pub struct Config {
    /// The REST listener address (`server.rest.addr`, ":7788" form).
    pub rest_addr: String,
    pub rest_timeout_secs: u64,
    pub enable_swagger: bool,
    pub enable_redoc: bool,
    pub cors_allow_credentials: bool,
    pub cors_headers: Vec<String>,
    pub cors_methods: Vec<String>,
    pub cors_origins: Vec<String>,
    pub sse_addr: String,
    #[allow(dead_code)] // the events path lands with the SSE handler query surface
    pub sse_path: String,
    pub database_source: String,
    #[allow(dead_code)] // golden-DDL pipeline switch (storage phase)
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
    access_token_expires: String,
    #[serde(default)]
    refresh_token_expires: String,
}

#[derive(Debug, Deserialize)]
struct ServerFile {
    server: Option<ServerSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ServerSection {
    rest: Option<RestSection>,
    sse: Option<SseSection>,
}

#[derive(Debug, Default, Deserialize)]
struct RestSection {
    #[serde(default)]
    addr: String,
    #[serde(default)]
    timeout: String,
    #[serde(default)]
    enable_swagger: bool,
    #[serde(default)]
    enable_redoc: bool,
    #[serde(default)]
    cors: Option<CorsSection>,
}

#[derive(Debug, Deserialize, Default)]
struct CorsSection {
    #[serde(default)]
    allow_credentials: bool,
    #[serde(default)]
    headers: Vec<String>,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    origins: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SseSection {
    #[serde(default)]
    addr: String,
    #[serde(default)]
    path: String,
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

/// Parses a Go `time.ParseDuration` subset: `300s`, `1.5h`, `0.4s`, `90m`.
fn parse_go_duration(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (value, unit) = text.split_at(text.find(|c: char| c.is_alphabetic())?);
    let value: f64 = value.parse().ok()?;
    let secs = match unit {
        "ns" => value / 1e9,
        "us" | "µs" => value / 1e6,
        "ms" => value / 1e3,
        "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        _ => return None,
    };
    Some(secs)
}

impl Config {
    /// Loads the vendored yaml files with env overrides applied.
    pub fn load() -> Result<Self, String> {
        let data: DataFile =
            serde_yaml::from_str(DATA_YAML).map_err(|e| format!("parse data.yaml: {e}"))?;
        let auth: AuthFile =
            serde_yaml::from_str(AUTH_YAML).map_err(|e| format!("parse auth.yaml: {e}"))?;
        let oss: OssFile = serde_yaml::from_str(OSS_YAML).unwrap_or(OssFile { oss: None });
        let server: ServerFile =
            serde_yaml::from_str(SERVER_YAML).unwrap_or(ServerFile { server: None });
        let server_section = server.server.unwrap_or_default();
        let rest_section = server_section.rest.unwrap_or(RestSection {
            addr: ":7788".into(),
            timeout: String::new(),
            enable_swagger: false,
            enable_redoc: false,
            cors: None,
        });
        let sse_section = server_section.sse.unwrap_or(SseSection {
            addr: ":7789".into(),
            path: "/events".into(),
        });

        let data_section = data.data.unwrap_or_default();
        let database = data_section.database.unwrap_or_default();
        let redis_section = data_section.redis.unwrap_or_default();
        let jwt = auth.authn.and_then(|a| a.jwt).unwrap_or(JwtSection {
            method: "HS256".into(),
            key: String::new(),
            private_key: String::new(),
            public_key: String::new(),
            access_token_expires: String::new(),
            refresh_token_expires: String::new(),
        });

        // The code defaults : access 15 min,
        // refresh 7 days.
        let access_token_expires_secs = std::env::var("RUSHWIND_ACCESS_TOKEN_EXPIRES_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| parse_go_duration(&jwt.access_token_expires).map(|s| s as i64))
            .unwrap_or(900);
        let refresh_token_expires_secs = std::env::var("RUSHWIND_REFRESH_TOKEN_EXPIRES_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| parse_go_duration(&jwt.refresh_token_expires).map(|s| s as i64))
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
            rest_addr: rest_section.addr.clone(),
            rest_timeout_secs: parse_go_duration(&rest_section.timeout)
                .map(|s| s as u64)
                .unwrap_or(10),
            enable_swagger: rest_section.enable_swagger,
            enable_redoc: rest_section.enable_redoc,
            cors_allow_credentials: rest_section
                .cors
                .as_ref()
                .map(|c| c.allow_credentials)
                .unwrap_or(false),
            cors_headers: rest_section
                .cors
                .as_ref()
                .map(|c| c.headers.clone())
                .unwrap_or_default(),
            cors_methods: rest_section
                .cors
                .as_ref()
                .map(|c| c.methods.clone())
                .unwrap_or_default(),
            cors_origins: rest_section
                .cors
                .as_ref()
                .map(|c| c.origins.clone())
                .unwrap_or_default(),
            sse_addr: sse_section.addr.clone(),
            sse_path: sse_section.path.clone(),
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

#[cfg(test)]
mod tests {
    use super::parse_go_duration;

    #[test]
    fn parses_plain_units() {
        assert_eq!(parse_go_duration("300s"), Some(300.0));
        assert_eq!(parse_go_duration("90m"), Some(5400.0));
        assert_eq!(parse_go_duration("1.5h"), Some(5400.0));
        assert_eq!(parse_go_duration("0.4s"), Some(0.4));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("abc"), None);
        assert_eq!(parse_go_duration("12q"), None);
    }
}
