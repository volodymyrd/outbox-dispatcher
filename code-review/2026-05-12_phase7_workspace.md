# Code Review — PR #6 (Phase 7: observability, OpenTelemetry, retention worker)

**Date:** 2026-05-12T17:47:12Z
**Branch:** phase7
**Reviewed by:** Claude (review command)
**Scope:** full workspace — diff vs `main`
(`crates/core/src/metrics.rs`, `crates/core/src/retention.rs`, `crates/core/src/config.rs`,
`crates/core/src/repo.rs`, `crates/core/src/dispatch.rs`, `crates/core/src/scheduler.rs`,
`crates/core/src/timeout_sweep.rs`, `crates/bin/src/main.rs`)

---

## Findings

### Finding 1 — Spec metrics declared but never emitted

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/metrics.rs:114-147,174-181` |
| **Severity** | High |
| **Category** | Correctness |

**Problem**

Five metrics required by §12.1 are exported as constants with helper functions, but no
call site in the workspace ever invokes them:

- `outbox_lag_seconds` — `set_lag_seconds`
- `outbox_pending_deliveries` — `set_pending_deliveries`
- `outbox_external_pending_deliveries` — `set_external_pending_deliveries`
- `outbox_external_pending_seconds` — `record_external_pending_seconds`
- `outbox_signing_key_resolution_failures_total` — `inc_signing_key_resolution_failures`

For the last one the exact call site exists in `crates/http-callback/src/client.rs:97-103`
(the `KeyRing::get` returns `None` branch), but it currently returns the transient error
without metering. The first four are gauges/histograms that need a periodic stats sampler
fed from `Repo::fetch_stats()` — there is no such task wired up. Phase 7's deliverable
is "Prometheus metrics", and these five are spec'd as required.

**Context** (`crates/http-callback/src/client.rs:90-107` — concrete missed call site)

```rust
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
```

**Recommended fix**

For the signing-key-resolution metric, emit it from the http-callback (it has the key id
and the callback name to label):

```rust
None => {
    outbox_dispatcher_core::metrics::inc_signing_key_resolution_failures(
        key_id.as_str(),
        target.name.as_str(),
    );
    return Err(CallbackError::Transient {
        reason: format!("signing_key_id '{key_id}' not registered"),
        retry_after: None,
    });
}
```

For the four queue-state gauges, add a periodic sampler in `crates/bin/src/main.rs`
(or a new `crates/core/src/stats_sampler.rs`) that calls `repo.fetch_stats()` every
`poll_interval`, then publishes:

```rust
metrics::set_lag_seconds(stats.oldest_pending_age_seconds.unwrap_or(0.0));
for (cb, s) in &stats.callbacks {
    metrics::set_pending_deliveries(cb, "managed", s.pending as f64);
    metrics::set_external_pending_deliveries(cb, s.external_pending as f64);
}
```

If `outbox_external_pending_seconds` is intentionally a histogram of *per-row* ages,
extend `Repo::fetch_stats` (or add `Repo::sample_external_pending_ages`) to return a
list of ages and call `record_external_pending_seconds(cb, age)` in a loop.

**Why this fix**

The metric registry advertises a contract that operators will dashboard against; if the
helpers exist but never fire, alerting on those series will look "all good" while the
queue is actually backed up. Either wire them or delete the unused helpers (smaller scope
but loses the spec coverage).

---

### Finding 2 — OpenTelemetry tracer provider is never shut down

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:373-401` |
| **Severity** | Medium |
| **Category** | Correctness / Observability |

**Problem**

`SdkTracerProvider` is built with `with_batch_exporter` and stashed in the global
registry, but graceful shutdown never invokes `tracer_provider.shutdown()`. The batch
exporter buffers spans in memory and flushes asynchronously; on SIGTERM the process
returns from `main` while the buffer is still pending, silently losing the trailing
spans (often the most interesting ones — the shutdown itself, the last failures).

