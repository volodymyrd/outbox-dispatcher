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

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of structurally parsing and validating a callbacks JSON array.
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

    let array = match callbacks_json.as_array() {
        Some(a) => a,
        None => return ParsedCallbacks { valid, invalid },
    };

    for (i, element) in array.iter().enumerate() {
        match parse_one_callback(element, i, config) {
            Ok(target) => {
                let name = target.name.clone();
                if !seen_names.insert(name.clone()) {
                    invalid.push((
                        name,
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

const MAX_EXTERNAL_TIMEOUT_SECS: u64 = 7 * 86_400;

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

    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                format!("[{index}]"),
                "invalid_callback: missing required field 'name'".to_string(),
            )
        })?
        .to_string();

    if !is_valid_callback_name(&name) {
        return Err((
            name,
            "invalid_callback: name must match ^[a-z][a-z0-9_]{2,63}$".to_string(),
        ));
    }

    let url = obj
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                name.clone(),
                "invalid_callback: missing required field 'url'".to_string(),
            )
        })?
        .to_string();

    validate_url(&url, config.allow_insecure_urls).map_err(|r| (name.clone(), r))?;

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

    let max_completion_cycles = obj
        .get("max_completion_cycles")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(config.max_completion_cycles);

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

fn validate_url(url_str: &str, allow_insecure: bool) -> Result<(), String> {
    let parsed = url::Url::parse(url_str)
        .map_err(|_| format!("invalid_callback: url '{url_str}' is structurally malformed"))?;

    let scheme = parsed.scheme();
    if scheme == "https" {
        return Ok(());
    }
    if allow_insecure && scheme == "http" {
        return Ok(());
    }
    if allow_insecure {
        Err(format!(
            "invalid_callback: url must use http:// or https:// scheme; got '{url_str}'"
        ))
    } else {
        Err(format!(
            "invalid_callback: url must use https:// scheme; got '{url_str}'"
        ))
    }
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

fn parse_headers(val: Option<&serde_json::Value>) -> Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    let Some(obj) = val.and_then(|v| v.as_object()) else {
        return Ok(headers);
    };
    for (k, v) in obj {
        let k_lower = k.to_ascii_lowercase();
        if k_lower == "authorization" || k_lower == "cookie" || k_lower.starts_with("x-outbox-") {
            return Err(format!(
                "invalid_callback: header '{k}' is reserved and cannot be set"
            ));
        }
        let val_str = v
            .as_str()
            .ok_or_else(|| format!("invalid_callback: header '{k}' value must be a string"))?;
        if val_str.contains('\r') || val_str.contains('\n') {
            return Err(format!(
                "invalid_callback: header '{k}' value contains illegal newline characters"
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
    if !(1..=50).contains(&n) {
        return Err(format!(
            "invalid_callback: max_attempts must be between 1 and 50; got {n}"
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
            Ok(Duration::from_secs(n))
        })
        .collect()
}

fn parse_timeout(val: Option<&serde_json::Value>, default: Duration) -> Result<Duration, String> {
    let Some(v) = val else { return Ok(default) };
    let n = v.as_u64().ok_or_else(|| {
        "invalid_callback: timeout_seconds must be a positive integer".to_string()
    })?;
    if !(1..=300).contains(&n) {
        return Err(format!(
            "invalid_callback: timeout_seconds must be between 1 and 300; got {n}"
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
    fn payload_too_large_error_format() {
        let msg = payload_too_large_error(2_000_000, 1_048_576);
        assert_eq!(
            msg,
            "source_payload_too_large: 2000000 bytes > 1048576 bytes \
             (rejected by dispatcher before send)"
        );
    }

    #[test]
    fn payload_too_large_error_uses_exact_values() {
        let msg = payload_too_large_error(1025, 1024);
        assert!(msg.starts_with("source_payload_too_large:"));
        assert!(msg.contains("1025 bytes > 1024 bytes"));
    }

    // ── is_valid_callback_name ─────────────────────────────────────────────────

    #[test]
    fn valid_name_examples() {
        assert!(is_valid_callback_name("abc"));
        assert!(is_valid_callback_name("send_email"));
        assert!(is_valid_callback_name("a00"));
        assert!(is_valid_callback_name(&"a".repeat(64)));
    }

    #[test]
    fn invalid_name_too_short() {
        assert!(!is_valid_callback_name("ab")); // only 2 chars
        assert!(!is_valid_callback_name("a"));
    }

    #[test]
    fn invalid_name_starts_with_digit() {
        assert!(!is_valid_callback_name("1abc"));
    }

    #[test]
    fn invalid_name_starts_with_uppercase() {
        assert!(!is_valid_callback_name("Abc"));
    }

    #[test]
    fn invalid_name_contains_hyphen() {
        assert!(!is_valid_callback_name("ab-cd"));
    }

    #[test]
    fn invalid_name_too_long() {
        assert!(!is_valid_callback_name(&"a".repeat(65)));
    }

    // ── parse_callbacks happy path ─────────────────────────────────────────────

    #[test]
    fn empty_array_returns_empty_result() {
        let result = parse_callbacks(&json!([]), &default_config());
        assert!(result.valid.is_empty());
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn non_array_returns_empty_result() {
        let result = parse_callbacks(&json!({}), &default_config());
        assert!(result.valid.is_empty());
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn single_valid_callback_is_accepted() {
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
    fn mode_defaults_to_managed() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid[0].mode, CompletionMode::Managed);
    }

    #[test]
    fn external_mode_is_parsed() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "mode": "external",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.valid[0].mode, CompletionMode::External);
    }

    #[test]
    fn per_callback_overrides_are_applied() {
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
    fn defaults_from_config_are_used_when_fields_absent() {
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
    fn multiple_valid_callbacks_are_accepted() {
        let cbs = json!([
            { "name": "abc", "url": "https://a.example.com/", "signing_key_id": "k1" },
            { "name": "def", "url": "https://b.example.com/", "signing_key_id": "k2" }
        ]);
        let result = parse_callbacks(&cbs, &default_config());
        assert_eq!(result.valid.len(), 2);
        assert!(result.invalid.is_empty());
    }

    #[test]
    fn custom_headers_are_stored() {
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
    fn unknown_signing_key_id_is_not_rejected_at_parse_time() {
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
    fn missing_name_goes_to_invalid() {
        let cb = json!({ "url": "https://example.com/", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert!(result.valid.is_empty());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("'name'"));
    }

    #[test]
    fn invalid_name_regex_goes_to_invalid() {
        let cb = json!({ "name": "Ab", "url": "https://example.com/", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("invalid_callback"));
    }

    #[test]
    fn missing_url_goes_to_invalid() {
        let cb = json!({ "name": "abc", "signing_key_id": "k" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("'url'"));
    }

    #[test]
    fn http_url_rejected_when_insecure_disabled() {
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
    fn http_url_accepted_when_insecure_allowed() {
        let cb = json!({
            "name": "abc",
            "url": "http://localhost:8080/hook",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &insecure_config());
        assert_eq!(result.valid.len(), 1);
    }

    #[test]
    fn unknown_scheme_rejected_even_with_insecure_allowed() {
        let cb = json!({
            "name": "abc",
            "url": "ftp://example.com/",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &insecure_config());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn invalid_mode_goes_to_invalid() {
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
    fn missing_signing_key_id_rejected_when_unsigned_not_allowed() {
        let cb = json!({ "name": "abc", "url": "https://example.com/" });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("signing_key_id"));
    }

    #[test]
    fn missing_signing_key_id_accepted_when_allow_unsigned() {
        let cb = json!({ "name": "abc", "url": "https://example.com/" });
        let result = parse_callbacks(&json!([cb]), &unsigned_config());
        assert_eq!(result.valid.len(), 1);
        assert!(result.valid[0].signing_key_id.is_none());
    }

    #[test]
    fn reserved_authorization_header_rejected() {
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
    fn reserved_cookie_header_rejected() {
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
    fn x_outbox_prefix_header_rejected() {
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
    fn max_attempts_out_of_range_rejected() {
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
    fn max_attempts_boundary_values_accepted() {
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
    fn empty_backoff_array_rejected() {
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
    fn zero_in_backoff_array_rejected() {
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
    fn timeout_out_of_range_rejected() {
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
    fn timeout_boundary_values_accepted() {
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
    fn external_timeout_too_large_rejected() {
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
    fn external_timeout_at_maximum_accepted() {
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
    fn duplicate_names_second_goes_to_invalid() {
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
    fn mixed_valid_and_invalid_split_correctly() {
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
    fn non_object_element_goes_to_invalid() {
        let cbs = json!(["not_an_object"]);
        let result = parse_callbacks(&cbs, &default_config());
        assert!(result.valid.is_empty());
        assert_eq!(result.invalid.len(), 1);
    }

    #[test]
    fn invalid_error_messages_are_prefixed_with_invalid_callback() {
        let cbs = json!([
            { "name": "1bad", "url": "https://example.com/", "signing_key_id": "k" }
        ]);
        let result = parse_callbacks(&cbs, &default_config());
        assert!(result.invalid[0].1.starts_with("invalid_callback:"));
    }

    #[test]
    fn reserved_headers_are_case_insensitive() {
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
    fn x_outbox_prefix_header_case_insensitive() {
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
    fn header_value_with_newline_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https://example.com/",
            "signing_key_id": "k",
            "headers": { "X-Custom": "value\r\nX-Injected: evil" }
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("newline"));
    }

    #[test]
    fn structurally_invalid_url_is_rejected() {
        let cb = json!({
            "name": "abc",
            "url": "https:// this is not a valid url",
            "signing_key_id": "k"
        });
        let result = parse_callbacks(&json!([cb]), &default_config());
        assert_eq!(result.invalid.len(), 1);
        assert!(result.invalid[0].1.contains("malformed"));
    }
}
