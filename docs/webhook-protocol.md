# Webhook Protocol

This document describes the HTTP protocol between `outbox-dispatcher` and webhook receivers.

## Overview

The dispatcher sends an HTTPS POST to each callback URL defined in an event's `callbacks` JSONB array. Receivers reply with an HTTP status code. The dispatcher interprets the response and updates delivery state accordingly.

## Request format

```
POST /your/configured/path HTTP/1.1
Host: your-service.example.com
Content-Type: application/json
User-Agent: outbox-dispatcher/1.0
X-Outbox-Event-Id: 4d3e1c8a-0000-0000-0000-000000000001
X-Outbox-Delivery-Id: 4815162342
X-Outbox-Callback-Name: welcome_email
X-Outbox-Kind: user.registered@v1
X-Outbox-Mode: managed
X-Outbox-Attempt: 1
X-Outbox-Signing-Key-Id: welcome-v1
X-Outbox-Signature: t=1714229400,v1=a3f5c2...

{
  "delivery_id": 4815162342,
  "event_id": "4d3e1c8a-0000-0000-0000-000000000001",
  "kind": "user.registered@v1",
  "callback_name": "welcome_email",
  "mode": "managed",
  "aggregate_type": "user",
  "aggregate_id": "11111111-0000-0000-0000-000000000001",
  "payload": { "user_id": "...", "email": "alice@example.com" },
  "metadata": {},
  "actor_id": null,
  "correlation_id": "aaaa0000-0000-0000-0000-000000000001",
  "causation_id": null,
  "created_at": "2026-04-27T18:30:00Z",
  "attempt": 1
}
```

Headers duplicate selected body fields for receivers that want to route or filter before parsing the body. The JSON body is the canonical source of truth.

### Request headers

| Header | Description |
|--------|-------------|
| `X-Outbox-Event-Id` | UUID of the source event |
| `X-Outbox-Delivery-Id` | `outbox_deliveries.id` — use this to mark external completions |
| `X-Outbox-Callback-Name` | Name of this callback within the event |
| `X-Outbox-Kind` | Event kind string (e.g. `user.registered@v1`) |
| `X-Outbox-Mode` | `managed` or `external` |
| `X-Outbox-Attempt` | Attempt number, starting at 1. `> 1` means this is a retry. |
| `X-Outbox-Signing-Key-Id` | Key id used to compute the signature |
| `X-Outbox-Signature` | HMAC-SHA256 signature — see §Signing |
| Custom headers | Any headers defined in the callback's `headers` map |

### Body fields

| Field | Type | Description |
|-------|------|-------------|
| `delivery_id` | integer | `outbox_deliveries.id` |
| `event_id` | UUID string | Source event identifier |
| `kind` | string | Event kind |
| `callback_name` | string | Callback name |
| `mode` | `"managed"` \| `"external"` | Completion mode |
| `aggregate_type` | string | Entity type |
| `aggregate_id` | UUID string | Entity identifier |
| `payload` | object | Event payload (opaque to the dispatcher) |
| `metadata` | object | Non-semantic context |
| `actor_id` | UUID string \| null | Who triggered the event |
| `correlation_id` | UUID string \| null | Distributed-tracing correlation |
| `causation_id` | UUID string \| null | Distributed-tracing causation |
| `created_at` | RFC 3339 string | Event insert time |
| `attempt` | integer | Same as `X-Outbox-Attempt` |

## Signing

When a callback specifies a `signing_key_id`, the dispatcher signs the request body with HMAC-SHA256.

### Signature computation

```
signing_string = "{unix_timestamp_seconds}.{raw_request_body}"
signature      = HMAC-SHA256(secret, signing_string)
header_value   = "t={timestamp},v1={hex(signature)}"
```

- `timestamp` is Unix seconds at HTTP send time.
- `secret` is the raw bytes decoded from the base64 value in the named env var.
- The signing string is computed by streaming `"{ts}."` then the raw body bytes — no full string allocation.

### Verification (receiver side)

1. Parse `t=` from `X-Outbox-Signature`.
2. Reject if `|now - t| > tolerance` (default: 300 seconds). Tune to your clock-sync confidence.
3. Compute `HMAC-SHA256(secret, "{t}.{raw_body}")`.
4. Compare with the `v1=` hex digest using **constant-time comparison**.
5. Reject if they do not match.

