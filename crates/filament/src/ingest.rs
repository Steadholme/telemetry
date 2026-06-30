//! Liberal span ingest normalization.
//!
//! Filament accepts spans from heterogeneous collectors, so the parser is deliberately permissive.
//! It understands TWO shapes and normalizes both into [`Span`]:
//!
//! 1. OTLP/HTTP-ish JSON: `{ "resourceSpans": [ { "resource": {...}, "scopeSpans": [ { "spans":
//!    [ { "traceId", "spanId", "parentSpanId", "name", "startTimeUnixNano", "endTimeUnixNano",
//!    "status": {"code"}, "attributes": [...] } ] } ] } ] }`. `service.name` is read from the
//!    resource attributes; nano timestamps are divided to micros.
//! 2. A flat array (or `{ "spans": [...] }`) of `{ trace_id, span_id, parent_id?, name, service,
//!    start_us, end_us, status?, attributes? }`.
//!
//! Anything unrecognized in a span is ignored; a span missing a `trace_id`/`span_id` is skipped
//! rather than failing the whole batch. The raw attributes (whichever shape) are stringified and
//! stored opaquely (bounded length) — they are never re-parsed for rendering.

use serde_json::Value;

use crate::store::Span;

/// Cap on the stored `attrs` blob, so a pathological attribute set cannot bloat a row.
const ATTRS_MAX: usize = 4096;

/// Normalize a parsed JSON ingest body into spans. Liberal: unknown shapes yield an empty Vec, and
/// individually malformed spans are skipped.
pub fn parse_spans(body: &Value) -> Vec<Span> {
    // OTLP/HTTP: { resourceSpans: [...] }
    if let Some(resource_spans) = body.get("resourceSpans").and_then(Value::as_array) {
        return parse_otlp(resource_spans);
    }
    // Flat array of spans, or { spans: [...] }.
    if let Some(arr) = body.as_array() {
        return arr.iter().filter_map(|v| extract_span(v, "")).collect();
    }
    if let Some(arr) = body.get("spans").and_then(Value::as_array) {
        return arr.iter().filter_map(|v| extract_span(v, "")).collect();
    }
    Vec::new()
}

/// Walk OTLP `resourceSpans[].scopeSpans[].spans[]`, carrying the resource `service.name` down.
fn parse_otlp(resource_spans: &[Value]) -> Vec<Span> {
    let mut out = Vec::new();
    for rs in resource_spans {
        let service = rs
            .get("resource")
            .and_then(|r| r.get("attributes"))
            .and_then(Value::as_array)
            .and_then(|a| otlp_service_name(a))
            .unwrap_or_default();

        // Accept both `scopeSpans` (current) and `instrumentationLibrarySpans` (legacy).
        let scope_arrays = rs
            .get("scopeSpans")
            .and_then(Value::as_array)
            .or_else(|| rs.get("instrumentationLibrarySpans").and_then(Value::as_array));
        let Some(scopes) = scope_arrays else { continue };
        for scope in scopes {
            let Some(spans) = scope.get("spans").and_then(Value::as_array) else {
                continue;
            };
            for sp in spans {
                if let Some(span) = extract_span(sp, &service) {
                    out.push(span);
                }
            }
        }
    }
    out
}

