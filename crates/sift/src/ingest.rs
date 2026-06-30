//! The ingest pipeline: turn a raw frame (syslog line, JSON log object, OTLP record, plain text)
//! into a stored [`LogEntry`] + a clustered [`Template`], and emit `sift.unknown_template` the
//! first time a template shape appears.
//!
//! All paths funnel through [`record`], which computes the template id, upserts the template
//! (bumping its count), inserts the log row, and — only when the template was brand new — fires
//! the audit event. Store errors are logged, never propagated to the sender: ingest is
//! best-effort and a single bad row never fails a batch.

use serde_json::Value;

use crate::audit::AuditEvent;
use crate::store::{LogEntry, Template};
use crate::{new_log_id, now_secs, template, AppState};

/// Clamp a free-form severity label to the known syslog set; anything else becomes `info`.
fn normalize_severity(raw: &str) -> String {
    let s = raw.trim().to_lowercase();
    match s.as_str() {
        "emerg" | "emergency" | "panic" => "emerg",
        "alert" => "alert",
        "crit" | "critical" | "fatal" => "crit",
        "err" | "error" => "err",
        "warning" | "warn" => "warning",
        "notice" => "notice",
        "info" | "information" | "informational" => "info",
        "debug" | "trace" => "debug",
        _ => "info",
    }
    .to_string()
}

/// Truncate to at most `n` chars on a char boundary (keeps the stored `sample` bounded).
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// Core ingest: cluster + persist one log line, emitting the new-template audit when warranted.
///
/// `actor` labels the source on the audit event (a sender IP for syslog, `"ingest"` for the HTTP
/// path). Never returns an error — failures are logged and swallowed so a batch keeps flowing.
pub async fn record(
    state: &AppState,
    host: String,
    app: String,
    severity: String,
    message: String,
    ts: i64,
    actor: &str,
) {
    let message = message.trim().to_string();
    if message.is_empty() {
        return;
    }
    let severity = normalize_severity(&severity);

    // --- template clustering ------------------------------------------------
    let pattern = template::normalize(&message);
    let tid = template::template_id(&pattern);
    let tmpl = Template {
        id: tid.clone(),
        pattern,
        sample: truncate_chars(&message, 512),
        count: 1,
        first_seen: ts,
        last_seen: ts,
    };
    let is_new = match state.store.upsert_template(&tmpl).await {
        Ok(new) => new,
        Err(e) => {
            tracing::warn!(error = %e, "template upsert failed");
            false
        }
    };

    // --- the log row --------------------------------------------------------
    let log = LogEntry {
        id: new_log_id(),
        ts,
        host,
        app: app.clone(),
        severity,
        message,
        template_id: tid.clone(),
    };
    if let Err(e) = state.store.insert_log(&log).await {
        tracing::warn!(error = %e, "log insert failed");
    }

    // --- audit a brand-new template -----------------------------------------
    if is_new {
        let detail = if app.is_empty() {
            "new message template".to_string()
        } else {
            format!("new message template from {app}")
        };
        state
            .audit
            .emit(AuditEvent::notice("sift.unknown_template", actor, &tid, &detail));
    }
}

/// Ingest one raw syslog/plain line (the UDP/TCP listeners + the ndjson text fallback). `peer` is
/// the sender's IP, used as the host when the frame carries none.
pub async fn ingest_syslog_line(state: &AppState, line: &str, peer: &str) {
    let parsed = crate::syslog::parse(line);
    let host = if parsed.host.is_empty() {
        peer.to_string()
    } else {
        parsed.host
    };
    record(state, host, parsed.app, parsed.severity, parsed.message, now_secs(), peer).await;
}

