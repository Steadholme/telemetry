//! Server configuration, env-driven with working dev defaults.
//!
//! Every value keeps its dev default when the corresponding env var is unset/empty, so the
//! in-memory dev path boots with NO configuration and NO database — exactly like
//! inkwell/sanctum. Production overrides each via the environment.

/// Default dashboard listen address (all interfaces, internal-only port 9100).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9100";

/// Default syslog listen address (UDP + TCP share this socket address, internal-only :5514).
pub const DEFAULT_SYSLOG_ADDR: &str = "0.0.0.0:5514";

/// Dev/test default ingest bearer token. Production MUST override `SIFT_INGEST_TOKEN`.
pub const DEFAULT_INGEST_TOKEN: &str = "sift-dev-ingest-token-change-me";

/// Hard cap on how many log rows a single search returns (keeps the table bounded).
pub const SEARCH_LIMIT: usize = 500;

/// Default number of rows the dashboard table renders when no explicit limit is given.
pub const DEFAULT_PAGE_LIMIT: usize = 100;

/// How many top templates the side panel shows.
pub const TEMPLATE_PANEL_LIMIT: usize = 25;

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Dashboard / HTTP listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// Syslog UDP+TCP listen address (`SIFT_SYSLOG_ADDR`).
    pub syslog_addr: String,
    /// Bearer token guarding `POST /ingest` (`SIFT_INGEST_TOKEN`). The `/ingest` path is
    /// internal-only (never routed through the public gateway), so it does its OWN auth.
    pub ingest_token: String,
}

impl Config {
    /// Default development configuration (in-memory friendly, no database).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            syslog_addr: DEFAULT_SYSLOG_ADDR.to_string(),
            ingest_token: DEFAULT_INGEST_TOKEN.to_string(),
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        let mut config = Config::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            config.bind_addr = v;
        }
        if let Some(v) = env_nonempty("SIFT_SYSLOG_ADDR") {
            config.syslog_addr = v;
        }
        if let Some(v) = env_nonempty("SIFT_INGEST_TOKEN") {
            config.ingest_token = v;
        }
        config
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::dev()
    }
}

/// Read an env var, returning `None` when unset OR empty (empty never clobbers a default).
pub fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}
