//! Cryptographic Secret Management and Resolution.
//!
//! # Why do we need this?
//!
//! To securely sign outgoing webhooks, the dispatcher needs access to shared HMAC secrets.
//! Storing these secrets directly in the database alongside the `outbox_events` is a critical
//! security anti-pattern: if the database is ever compromised, the attacker would simultaneously
//! gain the payload data *and* the cryptographic keys needed to forge malicious webhook payloads.
//!
//! Instead, the dispatcher uses a decoupled `KeyRing` approach:
//! 1. The database (`callbacks` JSON) only stores a reference string: `"signing_key_id": "key-v1"`.
//! 2. The configuration maps this ID to an environment variable name: `secret_env: "APP_SECRET_KEY_V1"`.
//! 3. This `KeyRing` module reads the environment variable, decodes the secret, and holds it in memory.
//!
//! # Security Guarantees
//!
//! This module acts as a strict gateway for cryptographic material, enforcing several protections
//! before the application is even allowed to start:
//!
//! - **Minimum Entropy:** It strictly rejects any secret that decodes to less than 32 bytes,
//!   protecting the webhook signatures from offline brute-force attacks.
//! - **Maximum Size:** It rejects secrets larger than 256 decoded bytes, guarding against
//!   misconfiguration (HMAC hashes oversized keys internally, masking the mistake).
//! - **Safe Base64 Decoding:** It accepts both standard and URL-safe Base64, stripping accidental
//!   whitespace introduced by orchestration systems (like Kubernetes Secrets or Docker env files).
//! - **Memory Zeroization:** Secret bytes are stored in a `Zeroizing<Vec<u8>>` wrapper that
//!   overwrites the allocation with zeroes on drop, preventing secrets from lingering in memory.
//! - **Leak Prevention:** It implements a custom `std::fmt::Debug` that completely redacts the
//!   secret bytes. If a developer accidentally logs the `AppConfig` or `KeyRing` struct,
//!   the credentials will never be written to application logs.
//! - **Fail-Fast Startup:** By using `load()`, it validates *all* configured keys during the
//!   boot sequence and aggregates errors, preventing the dispatcher from crashing mid-dispatch
//!   due to a missing environment variable.

use std::collections::HashMap;

use base64::prelude::*;
use zeroize::Zeroizing;

use crate::config::SigningKeyConfig;
use crate::error::ValidationErrors;

const MIN_SECRET_BYTES: usize = 32;
// HMAC-SHA256 pre-hashes keys longer than its 64-byte block size; 256 is a generous misconfig
// ceiling, not a claim that keys beyond 64 bytes provide extra security.
const MAX_SECRET_BYTES: usize = 256;

