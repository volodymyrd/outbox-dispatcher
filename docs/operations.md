# Operations Guide

Day-to-day runbook for `outbox-dispatcher` operators.

## Admin API

All protected endpoints require `Authorization: Bearer <ADMIN_TOKEN>`.

Base URL: `http://your-dispatcher:9090`

### Health and readiness

```bash
# Liveness — always 200 if the process is alive
curl http://dispatcher:9090/health

# Readiness — checks DB ping, LISTEN status, scheduler heartbeat
curl http://dispatcher:9090/ready
```

`/ready` returns `503` with `{"status":"not_ready"}` if any of the following is true:
- The database ping fails.
- The LISTEN/NOTIFY connection to Postgres is down.
- The scheduler has not completed a cycle within `2 × poll_interval`.

Use `/health` for liveness probes and `/ready` for readiness probes.

### Stats

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     http://dispatcher:9090/v1/stats
```

Returns aggregate counts of pending, dispatched, processed, and dead-lettered deliveries, plus the age of the oldest pending delivery.

### Dead letters

List deliveries that have exhausted all retry attempts:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/dead-letters?limit=50"
```

Filter by callback name:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/dead-letters?callback_name=welcome_email&limit=50"
```

Paginate using `before` (the `id` of the last row from the previous page):

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/dead-letters?before=12345&limit=50"
```

The response includes a `next_cursor` field. If `null`, there are no more pages.

### External pending

List external-mode deliveries that have been dispatched but not yet completed by the receiver:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/external-pending?limit=50"
```

Filter rows older than a threshold (seconds):

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/external-pending?older_than_secs=3600"
```

### Event detail

Fetch a full event row with all of its delivery rows:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/events/<event_uuid>"
```

Useful for diagnosing why a particular event was not delivered, inspecting `last_error`, etc.

## Delivery operations

### Retry a dead-lettered delivery

Resets `attempts`, `dead_letter`, `dispatched_at`, `processed_at`, `locked_until`, and `completion_cycles` to their initial state. The row re-enters the pending queue immediately.

```bash
curl -X POST \
     -H "Authorization: Bearer $ADMIN_TOKEN" \
     http://dispatcher:9090/v1/deliveries/<delivery_id>/retry
```

Returns:
- `200 {"ok": true}` — queued for retry.
- `404` — delivery not found.
- `409` — delivery is currently locked (in flight). Wait for the lock to expire and retry.

### Complete an external delivery

Marks a delivery as fully processed without redelivery. Equivalent to the receiver writing `processed_at` directly.

```bash
curl -X POST \
     -H "Authorization: Bearer $ADMIN_TOKEN" \
     http://dispatcher:9090/v1/deliveries/<delivery_id>/complete
```

Returns `200 {"ok": true}` or `404`.

### Abandon a delivery

Dead-letters a delivery immediately, regardless of remaining attempts. Use when you know the receiver will never succeed (e.g., the callback URL is decommissioned).

```bash
curl -X POST \
     -H "Authorization: Bearer $ADMIN_TOKEN" \
     http://dispatcher:9090/v1/deliveries/<delivery_id>/abandon
```

Returns `200 {"ok": true}` or `404`.

## Metrics (Prometheus)

Scrape endpoint: `http://dispatcher:9091/metrics` (no auth)

### Key metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `outbox_events_total` | Counter | `kind` | Events observed by the scheduler |
| `outbox_deliveries_total` | Counter | `callback`, `mode`, `result` | All delivery outcomes (`result` ∈ `ok`/`transient`/`timeout`/`invalid`/`external_reset`) |
| `outbox_dispatch_duration_seconds` | Histogram | `callback`, `mode` | HTTP round-trip duration |
| `outbox_lag_seconds` | Gauge | — | Age of the oldest pending delivery |
| `outbox_pending_deliveries` | Gauge | `callback`, `mode` | Current pending delivery count |
| `outbox_external_pending_deliveries` | Gauge | `callback` | External deliveries awaiting completion |
| `outbox_external_pending_seconds` | Histogram | `callback` | Age distribution of external-pending rows |
| `outbox_dead_letters_total` | Counter | `callback` | Deliveries reaching dead-letter state |
| `outbox_external_timeout_resets_total` | Counter | `callback` | External completions that timed out and were reset |
| `outbox_completion_cycles_exhausted_total` | Counter | `callback` | External deliveries dead-lettered after max cycles |
| `outbox_signing_key_resolution_failures_total` | Counter | `signing_key_id`, `callback` | Unknown signing key at dispatch time |
| `outbox_invalid_callbacks_total` | Counter | `reason` | Schedule-time invalid callbacks |
| `outbox_payload_size_rejections_total` | Counter | `kind` | Events rejected for exceeding `payload_size_limit_bytes` |
| `outbox_retention_deletions_total` | Counter | `reason` | Rows deleted by the retention worker (`reason` ∈ `processed`/`dead_letter`) |
| `outbox_retention_cycle_duration_seconds` | Histogram | — | Retention worker cycle duration |
| `outbox_retention_oldest_event_age_seconds` | Gauge | — | Age of oldest terminal event still on disk (NaN when none) |
| `outbox_corrupted_rows_total` | Counter | `stage` | Poison-pill rows skipped (`stage` ∈ `schedule`/`dispatch`/`sweep`/`retention`) |
| `outbox_cycle_duration_seconds` | Histogram | — | Scheduler loop duration (use `_count` as a liveness counter) |
| `outbox_listener_connection_status` | Gauge | — | LISTEN connection up (1) or down (0) |

