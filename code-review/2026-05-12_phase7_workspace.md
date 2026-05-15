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

---

## Third-pass review (2026-05-13T15:28:35Z)

Findings 1–13 are all addressed. The following new findings were identified on a
fresh re-read of the resulting code paths in the `phase7` branch (PR #6).

### Finding 14 — `outbox_completion_cycles_exhausted_total` over-emitted on every max-attempts dead-letter

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/dispatch.rs:184-185,230-231` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`outbox_completion_cycles_exhausted_total{callback}` is documented (§12.1 and the
metric index in `crates/core/src/metrics.rs:21-22`) as counting external-mode
deliveries that were dead-lettered specifically because they exhausted
`max_completion_cycles` — i.e. the receiver kept failing to POST the
external-completion confirmation. The legitimate emit point is
`crates/core/src/timeout_sweep.rs:43`.

`dispatch.rs` now also bumps the same counter for **every** max-attempts
exhaustion in the dispatch path — once after a transient-failure dead-letter
(`dispatch.rs:184-185`) and once after a callback-timeout dead-letter
(`dispatch.rs:230-231`). Those are regular delivery exhaustions, already covered
by `outbox_dead_letters_total{callback}`. Dashboards keyed on
`outbox_completion_cycles_exhausted_total` will therefore double-count: every
dead-letter increments both metrics, and an operator alerting on "external
completion confirmation is failing" will fire false positives whenever a
managed-mode webhook simply runs out of retries.

**Context** (`crates/core/src/dispatch.rs:180-237`)

```rust
let dead = next_attempt >= due.target.max_attempts as i32;

if dead {
    metrics::inc_dead_letters_total(cb_name);
    metrics::inc_completion_cycles_exhausted_total(cb_name);   // ← wrong: not a completion-cycle event
    warn!(
        delivery_id = due.delivery_id,
        attempts = next_attempt,
        max_attempts = due.target.max_attempts,
        reason = %reason,
        "delivery exhausted retries — dead-lettering"
    );
}
// ...
// Tokio timeout — treat as a transient failure
Err(_timeout) => {
    // ...
    if dead {
        metrics::inc_dead_letters_total(cb_name);
        metrics::inc_completion_cycles_exhausted_total(cb_name);   // ← same bug
        warn!(
            delivery_id = due.delivery_id,
            max_attempts = due.target.max_attempts,
            "delivery exhausted retries after timeout — dead-lettering"
        );
    }
```

**Recommended fix**

Drop the `inc_completion_cycles_exhausted_total` call from both dispatch
branches; leave it only in `timeout_sweep.rs` where the exhaustion is genuinely
about external completion cycles:

```rust
if dead {
    metrics::inc_dead_letters_total(cb_name);
    warn!(
        delivery_id = due.delivery_id,
        attempts = next_attempt,
        max_attempts = due.target.max_attempts,
        reason = %reason,
        "delivery exhausted retries — dead-lettering"
    );
}
```

**Why this fix**

Two distinct events (max-attempts exhaustion vs. completion-cycles exhaustion)
should map to two distinct counters. Conflating them either makes the
completion-cycles counter unusable for alerting or double-counts the
dead-letter rate. Operators who want a single "anything dead-lettered" series
already have `outbox_dead_letters_total`.

---

### Finding 15 — `outbox_invalid_callbacks_total` label has unbounded, free-text values

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:302-316,322-330` |
| **Severity** | Low |
| **Category** | Idiom / Observability |

**Problem**

The label-derivation logic strips the `"invalid_callback: "` prefix from the
parser's error message and takes the chunk before the first `:` as the metric
label:

```rust
let label = r
    .strip_prefix("invalid_callback: ")
    .unwrap_or(r.as_str())
    .split(':')
    .next()
    .unwrap_or("unknown");
metrics::inc_invalid_callbacks_total(label);
```

The parser's reason strings (see `crates/core/src/callbacks.rs:188-558`) include
free-text fragments such as
`"url 'https://attacker.example/foo' must use https:// scheme; got 'http'"`,
`"header 'X-Custom-Foo' is reserved and cannot be set"`, and
`"max_attempts must be between 1 and 50; got 200"`. After the split-on-`:` the
labels become things like `url 'https`, `header 'X-Custom-Foo' is reserved and cannot be set`,
or `max_attempts must be between 1 and 50; got 200` — i.e. arbitrarily long,
user-influenced strings rather than stable short identifiers. This produces:

- High-cardinality time-series in Prometheus (one per distinct header name or
  reason variant); the metric explodes if an attacker can submit varied
  callback specs.
- Labels that are hostile to dashboards (long sentences, embedded quotes).
- A contract that silently changes whenever an error message in
  `callbacks.rs` is reworded.

**Recommended fix**

Have `parse_callbacks` (and `payload_too_large_error`) return a structured
reason code in addition to the human-readable string. A minimal version uses a
small enum mapped to a stable static label:

```rust
// callbacks.rs
pub enum InvalidReason {
    MissingName,
    NameInvalid,
    MissingUrl,
    UrlNotHttps,
    UrlPrivateHost,
    HeaderReserved,
    HeaderInvalidName,
    MaxAttemptsOutOfRange,
    DuplicateName,
    TooManyCallbacks,
    PayloadTooLarge,
    // …
    Other,
}

impl InvalidReason {
    pub fn metric_label(self) -> &'static str {
        match self {
            Self::MissingName            => "missing_name",
            Self::NameInvalid            => "name_invalid",
            Self::MissingUrl             => "missing_url",
            Self::UrlNotHttps            => "url_not_https",
            Self::UrlPrivateHost         => "url_private_host",
            Self::HeaderReserved         => "header_reserved",
            Self::HeaderInvalidName      => "header_invalid_name",
            Self::MaxAttemptsOutOfRange  => "max_attempts_out_of_range",
            Self::DuplicateName          => "duplicate_name",
            Self::TooManyCallbacks       => "too_many_callbacks",
            Self::PayloadTooLarge        => "payload_too_large",
            Self::Other                  => "other",
        }
    }
}

// scheduler.rs
for (_, reason) in &parsed.invalid {
    metrics::inc_invalid_callbacks_total(reason.code.metric_label());
}
```

If the refactor is too invasive for this PR, an interim fix is to keep the
free-text reasons but map them to a small set of stable labels at the emit site
via a pattern match on prefixes, and never pass user input straight through.

**Why this fix**

Prometheus best practice is bounded, short, stable label values. The current
emit point makes the metric's cardinality a function of message wording and
operator-submitted strings (callback name, URL scheme prefix, header name),
which couples observability stability to internal error text — every reword in
`callbacks.rs` silently breaks dashboards.

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
| 14 | `outbox_completion_cycles_exhausted_total` over-emitted on every max-attempts dead-letter | `crates/core/src/dispatch.rs:184-185,230-231` | Medium | Correctness | DONE | Removed both `inc_completion_cycles_exhausted_total` calls from the dispatch transient-failure and timeout dead-letter branches; the metric is now only emitted by `timeout_sweep.rs` as intended |
| 15 | `outbox_invalid_callbacks_total` label uses free-text reason fragments | `crates/core/src/scheduler.rs:302-316,322-330` | Low | Idiom / Observability | DONE | Added `InvalidReason` enum with `metric_label()` to `callbacks.rs`; `ParsedCallbacks.invalid` changed to `(name, InvalidReason, message)`; both `scheduler.rs` emit sites now call `reason_code.metric_label()` for a stable, bounded Prometheus label |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.

---

## Fourth-pass review (2026-05-13T15:48:03Z)

Findings 1–15 are all addressed and verified. A fresh re-read of the resulting code
on `phase7` (HEAD: `fb27ed5`) surfaced four additional findings: one Medium, three Low.

### Finding 16 — Spawned background tasks are not awaited on shutdown

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:244-286,288-308` |
| **Severity** | Medium |
| **Category** | Concurrency / Correctness |

**Problem**

Three background tasks are launched via `tokio::spawn(...)` and their `JoinHandle`s
are dropped immediately: the admin HTTP server (`main.rs:244-266`), the retention
worker (`main.rs:273-275`), and the stats sampler (`main.rs:283-285`). Each task
*internally* watches `shutdown.cancelled()`, but the main flow does not await any
of them — after `run_scheduler_with_cycle_tracker` returns, `run()` proceeds
straight to `tracer_provider.shutdown()` and returns, at which point `#[tokio::main]`
drops the runtime and **aborts** every still-running task abruptly.

Concrete consequences:

1. The admin server's `axum::serve(...).with_graceful_shutdown(...)` begins draining
   in-flight requests when the cancellation token fires, but the spawned task running
   it is dropped before the drain can complete. Open admin connections (e.g. an
   operator retrying a delivery during a deploy) are reset rather than cleanly closed.
2. The retention worker may be in the middle of `delete_terminal_events` when shutdown
   fires; aborting that future leaves the in-flight `DELETE ... RETURNING` to be
   rolled back by Postgres but loses the resulting `info!` log line that the cycle
   ran at all.
3. The stats sampler's last gauge write is lost, which is benign but breaks the
   pattern.
4. `tracer_provider.shutdown()` runs **before** the other tasks finish, so any spans
   they emit during their final drain step (the retention worker's "cycle completed"
   log, an admin handler's error response span) are dropped by the very batch
   exporter Finding 2 set up to flush them.

**Context** (`crates/bin/src/main.rs:280-308` — task spawns + final shutdown sequence)

```rust
// Spawn the periodic stats sampler that publishes queue-state gauges.
{
    let stats_repo = repo.clone();
    let poll_interval = Duration::from_secs(config.dispatch.poll_interval_secs);
    let stats_shutdown = shutdown.clone();
    tokio::spawn(async move {
        run_stats_sampler(stats_repo, poll_interval, stats_shutdown).await;
    });
}

info!("starting scheduler");
run_scheduler_with_cycle_tracker(/* ... */).await
    .context("scheduler exited with error")?;

// Flush any buffered spans before the process exits.
if let Some(provider) = tracer_provider
    && let Err(e) = provider.shutdown()
{
    warn!(error = ?e, "OpenTelemetry tracer shutdown failed");
}

info!("outbox-dispatcher stopped cleanly");
```

**Recommended fix**

Collect the join handles into a `JoinSet` (or a `Vec<JoinHandle<()>>`) and await them
after the scheduler exits but *before* the tracer shutdown:

```rust
let mut workers: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

// admin server
workers.spawn(async move { /* axum::serve(...).await as today */ });

// retention worker (only when enabled)
if config.retention.enabled {
    let retention_repo = repo.clone();
    let retention_cfg = config.retention.clone();
    let retention_shutdown = shutdown.clone();
    workers.spawn(async move {
        run_retention_worker(retention_repo, retention_cfg, retention_shutdown).await;
    });
}

// stats sampler
{
    let stats_repo = repo.clone();
    let poll_interval = Duration::from_secs(config.dispatch.poll_interval_secs);
    let stats_shutdown = shutdown.clone();
    workers.spawn(async move {
        run_stats_sampler(stats_repo, poll_interval, stats_shutdown).await;
    });
}

info!("starting scheduler");
run_scheduler_with_cycle_tracker(/* ... */).await
    .context("scheduler exited with error")?;

// Wait for the admin server, retention worker, and stats sampler to drain.
while let Some(res) = workers.join_next().await {
    if let Err(e) = res {
        warn!(error = ?e, "background worker exited abnormally during shutdown");
    }
}

// Now it's safe to flush the tracer.
if let Some(provider) = tracer_provider
    && let Err(e) = provider.shutdown()
{
    warn!(error = ?e, "OpenTelemetry tracer shutdown failed");
}
```

**Why this fix**

Each spawned task already responds to the shutdown signal — wiring the main thread
to wait for them turns "best-effort cancellation" into "true graceful shutdown",
which is the explicit contract advertised by `info!("outbox-dispatcher stopped
cleanly")`. It also restores the intended ordering with Finding 2's tracer flush:
spans emitted during the workers' final drain step are queued *before* the batch
exporter is shut down.

---

### Finding 17 — `outbox_invalid_callbacks_total` incremented in a per-row `for` loop

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:306-308` |
| **Severity** | Low |
| **Category** | Performance / Idiom |

**Problem**

Identical anti-pattern to Finding 7 (which added `_by(count: u64)` helpers for the
timeout-sweep counters), now in the scheduler's array-level rejection path:

```rust
for _ in &entries {
    metrics::inc_invalid_callbacks_total(reason_code.metric_label());
}
```

`entries.len()` is bounded above by `max_callbacks_per_event` (≤ 1 000), so the
performance impact is small in absolute terms — but the pattern is exactly the one
Finding 7 corrected for the sweeper counters, and `parsed.invalid` higher up uses a
similar emit-once-per-element loop at `scheduler.rs:328-330`. Adopting the
`_by(count)` shape keeps the metric helpers consistent.

**Context** (`crates/core/src/scheduler.rs:302-310,326-330`)

```rust
let entries: Vec<(String, String)> = extract_callback_names(&event.callbacks)
    .into_iter()
    .map(|n| (n, reason_msg.clone()))
    .collect();
for _ in &entries {
    metrics::inc_invalid_callbacks_total(reason_code.metric_label());
}
// ...
// Emit invalid_callbacks_total for each structurally invalid callback.
for (_, reason_code, _) in &parsed.invalid {
    metrics::inc_invalid_callbacks_total(reason_code.metric_label());
}
```

**Recommended fix**

Add `inc_invalid_callbacks_total_by(label, count: u64)` in `metrics.rs` (mirroring
`inc_external_timeout_resets_total_by` and `inc_completion_cycles_exhausted_total_by`),
and use it for the array-level path. The per-element parser loop is justified —
each row has a *different* `reason_code` — but can collapse to a single call when
all entries share the same label.

```rust
// metrics.rs
#[inline]
pub fn inc_invalid_callbacks_total_by(reason: &str, count: u64) {
    metrics::counter!(INVALID_CALLBACKS_TOTAL, "reason" => reason.to_owned())
        .increment(count);
}

// scheduler.rs (array-level rejection branch)
metrics::inc_invalid_callbacks_total_by(reason_code.metric_label(), entries.len() as u64);
```

**Why this fix**

Consistency with Finding 7's resolution; one allocation/macro expansion per batch
instead of N. Low impact on its own, but worth doing once for symmetry with the
other `_by` helpers.

---

### Finding 18 — `record_external_pending_seconds` is a snapshot histogram, biased toward long-lived rows

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:528-540`, `crates/core/src/repo.rs:873-896` |
| **Severity** | Low |
| **Category** | Correctness / Observability |

**Problem**

The stats sampler calls `repo.sample_external_pending_ages(1000)` every
`poll_interval`, then records each `(callback_name, age_seconds)` pair as a
histogram observation. This is the Finding-13 wiring as designed, but it has two
distortions worth surfacing:

1. **Time-weighted, not row-weighted.** A delivery that sits external-pending for
   one hour is observed `3600 / poll_interval` times (≈720 observations at the
   default 5 s interval), with progressively-larger ages. A delivery that completes
   in 5 s is observed once. The histogram's p50/p99 therefore reflect "time spent
   pending" not "row pending duration", which is a less useful percentile and is
   the opposite of what most dashboard authors will assume.
2. **Selection bias on overflow.** `ORDER BY dispatched_at LIMIT 1000` returns the
   *oldest* 1 000 external-pending rows; once the backlog exceeds 1 000, the
   newest rows are never observed, so the histogram's lower buckets become
   permanently empty until the backlog drains. Operators alerting on
   `histogram_quantile(0.5, ...) < threshold` will see the gauge stay pegged high
   even after recent rows complete quickly.

**Context** (`crates/bin/src/main.rs:528-540`)

```rust
// Populate the outbox_external_pending_seconds histogram with per-row ages
// for external-mode deliveries currently awaiting completion confirmation.
match repo.sample_external_pending_ages(1000).await {
    Ok(samples) => {
        for (cb, age_secs) in samples {
            metrics::record_external_pending_seconds(&cb, age_secs);
        }
    }
    Err(e) => {
        warn!(error = %e, "stats sampler: sample_external_pending_ages failed");
    }
}
```

**Recommended fix**

Two options, pick one based on what the metric is meant to answer:

- **Terminal histogram (recommended).** Record the age *exactly once*, at the moment
  the row transitions out of external-pending state — either to `processed_at`
  (admin `/v1/deliveries/{id}/complete`, success callback) or to a sweep reset /
  dead-letter. This requires the relevant repo methods to also return the
  `dispatched_at` timestamp (or the elapsed seconds) so the dispatcher / sweeper /
  admin handler can compute the age. The histogram then represents "how long
  external completions actually took" — exactly the quantity operators expect.
- **Document the snapshot semantics.** Rename the constant to
  `outbox_external_pending_seconds_snapshot` and add a doc comment on
  `record_external_pending_seconds` explaining that observations are time-weighted
  and that overflow at 1 000 rows truncates the lower buckets.

If the terminal-histogram path is too invasive for this PR, at minimum drop the
`LIMIT 1000` cap in `sample_external_pending_ages` (or raise it to the
configured `payload_size_limit_bytes` / a generous bound) and add a `WARN!` when
the row count returned equals the limit so operators know the histogram has been
truncated.

**Why this fix**

A Prometheus histogram with `_bucket{le=...}` series is universally interpreted as
"distribution of event durations". The current implementation produces a different
distribution — and silently truncates it — which is more misleading than no metric
at all.

---

### Finding 19 — `outbox_cycle_duration_seconds` only measures the dispatch step

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/scheduler.rs:196-201` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

The helper is documented as "full scheduler cycle duration" (and the metric index
in `metrics.rs:29` lists `outbox_cycle_duration_seconds` as the cycle-duration
histogram), but the call site brackets only the dispatch step. The
`schedule_new_deliveries` step (Step 1, including any per-event DB writes) and the
`sweep_hung_external` step (Step 3, when it fires) are not included in the
observation.

**Context** (`crates/core/src/scheduler.rs:170-209`)

```rust
// ── Step 1: discover new events, expand callbacks into deliveries ─────
match schedule_new_deliveries(repo.as_ref(), &config, cursor).await {
    Ok(new_cursor) => { /* ... */ }
    Err(e) => { /* ... backoff + continue ... */ }
}

// ── Step 2: dispatch due deliveries ───────────────────────────────────
let cycle_start = std::time::Instant::now();
if let Err(e) = dispatch_due(repo.as_ref(), callback.as_ref(), &config).await {
    error!(error = %e, "error fetching due deliveries for dispatch");
}
metrics::record_cycle_duration(cycle_start.elapsed().as_secs_f64());

// ── Step 3: external-completion timeout sweep (rate-limited) ─────────
if last_sweep.elapsed() >= sweep_interval {
    if let Err(e) = sweep_hung_external(repo.as_ref(), &config).await { /* ... */ }
    last_sweep = Instant::now();
}
```

The dispatch-only timing is also valuable, but it is not what the metric name and
helper docstring advertise.

**Recommended fix**

Either move `cycle_start` to just after the wake (covering schedule + dispatch +
sweep), or rename the helper / metric so the name matches the measurement. Moving
the timer is the smaller diff and matches the docstring:

```rust
// Start the cycle clock right after the wake select (before Step 1).
let cycle_start = std::time::Instant::now();

// ── Step 1: schedule_new_deliveries (as today) ──
// ── Step 2: dispatch_due           (as today) ──
// ── Step 3: sweep_hung_external    (as today) ──

metrics::record_cycle_duration(cycle_start.elapsed().as_secs_f64());
```

**Why this fix**

A metric labelled "cycle duration" is the natural anchor for an SLO like "p99
cycle < 250 ms" or for spotting a slow `fetch_new_events`; measuring only the
dispatch step hides exactly those regressions.

---

## Updated summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 16 | Spawned background tasks not awaited on shutdown | `crates/bin/src/main.rs:244-308` | Medium | Concurrency | DONE | All three background tasks (admin server, retention worker, stats sampler) collected into a `JoinSet`; `join_next` loop drains them after the scheduler exits and before the OTel tracer flush |
| 17 | `outbox_invalid_callbacks_total` incremented in a per-row `for` loop | `crates/core/src/scheduler.rs:306-308` | Low | Performance / Idiom | DONE | Added `inc_invalid_callbacks_total_by(reason, count)` to `metrics.rs`; array-level rejection path now calls it with `entries.len() as u64` |
| 18 | `record_external_pending_seconds` is a biased snapshot histogram | `crates/bin/src/main.rs:528-540` + `crates/core/src/repo.rs:873-896` | Low | Correctness / Observability | DONE | Documented snapshot semantics on the trait method; sampler now emits `warn!` with the limit value when `samples.len() == SAMPLE_LIMIT` so operators know the histogram has been truncated |
| 19 | `outbox_cycle_duration_seconds` only measures the dispatch step | `crates/core/src/scheduler.rs:196-201` | Low | Correctness | DONE | `cycle_start` moved to before Step 1; `record_cycle_duration` called after Step 3 so the observation covers the full schedule + dispatch + sweep cycle |

> **Instructions for the implementing LLM:** same conventions as the table above —
> `TODO` → `DONE` once resolved, or `SKIPPED` with a reason. Rows must not be deleted.

---

## PR state (fourth pass — 2026-05-13)

**Status: READY TO MERGE (with follow-ups recommended)**

The original 15 findings remain fully addressed and the workspace builds & tests
cleanly:

- `cargo fmt --all`, `cargo check --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` — all pass on `phase7` (HEAD `fb27ed5`):
  277 core + 23 admin + 20 http-callback + 9 + 4 integration tests = **333 tests green**.

The four new findings are non-blocking:

- **Finding 16 (Medium)** — graceful-shutdown completeness. The current behaviour
  is "best-effort cancellation"; existing integration tests still pass because no
  test exercises a SIGINT-mid-admin-request scenario. Recommend fixing before the
  next release tag, but not before merging this PR.
- **Findings 17 / 18 / 19 (Low)** — metric-shape consistency, histogram semantics,
  and cycle-duration scope. None of them affect correctness of dispatch, retention,
  or admin paths; they are observability polish that can ship in a follow-up.

If the maintainer prefers to land all 19 findings before merge, the suggested order
is F16 → F19 → F17 → F18 (F18 is the largest behavioural change).

---

## Fifth-pass review (2026-05-14T14:23:10Z)

Findings 1–19 verified as DONE on a fresh re-read of the resulting code on `phase7`
(HEAD `61c1d40`). The full workspace still builds cleanly:

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`
  pass with no warnings.
- `cargo test --workspace` — **334 tests green** (278 core + 23 admin +
  20 http-callback + 9 scheduler-integration + 4 dispatch-integration).

Five new findings emerged on this pass — one Medium and four Low. None are
regressions of the previous fixes; all are observability / DB-load concerns that
are now visible *because* the metrics/sampler wiring is in place.

### Finding 20 — Stats sampler issues 3–4 heavy aggregate queries every `poll_interval`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:507-566`, `crates/core/src/repo.rs:787-902` |
| **Severity** | Medium |
| **Category** | Performance |

**Problem**

`run_stats_sampler` calls `repo.fetch_stats()` and `repo.sample_external_pending_ages(1000)`
every `interval` (which is `config.dispatch.poll_interval` — **5 seconds by default**).
`fetch_stats` issues three SQL statements, each of which sequential-scans (or
index-scans without a covering predicate) the entire `outbox_deliveries` table:

1. `SELECT COUNT(*) FROM outbox_events`
2. Global `COUNT(*) FILTER (...)` aggregation with four conditional aggregates and a
   `MIN(available_at) FILTER (...)` on `outbox_deliveries`.
3. Per-callback `COUNT(*) FILTER (...) GROUP BY callback_name` on `outbox_deliveries`.

Then `sample_external_pending_ages` runs a fourth statement scanning external rows
ordered by `dispatched_at`. For a deployment with a 10 M-row deliveries table, each
of those `COUNT FILTER` aggregations is O(N) and can take hundreds of milliseconds —
yet they run **12× per minute, 24/7**, regardless of whether anything changed. The
practical effect is sustained DB CPU pressure that scales with table size, not with
event arrival rate.

The stats sampler's purpose is to populate Prometheus gauges that are scraped
typically every 15–60 s; sampling more aggressively than the scrape interval yields
no operator benefit and only adds load.

**Context** (`crates/bin/src/main.rs:502-512`)

```rust
async fn run_stats_sampler(repo: Arc<dyn Repo>, interval: Duration, shutdown: CancellationToken) {
    let mut last_callbacks: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }

        match repo.fetch_stats().await {
            // …
```

**Recommended fix**

Decouple the stats sampler cadence from `poll_interval`. Either:

- Add a dedicated `observability.stats_sample_interval_secs` config knob, defaulting
  to 30 s or 60 s (matching typical Prometheus scrape cadence). Pass that into
  `run_stats_sampler` instead of `poll_interval`.
- Or sample lazily on the `/metrics` scrape itself by integrating a recorder that
  invokes a callback at scrape time (the `metrics-exporter-prometheus` crate supports
  this via `set_describe`/`pre_render` hooks). That eliminates the polling loop and
  guarantees sample frequency tracks scrape frequency exactly.

A minimal patch:

```rust
// crates/core/src/config.rs (ObservabilityConfig)
#[serde(default = "default_stats_sample_interval_secs")]
pub stats_sample_interval_secs: u64,

fn default_stats_sample_interval_secs() -> u64 { 30 }

// crates/bin/src/main.rs (None command branch)
let stats_interval = Duration::from_secs(config.observability.stats_sample_interval_secs);
workers.spawn(async move { run_stats_sampler(stats_repo, stats_interval, stats_shutdown).await });
```

**Why this fix**

Polling every 5 s is hostile to large outboxes; aligning sample cadence to scrape
cadence (~30 s) cuts DB load by 6× with no observable change in dashboard fidelity.

---

### Finding 21 — Histogram bucket boundaries are exporter defaults; `outbox_external_pending_seconds` is unreadable above ~10 s

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:476-491`, `crates/core/src/metrics.rs:141-147` |
| **Severity** | Low |
| **Category** | Observability |

**Problem**

`PrometheusBuilder::new().with_http_listener(addr).install()` is the entire metrics
exporter configuration — no `set_buckets_for_metric` or `set_buckets` is called. The
`metrics_exporter_prometheus` crate's default bucket boundaries cover roughly
`[0.005 s, 10 s]` (the same defaults Prometheus client libraries traditionally use
for "request latency"). Three of the four histograms in `metrics.rs` fit that range:

- `outbox_dispatch_duration_seconds` — typically 50 ms – 30 s ✓ default OK
- `outbox_cycle_duration_seconds` — typically 1 ms – 1 s ✓ default OK
- `outbox_retention_cycle_duration_seconds` — typically 10 ms – 10 s ✓ default OK

But `outbox_external_pending_seconds` records the **age** of external-mode
deliveries awaiting confirmation, which the TDD describes as ranging from seconds
to multiple days (§8.4 — `external_completion_timeout` may be `7 * 86_400`). With
the default buckets, every observation above 10 s falls in the `+Inf` overflow
bucket, so `histogram_quantile(0.5, …)` returns either ≤10 s or `+Inf`. The
histogram is functionally unusable for the metric's intended purpose.

**Recommended fix**

Configure dedicated buckets per histogram at exporter installation time:

```rust
fn init_metrics(obs: &ObservabilityConfig) -> Result<()> {
    use metrics_exporter_prometheus::PrometheusBuilder;

    let addr: std::net::SocketAddr = obs
        .metrics_bind
        .parse()
        .context("parsing observability.metrics_bind")?;

    PrometheusBuilder::new()
        .with_http_listener(addr)
        // dispatch + cycle + retention duration: ms .. tens of seconds
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Suffix("_duration_seconds".to_string()),
            &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0],
        )?
        // external-pending age: seconds .. 7 days
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                outbox_dispatcher_core::metrics::EXTERNAL_PENDING_SECONDS.to_string(),
            ),
            &[5.0, 30.0, 60.0, 300.0, 900.0, 3600.0, 21_600.0, 86_400.0, 259_200.0, 604_800.0],
        )?
        .install()
        .context("installing Prometheus metrics exporter")?;

    info!(bind = %addr, "Prometheus metrics endpoint listening");
    Ok(())
}
```

**Why this fix**

`metric_label()` and `set_lag_seconds` already advertise a contract via the metric
registry; without bucket configuration, the `outbox_external_pending_seconds`
histogram's `_bucket{le=…}` series can never report anything above 10 s, which is
the entire interesting range of the metric. Cheap, one-time configuration; large
visibility improvement.

---

### Finding 22 — `outbox_lag_seconds` measures next-retry lag, not event age

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/repo.rs:810-816`, `crates/bin/src/main.rs:518` |
| **Severity** | Low |
| **Category** | Correctness |

