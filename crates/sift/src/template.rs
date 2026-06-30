//! Message template clustering: normalize a log message into a stable pattern, then hash it.
//!
//! Two log lines that differ only in their variable parts (timestamps, ids, ip addresses,
//! request numbers, quoted payloads) collapse to ONE template, so the dashboard can show
//! "this shape of message happened 4,210 times" instead of 4,210 near-identical rows. The
//! normalization replaces the variable tokens with placeholders (`<UUID>`, `<IP>`, `<HEX>`,
//! `<NUM>`, `<STR>`), and [`template_id`] hashes the result with a stable, self-contained
//! FNV-1a so the same pattern always maps to the same id (across restarts AND across the
//! in-memory and Postgres stores).

/// Normalize a raw message into its template pattern by masking the variable tokens.
///
/// Scans by `char` (UTF-8 safe). Order matters: quoted strings first (they may contain
/// numbers/ips we should not separately mask), then composite tokens (UUID / IPv4 / hex digest),
/// and finally any remaining run of ASCII digits inside a token (so `1234ms` -> `<NUM>ms`).
pub fn normalize(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Quoted string: "..." or '...' -> <STR> (tolerate an unterminated quote).
        if c == '"' || c == '\'' {
            if let Some(end) = find_close_quote(&chars, i, c) {
                out.push_str("<STR>");
                i = end + 1;
                continue;
            }
        }

        // A maximal run of "token" characters (alphanumeric + id punctuation), classified as a
        // whole so a UUID/IP/hex digest is treated as one unit.
        if is_token_char(c) {
            let start = i;
            while i < chars.len() && is_token_char(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            match classify_composite(&token) {
                Some(placeholder) => out.push_str(placeholder),
                None => push_digit_masked(&mut out, &token),
            }
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Stable 64-bit FNV-1a hash of a pattern, rendered as `t_<16 hex>`. Self-contained (no external
/// hasher whose output could drift between std versions), so a given pattern's id is permanent.
pub fn template_id(pattern: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in pattern.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("t_{h:016x}")
}

/// Token chars: alphanumerics plus the punctuation that appears INSIDE a single logical value
/// (uuid dashes, dotted ip/version, colons in timestamps/addresses, underscores). Whitespace and
/// most symbols break a token.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_')
}

/// Classify a whole token as a composite variable (UUID / IPv4 / hex digest). Returns `None` for
/// anything else; the caller then digit-masks the token so bare/embedded numbers still collapse.
fn classify_composite(token: &str) -> Option<&'static str> {
    if is_uuid(token) {
        Some("<UUID>")
    } else if is_ipv4(token) {
        Some("<IP>")
    } else if is_hex_blob(token) {
        Some("<HEX>")
    } else {
        None
    }
}

/// Push `token` to `out`, collapsing every maximal run of ASCII digits to a single `<NUM>` and
/// keeping all other characters verbatim. So `1234ms` -> `<NUM>ms`, `error` -> `error`,
/// `22:14:15` -> `<NUM>:<NUM>:<NUM>`.
fn push_digit_masked(out: &mut String, token: &str) {
    let mut in_num = false;
    for c in token.chars() {
        if c.is_ascii_digit() {
            if !in_num {
                out.push_str("<NUM>");
                in_num = true;
            }
        } else {
            out.push(c);
            in_num = false;
        }
    }
}

/// Index of the matching closing quote in `chars`, if any.
fn find_close_quote(chars: &[char], open: usize, quote: char) -> Option<usize> {
    let mut j = open + 1;
    while j < chars.len() {
        if chars[j] == quote {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn is_uuid(token: &str) -> bool {
    let parts: Vec<&str> = token.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(lens.iter())
        .all(|(p, &n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_ipv4(token: &str) -> bool {
    let octets: Vec<&str> = token.split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    octets.iter().all(|o| {
        !o.is_empty()
            && o.len() <= 3
            && o.chars().all(|c| c.is_ascii_digit())
            && o.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
    })
}

/// A hex blob: an optional `0x` prefix, then >= 8 hex digits with at least one letter (digests,
/// addresses, ids). Pure-decimal runs are handled by digit masking, so this only fires on actual
/// hex with letters.
fn is_hex_blob(token: &str) -> bool {
    let t = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
        .unwrap_or(token);
    t.len() >= 8
        && t.chars().all(|c| c.is_ascii_hexdigit())
        && t.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_numbers_ips_uuids() {
        assert_eq!(
            normalize("connection from 10.0.0.1 took 1234ms"),
            "connection from <IP> took <NUM>ms"
        );
        assert_eq!(
            normalize("request 550e8400-e29b-41d4-a716-446655440000 ok"),
            "request <UUID> ok"
        );
        assert_eq!(normalize("retrying after 5 attempts"), "retrying after <NUM> attempts");
    }

    #[test]
    fn masks_quoted_strings_and_hex() {
        assert_eq!(normalize("user said \"hello world 42\""), "user said <STR>");
        assert_eq!(normalize("token 0xdeadbeef99 expired"), "token <HEX> expired");
    }

    #[test]
    fn same_shape_same_id() {
        let a = template_id(&normalize("user 1 logged in from 10.0.0.1"));
        let b = template_id(&normalize("user 99 logged in from 192.168.1.7"));
        assert_eq!(a, b);
        let c = template_id(&normalize("user 1 logged OUT from 10.0.0.1"));
        assert_ne!(a, c);
    }

    #[test]
    fn id_is_stable_and_shaped() {
        let id = template_id("<NUM> errors");
        assert!(id.starts_with("t_"));
        assert_eq!(id.len(), 2 + 16);
        assert_eq!(id, template_id("<NUM> errors"));
    }

    #[test]
    fn utf8_is_preserved() {
        assert_eq!(normalize("café closed 3 times"), "café closed <NUM> times");
    }
}