**Context** (`crates/bin/src/main.rs:373-401`)

```rust
if !obs.otel_endpoint.is_empty() {
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&obs.otel_endpoint)
        .build()
        .context("building OTLP span exporter")?;

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    // Obtain the tracer before moving the provider into the global registry.
    let tracer =
        opentelemetry::trace::TracerProvider::tracer(&tracer_provider, "outbox-dispatcher");
    opentelemetry::global::set_tracer_provider(tracer_provider);
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    ...
}
```

**Recommended fix**

Have `init_tracing` return an optional handle, and call `.shutdown()` from `run()` after
the scheduler exits:

```rust
// init_tracing now returns Option<SdkTracerProvider>
fn init_tracing(log: &LogConfig, obs: &ObservabilityConfig)
    -> Result<Option<opentelemetry_sdk::trace::SdkTracerProvider>>
{
    // ... (unchanged) ...
    if !obs.otel_endpoint.is_empty() {
        // ... build exporter and provider ...
        let provider_for_shutdown = tracer_provider.clone();
        opentelemetry::global::set_tracer_provider(tracer_provider);
        // ... wire fmt + otel layers ...
        Ok(Some(provider_for_shutdown))
    } else {
        // ... wire fmt layer only ...
        Ok(None)
    }
}

// In run(), after the scheduler exits cleanly:
if let Some(provider) = tracer_provider_handle {
    if let Err(e) = provider.shutdown() {
        warn!(error = ?e, "OpenTelemetry tracer shutdown failed");
    }
}
```

**Why this fix**

`shutdown()` flushes the BatchSpanProcessor synchronously; without it spans queued in
the last 5 s (default batch interval) are dropped on every restart, which is exactly
the window operators most want to inspect.

---

### Finding 3 — Retention reason metric label is hard-coded to `processed`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/retention.rs:125-132` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`outbox_retention_deletions_total{reason}` is spec'd with two label values:
`processed` and `dead_letter`. The SQL deletes a mix of both categories in a single
batched statement, but the metric emit attributes every deletion to `reason="processed"`.
Operators dashboarding dead-letter retention will always see zero, even when the
worker is deleting dead-lettered events. The inline comment acknowledges this and
punts to a "future iteration"; the dashboards and alerts that depend on the label
are broken until that future arrives.

**Context** (`crates/core/src/retention.rs:115-132`)

```rust
let deleted = repo
    .delete_terminal_events(
        dead_letter_cutoff,
        processed_cutoff,
        config.batch_limit as i64,
    )
    .await?;

// ...
if deleted > 0 {
    // Attribute all deletions to "processed" for simplicity; a future iteration
    // can split the query to produce separate counts.
    metrics::inc_retention_deletions_total(metrics::retention_reason::PROCESSED, deleted);
    warn!(deleted, "retention worker deleted terminal events");
}
```

**Recommended fix**

Return separate counts from the repo by adding a `reason` column to the RETURNING clause
(based on whether any sibling delivery has `dead_letter = TRUE`), then sum into a small
struct:

```rust
// schema.rs
#[derive(Debug, Default, Clone, Copy)]
pub struct RetentionDeleted {
    pub processed: u64,
    pub dead_letter: u64,
}

// repo.rs — change DELETE ... RETURNING id  →  RETURNING id, reason
//   where the CTE selects 'dead_letter' or 'processed' based on the same
//   EXISTS subquery that picks the window.

// retention.rs
let deleted = repo.delete_terminal_events(...).await?;
if deleted.processed > 0 {
    metrics::inc_retention_deletions_total(metrics::retention_reason::PROCESSED, deleted.processed);
}
if deleted.dead_letter > 0 {
    metrics::inc_retention_deletions_total(metrics::retention_reason::DEAD_LETTER, deleted.dead_letter);
}
```

**Why this fix**

The `retention_reason::DEAD_LETTER` constant already exists and is exported precisely
for this case. Without splitting the count, the metric is misleading and the constant
is dead code.