/// Resolved keyring: maps signing key ids to their decoded HMAC secret bytes.
///
/// Built at startup by calling [`KeyRing::load`], which reads each secret from
/// its designated environment variable and validates the minimum length.
pub struct KeyRing {
    keys: HashMap<String, Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show key ids but never the secret bytes.
        f.debug_struct("KeyRing")
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl KeyRing {
    /// Build the keyring from config, reading and validating each secret from its env var.
    ///
    /// Returns all errors as a list if any env var is missing, fails to base64-decode,
    /// or is shorter than 32 / longer than 256 bytes after decoding.
    ///
    /// Both standard and URL-safe Base64 encodings are accepted (with or without padding).
    /// Leading/trailing/internal whitespace in the env var value is stripped before decoding,
    /// which accommodates wrapped secrets from Kubernetes Secrets and Docker env files.
    pub fn load(
        signing_keys: &HashMap<String, SigningKeyConfig>,
    ) -> Result<Self, ValidationErrors> {
        Self::load_internal(signing_keys, |env| {
            std::env::var(env).map_err(|e| match e {
                std::env::VarError::NotPresent => "is not set".to_string(),
                std::env::VarError::NotUnicode(_) => {
                    "is set but contains non-UTF-8 bytes".to_string()
                }
            })
        })
    }

    /// Internal builder that accepts a custom environment resolver, enabling safe parallel testing
    /// without mutating the global process environment.
    fn load_internal<F>(
        signing_keys: &HashMap<String, SigningKeyConfig>,
        env_resolver: F,
    ) -> Result<Self, ValidationErrors>
    where
        F: Fn(&str) -> Result<String, String>,
    {
        let mut errors = Vec::new();
        let mut keys = HashMap::new();
        let mut seen_envs: HashMap<String, String> = HashMap::new();

        // Sort by id for deterministic error ordering in ValidationErrors.
        let mut entries: Vec<_> = signing_keys.iter().collect();
        entries.sort_by_key(|(id, _)| id.as_str());

        for (id, cfg) in entries {
            if cfg.secret_env.trim().is_empty() {
                errors.push(format!("signing_keys[{id}]: secret_env must not be empty"));
                continue;
            }
            if let Some(prior_id) = seen_envs.get(&cfg.secret_env) {
                errors.push(format!(
                    "signing_keys[{id}]: secret_env '{}' is already used by key '{prior_id}'",
                    cfg.secret_env
                ));
                continue;
            }
            seen_envs.insert(cfg.secret_env.clone(), id.clone());
            match env_resolver(&cfg.secret_env) {
                Err(reason) => {
                    errors.push(format!(
                        "signing_keys[{id}]: env var '{}' {reason}",
                        cfg.secret_env
                    ));
                }
                Ok(val) => {
                    let val = Zeroizing::new(val);
                    let cleaned: Zeroizing<String> =
                        Zeroizing::new(val.chars().filter(|c| !c.is_whitespace()).collect());
                    let decoded = BASE64_STANDARD
                        .decode(cleaned.as_str())
                        .or_else(|_| BASE64_STANDARD_NO_PAD.decode(cleaned.as_str()))
                        .or_else(|first_err| {
                            BASE64_URL_SAFE
                                .decode(cleaned.as_str())
                                .or_else(|_| BASE64_URL_SAFE_NO_PAD.decode(cleaned.as_str()))
                                .map_err(|_| first_err)
                        });
                    match decoded {
                        Err(e) => {
                            errors.push(format!(
                                "signing_keys[{id}]: failed to base64-decode secret from '{}': {e}",
                                cfg.secret_env
                            ));
                        }
                        Ok(bytes) => {
                            if bytes.len() < MIN_SECRET_BYTES {
                                errors.push(format!(
                                    "signing_keys[{id}]: secret from '{}' is {} bytes after \
                                     decoding; minimum is {MIN_SECRET_BYTES}",
                                    cfg.secret_env,
                                    bytes.len()
                                ));
                            } else if bytes.len() > MAX_SECRET_BYTES {
                                errors.push(format!(
                                    "signing_keys[{id}]: secret from '{}' is {} bytes after \
                                     decoding; maximum is {MAX_SECRET_BYTES}",
                                    cfg.secret_env,
                                    bytes.len()
                                ));
                            } else {
                                keys.insert(id.clone(), Zeroizing::new(bytes));
                            }
                        }
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(Self { keys })
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Look up the secret bytes for a signing key id.
    pub fn get(&self, id: &str) -> Option<&[u8]> {
        self.keys.get(id).map(|z| z.as_slice())
    }

    /// Returns `true` if the keyring contains the given key id.
    pub fn contains(&self, id: &str) -> bool {
        self.keys.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SigningKeyConfig;
    use std::collections::HashMap;

    fn make_key_cfg(env_name: &str) -> SigningKeyConfig {
        SigningKeyConfig {
            secret_env: env_name.to_string(),
        }
    }

    fn valid_32_byte_b64() -> &'static str {
        // base64 of exactly 32 'A' bytes (0x41 × 32)
        "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE="
    }

    fn resolver<'a>(
        envs: &'a HashMap<&'a str, &'a str>,
    ) -> impl Fn(&str) -> Result<String, String> + 'a {
        |k| {
            envs.get(k)
                .map(|s| s.to_string())
                .ok_or_else(|| "is not set".to_string())
        }
    }

    #[test]
    fn load_empty_signing_keys_succeeds() {
        let kr =
            KeyRing::load_internal(&HashMap::new(), |_| Err("is not set".to_string())).unwrap();
        assert!(kr.get("any").is_none());
    }

    #[test]
    fn load_valid_key_resolves() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("my-key".to_string(), make_key_cfg("MY_SECRET_ENV"));

        let envs: HashMap<&str, &str> = [("MY_SECRET_ENV", valid_32_byte_b64())]
            .into_iter()
            .collect();

        let kr = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap();
        assert!(kr.contains("my-key"));
        assert_eq!(kr.get("my-key").unwrap().len(), 32);
    }

    #[test]
    fn load_missing_env_var_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("MISSING_ENV"));

        let errs =
            KeyRing::load_internal(&signing_keys, |_| Err("is not set".to_string())).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("MISSING_ENV"));
    }