/// Ingest a `POST /ingest` body. Liberal about the shape:
/// - a JSON object -> one log (OTLP `{resourceLogs...}` is detected and walked),
/// - a JSON array -> many logs,
/// - otherwise newline-delimited text, each line parsed as JSON-log-or-syslog.
///
/// Returns the number of log rows accepted.
pub async fn ingest_body(state: &AppState, body: &[u8]) -> Result<usize, crate::error::AppError> {
    use crate::error::AppError;

    let text = std::str::from_utf8(body)
        .map_err(|_| AppError::InvalidRequest("body is not valid UTF-8".to_string()))?;
    let trimmed = text.trim_start();

    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| AppError::InvalidRequest(format!("invalid JSON: {e}")))?;
        match value {
            Value::Array(items) => {
                let mut n = 0;
                for item in &items {
                    n += record_from_json(state, item, "ingest").await;
                }
                Ok(n)
            }
            Value::Object(ref map)
                if map.contains_key("resourceLogs") || map.contains_key("resource_logs") =>
            {
                Ok(ingest_otlp(state, &value).await)
            }
            obj @ Value::Object(_) => Ok(record_from_json(state, &obj, "ingest").await),
            _ => Err(AppError::InvalidRequest("unsupported JSON log shape".to_string())),
        }
    } else {
        // Newline-delimited text. Each line may itself be a JSON log object (ndjson) or syslog.
        let mut n = 0;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(Value::Object(_)) = serde_json::from_str::<Value>(line.trim()) {
                let v: Value = serde_json::from_str(line.trim()).unwrap();
                n += record_from_json(state, &v, "ingest").await;
            } else {
                ingest_syslog_line(state, line, "ingest").await;
                n += 1;
            }
        }
        Ok(n)
    }
}

/// Record one log from a JSON object, pulling fields from a generous set of aliases. A non-object
/// value (e.g. a bare string in an array) is treated as a raw line. Returns 1 if a row was taken.
async fn record_from_json(state: &AppState, v: &Value, actor: &str) -> usize {
    if let Some(s) = v.as_str() {
        ingest_syslog_line(state, s, actor).await;
        return 1;
    }
    let Some(obj) = v.as_object() else { return 0 };

    let host = first_str(obj, &["host", "hostname", "source", "source_host"]).unwrap_or_default();
    let app = first_str(
        obj,
        &["app", "application", "program", "tag", "service", "logger", "unit"],
    )
    .unwrap_or_default();
    let severity =
        first_str(obj, &["severity", "level", "severitytext", "loglevel", "lvl"]).unwrap_or_default();
    let message =
        first_str(obj, &["message", "msg", "body", "log", "text", "@message"]).unwrap_or_default();
    if message.trim().is_empty() {
        return 0;
    }
    let ts = first_ts(obj).unwrap_or_else(now_secs);

    record(state, host, app, severity, message, ts, actor).await;
    1
}