---

### Finding 4 — `log.format` silently ignored when OTel is enabled

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:373-401` |
| **Severity** | Medium |
| **Category** | Config |

**Problem**

When `observability.otel_endpoint` is set, `init_tracing` builds the subscriber with
`tracing_subscriber::fmt::layer()` (default = pretty, ANSI-coloured), regardless of
`log.format`. In production an operator who set `log.format = "json"` (as the base
`envs/app_config.toml` does today) will be surprised when enabling OTel makes the
local logs revert to ANSI-coloured pretty output, which is not parseable by log
aggregators and is harder to read in container logs.

**Context** (`crates/bin/src/main.rs:392-417`)

```rust
// fmt layer and otel layer must both be added to the same subscriber base.
// Using `json()` or plain fmt both work; otel layer is indifferent to field format.
tracing_subscriber::registry()
    .with(filter)
    .with(tracing_subscriber::fmt::layer())
    .with(otel_layer)
    .try_init()
    .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;

info!(endpoint = %obs.otel_endpoint, "OpenTelemetry tracing enabled");
} else {
    match log.format {
        LogFormat::Json => {
            tracing_subscriber::fmt().json()
                .with_env_filter(filter).try_init()...
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_env_filter(filter).try_init()...
        }
    }
}
```

**Recommended fix**

Honour `log.format` in the OTel branch too. The fmt layer builder has matching `.json()`
support:

```rust
let fmt_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = match log.format {
    LogFormat::Json => Box::new(
        tracing_subscriber::fmt::layer().json().with_current_span(true),
    ),
    LogFormat::Pretty => Box::new(tracing_subscriber::fmt::layer()),
};

tracing_subscriber::registry()
    .with(filter)
    .with(fmt_layer)
    .with(otel_layer)
    .try_init()
    .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;
```

**Why this fix**

`log.format` is a deliberate operator-facing knob, validated at startup; ignoring it
for an unrelated reason (OTel on/off) breaks the config contract.

---

### Finding 5 — Successful retention deletions logged at `warn!`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/retention.rs:131` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

A successful retention cycle is logged at `warn!`. Per CLAUDE.md the convention is
`warn!` for "permission denials"; this is routine background cleanup, so it should
be `info!`. Operators tailing logs at `warn` and above will see periodic noise that
isn't actionable; alerting rules keyed on `warn` log volume will misfire.

**Context** (`crates/core/src/retention.rs:127-132`)

```rust
if deleted > 0 {
    // Attribute all deletions to "processed" for simplicity; a future iteration
    // can split the query to produce separate counts.
    metrics::inc_retention_deletions_total(metrics::retention_reason::PROCESSED, deleted);
    warn!(deleted, "retention worker deleted terminal events");
}
```

**Recommended fix**

```rust
if deleted > 0 {
    metrics::inc_retention_deletions_total(metrics::retention_reason::PROCESSED, deleted);
    info!(deleted, "retention worker deleted terminal events");
}
```

**Why this fix**

Note that `run_retention_worker` already logs the same fact at `info!` (line 92-93)
on receipt of the report — this second `warn!` is redundant. Either drop it entirely
or downgrade.

---