### Alerting recommendations

| Alert | Condition | Severity |
|-------|-----------|----------|
| Dead letters accumulating | `rate(outbox_dead_letters_total[5m]) > 0` | Warning |
| High dispatch lag | `outbox_lag_seconds > 300` | Warning |
| Listener down | `outbox_listener_connection_status == 0 for 2m` | Critical |
| Scheduler stalled | `rate(outbox_cycle_duration_seconds_count[2 * poll_interval]) == 0` | Critical |
| Signing key drift | `rate(outbox_signing_key_resolution_failures_total[5m]) > 0` | Warning |
| Corrupted rows | `rate(outbox_corrupted_rows_total[5m]) > 0` | Warning |

## Common issues

### Dead letters appearing for a callback

1. Check `last_error` via the admin API:
   ```bash
   curl -H "Authorization: Bearer $ADMIN_TOKEN" \
        "http://dispatcher:9090/v1/dead-letters?callback_name=my_callback"
   ```
2. Common errors:
   - `signing_key_id 'foo' not registered` — the key is missing from `[signing_keys]`. Add it and send a SIGHUP or restart.
   - `HTTP 5xx …` — the receiver is returning errors. Check receiver logs.
   - `callback timeout` — the receiver is too slow. Increase `timeout_seconds` in the callback definition or optimize the receiver.
   - `invalid_callback: …` — the callback JSON in `outbox_events.callbacks` failed structural validation. Fix the publisher.

3. Retry once the underlying issue is resolved:
   ```bash
   curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
        http://dispatcher:9090/v1/deliveries/<id>/retry
   ```

### Signing key rotation

1. Generate a new secret and update the env var (or Kubernetes secret).
2. Add the new key id to `[signing_keys]` in config (or update the existing entry to point to the new env var).
3. Send SIGHUP or restart the dispatcher. The new key takes effect on the next delivery attempt.
4. Publishers can start writing the new key id immediately after the dispatcher config is updated.
5. Keep the old key id in `[signing_keys]` (pointing to the old secret) until all in-flight deliveries using it have completed.

> Removing a key id while deliveries still reference it will cause those deliveries to retry with `signing_key_id 'foo' not registered` until they dead-letter.

### Backfill after a gap

If the dispatcher was down and events were written during the outage, deliveries are created automatically when the scheduler catches up. No manual action is needed — the cursor advances through the backlog on the next cycle.

To trigger an immediate backfill from a specific timestamp:

```bash
outbox-dispatcher rescan --since 2026-04-01T00:00:00Z
```

### Stuck external-mode delivery

If a receiver never wrote `processed_at` and the delivery is not auto-redelivered (no `external_completion_timeout_seconds` was set):

```bash
# Option 1: mark it complete (do not redeliver)
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
     http://dispatcher:9090/v1/deliveries/<id>/complete

# Option 2: reset it for redelivery
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
     http://dispatcher:9090/v1/deliveries/<id>/retry
```

### Corrupted row in `outbox_events.callbacks`

The dispatcher logs at `error` and increments `outbox_corrupted_rows_total{stage="schedule"}` when it encounters a row whose `callbacks` JSONB is structurally malformed (not a non-empty array, or contains an element that cannot be deserialized).

The row is skipped; the cursor advances. To inspect it:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
     "http://dispatcher:9090/v1/events/<event_uuid>"
```

Removal of the corrupted row is an operator decision — the dispatcher never deletes events.

## Retention

The retention worker is disabled by default. Enable it in config:

```toml
[retention]
enabled = true
processed_retention_days = 7    # delete fully-processed events after 7 days
dead_letter_retention_days = 30  # delete dead-lettered events after 30 days
cleanup_interval_secs = 3600     # run hourly
batch_limit = 1000               # max rows per cycle
```

The worker deletes `outbox_events` rows whose deliveries are all in a terminal state (all `processed_at != NULL`, or all `dead_letter = TRUE`) older than the configured retention window. ON DELETE CASCADE removes the corresponding `outbox_deliveries` rows.

Monitor via `outbox_retention_deletions_total{reason}` and `outbox_retention_oldest_event_age_seconds`.

## SIGHUP (hot reload)

Send SIGHUP to reload config without a restart:

```bash
kill -HUP <dispatcher_pid>
# or with Docker:
docker kill --signal=SIGHUP outbox-dispatcher
```

The following settings take effect immediately:
- `[signing_keys]` keyring
- `[dispatch]` defaults (poll interval, batch size, backoff, timeouts, etc.)
- `[observability]` settings

Database connection settings require a restart.