Publisher libraries (`outbox-publisher-rs`, `outbox-publisher-java`) provide ready-made helpers for step 1–5.

> **Never use string equality for signature comparison.** Timing attacks against `==` on short strings are realistic. Use the constant-time helper in your language's crypto library.

### Replay tolerance

Tolerance is a receiver-side configuration. 300 seconds is fine for NTP-synced hosts in the same region. Cross-region or weakly-synced environments may need 600–900 seconds. Do not rely on a header from the dispatcher to tell you what tolerance to use — that would undermine the signature's purpose.

## Response interpretation

| Response | Interpretation |
|----------|----------------|
| `2xx` | Success. Managed mode: `dispatched_at = now()`, `processed_at = now()`. External mode: `dispatched_at = now()` only. |
| Non-2xx (4xx, 5xx, 408, 429, …) | Transient failure. `attempts++`, apply backoff. Dead-letter if `attempts ≥ max_attempts`. |
| `3xx` redirect | Treated as transient failure. The dispatcher never follows redirects. |
| Connection error / timeout | Transient failure. Same path as non-2xx. |

There is no permanent-failure response code. Every non-2xx response burns one retry. If you need to short-circuit a callback from the receiver side, return a non-2xx and ask an operator to run `POST /v1/deliveries/{id}/abandon` from the admin API.

### `Retry-After` header

If the receiver returns `429 Too Many Requests` or `503 Service Unavailable` with a `Retry-After` header, the dispatcher parses it (integer seconds or RFC 7231 HTTP-date) and uses it as the floor for `available_at` on the next retry. The configured backoff schedule still applies if it produces a later time.

## Managed mode

The receiver's job is simple: process the event, return `2xx`. The dispatcher considers the delivery complete.

Receiver requirements:
1. Verify the signature before reading the body.
2. Verify the timestamp is within tolerance.
3. Be idempotent on `event_id`. The dispatcher guarantees **at-least-once** delivery, not exactly-once. Use `attempt > 1` as a hint that this is a retry.
4. Respond within `timeout_seconds` (default 30s). Timeout is treated as transient.
5. Do not return 3xx — the dispatcher will not follow redirects.

## External mode

Use external mode when processing requires asynchronous work after the webhook ack (e.g. sending an email, calling a downstream service, waiting for a human action).

The dispatcher sends the same webhook. The receiver acks with `2xx` to acknowledge receipt, then completes the work and notifies the dispatcher when done.

### Marking a delivery complete

**Option 1 — Direct SQL (preferred):**

```sql
UPDATE outbox_deliveries
SET processed_at = COALESCE(processed_at, now())
WHERE id = $1;
```

`COALESCE` makes the call idempotent. The receiver's database role needs:

```sql
GRANT UPDATE (processed_at) ON outbox_deliveries TO your_role;
```

**Option 2 — Admin API:**

```bash
curl -X POST http://dispatcher:9090/v1/deliveries/{delivery_id}/complete \
     -H "Authorization: Bearer $ADMIN_TOKEN"
```

Use this when the receiver cannot reach Postgres directly.

### External completion timeout

If the callback specifies `external_completion_timeout_seconds`, the dispatcher will redeliver the webhook if `processed_at` is not written within the timeout window. The redelivered request has a higher `attempt` value. Receivers must be idempotent against this.

The number of redelivery cycles is bounded by `max_completion_cycles` (default 20). After exhaustion, the delivery is dead-lettered with `last_error = "external_completion_cycles_exhausted"`.

### Edge case: completion before 2xx

A receiver in external mode may write `processed_at` synchronously before returning 2xx. The dispatcher's success update sets only `dispatched_at` (not `processed_at`) in external mode, so no overwrite occurs. The row ends up with `processed_at < dispatched_at` by milliseconds — unusual but harmless.

## Custom headers

Callbacks may include static headers to be sent with every webhook:

```json
{
  "name": "welcome_email",
  "url": "https://api.example.com/webhooks/welcome",
  "headers": {
    "X-Service": "engram-api",
    "X-Environment": "production"
  }
}
```

Restrictions:
- Header names must be valid RFC 7230 token characters.
- These header names are reserved and rejected: `Authorization`, `Cookie`, `Content-Type`, `Content-Length`, `Host`, `User-Agent`.
- Any header prefixed `X-Outbox-` is reserved for the dispatcher and rejected.
- Control characters in names or values are rejected.