### Finding 6 — `oldest_event_age` gauge sentinel collides with real value

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/retention.rs:86-90` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

When no events are retention-eligible, the gauge is forced to `0.0`. A genuine
zero-age value (an event whose `created_at == now()` and is already terminal) is
indistinguishable from "queue empty". The `RetentionReport.oldest_age_seconds`
type already encodes the distinction as `Option<f64>` — the metric throws it away.

**Context** (`crates/core/src/retention.rs:86-90`)

```rust
if let Some(age) = report.oldest_age_seconds {
    metrics::set_retention_oldest_event_age_seconds(age);
} else {
    metrics::set_retention_oldest_event_age_seconds(0.0);
}
```

**Recommended fix**

Either skip the update so the previous value persists with staleness, or use a
distinct sentinel value (NaN, which Prometheus treats as "no value"):

```rust
if let Some(age) = report.oldest_age_seconds {
    metrics::set_retention_oldest_event_age_seconds(age);
} else {
    // No eligible events: publish NaN so alerts on "oldest age > N" don't trigger
    // for an empty queue. Prometheus exporter renders NaN as a missing sample.
    metrics::set_retention_oldest_event_age_seconds(f64::NAN);
}
```

**Why this fix**

Avoids ambiguity for alerting rules of the form `outbox_retention_oldest_event_age_seconds > 3600`,
which would correctly stay quiet on an empty queue and on a freshly-deleted queue.

---

### Finding 7 — Counter increments in a `for` loop

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/timeout_sweep.rs:36-38,45-47` |
| **Severity** | Low |
| **Category** | Performance / Idiom |

**Problem**

The sweep handler increments each counter once per row instead of by a count. Under
load (e.g. 10 000 timeout resets in one cycle) this performs 10 000 `metrics::counter!`
macro expansions and 10 000 label-map allocations where a single `.increment(N)`
would suffice.

**Context** (`crates/core/src/timeout_sweep.rs:30-49`)

```rust
if report.reset > 0 {
    info!(reset = report.reset, "external timeout sweep reset deliveries for redelivery");
    // We don't have per-callback counts from the sweeper, so use "_all" as label.
    for _ in 0..report.reset {
        metrics::inc_external_timeout_resets_total("_all");
    }
}
if report.exhausted > 0 {
    warn!(exhausted = report.exhausted, "external timeout sweep dead-lettered rows after max_completion_cycles");
    for _ in 0..report.exhausted {
        metrics::inc_completion_cycles_exhausted_total("_all");
    }
}
```

**Recommended fix**

Add a count parameter to the metric helpers (or expand inline):

```rust
// metrics.rs
#[inline]
pub fn inc_external_timeout_resets_total_by(callback: &str, count: u64) {
    metrics::counter!(EXTERNAL_TIMEOUT_RESETS_TOTAL, "callback" => callback.to_owned())
        .increment(count);
}
#[inline]
pub fn inc_completion_cycles_exhausted_total_by(callback: &str, count: u64) {
    metrics::counter!(COMPLETION_CYCLES_EXHAUSTED_TOTAL, "callback" => callback.to_owned())
        .increment(count);
}

// timeout_sweep.rs
if report.reset > 0 {
    info!(reset = report.reset, "external timeout sweep reset deliveries for redelivery");
    metrics::inc_external_timeout_resets_total_by("_all", report.reset);
}
if report.exhausted > 0 {
    warn!(exhausted = report.exhausted, "external timeout sweep dead-lettered rows after max_completion_cycles");
    metrics::inc_completion_cycles_exhausted_total_by("_all", report.exhausted);
}
```

**Why this fix**

`metrics::inc_retention_deletions_total` already takes a `count: u64`; matching that
shape for the sweeper counters is consistent and cheap.

---

### Finding 8 — Retention worker has no startup jitter

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/retention.rs:70-78` |
| **Severity** | Low |
| **Category** | Concurrency |

**Problem**

Multiple replicas of the dispatcher starting at the same time (e.g. a Kubernetes
rolling restart) will all enter the retention loop at the same instant. With
`cleanup_interval_secs = 3600` (default), they all wake at minute 0 of every hour
and race for the same batch of rows. The DELETE uses `LIMIT $3` but no `FOR UPDATE
SKIP LOCKED`, so up to N replicas serialize on row locks every cycle.

**Context** (`crates/core/src/retention.rs:70-78`)

```rust
loop {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            info!("retention worker received shutdown signal, stopping");
            return;
        }
        _ = tokio::time::sleep(interval) => {}
    }
    ...
}
```

**Recommended fix**

Apply a small initial jitter (e.g. ±25 % of the interval) before the first sleep, and
optionally between subsequent cycles:

```rust
use rand::Rng;
let mut rng = rand::thread_rng();
let initial_jitter = Duration::from_secs(rng.gen_range(0..config.cleanup_interval_secs));
tokio::time::sleep(initial_jitter).await;

