use std::collections::HashMap;

use base64::prelude::*;

use crate::config::SigningKeyConfig;

/// Resolved keyring: maps signing key ids to their decoded HMAC secret bytes.
///
/// Built at startup by calling [`KeyRing::load`], which reads each secret from
/// its designated environment variable and validates the minimum length.
pub struct KeyRing {
    keys: HashMap<String, Vec<u8>>,
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
    /// or is shorter than 32 bytes after decoding.
    pub fn load(signing_keys: &HashMap<String, SigningKeyConfig>) -> Result<Self, Vec<String>> {
        Self::load_internal(signing_keys, |env| std::env::var(env))
    }

    /// Internal builder that accepts a custom environment resolver, enabling safe parallel testing
    /// without mutating the global process environment.
    fn load_internal<F, E>(
        signing_keys: &HashMap<String, SigningKeyConfig>,
        env_resolver: F,
    ) -> Result<Self, Vec<String>>
    where
        F: Fn(&str) -> Result<String, E>,
    {
        let mut errors = Vec::new();
        let mut keys = HashMap::new();

        for (id, cfg) in signing_keys {
            match env_resolver(&cfg.secret_env) {
                Err(_) => {
                    errors.push(format!(
                        "signing_keys[{id}]: env var '{}' is not set",
                        cfg.secret_env
                    ));
                }
                Ok(val) => match BASE64_STANDARD.decode(val.trim()) {
                    Err(e) => {
                        errors.push(format!(
                            "signing_keys[{id}]: failed to base64-decode secret from '{}': {e}",
                            cfg.secret_env
                        ));
                    }
                    Ok(bytes) if bytes.len() < 32 => {
                        errors.push(format!(
                            "signing_keys[{id}]: secret from '{}' is {} bytes after decoding; minimum is 32",
                            cfg.secret_env,
                            bytes.len()
                        ));
                    }
                    Ok(bytes) => {
                        keys.insert(id.clone(), bytes);
                    }
                },
            }
        }

        if errors.is_empty() {
            Ok(Self { keys })
        } else {
            Err(errors)
        }
    }

    /// Look up the secret bytes for a signing key id.
    pub fn get(&self, id: &str) -> Option<&[u8]> {
        self.keys.get(id).map(Vec::as_slice)
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
    ) -> impl Fn(&str) -> Result<String, ()> + 'a {
        |k| envs.get(k).map(|s| s.to_string()).ok_or(())
    }

    #[test]
    fn load_empty_signing_keys_succeeds() {
        let kr = KeyRing::load_internal::<_, ()>(&HashMap::new(), |_| Err(())).unwrap();
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

        let errs = KeyRing::load_internal::<_, ()>(&signing_keys, |_| Err(())).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("MISSING_ENV"));
    }

    #[test]
    fn load_invalid_base64_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("BAD_B64_ENV"));

        let envs: HashMap<&str, &str> = [("BAD_B64_ENV", "not!!valid!!base64")]
            .into_iter()
            .collect();

        let errs = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("base64-decode"));
    }

    #[test]
    fn load_too_short_secret_returns_error() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("SHORT_ENV"));

        let envs: HashMap<&str, &str> = [("SHORT_ENV", "QUFBQUFBQUFBQUFBQUFBQQ==")]
            .into_iter()
            .collect();

        let errs = KeyRing::load_internal(&signing_keys, resolver(&envs)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("minimum is 32"));
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
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn get_unknown_key_returns_none() {
        let kr = KeyRing::load_internal::<_, ()>(&HashMap::new(), |_| Err(())).unwrap();
        assert!(kr.get("no-such-key").is_none());
        assert!(!kr.contains("no-such-key"));
    }

    #[test]
    fn key_with_whitespace_in_env_is_trimmed() {
        let mut signing_keys = HashMap::new();
        signing_keys.insert("k".to_string(), make_key_cfg("WHITESPACE_ENV"));

        let spaced = format!("  {}  ", valid_32_byte_b64());
        let envs: HashMap<&str, String> = [("WHITESPACE_ENV", spaced)].into_iter().collect();

        let kr = KeyRing::load_internal(&signing_keys, |k| envs.get(k).cloned().ok_or(())).unwrap();
        assert!(kr.contains("k"));
    }
}
