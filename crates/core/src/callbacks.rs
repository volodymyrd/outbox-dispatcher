//! Callback Parsing and Structural Validation.
//!
//! # Why do we need this?
//!
//! The `outbox-dispatcher` reads events from a database where the destination
//! webhooks (`callbacks`) are stored as raw JSON. Because this JSON is generated
//! by upstream publisher applications, the dispatcher must treat it as untrusted input.
//!
//! If we attempted to dispatch webhooks without upfront validation:
//! - A malformed URL would cause the HTTP client to fail repeatedly, wasting retries.
//! - A publisher could inject an `Authorization` header, bypassing the HMAC signing scheme.
//! - A publisher could inject `\r\n` characters into a header, executing an HTTP Request Smuggling attack.
//!
//! # Upfront Dead-Lettering
//!
//! This module structurally validates every callback *before* it is scheduled.
//! If a callback definition is structurally invalid (e.g., missing a URL, invalid scheme,
//! out-of-bounds timeout), it is separated into the `invalid` list.
//!
//! The scheduler will immediately write these invalid callbacks to the database as
//! `dead_letter = TRUE`, ensuring they never enter the dispatch retry loop, while
//! valid callbacks from the same event proceed normally.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::config::DispatchConfig;
use crate::schema::{CallbackTarget, CompletionMode};

// ── Limits ────────────────────────────────────────────────────────────────────