/// Loosely walk an OTLP-ish `{resourceLogs:[{scopeLogs:[{logRecords:[...]}]}]}` payload, accepting
/// either camelCase or snake_case keys. Returns the number of records ingested.
async fn ingest_otlp(state: &AppState, root: &Value) -> usize {
    let mut n = 0;
    let resource_logs = root
        .get("resourceLogs")
        .or_else(|| root.get("resource_logs"))
        .and_then(Value::as_array);
    let Some(resource_logs) = resource_logs else { return 0 };

    for rl in resource_logs {
        // resource-level service.name, if present, seeds the app.
        let res_app = rl
            .get("resource")
            .and_then(|r| r.get("attributes"))
            .and_then(Value::as_array)
            .and_then(|attrs| find_attr(attrs, "service.name"))
            .unwrap_or_default();

        let scope_logs = rl
            .get("scopeLogs")
            .or_else(|| rl.get("scope_logs"))
            .and_then(Value::as_array);
        let Some(scope_logs) = scope_logs else { continue };

        for sl in scope_logs {
            let records = sl
                .get("logRecords")
                .or_else(|| sl.get("log_records"))
                .and_then(Value::as_array);
            let Some(records) = records else { continue };

            for rec in records {
                let message = otlp_body(rec);
                if message.trim().is_empty() {
                    continue;
                }
                let severity = rec
                    .get("severityText")
                    .or_else(|| rec.get("severity_text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let attrs = rec.get("attributes").and_then(Value::as_array);
                let host = attrs
                    .and_then(|a| find_attr(a, "host.name").or_else(|| find_attr(a, "host")))
                    .unwrap_or_default();
                let app = attrs
                    .and_then(|a| find_attr(a, "service.name"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| res_app.clone());

                record(state, host, app, severity, message, now_secs(), "ingest").await;
                n += 1;
            }
        }
    }
    n
}

/// Extract an OTLP log record's body, which may be `{stringValue: "..."}` or a bare string.
fn otlp_body(rec: &Value) -> String {
    let Some(body) = rec.get("body") else { return String::new() };
    if let Some(s) = body.as_str() {
        return s.to_string();
    }
    body.get("stringValue")
        .or_else(|| body.get("string_value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Find an OTLP attribute by key in an `attributes: [{key, value:{stringValue}}]` array.
fn find_attr(attrs: &[Value], key: &str) -> Option<String> {
    for a in attrs {
        if a.get("key").and_then(Value::as_str) == Some(key) {
            let v = a.get("value")?;
            if let Some(s) = v.as_str() {
                return Some(s.to_string());
            }
            if let Some(s) = v
                .get("stringValue")
                .or_else(|| v.get("string_value"))
                .and_then(Value::as_str)
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// First present, non-empty string field among `keys` (case-insensitive on the JSON key).
fn first_str(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for (k, v) in obj {
        let lk = k.to_lowercase();
        if keys.contains(&lk.as_str()) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            } else if v.is_number() || v.is_boolean() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Best-effort timestamp (epoch seconds) from a JSON log's `ts`/`timestamp`/`time` field. Numbers
/// are interpreted by magnitude (seconds / millis / nanos); strings that are pure numbers are
/// treated likewise; anything else (e.g. an RFC3339 string) yields `None` so the caller uses now.
fn first_ts(obj: &serde_json::Map<String, Value>) -> Option<i64> {
    const KEYS: &[&str] = &["ts", "timestamp", "time", "@timestamp", "timeunixnano", "epoch"];
    for (k, v) in obj {
        let lk = k.to_lowercase();
        if !KEYS.contains(&lk.as_str()) {
            continue;
        }
        let n = if let Some(n) = v.as_i64() {
            n
        } else if let Some(s) = v.as_str() {
            match s.parse::<i64>() {
                Ok(n) => n,
                Err(_) => continue,
            }
        } else {
            continue;
        };
        return Some(scale_to_secs(n));
    }
    None
}

/// Normalize an epoch value to seconds based on its magnitude (s / ms / us / ns).
fn scale_to_secs(n: i64) -> i64 {
    let a = n.unsigned_abs();
    if a >= 1_000_000_000_000_000_000 {
        n / 1_000_000_000 // nanoseconds
    } else if a >= 1_000_000_000_000_000 {
        n / 1_000_000 // microseconds
    } else if a >= 1_000_000_000_000 {
        n / 1_000 // milliseconds
    } else {
        n // seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_dev_state;

    #[tokio::test]
    async fn ingests_single_json_object() {
        let state = build_dev_state();
        let body = br#"{"host":"h1","app":"web","level":"error","message":"db down 5 times"}"#;
        let n = ingest_body(&state, body).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(state.store.count_logs().await.unwrap(), 1);
        assert_eq!(state.store.count_templates().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn ingests_json_array_and_clusters() {
        let state = build_dev_state();
        let body = br#"[
            {"app":"web","message":"request 1 took 10ms"},
            {"app":"web","message":"request 2 took 99ms"},
            {"app":"web","message":"shutting down"}
        ]"#;
        let n = ingest_body(&state, body).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(state.store.count_logs().await.unwrap(), 3);
        // The two "request N took Mms" lines cluster to one template; "shutting down" is another.
        assert_eq!(state.store.count_templates().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn ingests_ndjson_and_syslog_text() {
        let state = build_dev_state();
        let body = b"<34>Oct 11 22:14:15 host su: login failed\nplain text line\n{\"message\":\"json line\"}";
        let n = ingest_body(&state, body).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(state.store.count_logs().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn ingests_otlp_loose() {
        let state = build_dev_state();
        let body = br#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"api"}}]},
            "scopeLogs":[{"logRecords":[
                {"body":{"stringValue":"handled request 7"},"severityText":"INFO"},
                {"body":"raw body string"}
            ]}]}]}"#;
        let n = ingest_body(&state, body).await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(state.store.count_logs().await.unwrap(), 2);
    }
}