/// Read `service.name` out of an OTLP resource attribute list.
fn otlp_service_name(attrs: &[Value]) -> Option<String> {
    for a in attrs {
        if a.get("key").and_then(Value::as_str) == Some("service.name") {
            if let Some(v) = a
                .get("value")
                .and_then(|v| v.get("stringValue"))
                .and_then(Value::as_str)
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Extract one span from either shape. `default_service` fills `service` when the span itself
/// carries none (the OTLP resource service). Returns `None` when `trace_id`/`span_id` are absent.
fn extract_span(v: &Value, default_service: &str) -> Option<Span> {
    let trace_id = str_field(v, &["trace_id", "traceId"])?;
    let span_id = str_field(v, &["span_id", "spanId"])?;
    if trace_id.is_empty() || span_id.is_empty() {
        return None;
    }
    let parent_id = str_field(v, &["parent_id", "parentSpanId", "parentId"]).unwrap_or_default();
    let name = str_field(v, &["name"]).filter(|s| !s.is_empty()).unwrap_or_else(|| "span".to_string());

    let service = str_field(v, &["service", "serviceName"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_service.to_string());

    // Micros first (flat), else nanos (OTLP) divided down.
    let start_us = int_field(v, &["start_us", "startUs"])
        .or_else(|| int_field(v, &["startTimeUnixNano", "start_time_unix_nano"]).map(nanos_to_us))
        .unwrap_or(0);
    let end_us = int_field(v, &["end_us", "endUs"])
        .or_else(|| int_field(v, &["endTimeUnixNano", "end_time_unix_nano"]).map(nanos_to_us))
        .unwrap_or(start_us);

    let status = status_of(v);
    let attrs = attrs_of(v);

    Some(Span {
        span_id,
        trace_id,
        parent_id,
        name,
        service,
        start_us,
        end_us,
        status,
        attrs,
    })
}

/// nanoseconds -> microseconds.
fn nanos_to_us(nanos: i64) -> i64 {
    nanos / 1000
}

/// Resolve the span status to `ok` | `error`. Accepts a flat string (`status`/`statusCode`,
/// where anything matching error/2 is an error) or an OTLP `status.code` (2 = ERROR).
fn status_of(v: &Value) -> String {
    // Flat string status.
    if let Some(s) = str_field(v, &["status", "statusCode"]) {
        let lc = s.to_ascii_lowercase();
        if lc.contains("error") || lc == "2" || lc == "err" {
            return "error".to_string();
        }
        if !lc.is_empty() {
            return "ok".to_string();
        }
    }
    // OTLP status object: { code: 2 } (ERROR) or a string code.
    if let Some(code) = v.get("status").and_then(|st| st.get("code")) {
        let is_err = code.as_i64() == Some(2)
            || code
                .as_str()
                .map(|c| c.to_ascii_uppercase().contains("ERROR"))
                .unwrap_or(false);
        if is_err {
            return "error".to_string();
        }
    }
    "ok".to_string()
}

/// Stringify whatever attributes the span carries (flat object/array or OTLP attribute list),
/// bounded to [`ATTRS_MAX`] chars. Stored opaquely; never re-parsed.
fn attrs_of(v: &Value) -> String {
    let raw = v
        .get("attributes")
        .or_else(|| v.get("attrs"))
        .or_else(|| v.get("tags"));
    let Some(raw) = raw else { return String::new() };
    let s = raw.to_string();
    if s == "null" || s == "{}" || s == "[]" {
        return String::new();
    }
    truncate_chars(&s, ATTRS_MAX)
}

/// Read a string from the first present key. Accepts a JSON string OR a number (coerced to its
/// decimal string), so a numeric `traceId` still resolves.
fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        match v.get(*k) {
            Some(Value::String(s)) => return Some(s.clone()),
            Some(Value::Number(n)) => return Some(n.to_string()),
            _ => {}
        }
    }
    None
}

/// Read an integer from the first present key. Accepts a JSON number (int or float, truncated) OR
/// a numeric string (OTLP often encodes 64-bit nanos as strings).
fn int_field(v: &Value, keys: &[&str]) -> Option<i64> {
    for k in keys {
        match v.get(*k) {
            Some(Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    return Some(i);
                }
                if let Some(f) = n.as_f64() {
                    return Some(f as i64);
                }
            }
            Some(Value::String(s)) => {
                let t = s.trim();
                if let Ok(i) = t.parse::<i64>() {
                    return Some(i);
                }
                if let Ok(f) = t.parse::<f64>() {
                    return Some(f as i64);
                }
            }
            _ => {}
        }
    }
    None
}

/// Truncate to at most `n` chars on a char boundary.
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_flat_array() {
        let body = json!([
            {"trace_id": "t1", "span_id": "a", "name": "GET /", "service": "gw",
             "start_us": 1000, "end_us": 5000, "status": "ok",
             "attributes": {"http.method": "GET"}},
            {"trace_id": "t1", "span_id": "b", "parent_id": "a", "name": "query",
             "service": "db", "start_us": 1500, "end_us": 2500, "status": "error"}
        ]);
        let spans = parse_spans(&body);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].span_id, "a");
        assert_eq!(spans[0].service, "gw");
        assert!(spans[0].attrs.contains("http.method"));
        assert!(spans[1].is_error());
        assert_eq!(spans[1].parent_id, "a");
    }

    #[test]
    fn parses_flat_spans_object() {
        let body = json!({"spans": [
            {"trace_id": "t9", "span_id": "z", "name": "x", "start_us": 0, "end_us": 1}
        ]});
        let spans = parse_spans(&body);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].trace_id, "t9");
        assert_eq!(spans[0].status, "ok");
    }

    #[test]
    fn parses_otlp_with_service_and_nanos() {
        let body = json!({
            "resourceSpans": [{
                "resource": {"attributes": [
                    {"key": "service.name", "value": {"stringValue": "checkout"}}
                ]},
                "scopeSpans": [{
                    "spans": [{
                        "traceId": "abc", "spanId": "s1", "parentSpanId": "",
                        "name": "POST /pay",
                        "startTimeUnixNano": "1700000000000000000",
                        "endTimeUnixNano": "1700000000005000000",
                        "status": {"code": 2},
                        "attributes": [{"key": "http.status", "value": {"intValue": "500"}}]
                    }]
                }]
            }]
        });
        let spans = parse_spans(&body);
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!(s.trace_id, "abc");
        assert_eq!(s.span_id, "s1");
        assert_eq!(s.service, "checkout");
        // nanos -> micros: 5_000_000 ns span = 5_000 us.
        assert_eq!(s.end_us - s.start_us, 5000);
        assert!(s.is_error(), "OTLP status.code 2 -> error");
        assert!(s.attrs.contains("http.status"));
    }

    #[test]
    fn skips_spans_without_ids_and_unknown_shapes() {
        let body = json!([
            {"span_id": "no-trace", "name": "x"},
            {"trace_id": "t", "name": "no-span"}
        ]);
        assert!(parse_spans(&body).is_empty());
        assert!(parse_spans(&json!({"unexpected": true})).is_empty());
        assert!(parse_spans(&json!("a string")).is_empty());
    }
}