const MAX_EXTERNAL_TIMEOUT_SECS: u64 = 7 * 86_400;
/// Hard ceiling on per-callback `max_attempts`; also enforced by [`AppConfig::validate`].
pub const MAX_PER_CALLBACK_ATTEMPTS: u64 = 50;
/// Hard ceiling on per-callback `timeout_seconds`; also enforced by [`AppConfig::validate`].
pub const MAX_HANDLER_TIMEOUT_SECS: u64 = 300;
/// Hard ceiling on per-callback `max_completion_cycles`; also enforced by [`AppConfig::validate`].
pub const MAX_COMPLETION_CYCLES_LIMIT: u64 = 1_000;
/// Hard ceiling on each element of per-callback `backoff_seconds`; also enforced by [`AppConfig::validate`].
pub const MAX_BACKOFF_ELEMENT_SECS: u64 = 7 * 86_400;

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of structurally parsing and validating a callbacks JSON array.
#[derive(Debug)]
pub struct ParsedCallbacks {
    /// Callbacks that passed all structural checks and are ready for scheduling.
    pub valid: Vec<CallbackTarget>,
    /// Callbacks that failed structural validation: (name_or_index, error_message).
    ///
    /// The error message is already prefixed with `"invalid_callback: "` so it can
    /// be stored directly in `outbox_deliveries.last_error`.
    pub invalid: Vec<(String, String)>,
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Parse and structurally validate the `callbacks` JSONB array from an event row.
///
/// **Does not** resolve `signing_key_id` against the keyring — that is deferred to
/// dispatch time to tolerate short publisher/dispatcher version skew during deploys.
pub fn parse_callbacks(
    callbacks_json: &serde_json::Value,
    config: &DispatchConfig,
) -> ParsedCallbacks {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    let Some(array) = callbacks_json.as_array() else {
        invalid.push((
            "<root>".to_string(),
            "invalid_callback: top-level callbacks value is not a JSON array".to_string(),
        ));
        return ParsedCallbacks { valid, invalid };
    };

    if array.len() > config.max_callbacks_per_event as usize {
        invalid.push((
            "<root>".to_string(),
            format!(
                "invalid_callback: too many callbacks in event ({}); max is {}",
                array.len(),
                config.max_callbacks_per_event
            ),
        ));
        return ParsedCallbacks { valid, invalid };
    }

    for (i, element) in array.iter().enumerate() {
        match parse_one_callback(element, i, config) {
            Ok(target) => {
                if !seen_names.insert(target.name.clone()) {
                    invalid.push((
                        target.name,
                        "invalid_callback: duplicate callback name within event".to_string(),
                    ));
                } else {
                    valid.push(target);
                }
            }
            Err(e) => invalid.push(e),
        }
    }

    ParsedCallbacks { valid, invalid }
}

/// Produces the canonical `last_error` string for a schedule-time payload-size rejection.
///
/// This prefix (`source_payload_too_large:`) is documented so operators can filter
/// dispatcher-side rejections from receiver-side HTTP 413 responses.
pub fn payload_too_large_error(actual_bytes: i64, limit_bytes: i64) -> String {
    format!(
        "source_payload_too_large: {actual_bytes} bytes > {limit_bytes} bytes \
         (rejected by dispatcher before send)"
    )
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn parse_one_callback(
    element: &serde_json::Value,
    index: usize,
    config: &DispatchConfig,
) -> Result<CallbackTarget, (String, String)> {
    let obj = element.as_object().ok_or_else(|| {
        (
            format!("[{index}]"),
            "invalid_callback: element is not an object".to_string(),
        )
    })?;

    let name = match obj.get("name") {
        None => {
            return Err((
                format!("[{index}]"),
                "invalid_callback: missing required field 'name'".to_string(),
            ));
        }
        Some(v) => v.as_str().ok_or_else(|| {
            (
                format!("[{index}]"),
                "invalid_callback: 'name' must be a string".to_string(),
            )
        })?,
    }
    .to_string();

    if !is_valid_callback_name(&name) {
        return Err((
            name,
            "invalid_callback: name must match ^[a-z][a-z0-9_]{2,63}$".to_string(),
        ));
    }

    let url = match obj.get("url") {
        None => {
            return Err((
                name.clone(),
                "invalid_callback: missing required field 'url'".to_string(),
            ));
        }
        Some(v) => v.as_str().ok_or_else(|| {
            (
                name.clone(),
                "invalid_callback: 'url' must be a string".to_string(),
            )
        })?,
    }
    .to_string();

    validate_url(
        &url,
        config.allow_insecure_urls,
        config.allow_private_ip_targets,
    )
    .map_err(|r| (name.clone(), r))?;

    let mode =
        parse_mode(obj.get("mode").and_then(|v| v.as_str())).map_err(|r| (name.clone(), r))?;

    let signing_key_id = obj
        .get("signing_key_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if !config.allow_unsigned_callbacks && signing_key_id.is_none() {
        return Err((
            name,
            "invalid_callback: missing required field 'signing_key_id' \
             (set allow_unsigned_callbacks=true to omit)"
                .to_string(),
        ));
    }

    let headers = parse_headers(obj.get("headers")).map_err(|r| (name.clone(), r))?;

    let max_attempts = parse_max_attempts(obj.get("max_attempts"), config.max_attempts)
        .map_err(|r| (name.clone(), r))?;

    let backoff = parse_backoff(obj.get("backoff_seconds"), &config.backoff)
        .map_err(|r| (name.clone(), r))?;

    let timeout = parse_timeout(obj.get("timeout_seconds"), config.handler_timeout)
        .map_err(|r| (name.clone(), r))?;

    let external_completion_timeout =
        parse_external_timeout(obj.get("external_completion_timeout_seconds"))
            .map_err(|r| (name.clone(), r))?;

    let max_completion_cycles = parse_max_completion_cycles(
        obj.get("max_completion_cycles"),
        config.max_completion_cycles,
    )
    .map_err(|r| (name.clone(), r))?;

    // External mode requires a timeout so the sweeper knows when to redeliver.
    if mode == CompletionMode::External && external_completion_timeout.is_none() {
        return Err((
            name,
            "invalid_callback: mode='external' requires \
             'external_completion_timeout_seconds'"
                .to_string(),
        ));
    }

    Ok(CallbackTarget {
        name,
        url,
        mode,
        signing_key_id,
        headers,
        max_attempts,
        backoff,
        timeout,
        external_completion_timeout,
        max_completion_cycles,
    })
}

fn is_valid_callback_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 3
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn validate_url(
    url_str: &str,
    allow_insecure: bool,
    allow_private_ip_targets: bool,
) -> Result<(), String> {
    let parsed = url::Url::parse(url_str)
        .map_err(|_| format!("invalid_callback: url '{url_str}' is structurally malformed"))?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "invalid_callback: url '{url_str}' must not contain userinfo (username/password)"
        ));
    }

    let scheme = parsed.scheme();
    match (scheme, allow_insecure) {
        ("https", _) | ("http", true) => {}
        _ => {
            return Err(if allow_insecure {
                format!(
                    "invalid_callback: url must use http:// or https:// scheme; got '{url_str}'"
                )
            } else {
                format!("invalid_callback: url must use https:// scheme; got '{url_str}'")
            });
        }
    }

    let host = parsed
        .host()
        .ok_or_else(|| format!("invalid_callback: url '{url_str}' has no host"))?;

    if !allow_private_ip_targets && is_private_host(host) {
        return Err(format!(
            "invalid_callback: url '{url_str}' targets a private or loopback address \
             (set allow_private_ip_targets=true to allow local targets)"
        ));
    }

    Ok(())
}

