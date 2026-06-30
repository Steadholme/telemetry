//! Syslog parsing (RFC 5424 + RFC 3164) and the UDP/TCP listeners.
//!
//! Both listeners bind the same internal address (`SIFT_SYSLOG_ADDR`, default `0.0.0.0:5514`) and
//! run concurrently with the HTTP server on the one tokio runtime (see [`crate::run`]). Parsing
//! is BEST-EFFORT and tolerant of junk: a line that matches no RFC shape still lands as a log row
//! with the whole line as its message (host falls back to the sender's IP, severity to `info`),
//! so nothing a sender emits is ever dropped on the floor.

use std::net::SocketAddr;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};

use crate::AppState;

/// The fields we extract from a syslog frame. The receive time is stamped by the ingest layer,
/// not parsed from the frame (arbitrary sender clocks/timestamps are brittle; receive time is
/// monotonic and good enough for aggregation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSyslog {
    pub host: String,
    pub app: String,
    pub severity: String,
    pub message: String,
}

/// Map a syslog PRI severity (PRI % 8) to a lowercase name.
pub fn severity_name(sev: u8) -> &'static str {
    match sev {
        0 => "emerg",
        1 => "alert",
        2 => "crit",
        3 => "err",
        4 => "warning",
        5 => "notice",
        6 => "info",
        7 => "debug",
        _ => "info",
    }
}

/// Parse one syslog line, best-effort. Never fails: unrecognized input becomes a message-only row.
pub fn parse(line: &str) -> ParsedSyslog {
    let line = line.trim();
    let mut severity = "info".to_string();
    let mut rest = line;

    // Optional `<PRI>` priority value.
    if let Some(stripped) = line.strip_prefix('<') {
        if let Some(gt) = stripped.find('>') {
            if let Ok(pri) = stripped[..gt].parse::<u16>() {
                severity = severity_name((pri % 8) as u8).to_string();
            }
            rest = &stripped[gt + 1..];
        }
    }

    // RFC 5424 begins with a numeric VERSION immediately after the PRI.
    if let Some((host, app, message)) = parse_5424(rest) {
        return ParsedSyslog { host, app, severity, message };
    }

    // Otherwise RFC 3164 / freeform.
    let (host, app, message) = parse_3164(rest);
    ParsedSyslog { host, app, severity, message }
}

/// RFC 5424: `VERSION SP TIMESTAMP SP HOSTNAME SP APP-NAME SP PROCID SP MSGID SP SD SP MSG`.
/// Returns `None` when `rest` is not 5424-shaped (no leading numeric version).
fn parse_5424(rest: &str) -> Option<(String, String, String)> {
    let (ver, after) = rest.split_once(' ')?;
    if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // TIMESTAMP HOSTNAME APP-NAME PROCID then the remainder (MSGID SD... MSG).
    let mut it = after.splitn(5, ' ');
    let _timestamp = it.next()?;
    let hostname = it.next()?;
    let appname = it.next()?;
    let _procid = it.next()?;
    let tail = it.next()?;
    // tail = MSGID SP (SD | '-') SP MSG
    let (_msgid, after_msgid) = tail.split_once(' ')?;
    let message = strip_structured_data(after_msgid);
    Some((nil_to_empty(hostname), nil_to_empty(appname), message))
}

/// Strip the leading STRUCTURED-DATA (`-` for nil, or one-or-more `[...]` groups) and return the
/// MSG. Honors `\]` escapes inside an SD param.
fn strip_structured_data(s: &str) -> String {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('-') {
        return r.trim_start().to_string();
    }
    if s.starts_with('[') {
        let bytes = s.as_bytes();
        let mut i = 0;
        // Skip consecutive balanced [...] groups.
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1; // consume '['
            while i < bytes.len() && bytes[i] != b']' {
                if bytes[i] == b'\\' {
                    i += 1; // skip the escaped char
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // consume ']'
            }
        }
        return s[i..].trim_start().to_string();
    }
    s.to_string()
}

/// RFC 3164: `TIMESTAMP(Mmm dd hh:mm:ss) SP HOSTNAME SP TAG[pid]: MSG`. Every part is optional in
/// the wild, so this degrades gracefully to a message-only row.
fn parse_3164(rest: &str) -> (String, String, String) {
    let mut work = rest.trim();
    let mut host = String::new();

    if let Some(after_ts) = strip_3164_timestamp(work) {
        work = after_ts.trim_start();
        // The hostname is the next whitespace-delimited token.
        if let Some((h, r)) = work.split_once(' ') {
            host = h.to_string();
            work = r;
        }
    }

    // `tag[pid]: message` — only treat the head as a tag when it is a single short token (so a
    // stray colon in freeform text is not mistaken for a tag separator).
    if let Some((tag, msg)) = work.split_once(": ") {
        let tag = tag.trim();
        if !tag.is_empty() && !tag.contains(' ') && tag.len() <= 48 {
            let app = tag.split('[').next().unwrap_or(tag).to_string();
            return (host, app, msg.trim().to_string());
        }
    }
    (host, String::new(), work.trim().to_string())
}