loop {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => { ... return; }
        _ = tokio::time::sleep(interval) => {}
    }
    ...
}
```

**Why this fix**

The dispatch retry path already applies ±25 % jitter (per CLAUDE.md key design notes);
applying the same convention to the retention worker keeps multi-replica deployments
from thundering.

---

---

## Second-pass review (2026-05-13T15:07:35Z)

The eight findings above are addressed. The following new findings were identified during a
re-review of the resulting code paths.

### Finding 9 — Stats sampler hard-codes `mode="managed"` for all pending deliveries

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:485-488` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`Repo::fetch_stats` returns a single `pending` count per callback that aggregates
**both** `managed` and `external` mode rows still awaiting dispatch (the SQL filter
in `crates/core/src/repo.rs:815-820` does not constrain `completion_mode`). The
stats sampler then publishes that combined count with a hard-coded `mode="managed"`
label, so an external-only callback shows up on the dashboard as "managed pending"
and the `mode="external"` series is permanently empty. Spec §12.1 defines
`outbox_pending_deliveries{callback, mode}` as a per-mode gauge; the current
emission contradicts that contract.

**Context** (`crates/bin/src/main.rs:482-489`)

```rust
match repo.fetch_stats().await {
    Ok(stats) => {
        metrics::set_lag_seconds(stats.oldest_pending_age_seconds.unwrap_or(0.0));
        for (cb, s) in &stats.callbacks {
            metrics::set_pending_deliveries(cb, "managed", s.pending as f64);
            metrics::set_external_pending_deliveries(cb, s.external_pending as f64);
        }
    }
```

**Recommended fix**

Split the per-callback pending count by `completion_mode` in `fetch_stats`. Extend
`CallbackStats` with `pending_managed` and `pending_external`, change the SQL
GROUP BY to emit both filters, and publish two gauge series per callback:

```rust
// schema.rs
pub struct CallbackStats {
    pub pending_managed: i64,
    pub pending_external: i64,
    pub external_pending: i64,   // post-dispatch, awaiting completion
    pub dead_lettered: i64,
}

// repo.rs (StatsRow SQL)
COUNT(*) FILTER (
    WHERE completion_mode = 'managed'
      AND dispatched_at IS NULL
      AND processed_at  IS NULL
      AND dead_letter   = FALSE
) AS "pending_managed!",
COUNT(*) FILTER (
    WHERE completion_mode = 'external'
      AND dispatched_at IS NULL
      AND processed_at  IS NULL
      AND dead_letter   = FALSE
) AS "pending_external!",

// main.rs
for (cb, s) in &stats.callbacks {
    metrics::set_pending_deliveries(cb, "managed",  s.pending_managed  as f64);
    metrics::set_pending_deliveries(cb, "external", s.pending_external as f64);
    metrics::set_external_pending_deliveries(cb, s.external_pending as f64);
}
```

**Why this fix**

Operators alerting on `outbox_pending_deliveries{mode="external"}` will silently
miss real backlogs today; the metric name claims a `mode` label that has no
information content. Splitting the count restores the contract.

---