fn is_private_host(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        url::Host::Ipv6(ip) => {
            // `to_ipv4()` inherently extracts the underlying IPv4 address from BOTH
            // IPv4-mapped (::ffff:x/96) and IPv4-compatible (::x/96) addresses.
            if let Some(v4) = ip.to_ipv4()
                && (v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified())
            {
                return true;
            }

            // Fall through to native IPv6 checks.
            // (Note: `::1` yields `0.0.0.1` from `to_ipv4()`, which skips the v4 checks
            // above and is correctly caught right here by `is_loopback()`).
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
        }
        // Domain targets bypass DNS-level SSRF checks at parse time; dispatch-time DNS
        // resolution (Phase 4) is the real fix. Block the most dangerous well-known names now.
        url::Host::Domain(d) => is_blocked_domain(d),
    }
}

/// Returns `true` for domain names that are definitively private or SSRF-dangerous.
///
/// This is a stopgap denylist; full protection requires dispatch-time DNS resolution (Phase 4).
fn is_blocked_domain(d: &str) -> bool {
    // Trim a trailing dot: DNS FQDN notation (`localhost.`) resolves identically to
    // `localhost` but would otherwise bypass all string comparisons below.
    let lower = d.trim_end_matches('.').to_ascii_lowercase();
    lower == "localhost"
        || lower.ends_with(".local")
        || lower.ends_with(".localhost")
        || lower.ends_with(".internal")
        || lower.ends_with(".lan")
        || lower.ends_with(".home.arpa")
}

fn parse_mode(mode_str: Option<&str>) -> Result<CompletionMode, String> {
    match mode_str.unwrap_or("managed") {
        "managed" => Ok(CompletionMode::Managed),
        "external" => Ok(CompletionMode::External),
        other => Err(format!(
            "invalid_callback: mode must be 'managed' or 'external'; got '{other}'"
        )),
    }
}

/// Returns true if `name` consists only of RFC 7230 tchar characters.
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
                    | b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
            )
        })
}

// Headers that the dispatcher or HTTP stack controls; publishers must not override them.
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "upgrade",
    "expect",
    "te",
    "trailer",
    "proxy-authorization",
];

fn parse_headers(val: Option<&serde_json::Value>) -> Result<HashMap<String, String>, String> {
    let Some(val) = val else {
        return Ok(HashMap::new());
    };
    let obj = val
        .as_object()
        .ok_or_else(|| "invalid_callback: headers must be a JSON object".to_string())?;
    let mut headers = HashMap::new();
    let mut seen_lower: HashSet<String> = HashSet::new();
    for (k, v) in obj {
        if !is_valid_header_name(k) {
            return Err(format!(
                "invalid_callback: header name '{k}' contains invalid characters"
            ));
        }
        let k_lower = k.to_ascii_lowercase();
        if RESERVED_HEADERS.contains(&k_lower.as_str()) || k_lower.starts_with("x-outbox-") {
            return Err(format!(
                "invalid_callback: header '{k}' is reserved and cannot be set"
            ));
        }
        if !seen_lower.insert(k_lower) {
            return Err(format!(
                "invalid_callback: header '{k}' is a duplicate (header names are case-insensitive)"
            ));
        }
        let val_str = v
            .as_str()
            .ok_or_else(|| format!("invalid_callback: header '{k}' value must be a string"))?;
        if val_str
            .bytes()
            .any(|b| (b < 0x20 && b != b'\t') || b >= 0x7f)
        {
            return Err(format!(
                "invalid_callback: header '{k}' value contains illegal characters \
                 (control characters and non-ASCII bytes are not allowed)"
            ));
        }
        headers.insert(k.clone(), val_str.to_string());
    }
    Ok(headers)
}

fn parse_max_attempts(val: Option<&serde_json::Value>, default: u32) -> Result<u32, String> {
    let Some(v) = val else { return Ok(default) };
    let n = v
        .as_u64()
        .ok_or_else(|| "invalid_callback: max_attempts must be a positive integer".to_string())?;
    if !(1..=MAX_PER_CALLBACK_ATTEMPTS).contains(&n) {
        return Err(format!(
            "invalid_callback: max_attempts must be between 1 and {MAX_PER_CALLBACK_ATTEMPTS}; got {n}"
        ));
    }
    Ok(n as u32)
}

fn parse_backoff(
    val: Option<&serde_json::Value>,
    default: &[Duration],
) -> Result<Vec<Duration>, String> {
    let Some(v) = val else {
        return Ok(default.to_vec());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| "invalid_callback: backoff_seconds must be an array".to_string())?;
    if arr.is_empty() {
        return Err("invalid_callback: backoff_seconds must not be empty".to_string());
    }
    arr.iter()
        .map(|e| {
            let n = e.as_u64().ok_or_else(|| {
                "invalid_callback: backoff_seconds elements must be positive integers".to_string()
            })?;
            if n == 0 {
                return Err("invalid_callback: backoff_seconds elements must be > 0".to_string());
            }
            if n > MAX_BACKOFF_ELEMENT_SECS {
                return Err(format!(
                    "invalid_callback: backoff_seconds elements must be <= \
                     {MAX_BACKOFF_ELEMENT_SECS}; got {n}"
                ));
            }
            Ok(Duration::from_secs(n))
        })
        .collect()
}