**Problem**

The stats sampler publishes `outbox_lag_seconds` from `Stats.oldest_pending_age_seconds`,
which the SQL computes as `now() - MIN(available_at) FILTER (... pending ...)`. The
`available_at` column is **updated on every retry** to encode the next-retry deadline,
so a delivery that has been retried N times appears `backoff_secs[N]` younger than
its actual age. A 12-hour-old event whose next retry is scheduled for 30 s from now
contributes `0` (or even negative — clamped to `0`) to the gauge, not `43 200`.

Operators alerting on `outbox_lag_seconds > 600` ("nothing has been pending more
than 10 minutes") will silently miss a queue with many old-but-currently-backed-off
rows.

**Context** (`crates/core/src/repo.rs:810-816`)

```sql
EXTRACT(EPOCH FROM (
    now() - MIN(available_at) FILTER (
        WHERE dispatched_at IS NULL
          AND processed_at  IS NULL
          AND dead_letter   = FALSE
    )
))::float8 AS oldest_pending_age_seconds
```

**Recommended fix**

Two options depending on intended semantics:

- **Event age (recommended)** — `MIN(e.created_at)` joined from `outbox_events`, so
  the gauge reports "how long has the oldest still-pending event been waiting":
  ```sql
  EXTRACT(EPOCH FROM (
      now() - MIN(e.created_at) FILTER (
          WHERE d.dispatched_at IS NULL
            AND d.processed_at  IS NULL
            AND d.dead_letter   = FALSE
      )
  ))::float8 AS oldest_pending_age_seconds
  FROM outbox_deliveries d
  JOIN outbox_events e ON e.event_id = d.event_id
  ```
- **Two distinct gauges** — keep `outbox_lag_seconds` as "next-retry lag" and
  add `outbox_oldest_pending_event_age_seconds` for event age. More work, but
  preserves the existing meaning for any dashboards already built on it.

**Why this fix**

`outbox_lag_seconds` is the canonical lag SLO metric; consensus interpretation is
"how old is the oldest unprocessed thing". Reporting `now() - available_at` instead
of `now() - created_at` is a documented Prometheus anti-pattern (a delivery in
1-day backoff doesn't make the queue "fresh"). Spec §12.1 lists `outbox_lag_seconds`
without elaboration, so either fix is spec-compliant; the event-age interpretation
matches the metric's name and operator expectations.

---

### Finding 23 — Retention worker first cycle delayed by `[interval, 2 × interval)`, not `[0, interval)`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/retention.rs:71-95` |
| **Severity** | Low |
| **Category** | Concurrency / UX |

**Problem**

Finding 8's fix added a startup jitter to prevent multi-replica thundering, but
the jitter is *added to* the regular interval sleep rather than replacing the first
one:

```rust
// startup jitter
if config.cleanup_interval_secs > 0 {
    let jitter_secs = rand::rng().random_range(0..config.cleanup_interval_secs);
    tokio::time::sleep(Duration::from_secs(jitter_secs)).await;   // 0 .. interval-1
}

loop {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => { … }
        _ = tokio::time::sleep(interval) => {}                    // another full interval
    }
    // … run cycle …
}
```

Total wait before the first deletion runs: `jitter + interval`, i.e.
`[interval, 2 × interval)`. At the default `cleanup_interval_secs = 3600`, the first
cycle fires 1–2 hours after startup. Operators redeploying a service that's been
accumulating dead-lettered events will be surprised that retention "does nothing"
for an hour.

The thundering-herd protection wants spreads over `[0, interval)`, not
`[interval, 2 × interval)`.

**Recommended fix**

Apply jitter as the *first* sleep, then loop with the regular interval:

```rust
let mut next_sleep = if config.cleanup_interval_secs > 0 {
    use rand::RngExt;
    Duration::from_secs(rand::rng().random_range(0..config.cleanup_interval_secs))
} else {
    Duration::ZERO
};

loop {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => { …; return; }
        _ = tokio::time::sleep(next_sleep) => {}
    }

    // … run cycle …

    next_sleep = interval;
}
```

**Why this fix**

`[0, interval)` is the canonical jitter pattern: replicas spread out over one full
interval and the first cycle runs promptly after startup. The current
`[interval, 2 × interval)` shape both delays the first deletion and shifts the
spread window without buying anything.

---

### Finding 24 — Ctrl-C handler task is orphaned from the JoinSet

| Field | Value |
|-------|-------|
| **File:Line** | `crates/bin/src/main.rs:221-228` |
| **Severity** | Low |
| **Category** | Idiom |

**Problem**

Finding 16 collected the admin server, retention worker, and stats sampler into a
`JoinSet` that is awaited before `tracer_provider.shutdown()`. The earlier-spawned
signal-handler task is **not** in the set:

```rust
let shutdown_clone = shutdown.clone();
tokio::spawn(async move {
    if let Ok(()) = tokio::signal::ctrl_c().await {
        info!("received SIGINT/SIGTERM, requesting shutdown");
        shutdown_clone.cancel();
    }
});
```

After the scheduler exits cleanly (e.g. because the admin server bound failed and
called `admin_shutdown_trigger.cancel()`, or because some future code path triggers
shutdown), this task is still parked on `ctrl_c()`. When `run()` returns and
`#[tokio::main]` drops the runtime, the task is aborted. Functionally harmless —
the task has nothing to drain — but it's the one outstanding background spawn that
violates the "wait for everything before exit" contract Finding 16 established.

**Recommended fix**

Use a `select!` against the cancellation token so the task exits naturally on any
shutdown path, and collect its handle into the same JoinSet:

```rust
{
    let shutdown_clone = shutdown.clone();
    workers.spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT/SIGTERM, requesting shutdown");
                shutdown_clone.cancel();
            }
            _ = shutdown_clone.cancelled() => {
                // some other path triggered shutdown — exit cleanly
            }
        }
    });
}
```

**Why this fix**

Tiny, but it removes the last "spawn-and-forget" in the main flow and makes the
shutdown contract uniform across all background tasks. Especially relevant once a
SIGTERM handler is added (today only SIGINT/Ctrl-C is wired).

---

## Updated summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 20 | Stats sampler runs 3–4 heavy queries every `poll_interval` | `crates/bin/src/main.rs:507-566` + `crates/core/src/repo.rs:787-902` | Medium | Performance | DONE | Added `observability.stats_sample_interval_secs` (default 30 s); sampler interval decoupled from `poll_interval`; validation added for zero value |
| 21 | Histogram bucket boundaries not configured | `crates/bin/src/main.rs:476-491` | Low | Observability | DONE | `init_metrics` now calls `set_buckets_for_metric` for `_duration_seconds` (ms–30 s) and `EXTERNAL_PENDING_SECONDS` (5 s–7 days) |
| 22 | `outbox_lag_seconds` measures next-retry lag, not event age | `crates/core/src/repo.rs:810-816` | Low | Correctness | DONE | `fetch_stats` SQL now joins `outbox_events` and uses `MIN(e.created_at)` so the gauge reports true event age regardless of retry backoff |
| 23 | Retention first cycle delayed by interval + jitter | `crates/core/src/retention.rs:71-95` | Low | Concurrency / UX | DONE | Replaced separate jitter sleep + loop sleep with a `next_sleep` variable initialised to jitter (`[0, interval)`) and reset to `interval` after the first cycle |
| 24 | Ctrl-C handler task is orphaned from the JoinSet | `crates/bin/src/main.rs:221-228` | Low | Idiom | DONE | Signal handler moved into `workers` JoinSet with a `select!` that exits cleanly on either Ctrl-C or any other shutdown-token cancellation |

> **Instructions for the implementing LLM:** same conventions — `TODO` → `DONE` on
> resolution, `SKIPPED` with a reason if intentionally not applied. Do not delete rows.

---

## PR state (fifth pass — 2026-05-14)

**Status: READY TO MERGE (with follow-ups recommended)**

Findings 1–19 are fully addressed and verified; the workspace builds and tests
cleanly on `phase7` (HEAD `61c1d40`):

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`
  pass with no warnings.
- `cargo test --workspace` — 278 core + 23 admin + 20 http-callback + 9 + 4
  integration = **334 tests green**.

The five new findings are **non-blocking**:

- **Finding 20 (Medium)** — stats sampler DB load. Only material on large
  deployments; default configuration is still functional, just chatty against the
  database. Worth fixing before scaling beyond a few hundred thousand deliveries,
  but not a merge gate for the phase-7 milestone.
- **Findings 21 / 22 / 23 / 24 (Low)** — histogram buckets, lag semantics,
  retention startup delay, signal-handler orphaning. All observability /
  ergonomics polish; none affect correctness of dispatch, retention, admin, or
  shutdown paths.

If the maintainer prefers to land all 24 findings before merge, suggested order is
F22 → F20 → F21 → F23 → F24 (F22 changes a metric's semantic; do it first so any
follow-up alert tuning is built on the final shape).

---

## Sixth-pass review (2026-05-15T15:48:18Z)

Findings 1–24 verified as DONE on a fresh re-read of `phase7` (HEAD `3c2628d`,
PR #6). The fifth-pass items have concrete, working implementations:

- **F20** — `observability.stats_sample_interval_secs` (default `30`) wired into
  `run_stats_sampler`; validation rejects `0` (`config.rs:622-624`).
- **F21** — `init_metrics` configures buckets via
  `Matcher::Suffix("_duration_seconds")` and
  `Matcher::Full(EXTERNAL_PENDING_SECONDS)` (`main.rs:501-519`).
- **F22** — `fetch_stats` SQL now joins `outbox_events` and uses
  `MIN(e.created_at)` (`repo.rs:810-818`).
- **F23** — Retention worker's `next_sleep` is initialised to `[0, interval)`
  jitter and reset to `interval` after the first cycle, watched by the same
  `tokio::select!` as shutdown (`retention.rs:71-92`).
- **F24** — Signal handler joined into the `workers` `JoinSet` with a
  `select!` against `ctrl_c()` and the cancellation token (`main.rs:228-244`).

Build/test status on `phase7` HEAD `3c2628d`:

- `cargo check --workspace` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo sort --workspace --check` — clean
- `cargo test --workspace` — **335 tests green** (279 core + 23 admin +
  20 http-callback + 9 + 4 integration)
- `cargo fmt --all -- --check` — **FAILS** (see Finding 25)

Three new findings emerged on this pass — one High (CI gate), two Low.

### Finding 25 — `cargo fmt --all --check` fails on seven test assertions in `config.rs`

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/config.rs:1034-1042, 1052-1060, 1225-1233, 1237-1245, 1368-1376, 1433-1441, 1696-1704` |
| **Severity** | High |
| **Category** | Idiom / CI |

**Problem**

CLAUDE.md mandates `cargo fmt --all` after every code change, and the project
has no exclusions. On the current HEAD, `cargo fmt --all -- --check` exits
non-zero with seven diffs in `crates/core/src/config.rs`: chained-assert blocks
that the formatter wants wrapped across multiple `assert!` arg lines. None of
these are new code — they're tests that were touched mid-PR — but the file
ships unformatted, which will fail any CI step that runs the mandated
`cargo fmt --all -- --check`.

**Context** (`crates/core/src/config.rs:1696-1704` — one of seven identical
patterns)

```rust
#[test]
fn test_validate_observability_stats_sample_interval_zero() {
    let mut cfg = build_config(full_toml());
    cfg.observability.stats_sample_interval_secs = 0;
    let errs = cfg.validate().unwrap_err();
    assert!(errs
        .0
        .iter()
        .any(|e| e.contains("stats_sample_interval_secs")));
}
```

`rustfmt` wants this in the canonical `assert!(\n    <expr>\n);` shape:

```rust
assert!(
    errs.0
        .iter()
        .any(|e| e.contains("stats_sample_interval_secs"))
);
```

**Recommended fix**

Run `cargo fmt --all` once and commit the result. The change is mechanical and
touches exactly the seven sites listed above.

```bash
cargo fmt --all
git add crates/core/src/config.rs
git commit -m "cargo fmt"
```

**Why this fix**

The mandatory post-change checklist in `CLAUDE.md` explicitly lists
`cargo fmt --all` as the first step after every edit. Shipping an unformatted
tree silently violates that contract and will break any CI step that gates on
`rustfmt`.

---

### Finding 26 — `stats_sample_interval_secs` has no lower-bound sanity check

| Field | Value |
|-------|-------|
| **File:Line** | `crates/core/src/config.rs:622-624` |
| **Severity** | Low |
| **Category** | Config |

**Problem**

`AppConfig::validate` rejects `stats_sample_interval_secs == 0` but accepts any
positive value. Finding 20's motivation was that polling every 5 s issues
three or four heavy aggregate scans against `outbox_deliveries` and burns DB
CPU on large deployments. With no minimum, an operator can reintroduce
exactly that problem by setting `stats_sample_interval_secs = 1` (e.g. for
fast local-dev feedback) and then deploy that config to prod by mistake.

The closely related `dispatch.external_timeout_sweep_interval_secs` enforces a
10 s minimum (`config.rs:565-570` via `MIN_SWEEP_INTERVAL_SECS`); the same
pattern would apply here.

**Context** (`crates/core/src/config.rs:622-624`)

```rust
if self.observability.stats_sample_interval_secs == 0 {
    errors.push("observability.stats_sample_interval_secs must be > 0".to_string());
}
```

**Recommended fix**

Add a small minimum (10 s is conservative and still leaves dev workflows
usable) and surface the constant alongside the existing `MIN_*` constants:

```rust
const MIN_STATS_SAMPLE_INTERVAL_SECS: u64 = 10;

// in validate()
if self.observability.stats_sample_interval_secs < MIN_STATS_SAMPLE_INTERVAL_SECS {
    errors.push(format!(
        "observability.stats_sample_interval_secs must be >= {MIN_STATS_SAMPLE_INTERVAL_SECS} \
         (sub-10s sampling re-introduces sustained DB load — see review F20)"
    ));
}
```

**Why this fix**

Validation is the only place where the rationale of F20 can be enforced
across deployments. A bare `> 0` check leaves the foot-gun fully loaded.

---

### Finding 27 — `[observability]` section missing from base env config; new knob has no discovery surface

| Field | Value |
|-------|-------|
| **File:Line** | `envs/app_config.toml`, `envs/app_config_dev.toml`, `envs/app_config_local.toml` |
| **Severity** | Low |
| **Category** | Config |

**Problem**

None of the three checked-in env configs (`envs/app_config.toml`,
`app_config_dev.toml`, `app_config_local.toml`) contain an `[observability]`
section. All three observability knobs — `metrics_bind`, `otel_endpoint`,
and the newly-added `stats_sample_interval_secs` — are populated entirely from
`#[serde(default = "...")]` fallbacks in `ObservabilityConfig` (`config.rs:357-380`).
This pre-dates phase 7 for the first two keys, but landing a third unannotated
knob compounds the problem: operators tuning observability have no visible
reference to copy from, no comment on what the default means, and no signal
that the knob exists at all without grepping the Rust source.

**Context** (`envs/app_config.toml` — no `[observability]` block anywhere)

```toml
[retention]
enabled = false
processed_retention_days = 7
dead_letter_retention_days = 30
cleanup_interval_secs = 3600
batch_limit = 1000

# Signing keys: maps key id -> env var name holding the base64-encoded HMAC secret.
# Example:
# [signing_keys]
# "welcome-v1" = { secret_env = "WELCOME_HMAC_SECRET" }
[signing_keys]
```

**Recommended fix**

Add a commented `[observability]` block to `envs/app_config.toml` so the
defaults are self-documenting:

```toml
[observability]
# Address the Prometheus /metrics HTTP listener binds to.
metrics_bind = "0.0.0.0:9091"
# OTLP gRPC endpoint for traces. Empty disables OpenTelemetry export.
otel_endpoint = ""
# How often the stats sampler runs heavy aggregate queries (events/deliveries
# counts, oldest-pending age, external-pending ages). Defaults to 30 s to
# align with typical Prometheus scrape cadence — see review finding F20.
stats_sample_interval_secs = 30
```

`app_config_dev.toml` and `app_config_local.toml` can override only the keys
that differ.

**Why this fix**

`envs/app_config.toml` is the file CLAUDE.md describes as "Base defaults for
all envs"; operators reading it should be able to discover every operator-
facing knob without consulting source. The keys still work fine via defaults,
so this is documentation-only.

---

## Updated summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 25 | `cargo fmt --all --check` fails on seven test assertions in `config.rs` | `crates/core/src/config.rs:1034-1704` (7 sites) | High | Idiom / CI | TODO | Run `cargo fmt --all` and commit; CLAUDE.md mandates this after every change |
| 26 | `stats_sample_interval_secs` has no lower-bound sanity check | `crates/core/src/config.rs:622-624` | Low | Config | TODO | Add a `MIN_STATS_SAMPLE_INTERVAL_SECS` (e.g. 10 s) and reject below it, mirroring `MIN_SWEEP_INTERVAL_SECS` |
| 27 | `[observability]` block missing from base env config | `envs/app_config.toml`, `app_config_dev.toml`, `app_config_local.toml` | Low | Config | TODO | Add a documented `[observability]` block to `app_config.toml`; per-env files only override what differs |

> **Instructions for the implementing LLM:** same conventions — `TODO` → `DONE`
> on resolution, `SKIPPED` with a reason if intentionally not applied. Do not
> delete rows.

---

## PR state (sixth pass — 2026-05-15)

**Status: NOT READY TO MERGE — blocked on Finding 25 (one mechanical commit).**

All twenty-four prior findings are addressed and verified on `phase7`
HEAD `3c2628d`. The full workspace is functionally correct:

- `cargo check --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo sort --workspace --check`,
  `cargo test --workspace` all pass on the current HEAD — **335 tests green**.

The single blocker is **Finding 25**: `cargo fmt --all -- --check` exits
non-zero because seven test assertions in `crates/core/src/config.rs` were
edited mid-PR without re-running `cargo fmt`. CLAUDE.md explicitly mandates
`cargo fmt --all` as the first post-change step, and any CI gate following
that contract will fail. Resolution is one `cargo fmt --all && git commit -am`.

After F25 is landed:

- **Findings 26 / 27 (Low)** are documentation / defence-in-depth and can ship
  in a follow-up. F26 hardens validation against operators reintroducing the
  F20 problem; F27 surfaces the observability knobs in the base env config.
  Neither affects runtime behaviour with current configs.

Once F25 is fixed and pushed, this PR is ready to merge. Suggested follow-up
order: F25 (this commit) → F26 → F27.
