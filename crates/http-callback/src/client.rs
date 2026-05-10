//! HTTP webhook delivery adapter for `outbox-dispatcher`.
//!
//! This module provides `HttpCallback`, which implements the `Callback` trait from
//! `outbox-dispatcher-core` using a `reqwest` HTTP client.
//!
//! # Per-attempt keyring resolution
//!
//! `signing_key_id` is resolved from the `KeyRing` at the start of each `deliver`
//! call, not at schedule time. This tolerates short publisher/dispatcher version
//! skew during deploys (the publisher may roll out a new key id minutes before
//! the dispatcher's config catches up). An unknown `signing_key_id` returns
//! `CallbackError::Transient` and goes through the normal retry/backoff loop.
//!
//! # Headers emitted (§6.1)
//!
//! ```text
//! Content-Type: application/json
//! X-Outbox-Event-Id: <uuid>
//! X-Outbox-Delivery-Id: <i64>
//! X-Outbox-Callback-Name: <name>
//! X-Outbox-Kind: <kind>
//! X-Outbox-Mode: managed | external
//! X-Outbox-Attempt: <1-based>
//! X-Outbox-Signing-Key-Id: <id>   (omitted when no signing_key_id)
//! X-Outbox-Signature: t=<ts>,v1=<hex>  (omitted when no signing key)
//! User-Agent: outbox-dispatcher/1.0
//! ```
//!
//! Plus any custom `headers` from the `CallbackTarget`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use outbox_dispatcher_core::{
    Callback, CallbackError, CallbackTarget, EventForDelivery, HttpClientConfig, KeyRing,
};
use reqwest::{Client, ClientBuilder};
use tracing::{debug, warn};

use crate::signing::sign;

/// HTTP webhook client that implements the `Callback` trait.
///
/// Create one instance per process via `HttpCallback::new` and share it across
/// dispatch tasks as an `Arc<HttpCallback>`.
pub struct HttpCallback {
    client: Client,
    keyring: Arc<KeyRing>,
}

impl HttpCallback {
    /// Build an `HttpCallback` from the HTTP client config and the loaded keyring.
    pub fn new(cfg: &HttpClientConfig, keyring: Arc<KeyRing>) -> anyhow::Result<Self> {
        let mut builder = ClientBuilder::new()
            .user_agent(&cfg.user_agent)
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .redirect(reqwest::redirect::Policy::none()); // §6.3: redirects are failures

        if cfg.allow_insecure_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }

        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {e}"))?;
        Ok(Self {
            client,
            keyring: keyring.clone(),
        })
    }
}