### Finding 10 — Stats sampler leaves stale gauges for drained callbacks

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:482-489` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

The sampler only writes a gauge value for callbacks that appear in the current
`fetch_stats` result. The `StatsRow` SQL emits one row per `callback_name` *that
has at least one delivery row*; if every delivery for a callback is retention-
deleted (or, while retention is disabled, never happens) the row disappears and
the gauge with that label keeps its last-written value forever. A callback that
drained from 200 → 0 pending will scrape as `200` indefinitely.

**Recommended fix**

Track the set of callback labels emitted on the previous tick and overwrite each
omitted label with `0.0` on the next tick (or use the
`metrics_exporter_prometheus::PrometheusHandle::clear_metric` API, if exposed,
to drop the series). A minimal patch:

```rust
let mut last_callbacks: HashSet<String> = HashSet::new();
loop {
    // ... select! ...
    if let Ok(stats) = repo.fetch_stats().await {
        let mut now_seen = HashSet::new();
        for (cb, s) in &stats.callbacks {
            now_seen.insert(cb.clone());
            // ... set gauges ...
        }
        for stale in last_callbacks.difference(&now_seen) {
            metrics::set_pending_deliveries(stale, "managed",  0.0);
            metrics::set_pending_deliveries(stale, "external", 0.0);
            metrics::set_external_pending_deliveries(stale, 0.0);
        }
        last_callbacks = now_seen;
    }
}
```

**Why this fix**

Stale gauges are a textbook Prometheus pitfall: ops will see a phantom backlog
that never clears, and alerts won't auto-resolve. The fix is local to the sampler.

---

### Finding 11 — Lag gauge emits `0.0` instead of `NaN` when queue is empty

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:484` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

```rust
metrics::set_lag_seconds(stats.oldest_pending_age_seconds.unwrap_or(0.0));
```

The retention worker already learned this lesson — Finding 6 replaced the same
`0.0` sentinel with `f64::NAN` so that alerts on
`outbox_retention_oldest_event_age_seconds > N` don't conflate "queue empty"
with "freshly enqueued event". The same conflation now lives in the stats
sampler for `outbox_lag_seconds`.

**Recommended fix**

```rust
metrics::set_lag_seconds(stats.oldest_pending_age_seconds.unwrap_or(f64::NAN));
```

**Why this fix**

Consistency with Finding 6's fix and the Prometheus convention that NaN samples
are rendered as missing — alerts on "lag > N" will quietly stay quiet on an
empty queue.

---

### Finding 12 — OpenTelemetry tracer has no `service.name` resource

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:413-415` |
| **Severity** | Medium |
| **Category** | Observability |

**Problem**

`SdkTracerProvider::builder()` is built with only the batch exporter; no
`Resource` is attached, so emitted spans carry no `service.name` /
`service.version` attributes. Most OTLP backends (Jaeger, Tempo, Grafana Cloud
Traces, Honeycomb, Datadog APM) key on `service.name` for indexing — without it
spans land under the placeholder `unknown_service` or are dropped entirely.

**Context** (`crates/bin/src/main.rs:413-419`)

```rust
let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
    .with_batch_exporter(exporter)
    .build();

let tracer =
    opentelemetry::trace::TracerProvider::tracer(&tracer_provider, "outbox-dispatcher");
```

**Recommended fix**

```rust
use opentelemetry_sdk::Resource;
use opentelemetry::KeyValue;