/// If `s` opens with a fixed-width RFC 3164 timestamp (`Mmm dd hh:mm:ss`, 15 chars), return the
/// remainder after it; else `None`.
fn strip_3164_timestamp(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.len() < 15 {
        return None;
    }
    let is_month = matches!(
        &s[..3],
        "Jan" | "Feb" | "Mar" | "Apr" | "May" | "Jun" | "Jul" | "Aug" | "Sep" | "Oct" | "Nov" | "Dec"
    );
    // Colons of hh:mm:ss sit at offsets 9 and 12; a space separates date from time at offset 6.
    if is_month && b[6] == b' ' && b[9] == b':' && b[12] == b':' {
        Some(&s[15..])
    } else {
        None
    }
}

/// RFC nil value `-` maps to an empty string.
fn nil_to_empty(s: &str) -> String {
    if s == "-" {
        String::new()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Listeners
// ---------------------------------------------------------------------------

/// Max UDP datagram we read in one go (jumbo syslog frames included).
const UDP_BUF: usize = 64 * 1024;

/// Bind and serve the syslog UDP listener forever. A bind failure is logged and the task returns
/// (the rest of the service keeps running); per-datagram parse/ingest never crashes the loop.
pub async fn serve_udp(state: AppState, addr: SocketAddr) {
    let socket = match UdpSocket::bind(addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%addr, error = %e, "syslog UDP bind failed — UDP ingest disabled");
            return;
        }
    };
    tracing::info!(%addr, "syslog UDP listener up");
    let mut buf = vec![0u8; UDP_BUF];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, peer)) => {
                let data = String::from_utf8_lossy(&buf[..n]).into_owned();
                let st = state.clone();
                let peer_ip = peer.ip().to_string();
                // Offload parse+store so the recv loop keeps draining the socket.
                tokio::spawn(async move {
                    for line in data.lines() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        crate::ingest::ingest_syslog_line(&st, line, &peer_ip).await;
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "syslog UDP recv error"),
        }
    }
}

/// Bind and serve the syslog TCP listener forever (newline-delimited frames, one connection per
/// sender). A bind failure is logged and the task returns.
pub async fn serve_tcp(state: AppState, addr: SocketAddr) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "syslog TCP bind failed — TCP ingest disabled");
            return;
        }
    };
    tracing::info!(%addr, "syslog TCP listener up");
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let st = state.clone();
                tokio::spawn(handle_tcp_conn(st, stream, peer.ip().to_string()));
            }
            Err(e) => tracing::warn!(error = %e, "syslog TCP accept error"),
        }
    }
}

async fn handle_tcp_conn(state: AppState, stream: tokio::net::TcpStream, peer_ip: String) {
    let mut lines = BufReader::new(stream).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                crate::ingest::ingest_syslog_line(&state, &line, &peer_ip).await;
            }
            Ok(None) => break, // connection closed
            Err(e) => {
                tracing::warn!(error = %e, "syslog TCP read error");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc5424() {
        let p = parse(
            "<34>1 2003-10-11T22:14:15.003Z mymachine.example.com su 1234 ID47 - 'su root' failed",
        );
        assert_eq!(p.host, "mymachine.example.com");
        assert_eq!(p.app, "su");
        assert_eq!(p.severity, "crit"); // 34 % 8 = 2
        assert_eq!(p.message, "'su root' failed");
    }

    #[test]
    fn parses_rfc5424_with_structured_data() {
        let p = parse(
            "<165>1 2024-01-01T00:00:00Z host app 99 ID1 [exampleSDID@32473 iut=\"3\"] real message here",
        );
        assert_eq!(p.host, "host");
        assert_eq!(p.app, "app");
        assert_eq!(p.message, "real message here");
    }

    #[test]
    fn parses_rfc5424_nil_host_app() {
        let p = parse("<13>1 2024-01-01T00:00:00Z - - - - - just the message");
        assert_eq!(p.host, "");
        assert_eq!(p.app, "");
        assert_eq!(p.message, "just the message");
    }

    #[test]
    fn parses_rfc3164() {
        let p = parse("<34>Oct 11 22:14:15 mymachine su[1234]: 'su root' failed for lonvick");
        assert_eq!(p.severity, "crit");
        assert_eq!(p.host, "mymachine");
        assert_eq!(p.app, "su");
        assert_eq!(p.message, "'su root' failed for lonvick");
    }

    #[test]
    fn tolerates_junk() {
        let p = parse("this is not really syslog at all");
        assert_eq!(p.severity, "info");
        assert_eq!(p.host, "");
        assert_eq!(p.app, "");
        assert_eq!(p.message, "this is not really syslog at all");
    }

    #[test]
    fn pri_only_freeform() {
        let p = parse("<14>plain message after pri");
        assert_eq!(p.severity, "info"); // 14 % 8 = 6
        assert_eq!(p.message, "plain message after pri");
    }
}