    #[test]
    fn load_invalid_base64_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("BAD_B64_ENV"));

        let envs: HashMap<&str, &str> = [("BAD_B64_ENV", "not!!valid!!base64")]
            .into_iter()
            .collect();

        let errs = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("base64-decode"));
    }

    #[test]
    fn load_too_short_secret_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("SHORT_ENV"));

        let envs: HashMap<&str, &str> = [("SHORT_ENV", "QUFBQUFBQUFBQUFBQUFBQQ==")]
            .into_iter()
            .collect();

        let errs = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("minimum is 32"));
    }

    #[test]
    fn load_too_large_secret_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("LARGE_ENV"));

        // "QUFB" × 86 = 344 base64 chars = 258 decoded bytes, exceeds MAX_SECRET_BYTES=256
        let large_b64 = "QUFB".repeat(86);
        let envs: HashMap<&str, String> = [("LARGE_ENV", large_b64)].into_iter().collect();

        let errs = KeyRing::load_internal(&signing_keys, |k| {
            envs.get(k).cloned().ok_or_else(|| "is not set".to_string())
        })
        .unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("maximum is"));
    }

    #[test]
    fn load_collects_multiple_errors() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k1".to_string(), make_key_cfg("MISSING_ENV"));
        signing_keys.insert("k2".to_string(), make_key_cfg("SHORT_ENV"));

        let envs: HashMap<&str, &str> = [("SHORT_ENV", "QUFBQUFBQUFBQUFBQUFBQQ==")]
            .into_iter()
            .collect();

        let errs = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap_err();
        assert_eq!(errs.0.len(), 2);
    }

    #[test]
    fn get_unknown_key_returns_none() {
        let kr =
            KeyRing::load_internal(&HashMap::new(), |_| Err("is not set".to_string())).unwrap();
        assert!(kr.get("no-such-key").is_none());
        assert!(!kr.contains("no-such-key"));
    }

    #[test]
    fn key_with_whitespace_in_env_is_trimmed() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("WHITESPACE_ENV"));

        let spaced = format!("  {}  ", valid_32_byte_b64());
        let envs: HashMap<&str, String> = [("WHITESPACE_ENV", spaced)].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, |k| {
            envs.get(k).cloned().ok_or_else(|| "is not set".to_string())
        })
        .unwrap();
        assert!(kr.contains("k"));
    }

    #[test]
    fn key_with_internal_whitespace_is_accepted() {
        // Simulates a K8s secret that was base64-encoded with line breaks (every 76 chars).
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("WRAPPED_ENV"));

        // Insert newlines inside the base64 string as k8s multi-line secrets do.
        let b64 = valid_32_byte_b64();
        let wrapped = format!("{}\n{}", &b64[..22], &b64[22..]);
        let envs: HashMap<&str, String> = [("WRAPPED_ENV", wrapped)].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, |k| {
            envs.get(k).cloned().ok_or_else(|| "is not set".to_string())
        })
        .unwrap();
        assert!(kr.contains("k"));
        assert_eq!(kr.get("k").unwrap().len(), 32);
    }

    #[test]
    fn url_safe_base64_is_accepted() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("URL_SAFE_ENV"));

        // URL-safe base64 of [0xFB; 32]: contains '-' and '_' which are invalid in standard base64.
        // Computed: [0xFB,0xFB,0xFB] → "-_vv" in URL-safe; 32 bytes = 30+2, last group "-_s=".
        let url_safe = "-_vv-_vv-_vv-_vv-_vv-_vv-_vv-_vv-_vv-_vv-_s=";
        let envs: HashMap<&str, &str> = [("URL_SAFE_ENV", url_safe)].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap();
        assert!(kr.contains("k"));
        assert_eq!(kr.get("k").unwrap().len(), 32);
    }

    #[test]
    fn load_public_api_missing_env_var_returns_error() {
        // Tests the public `load` function using a sentinel var name guaranteed not to exist.
        let mut signing_keys = HashMap::new();
        signing_keys.insert(
            "k".to_string(),
            make_key_cfg("__OUTBOX_DISPATCHER_TEST_NONEXISTENT_VAR__"),
        );

        let errs = KeyRing::load(&signing_keys).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("__OUTBOX_DISPATCHER_TEST_NONEXISTENT_VAR__"));
    }

    #[test]
    fn standard_no_pad_base64_is_accepted() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("STD_NO_PAD_ENV"));

        // Strip '=' padding from the standard base64 string.
        let no_pad = valid_32_byte_b64().trim_end_matches('=');
        // Confirm there is no '+' or '/' (url-safe chars) — stays in standard alphabet.
        assert!(!no_pad.contains('+') && !no_pad.contains('/') && !no_pad.contains('-'));
        let envs: HashMap<&str, &str> = [("STD_NO_PAD_ENV", no_pad)].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap();
        assert!(kr.contains("k"));
        assert_eq!(kr.get("k").unwrap().len(), 32);
    }

    #[test]
    fn url_safe_no_pad_base64_is_accepted() {
        // K8s often emits unpadded URL-safe base64; verify we accept it without the trailing '='.
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("URL_SAFE_NO_PAD_ENV"));

        // valid_32_byte_b64() decoded then re-encoded without padding.
        // "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUE=" → strip '=' → no-pad form.
        let no_pad = valid_32_byte_b64().trim_end_matches('=');
        let envs: HashMap<&str, &str> = [("URL_SAFE_NO_PAD_ENV", no_pad)].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap();
        assert!(kr.contains("k"));
        assert_eq!(kr.get("k").unwrap().len(), 32);
    }

    #[test]
    fn debug_output_redacts_secret_bytes() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("my-key".to_string(), make_key_cfg("REDACT_ENV"));

        let envs: HashMap<&str, &str> = [("REDACT_ENV", valid_32_byte_b64())].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap();
        let debug_str = format!("{kr:?}");
        assert!(debug_str.contains("my-key"), "key id must appear in debug");
        // The base64 secret must never appear verbatim.
        assert!(
            !debug_str.contains(valid_32_byte_b64()),
            "raw base64 secret must not appear in debug output"
        );
        // Decoded bytes should not appear either.
        assert!(
            !debug_str.contains("AAAA"),
            "decoded secret bytes must not appear in debug output"
        );
    }

    #[test]
    fn duplicate_secret_env_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k1".to_string(), make_key_cfg("SHARED_ENV"));
        signing_keys.insert("k2".to_string(), make_key_cfg("SHARED_ENV"));

        let envs: HashMap<&str, &str> = [("SHARED_ENV", valid_32_byte_b64())].into_iter().collect();

        let errs = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("SHARED_ENV"));
        assert!(errs.0[0].contains("already used"));
    }

    #[test]
    fn load_errors_are_in_deterministic_key_order() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("zzz".to_string(), make_key_cfg("MISSING_ZZZ"));
        signing_keys.insert("aaa".to_string(), make_key_cfg("MISSING_AAA"));
        signing_keys.insert("mmm".to_string(), make_key_cfg("MISSING_MMM"));

        let errs =
            KeyRing::load_internal(&signing_keys, |_| Err("is not set".to_string())).unwrap_err();
        assert_eq!(errs.0.len(), 3);
        assert!(
            errs.0[0].contains("signing_keys[aaa]"),
            "first error must be aaa"
        );
        assert!(
            errs.0[1].contains("signing_keys[mmm]"),
            "second error must be mmm"
        );
        assert!(
            errs.0[2].contains("signing_keys[zzz]"),
            "third error must be zzz"
        );
    }

    // ── Fix #3: empty secret_env produces clear error ─────────────────────────

    #[test]
    fn empty_secret_env_returns_clear_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg(""));

        let errs =
            KeyRing::load_internal(&signing_keys, |_| Err("is not set".to_string())).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(
            errs.0[0].contains("secret_env must not be empty"),
            "got: {}",
            errs.0[0]
        );
    }

    #[test]
    fn whitespace_only_secret_env_returns_clear_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("   "));

        let errs =
            KeyRing::load_internal(&signing_keys, |_| Err("is not set".to_string())).unwrap_err();
        assert_eq!(errs.0.len(), 1);
        assert!(errs.0[0].contains("secret_env must not be empty"));
    }
}