let resource = Resource::builder()
    .with_attributes([
        KeyValue::new("service.name", "outbox-dispatcher"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("deployment.environment", app_env.clone()),
    ])
    .build();

let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
    .with_resource(resource)
    .with_batch_exporter(exporter)
    .build();
```

**Why this fix**

Phase 7's deliverable is end-to-end observability; spans that can't be queried
in the trace backend are functionally inert. `service.name` is the one resource
attribute every OTLP backend treats as mandatory, and `CARGO_PKG_VERSION` /
`APP_ENV` are already available at this point in startup.

---

### Finding 13 — `outbox_external_pending_seconds` histogram still not emitted

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/metrics.rs:140-147` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

Follow-up to Finding 1: of the five missing-call-site metrics, four were addressed
by the new stats sampler and the http-callback edit. The histogram
`outbox_external_pending_seconds` is still exported as a `pub fn` helper with no
production call site — only the unit test in `metrics.rs:288-290` exercises it.
Spec §12.1 names this metric explicitly, and operators dashboarding on per-row
external-pending-age distributions will see an empty series.

**Recommended fix**

Either wire it from the stats sampler by adding a repo method that returns a
sample of pending external-completion row ages, then `record_external_pending_seconds`
in a loop:

```rust
// repo.rs
async fn sample_external_pending_ages(&self, sample_size: i64) -> Result<Vec<(String, f64)>>;

// main.rs (inside run_stats_sampler)
if let Ok(samples) = repo.sample_external_pending_ages(1000).await {
    for (cb, age_secs) in samples {
        metrics::record_external_pending_seconds(&cb, age_secs);
    }
}
```

…or, if a histogram of per-row ages is not actually wanted, delete the helper
and the metric constant entirely so the public surface matches what the binary
emits.

**Why this fix**

The metric registry advertises a contract; an always-empty histogram is more
misleading than no metric at all. Whichever direction is chosen, the contract
and the implementation should agree.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | Spec metrics declared but never emitted | `crates/core/src/metrics.rs:114-181` + `crates/http-callback/src/client.rs:97` | High | Correctness | DONE | Signing-key metric wired in `client.rs`; queue-state gauges published by new `run_stats_sampler` task in `main.rs`. Follow-up F13 covers the remaining histogram. |
| 2 | OTel tracer provider not shut down | `crates/bin/src/main.rs:373-401` | Medium | Correctness | DONE | `init_tracing` returns `Option<SdkTracerProvider>`; shutdown called after scheduler exits |
| 3 | Retention reason label hard-coded to `processed` | `crates/core/src/retention.rs:125-132` | Medium | Correctness | DONE | Added `RetentionDeleted` struct; SQL now returns per-bucket counts; both labels emitted |
| 4 | `log.format` ignored when OTel is enabled | `crates/bin/src/main.rs:373-401` | Medium | Config | DONE | `init_tracing` builds `fmt_layer` from `log.format` before branching on OTel |
| 5 | `warn!` for routine retention deletions | `crates/core/src/retention.rs:131` | Low | Idiom | DONE | Removed stale `warn!`; success reported via the `info!` already at L92 |
| 6 | `oldest_event_age` gauge sentinel collides | `crates/core/src/retention.rs:86-90` | Low | Correctness | DONE | Publishes `f64::NAN` when no eligible events remain |
| 7 | Counter increments in a `for` loop | `crates/core/src/timeout_sweep.rs:36-47` | Low | Performance | DONE | Added `inc_*_by(count)` helpers; `for` loops replaced with single `increment(N)` calls |
| 8 | Retention worker has no startup jitter | `crates/core/src/retention.rs:70-78` | Low | Concurrency | DONE | Random initial delay `0..cleanup_interval_secs` added before the main loop |
| 9 | Stats sampler hard-codes `mode="managed"` | `crates/bin/src/main.rs:485-488` | Medium | Correctness | DONE | `CallbackStats` split into `pending_managed`/`pending_external`; SQL updated; both `mode` series emitted |
| 10 | Stale gauges for drained callbacks | `crates/bin/src/main.rs:482-489` | Low | Correctness | DONE | `last_callbacks` set tracks previous tick; missing labels zeroed on next tick |
| 11 | `outbox_lag_seconds` emits 0.0 instead of NaN | `crates/bin/src/main.rs:484` | Low | Correctness | DONE | `unwrap_or(f64::NAN)` consistent with F6 fix |
| 12 | OTel tracer missing `service.name` resource | `crates/bin/src/main.rs:413-415` | Medium | Observability | DONE | `Resource` with `service.name`, `service.version`, `deployment.environment` added; `app_env` passed into `init_tracing` |
| 13 | `outbox_external_pending_seconds` never emitted | `crates/core/src/metrics.rs:140-147` | Low | Correctness | DONE | New `Repo::sample_external_pending_ages` method + `PgRepo` impl; wired from `run_stats_sampler` |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