fn parse_timeout(val: Option<&serde_json::Value>, default: Duration) -> Result<Duration, String> {
    let Some(v) = val else { return Ok(default) };
    let n = v.as_u64().ok_or_else(|| {
        "invalid_callback: timeout_seconds must be a positive integer".to_string()
    })?;
    if !(1..=MAX_HANDLER_TIMEOUT_SECS).contains(&n) {
        return Err(format!(
            "invalid_callback: timeout_seconds must be between 1 and {MAX_HANDLER_TIMEOUT_SECS}; got {n}"
        ));
    }
    Ok(Duration::from_secs(n))
}

fn parse_external_timeout(val: Option<&serde_json::Value>) -> Result<Option<Duration>, String> {
    let Some(v) = val else { return Ok(None) };
    let n = v.as_u64().ok_or_else(|| {
        "invalid_callback: external_completion_timeout_seconds must be a positive integer"
            .to_string()
    })?;
    if !(1..=MAX_EXTERNAL_TIMEOUT_SECS).contains(&n) {
        return Err(format!(
            "invalid_callback: external_completion_timeout_seconds must be between 1 and \
             {MAX_EXTERNAL_TIMEOUT_SECS}; got {n}"
        ));
    }
    Ok(Some(Duration::from_secs(n)))
}

