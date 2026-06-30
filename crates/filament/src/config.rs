//! Server configuration, env-driven with working dev defaults.
//!
//! Every value keeps its dev default when the corresponding env var is unset/empty, so the
//! in-memory dev path boots with NO configuration and NO database — exactly like
//! watchtower/relay/sanctum. Production overrides each via the environment.

/// Default listen address (all interfaces, internal-only port 9230).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9230";

/// Dev/test default ingest bearer token. Production MUST override `FILAMENT_INGEST_TOKEN`.
pub const DEFAULT_INGEST_TOKEN: &str = "filament-dev-ingest-token-change-me";

/// Hard cap on how many recent spans the dashboard scans to reconstruct traces. Keeps an
/// unbounded grouping bounded; a real retention/rollup is a later concern, not a hypothetical.
pub const SPAN_SCAN_LIMIT: usize = 20_000;

/// Hard cap on how many traces the index renders / `/api/traces` returns.
pub const TRACE_LIST_LIMIT: usize = 200;

/// Hard cap on how many spans a single trace's waterfall renders (a pathological trace cannot
/// blow up the page).
pub const TRACE_SPAN_LIMIT: usize = 2_000;

/// Maximum parent/child indentation depth walked when laying out the waterfall. Bounds the
/// tree walk so a cyclic/self-referential `parent_id` can never loop.
pub const MAX_TREE_DEPTH: usize = 64;

/// Default error-trace audit sample rate: emit a `filament.trace.error` for 1-in-N error traces
/// (deterministic by `trace_id`, so the same trace samples consistently). `1` = emit every error
/// trace; raise `FILAMENT_ERROR_SAMPLE_N` in production to thin a noisy estate.
pub const DEFAULT_ERROR_SAMPLE_N: u64 = 1;

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// Bearer token guarding `POST /ingest` (`FILAMENT_INGEST_TOKEN`). Internal-only; this path
    /// is NOT gateway-routed, so Filament authenticates it itself.
    pub ingest_token: String,
    /// 1-in-N deterministic sampler for the error-trace audit (`FILAMENT_ERROR_SAMPLE_N`).
    pub error_sample_n: u64,
}

impl Config {
    /// Default development configuration (in-memory, no database, no persistence).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            ingest_token: DEFAULT_INGEST_TOKEN.to_string(),
            error_sample_n: DEFAULT_ERROR_SAMPLE_N,
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        let mut config = Config::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            config.bind_addr = v;
        }
        if let Some(v) = env_nonempty("FILAMENT_INGEST_TOKEN") {
            config.ingest_token = v;
        }
        if let Some(n) = env_nonempty("FILAMENT_ERROR_SAMPLE_N").and_then(|v| v.parse::<u64>().ok())
        {
            if n >= 1 {
                config.error_sample_n = n;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_defaults_are_sane() {
        let c = Config::dev();
        assert_eq!(c.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(c.ingest_token, DEFAULT_INGEST_TOKEN);
        assert_eq!(c.error_sample_n, 1);
    }
}