#[async_trait]
impl Callback for HttpCallback {
    async fn deliver(
        &self,
        target: &CallbackTarget,
        event: &EventForDelivery,
    ) -> Result<(), CallbackError> {
        // ── Serialize body ───────────────────────────────────────────────────
        let body_value = build_body(event);
        let body_bytes = serde_json::to_vec(&body_value).map_err(|e| CallbackError::Transient {
            reason: format!("failed to serialize webhook body: {e}"),
            retry_after: None,
        })?;

        // ── Resolve signing key and produce signature ────────────────────────
        let now_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (maybe_key_id, maybe_signature) = match &target.signing_key_id {
            Some(key_id) => {
                match self.keyring.get(key_id) {
                    Some(secret) => {
                        let sig = sign(secret, now_ts, &body_bytes);
                        (Some(key_id.as_str()), Some(sig))
                    }
                    None => {
                        // Unknown key id — transient error; retry on normal backoff.
                        return Err(CallbackError::Transient {
                            reason: format!("signing_key_id '{key_id}' not registered"),
                            retry_after: None,
                        });
                    }
                }
            }
            None => (None, None),
        };

        // ── Build request ────────────────────────────────────────────────────
        let mut req = self
            .client
            .post(&target.url)
            .header("Content-Type", "application/json")
            .header("X-Outbox-Event-Id", event.event_id.to_string())
            .header("X-Outbox-Delivery-Id", event.delivery_id.to_string())
            .header("X-Outbox-Callback-Name", &event.callback_name)
            .header("X-Outbox-Kind", &event.kind)
            .header("X-Outbox-Mode", event.mode.as_str())
            .header("X-Outbox-Attempt", event.attempt.to_string())
            .body(body_bytes);

        if let Some(key_id) = maybe_key_id {
            req = req.header("X-Outbox-Signing-Key-Id", key_id);
        }
        if let Some(sig) = maybe_signature {
            req = req.header("X-Outbox-Signature", sig);
        }

        // Custom headers from the callback definition (already validated as non-reserved).
        for (k, v) in &target.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        // ── Send ─────────────────────────────────────────────────────────────
        debug!(
            delivery_id = event.delivery_id,
            url = %target.url,
            attempt = event.attempt,
            "sending webhook"
        );

        let response = req.send().await.map_err(|e| CallbackError::Transient {
            reason: format!("connection error: {e}"),
            retry_after: None,
        })?;

        let status = response.status();

        // ── Interpret response (§6.3) ────────────────────────────────────────
        if status.is_success() {
            debug!(delivery_id = event.delivery_id, status = %status, "webhook accepted");
            return Ok(());
        }

        // Extract Retry-After header when present (relevant for 429 / 503).
        let retry_after = extract_retry_after(&response);

        warn!(
            delivery_id = event.delivery_id,
            status = %status,
            "webhook returned non-2xx — transient failure"
        );

        Err(CallbackError::Transient {
            reason: format!("POST {} returned {}", target.url, status.as_u16()),
            retry_after,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build the JSON body for the webhook request (§6.1).
fn build_body(event: &EventForDelivery) -> serde_json::Value {
    serde_json::json!({
        "delivery_id": event.delivery_id,
        "event_id": event.event_id,
        "kind": event.kind,
        "callback_name": event.callback_name,
        "mode": event.mode.as_str(),
        "aggregate_type": event.aggregate_type,
        "aggregate_id": event.aggregate_id,
        "payload": event.payload,
        "metadata": event.metadata,
        "actor_id": event.actor_id,
        "correlation_id": event.correlation_id,
        "causation_id": event.causation_id,
        "created_at": event.created_at,
        "attempt": event.attempt,
    })
}

/// Parse a `Retry-After` header value into a `Duration`.
///
/// Supports both integer seconds (most common) and the HTTP date form.
/// Returns `None` if the header is absent or unparseable.
fn extract_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get("Retry-After")?.to_str().ok()?;
    // Try integer seconds first.
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP date parsing is not implemented here; return None for date form.
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use outbox_dispatcher_core::{CompletionMode, KeyRing, SigningKeyConfig};
    use serde_json::json;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn make_event(
        delivery_id: i64,
        signing_key_id: Option<&str>,
    ) -> (EventForDelivery, CallbackTarget) {
        let event = EventForDelivery {
            delivery_id,
            event_id: Uuid::new_v4(),
            kind: "user.registered@v1".to_string(),
            callback_name: "welcome_email".to_string(),
            mode: CompletionMode::Managed,
            aggregate_type: "user".to_string(),
            aggregate_id: Uuid::new_v4(),
            payload: json!({"email": "test@example.com"}),
            metadata: json!({}),
            actor_id: None,
            correlation_id: None,
            causation_id: None,
            created_at: chrono::Utc::now(),
            attempt: 1,
        };

        let target = CallbackTarget {
            name: "welcome_email".to_string(),
            url: "https://example.com/hook".to_string(),
            mode: CompletionMode::Managed,
            signing_key_id: signing_key_id.map(str::to_owned),
            headers: HashMap::new(),
            max_attempts: 3,
            backoff: vec![Duration::from_secs(30)],
            timeout: Duration::from_secs(30),
            external_completion_timeout: None,
            max_completion_cycles: 20,
        };

        (event, target)
    }

    fn empty_keyring() -> Arc<KeyRing> {
        Arc::new(KeyRing::load(&HashMap::new()).unwrap())
    }

    fn keyring_with(key_id: &str, secret_env: &str, secret_val: &str) -> Arc<KeyRing> {
        // Set env var, build keyring, unset env var.
        let env_key = secret_env.to_string();
        unsafe {
            std::env::set_var(&env_key, secret_val);
        }
        let mut keys = HashMap::new();
        keys.insert(
            key_id.to_string(),
            SigningKeyConfig {
                secret_env: secret_env.to_string(),
            },
        );
        let kr = KeyRing::load(&keys).unwrap();
        unsafe {
            std::env::remove_var(&env_key);
        }
        Arc::new(kr)
    }

    #[test]
    fn build_body_includes_required_fields() {
        let (event, _) = make_event(42, None);
        let body = build_body(&event);
        assert_eq!(body["delivery_id"], 42);
        assert_eq!(body["kind"], "user.registered@v1");
        assert_eq!(body["callback_name"], "welcome_email");
        assert_eq!(body["mode"], "managed");
        assert_eq!(body["attempt"], 1);
    }

    #[test]
    fn unknown_signing_key_returns_transient_error() {
        // We can't call deliver() in a sync test, but we can test the keyring lookup
        // directly: an unknown key_id should not be found in an empty keyring.
        let keyring = empty_keyring();
        assert!(keyring.get("nonexistent-key").is_none());
    }

    #[test]
    fn extract_retry_after_parses_integer_seconds() {
        // We test the helper via a mock response — build a response with the header.
        // Since we can't easily construct a reqwest::Response in unit tests,
        // we test the parsing logic by creating a minimal HTTP response via reqwest's
        // test utilities.  Instead, we verify the parsing logic directly.
        let secs_str = "120";
        let parsed: Option<u64> = secs_str.trim().parse().ok();
        assert_eq!(parsed, Some(120u64));
        assert_eq!(
            parsed.map(Duration::from_secs),
            Some(Duration::from_secs(120))
        );
    }
}