fn parse_max_completion_cycles(
    val: Option<&serde_json::Value>,
    default: u32,
) -> Result<u32, String> {
    let Some(v) = val else { return Ok(default) };
    let n = v.as_u64().ok_or_else(|| {
        "invalid_callback: max_completion_cycles must be a positive integer".to_string()
    })?;
    if !(1..=MAX_COMPLETION_CYCLES_LIMIT).contains(&n) {
        return Err(format!(
            "invalid_callback: max_completion_cycles must be between 1 and \
             {MAX_COMPLETION_CYCLES_LIMIT}; got {n}"
        ));
    }
    Ok(n as u32)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn default_config() -> DispatchConfig {
        DispatchConfig::default()
    }

    fn unsigned_config() -> DispatchConfig {
        DispatchConfig {
            allow_unsigned_callbacks: true,
            ..Default::default()
        }
    }

    fn insecure_config() -> DispatchConfig {
        DispatchConfig {
            allow_insecure_urls: true,
            allow_unsigned_callbacks: true,
            ..Default::default()
        }
    }

    fn valid_callback() -> serde_json::Value {
        json!({
            "name": "send_email",
            "url": "https://example.com/webhook",
            "signing_key_id": "key-v1"
        })
    }

    // ── payload_too_large_error ────────────────────────────────────────────────

    #[test]
    fn test_payload_too_large_error_format() {
        let msg = payload_too_large_error(2_000_000, 1_048_576);
        assert_eq!(
            msg,
            "source_payload_too_large: 2000000 bytes > 1048576 bytes \
             (rejected by dispatcher before send)"
        );
    }

    #[test]
    fn test_payload_too_large_error_uses_exact_values() {
        let msg = payload_too_large_error(1025, 1024);
        assert!(msg.starts_with("source_payload_too_large:"));
        assert!(msg.contains("1025 bytes > 1024 bytes"));
    }

    // ── is_valid_callback_name ─────────────────────────────────────────────────

    #[test]
    fn test_valid_name_examples() {
        assert!(is_valid_callback_name("abc"));
        assert!(is_valid_callback_name("send_email"));
        assert!(is_valid_callback_name("a00"));
        assert!(is_valid_callback_name(&"a".repeat(64)));
    }

    #[test]
    fn test_invalid_name_too_short() {
        assert!(!is_valid_callback_name("ab")); // only 2 chars
        assert!(!is_valid_callback_name("a"));
    }

    #[test]
    fn test_invalid_name_starts_with_digit() {
        assert!(!is_valid_callback_name("1abc"));
    }

    #[test]
    fn test_invalid_name_starts_with_uppercase() {
        assert!(!is_valid_callback_name("Abc"));
    }

    #[test]
    fn test_invalid_name_contains_hyphen() {
        assert!(!is_valid_callback_name("ab-cd"));
    }

    #[test]
    fn test_invalid_name_too_long() {
        assert!(!is_valid_callback_name(&"a".repeat(65)));
    }

    // ── parse_callbacks happy path ─────────────────────────────────────────────

    #[test]
    fn test_empty_array_returns_empty_result() {
        let result = parse_callbacks(&json!([]), &default_config());
        assert!(result.valid.is_empty());
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn test_non_array_returns_invalid_result() {
        let result = parse_callbacks(&json!({}), &default_config());
        assert!(result.valid.is_empty());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("not a JSON array"));
    }

    #[test]
    fn test_single_valid_callback_is_accepted() {
        let result = parse_callbacks(&json!([valid_callback()]), &default_config());
        assert_eq!(result.valid.len(), 1);
        assert!(result.invalid.is_empty());
        let cb = &result.valid[0];
        assert_eq!(cb.name, "send_email");
        assert_eq!(cb.url, "https://example.com/webhook");
        assert_eq!(cb.mode, CompletionMode::Managed);
        assert_eq!(cb.signing_key_id.as_deref(), Some("key-v1"));
    }

    #[test]
    fn test_mode_defaults_to_managed() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid[0].mode, CompletionMode::Managed);
    }

    #[test]
    fn test_external_mode_requires_external_completion_timeout() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "mode": "external",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(
            result.invalid[0]
                .1
                .contains("external_completion_timeout_seconds"),
            "got: {}",
            result.invalid[0].1
        );
    }

    #[test]
    fn test_external_mode_with_timeout_is_valid() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "mode": "external",
            "signing_key_id": "k",
            "external_completion_timeout_seconds": 3600
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid.len(), 1);
        assert_eq!(result.valid[0].mode, CompletionMode::External);
    }

    #[test]
    fn test_external_mode_is_parsed() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "mode": "external",
            "signing_key_id": "k",
            "external_completion_timeout_seconds": 3600
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid[0].mode, CompletionMode::External);
    }

    #[test]
    fn test_per_callback_overrides_are_applied() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "max_attempts": 3,
            "backoff_seconds": [60, 120],
            "timeout_seconds": 10,
            "external_completion_timeout_seconds": 3600,
            "max_completion_cycles": 5
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid.len(), 1);
        let target = &result.valid[0];
        assert_eq!(target.max_attempts, 3);
        assert_eq!(
            target.backoff,
            vec![Duration::from_secs(60), Duration::from_secs(120)]
        );
        assert_eq!(target.timeout, Duration::from_secs(10));
        assert_eq!(
            target.external_completion_timeout,
            Some(Duration::from_secs(3600))
        );
        assert_eq!(target.max_completion_cycles, 5);
    }

    #[test]
    fn test_defaults_from_config_are_used_when_fields_absent() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        let target = &result.valid[0];
        assert_eq!(target.max_attempts, 6);
        assert_eq!(target.timeout, Duration::from_secs(30));
        assert_eq!(target.max_completion_cycles, 20);
        assert_eq!(target.external_completion_timeout, None);
    }

    #[test]
    fn test_multiple_valid_callbacks_are_accepted() {
        let cbs = json!([
            { "name": "abc", "url": "https://a.example.com/", "signing_key_id": "k1" },
            { "name": "def", "url": "https://b.example.com/", "signing_key_id": "k2" }
        ]);
        let result = parse_callbacks(&cbs, &default_config());
        assert_eq!(result.valid.len(), 2);
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn test_custom_headers_are_stored() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Service": "my-svc", "X-Env": "prod" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        let headers = &result.valid[0].headers;
        assert_eq!(headers.get("X-Service").map(String::as_str), Some("my-svc"));
        assert_eq!(headers.get("X-Env").map(String::as_str), Some("prod"));
    }

    // ── signing_key_id deferred — not resolved at parse time ──────────────────

    #[test]
    fn test_unknown_signing_key_id_is_not_rejected_at_parse_time() {
        // Even if "no-such-key" is not registered in any keyring, parse_callbacks must
        // not reject it here. Resolution is deferred to dispatch time (§2.3 layer 4).
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "no-such-key"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(
            result.valid.len(),
            1,
            "unknown signing_key_id must pass parse_callbacks"
        );
        assert!(result.invalid.is_empty());
    }

    // ── invalid callbacks ─────────────────────────────────────────────────────

    #[test]
    fn test_missing_name_goes_to_invalid() {
        let cb = json!({ "url": "https://example.com/", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert!(result.valid.is_empty());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("'name'"));
    }

    #[test]
    fn test_invalid_name_regex_goes_to_invalid() {
        let cb = json!({ "name": "Ab", "url": "https://example.com/", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("invalid_callback"));
    }

    #[test]
    fn test_missing_url_goes_to_invalid() {
        let cb = json!({ "name": "abc", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("'url'"));
    }

    #[test]
    fn test_http_url_rejected_when_insecure_disabled() {
        let cb = json!({
            "name": "abc",
            "url": "http://example.com/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("https://"));
    }

    #[test]
    fn test_http_url_accepted_when_insecure_allowed() {
        let cb = json!({
            "name": "abc",
            "url": "http://example.com:8080/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &insecure_config());
        assert_eq!(result.valid.len(), 1);
    }

    #[test]
    fn test_unknown_scheme_rejected_even_with_insecure_allowed() {
        let cb = json!({
            "name": "abc",
            "url": "ftp://example.com/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &insecure_config());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn test_invalid_mode_goes_to_invalid() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "mode": "async",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("mode"));
    }

    #[test]
    fn test_missing_signing_key_id_rejected_when_unsigned_not_allowed() {
        let cb = json!({ "name": "abc", "url": "https://example.com/" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("signing_key_id"));
    }

    #[test]
    fn test_missing_signing_key_id_accepted_when_allow_unsigned() {
        let cb = json!({ "name": "abc", "url": "https://example.com/" });
        let result = parse_callbacks(&json!([cb]), &unsigned_config());
        assert_eq!(result.valid.len(), 1);
        assert!(result.valid[0].signing_key_id.is_none());
    }

    #[test]
    fn test_reserved_authorization_header_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "Authorization": "Bearer token" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("Authorization"));
    }

    #[test]
    fn test_reserved_cookie_header_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "Cookie": "session=abc" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("Cookie"));
    }

    #[test]
    fn test_x_outbox_prefix_header_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Outbox-Event-Id": "override" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("X-Outbox-Event-Id"));
    }

    #[test]
    fn test_invalid_header_name_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X Custom": "value" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("header name"));
    }

    #[test]
    fn test_max_attempts_out_of_range_rejected() {
        for bad in [0_u64, 51] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "max_attempts": bad
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.invalid.len(),
                1,
                "max_attempts={bad} should be rejected"
            );
        }
    }

    #[test]
    fn test_max_attempts_boundary_values_accepted() {
        for good in [1_u64, 50] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "max_attempts": good
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.valid.len(),
                1,
                "max_attempts={good} should be accepted"
            );
        }
    }

    #[test]
    fn test_empty_backoff_array_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "backoff_seconds": []
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn test_zero_in_backoff_array_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "backoff_seconds": [30, 0, 120]
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn test_timeout_out_of_range_rejected() {
        for bad in [0_u64, 301] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "timeout_seconds": bad
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.invalid.len(),
                1,
                "timeout_seconds={bad} should be rejected"
            );
        }
    }

    #[test]
    fn test_timeout_boundary_values_accepted() {
        for good in [1_u64, 300] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "timeout_seconds": good
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.valid.len(),
                1,
                "timeout_seconds={good} should be accepted"
            );
        }
    }

    #[test]
    fn test_external_timeout_too_large_rejected() {
        let bad = MAX_EXTERNAL_TIMEOUT_SECS + 1;
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "external_completion_timeout_seconds": bad
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn test_external_timeout_at_maximum_accepted() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "external_completion_timeout_seconds": MAX_EXTERNAL_TIMEOUT_SECS
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid.len(), 1);
    }

    #[test]
    fn test_max_completion_cycles_out_of_range_rejected() {
        for bad in [0_u64, MAX_COMPLETION_CYCLES_LIMIT + 1] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "max_completion_cycles": bad
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.invalid.len(),
                1,
                "max_completion_cycles={bad} should be rejected"
            );
        }
    }

    #[test]
    fn test_max_completion_cycles_boundary_values_accepted() {
        for good in [1_u64, MAX_COMPLETION_CYCLES_LIMIT] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "max_completion_cycles": good
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.valid.len(),
                1,
                "max_completion_cycles={good} should be accepted"
            );
        }
    }

    #[test]
    fn test_duplicate_names_second_goes_to_invalid() {
        let cbs = json!([
            { "name": "abc", "url": "https://a.example.com/", "signing_key_id": "k" },
            { "name": "abc", "url": "https://b.example.com/", "signing_key_id": "k" }
        ]);
        let result = parse_callbacks(&cbs, &default_config());
        assert_eq!(result.valid.len(), 1);
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("duplicate"));
    }

    #[test]
    fn test_mixed_valid_and_invalid_split_correctly() {
        let cbs = json!([
            { "name": "good_one", "url": "https://example.com/", "signing_key_id": "k" },
            { "name": "1bad", "url": "https://example.com/", "signing_key_id": "k" },
            { "name": "good_two", "url": "https://example.com/", "signing_key_id": "k" }
        ]);
        let result = parse_callbacks(&cbs, &default_config());
        assert_eq!(result.valid.len(), 2);
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn test_non_object_element_goes_to_invalid() {
        let cbs = json!(["not_an_object"]);
        let result = parse_callbacks(&cbs, &default_config());
        assert!(result.valid.is_empty());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn test_invalid_error_messages_are_prefixed_with_invalid_callback() {
        let cbs = json!([
            { "name": "1bad", "url": "https://example.com/", "signing_key_id": "k" }
        ]);
        let result = parse_callbacks(&cbs, &default_config());
        assert!(result.invalid[0].1.starts_with("invalid_callback:"));
    }

    #[test]
    fn test_reserved_headers_are_case_insensitive() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "aUtHoRiZaTiOn": "Bearer token" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("reserved"));
    }

    #[test]
    fn test_x_outbox_prefix_header_case_insensitive() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "x-OuTbOx-Signature": "override" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("reserved"));
    }

    #[test]
    fn test_header_value_with_control_chars_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Custom": "value\r\nX-Injected: evil" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("control characters"));
    }

    #[test]
    fn test_header_value_with_null_byte_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Custom": "value\u{0000}" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("control characters"));
    }

    #[test]
    fn test_headers_must_be_object_rejected() {
        for bad_headers in [
            json!("Authorization: Bearer x"),
            json!(["X-A", "v"]),
            json!(42),
        ] {
            let cb = json!({
                "name": "abc",
                "url": "https://example.com/",
                "signing_key_id": "k",
                "headers": bad_headers
            });
            let result = parse_callbacks(&json!([cb]), &default_config());
            assert_eq!(
                result.invalid.len(),
                1,
                "non-object headers must be rejected"
            );
            assert!(result.invalid[0].1.contains("JSON object"));
        }
    }

    #[test]
    fn test_structurally_invalid_url_is_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https:// this is not a valid url",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("malformed"));
    }

    // ── H2: additional reserved headers ──────────────────────────────────────

    #[test]
    fn test_reserved_host_header_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "Host": "evil.example.com" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("reserved"));
    }

    #[test]
    fn test_reserved_content_length_header_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "Content-Length": "0" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("reserved"));
    }

    #[test]
    fn test_reserved_transfer_encoding_header_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "Transfer-Encoding": "chunked" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("reserved"));
    }

    // ── L1: case-insensitive duplicate header detection ───────────────────────

    #[test]
    fn test_duplicate_header_name_case_insensitive_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Service": "a", "x-service": "b" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("duplicate"));
    }

    // ── H3: URL host and SSRF checks ─────────────────────────────────────────

    #[test]
    fn test_loopback_ipv4_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://127.0.0.1/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_private_ipv4_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://192.168.1.1/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_link_local_ipv4_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://169.254.169.254/latest/meta-data/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_loopback_ipv6_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://[::1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_private_ip_accepted_when_allow_private_ip_targets_set() {
        let config = DispatchConfig {
            allow_private_ip_targets: true,
            allow_unsigned_callbacks: true,
            ..Default::default()
        };
        let cb = json!({
            "name": "abc",
            "url": "https://127.0.0.1/hook"
        });
        let result = parse_callbacks(&json!([cb]), &config);
        assert_eq!(result.valid.len(), 1);
    }

    #[test]
    fn test_public_ip_accepted_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://93.184.216.34/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid.len(), 1);
    }

    // ── L6: max_callbacks_per_event ───────────────────────────────────────────

    #[test]
    fn test_too_many_callbacks_rejected() {
        let config = DispatchConfig {
            max_callbacks_per_event: 2,
            ..Default::default()
        };
        let cbs = json!([
            { "name": "abc", "url": "https://a.example.com/", "signing_key_id": "k" },
            { "name": "def", "url": "https://b.example.com/", "signing_key_id": "k" },
            { "name": "ghi", "url": "https://c.example.com/", "signing_key_id": "k" }
        ]);
        let result = parse_callbacks(&cbs, &config);
        assert!(result.valid.is_empty());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("too many callbacks"));
    }

    #[test]
    fn test_callbacks_at_max_limit_accepted() {
        let config = DispatchConfig {
            max_callbacks_per_event: 2,
            ..Default::default()
        };
        let cbs = json!([
            { "name": "abc", "url": "https://a.example.com/", "signing_key_id": "k" },
            { "name": "def", "url": "https://b.example.com/", "signing_key_id": "k" }
        ]);
        let result = parse_callbacks(&cbs, &config);
        assert_eq!(result.valid.len(), 2);
        assert!(result.invalid.is_empty());
    }

    // ── URL userinfo (credentials) ────────────────────────────────────────────

    #[test]
    fn test_url_with_username_and_password_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://user:secret@example.com/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("userinfo"));
    }

    #[test]
    fn test_url_with_username_only_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://user@example.com/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("userinfo"));
    }

    // ── IPv6 SSRF: additional blocked ranges ─────────────────────────────────

    #[test]
    fn test_ipv6_unique_local_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://[fc00::1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_ipv6_link_local_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://[fe80::1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_ipv6_unspecified_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://[::]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_ipv4_mapped_ipv6_loopback_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://[::ffff:127.0.0.1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_ipv4_mapped_ipv6_private_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://[::ffff:192.168.1.1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    // ── backoff element ceiling ───────────────────────────────────────────────

    #[test]
    fn test_backoff_element_exceeds_max_rejected() {
        let too_large = MAX_BACKOFF_ELEMENT_SECS + 1;
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "backoff_seconds": [60, too_large]
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(
            result.invalid[0]
                .1
                .contains("backoff_seconds elements must be <=")
        );
    }

    #[test]
    fn test_backoff_element_at_max_accepted() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "backoff_seconds": [MAX_BACKOFF_ELEMENT_SECS]
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid.len(), 1);
    }

    // ── ParsedCallbacks is Debug ──────────────────────────────────────────────

    #[test]
    fn test_parsed_callbacks_implements_debug() {
        let result = parse_callbacks(&json!([valid_callback()]), &default_config());
        let s = format!("{result:?}");
        assert!(s.contains("ParsedCallbacks"));
    }

    // ── IPv4-compatible IPv6 SSRF ─────────────────────────────────────────────

    #[test]
    fn test_ipv4_compatible_ipv6_loopback_rejected_by_default() {
        // ::127.0.0.1 is a deprecated IPv4-compatible address — must be blocked.
        let cb = json!({
            "name": "abc",
            "url": "https://[::127.0.0.1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_ipv4_compatible_ipv6_private_rejected_by_default() {
        // ::192.168.1.1 in IPv4-compatible form — must be blocked.
        let cb = json!({
            "name": "abc",
            "url": "https://[::192.168.1.1]/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    // ── Header obs-text (non-ASCII bytes) ────────────────────────────────────

    #[test]
    fn test_header_value_with_obs_text_rejected() {
        // 0x80..=0xFF are RFC 9110 "obs-text" — obsolete and rejected here.
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Custom": "caf\u{00e9}" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("illegal characters"));
    }

    // ── Fix #1: domain-name denylist (SSRF stopgap) ──────────────────────────

    #[test]
    fn test_localhost_domain_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://localhost/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_dot_local_domain_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://intranet.local/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_dot_internal_domain_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://metadata.google.internal/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_dot_lan_domain_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://router.lan/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_dot_localhost_subdomain_rejected_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://app.localhost/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    // ── Trailing-dot FQDN bypass (regression for SSRF stopgap) ──────────────

    #[test]
    fn test_localhost_trailing_dot_fqdn_rejected() {
        // `https://localhost./` keeps the dot in url::Host::Domain("localhost."),
        // which bypassed the exact-match and suffix checks before the trim fix.
        let cb = json!({
            "name": "abc",
            "url": "https://localhost./hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_app_localhost_trailing_dot_fqdn_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://app.localhost./hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_internal_service_trailing_dot_fqdn_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://metadata.google.internal./hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("private or loopback"));
    }

    #[test]
    fn test_public_domain_accepted_by_default() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid.len(), 1);
    }

    #[test]
    fn test_blocked_domain_accepted_when_allow_private_ip_targets_set() {
        let config = DispatchConfig {
            allow_private_ip_targets: true,
            allow_unsigned_callbacks: true,
            ..Default::default()
        };
        let cb = json!({ "name": "abc", "url": "https://localhost/hook" });
        let result = parse_callbacks(&json!([cb]), &config);
        assert_eq!(result.valid.len(), 1);
    }

    // ── Fix #2: type errors for name/url fields ───────────────────────────────

    #[test]
    fn test_name_present_but_non_string_goes_to_invalid() {
        let cb = json!({ "name": 42, "url": "https://example.com/", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(
            result.invalid[0].1.contains("must be a string"),
            "got: {}",
            result.invalid[0].1
        );
    }

    #[test]
    fn test_url_present_but_non_string_goes_to_invalid() {
        let cb = json!({ "name": "abc", "url": 123, "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(
            result.invalid[0].1.contains("must be a string"),
            "got: {}",
            result.invalid[0].1
        );
    }
}
