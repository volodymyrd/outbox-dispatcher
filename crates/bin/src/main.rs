use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use outbox_dispatcher_admin_api::{AdminState, build_router};
use outbox_dispatcher_core::{
    AppConfig, DatabaseConfig, DispatchConfig, KeyRing, LogConfig, LogFormat, ObservabilityConfig,
    PgRepo, Repo, metrics, run_retention_worker, run_scheduler_with_cycle_tracker,
    schedule_new_deliveries,
};
use outbox_dispatcher_http_callback::HttpCallback;
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Migrations embedded at compile time from the workspace-root `migrations/` directory.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[derive(Parser)]
#[command(name = "outbox-dispatcher", version, about)]
struct Cli {
    /// Skip running database migrations on startup.
    #[arg(long, global = true)]
    skip_migrations: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run all pending database migrations and exit.
    Migrate,
    /// Subcommands for inspecting embedded migrations.
    #[command(name = "migrations")]
    Migrations {
        #[command(subcommand)]
        action: MigrationsAction,
    },
    /// Re-scan outbox_events since a given timestamp and ensure deliveries exist.
    ///
    /// Idempotent — safe to run multiple times. Useful after a crash during
    /// scheduling or after restoring a database backup that rewound the cursor.
    Rescan {
        /// ISO-8601 timestamp to scan from (e.g. "2026-01-01T00:00:00Z").
        #[arg(long)]
        since: String,
    },
}

#[derive(Subcommand)]
enum MigrationsAction {
    /// Print all embedded migration SQL to stdout.
    Dump,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:?}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // Parse CLI first so `--help` and `--version` work without a valid config.
    let cli = Cli::parse();

    let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "local".to_string());

    let config = AppConfig::load(&app_env)
        .with_context(|| format!("failed to load app config for env '{app_env}'"))?;

    if let Err(errors) = config.validate() {
        for e in &errors.0 {
            eprintln!("config error: {e}");
        }
        eprintln!("invalid configuration ({} error(s))", errors.0.len());
        std::process::exit(1);
    }

    // Fail fast if any configured signing key cannot be resolved from its env var.
    let keyring = match KeyRing::load(&config.signing_keys) {
        Ok(kr) => Arc::new(kr),
        Err(errors) => {
            for e in &errors.0 {
                eprintln!("signing key error: {e}");
            }
            eprintln!("invalid signing keys ({} error(s))", errors.0.len());
            std::process::exit(1);
        }
    };

    let tracer_provider = init_tracing(&config.log, &config.observability, &app_env)
        .context("initialising tracing subscriber")?;
    info!(env = %app_env, "outbox-dispatcher starting");

    if config.dispatch.allow_insecure_urls {
        warn!("allow_insecure_urls is enabled — HTTP (non-TLS) callback URLs are accepted");
    }
    if config.dispatch.allow_unsigned_callbacks {
        warn!(
            "allow_unsigned_callbacks is enabled — callbacks without a signing_key_id are accepted"
        );
    }
    if config.http_client.allow_insecure_tls {
        warn!(
            "http_client.allow_insecure_tls is enabled — TLS certificate verification is \
             DISABLED; this is for development only and must NOT be used in production"
        );
    }

    match cli.command {
        Some(Command::Migrations {
            action: MigrationsAction::Dump,
        }) => {
            dump_migrations();
            return Ok(());
        }

        Some(Command::Migrate) => {
            let pool = connect_pool(&config.database).await?;
            run_migrations(&pool).await?;
            info!("migrations complete");
            return Ok(());
        }

        Some(Command::Rescan { since }) => {
            let since_ts = since
                .parse::<chrono::DateTime<chrono::Utc>>()
                .with_context(|| {
                    format!(
                        "invalid --since timestamp '{since}'; \
                         use ISO-8601, e.g. 2026-01-01T00:00:00Z"
                    )
                })?;

            let pool = connect_pool(&config.database).await?;
            if !cli.skip_migrations {
                run_migrations(&pool).await?;
            }
            validate_schema(&pool).await?;

            let dispatch_config = DispatchConfig::from(config.dispatch.clone());
            let repo = Arc::new(PgRepo::new(pool.clone(), dispatch_config.clone()));

            // Find the cursor starting point: highest event id with created_at < since_ts.
            let cursor: i64 = sqlx::query_scalar!(
                r#"SELECT COALESCE(MAX(id), 0) AS "id!" FROM outbox_events WHERE created_at < $1"#,
                since_ts,
            )
            .fetch_one(&pool)
            .await
            .context("querying rescan start cursor")?;

            info!(
                since = %since_ts,
                start_cursor = cursor,
                "starting rescan"
            );

            // Iterate events in batches, calling ensure_deliveries for each.
            let mut current_cursor = cursor;
            let mut total_batches = 0u64;
            loop {
                let new_cursor =
                    schedule_new_deliveries(repo.as_ref(), &dispatch_config, current_cursor)
                        .await
                        .context("rescan batch failed")?;
                if new_cursor == current_cursor {
                    break; // no more events
                }
                current_cursor = new_cursor;
                total_batches += 1;
            }

            info!(
                batches = total_batches,
                final_cursor = current_cursor,
                "rescan complete"
            );
            return Ok(());
        }

        None => {
            let pool = connect_pool(&config.database).await?;

            if !cli.skip_migrations {
                run_migrations(&pool).await?;
            }

            validate_schema(&pool).await?;

            // Install the Prometheus metrics exporter before starting any workers.
            init_metrics(&config.observability).context("initialising Prometheus exporter")?;

            let dispatch_config = DispatchConfig::from(config.dispatch.clone());
            let repo = Arc::new(PgRepo::new(pool.clone(), dispatch_config.clone()));

            // Build the HTTP callback implementation backed by reqwest.
            let http_callback = HttpCallback::new(&config.http_client, keyring)
                .context("building HTTP callback client")?;
            let callback: Arc<dyn outbox_dispatcher_core::Callback> = Arc::new(http_callback);

            // Create a dedicated connection for LISTEN/NOTIFY (the pool cannot be used
            // for LISTEN because each .execute() call may use a different connection).
            let listener = PgListener::connect_with(&pool)
                .await
                .context("creating PgListener for LISTEN/NOTIFY")?;

            let listener_status = Arc::new(AtomicBool::new(false));

            // Shared state for the /ready endpoint: updated by the scheduler after each cycle.
            let last_cycle_at = Arc::new(AtomicI64::new(0));

            // Graceful-shutdown token — wired to SIGTERM/SIGINT.
            let shutdown = CancellationToken::new();

            // Collect background task handles so we can await them during graceful
            // shutdown. Each task watches `shutdown.cancelled()` internally — we just
            // need to wait for them to drain before flushing the tracer.
            let mut workers: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

            // Signal handler: cancel the shutdown token on SIGINT or SIGTERM. Also
            // exits cleanly when the token is cancelled by any other path (e.g. admin
            // bind failure) so the JoinSet can drain uniformly.
            //
            // Both signals must be handled: SIGINT covers interactive Ctrl-C and most
            // CI kill paths, while SIGTERM is what `docker stop` and the Kubernetes
            // kubelet send at pod termination. Without a SIGTERM handler the default
            // Linux disposition is "terminate immediately", which skips the JoinSet
            // drain and the OpenTelemetry tracer flush.
            {
                let shutdown_clone = shutdown.clone();
                workers.spawn(async move {
                    wait_for_shutdown_signal(shutdown_clone).await;
                });
            }

            // Spawn the admin HTTP server on a separate task.
            let admin_bind = config.admin.bind.clone();
            let admin_token = config.admin.auth_token.clone();
            let admin_state = AdminState {
                repo: repo.clone(),
                listener_status: listener_status.clone(),
                last_cycle_at: last_cycle_at.clone(),
                ready_deadline: Duration::from_secs(
                    config.dispatch.poll_interval_secs.saturating_mul(2),
                ),
            };
            let admin_router = build_router(admin_state, admin_token);
            let admin_shutdown_signal = shutdown.clone();
            let admin_shutdown_trigger = shutdown.clone();
            workers.spawn(async move {
                let addr: std::net::SocketAddr = admin_bind
                    .parse()
                    .expect("admin.bind was validated at startup");
                info!(bind = %addr, "admin API listening");
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        error!(error = %e, "failed to bind admin API listener; shutting down");
                        admin_shutdown_trigger.cancel();
                        return;
                    }
                };
                if let Err(e) = axum::serve(listener, admin_router)
                    .with_graceful_shutdown(async move {
                        admin_shutdown_signal.cancelled().await;
                    })
                    .await
                {
                    error!(error = %e, "admin API server error; shutting down");
                    admin_shutdown_trigger.cancel();
                }
            });

            // Spawn the optional retention worker (disabled by default).
            if config.retention.enabled {
                let retention_repo = repo.clone();
                let retention_cfg = config.retention.clone();
                let retention_shutdown = shutdown.clone();
                workers.spawn(async move {
                    run_retention_worker(retention_repo, retention_cfg, retention_shutdown).await;
                });
            }

            // Spawn the periodic stats sampler that publishes queue-state gauges.
            {
                let stats_repo = repo.clone();
                let stats_interval =
                    Duration::from_secs(config.observability.stats_sample_interval_secs);
                let stats_shutdown = shutdown.clone();
                workers.spawn(async move {
                    run_stats_sampler(stats_repo, stats_interval, stats_shutdown).await;
                });
            }

            info!("starting scheduler");
            run_scheduler_with_cycle_tracker(
                repo,
                callback,
                dispatch_config,
                listener,
                listener_status,
                shutdown,
                Some(last_cycle_at),
            )
            .await
            .context("scheduler exited with error")?;

            // Wait for the admin server, retention worker, and stats sampler to drain
            // before flushing the tracer. This ensures spans emitted during the workers'
            // final drain step are queued before the batch exporter is shut down.
            while let Some(res) = workers.join_next().await {
                if let Err(e) = res {
                    warn!(error = ?e, "background worker exited abnormally during shutdown");
                }
            }

            // Flush any buffered spans before the process exits.
            if let Some(provider) = tracer_provider
                && let Err(e) = provider.shutdown()
            {
                warn!(error = ?e, "OpenTelemetry tracer shutdown failed");
            }

            info!("outbox-dispatcher stopped cleanly");
        }
    }

    Ok(())
}

// ── Migration helpers ──────────────────────────────────────────────────────────

async fn run_migrations(pool: &PgPool) -> Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("running database migrations")?;
    info!("database migrations applied successfully");
    Ok(())
}

/// Refuses to start if the database schema is ahead of this binary (downgrade guard).
async fn validate_schema(pool: &PgPool) -> Result<()> {
    // 42P01 = undefined_table: migrations have never run on this DB, treat as version None.
    let max_db_version: Option<i64> =
        match sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_one(pool)
            .await
        {
            Ok(v) => v,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P01") => None,
            Err(e) => return Err(e).context("querying applied migration versions"),
        };

    if let Some(db_version) = max_db_version {
        let max_binary_version = MIGRATOR
            .migrations
            .iter()
            .map(|m| m.version)
            .max()
            .unwrap_or(0);

        if db_version > max_binary_version {
            anyhow::bail!(
                "schema version mismatch: database is at migration version {} but this binary \
                 only knows up to {}; upgrade the binary before running",
                db_version,
                max_binary_version
            );
        }
    }

    Ok(())
}

fn dump_migrations() {
    for migration in MIGRATOR.migrations.iter() {
        println!("-- Version: {}", migration.version);
        println!("-- Description: {}", migration.description);
        println!("{}", migration.sql);
        println!();
    }
}

// ── Database connection ────────────────────────────────────────────────────────

async fn connect_pool(db: &DatabaseConfig) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(db.max_connections)
        .acquire_timeout(Duration::from_secs(db.acquire_timeout_secs))
        .connect(&db.url)
        .await
        .context("connecting to Postgres")
}

// ── Observability ──────────────────────────────────────────────────────────────

fn init_tracing(
    log: &LogConfig,
    obs: &ObservabilityConfig,
    app_env: &str,
) -> Result<Option<opentelemetry_sdk::trace::SdkTracerProvider>> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| log.filter.clone()),
    )
    .context("invalid log filter directive")?;

    // Build the fmt layer, respecting `log.format` in both code paths.
    let fmt_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = match log.format {
        LogFormat::Json => Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true),
        ),
        LogFormat::Pretty => Box::new(tracing_subscriber::fmt::layer()),
    };

    // Optionally attach an OpenTelemetry tracing layer when an OTLP endpoint is configured.
    if !obs.otel_endpoint.is_empty() {
        use opentelemetry::KeyValue;
        use opentelemetry_otlp::WithExportConfig;
        use opentelemetry_sdk::Resource;

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&obs.otel_endpoint)
            .build()
            .context("building OTLP span exporter")?;

        let resource = Resource::builder()
            .with_attributes([
                KeyValue::new("service.name", "outbox-dispatcher"),
                KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                KeyValue::new("deployment.environment", app_env.to_owned()),
            ])
            .build();

        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(exporter)
            .build();

        // Obtain the tracer before moving the provider into the global registry.
        let tracer =
            opentelemetry::trace::TracerProvider::tracer(&tracer_provider, "outbox-dispatcher");
        // Keep a clone for graceful shutdown; the global registry owns the original.
        let provider_for_shutdown = tracer_provider.clone();
        opentelemetry::global::set_tracer_provider(tracer_provider);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .with(otel_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;

        info!(endpoint = %obs.otel_endpoint, "OpenTelemetry tracing enabled");
        Ok(Some(provider_for_shutdown))
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init()
            .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;

        Ok(None)
    }
}

/// Install the Prometheus metrics exporter and start the scrape HTTP listener.
///
/// The exporter listens on `obs.metrics_bind` and serves the standard `/metrics`
/// endpoint. The listener runs in a background thread managed by the exporter.
///
/// Histogram buckets are configured per-metric family:
/// - `*_duration_seconds` (dispatch, cycle, retention): ms–tens-of-seconds range.
/// - `outbox_external_pending_seconds`: seconds–7-days range (external completion
///   timeouts can be configured up to 7 days per the TDD §8.4).
fn init_metrics(obs: &ObservabilityConfig) -> Result<()> {
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

    let addr: std::net::SocketAddr = obs
        .metrics_bind
        .parse()
        .context("parsing observability.metrics_bind")?;

    PrometheusBuilder::new()
        .with_http_listener(addr)
        // Dispatch, cycle, and retention durations: milliseconds to tens of seconds.
        .set_buckets_for_metric(
            Matcher::Suffix("_duration_seconds".to_string()),
            &[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
            ],
        )
        .context("configuring _duration_seconds histogram buckets")?
        // External-pending age: seconds to 7 days (max external_completion_timeout per §8.4).
        .set_buckets_for_metric(
            Matcher::Full(outbox_dispatcher_core::metrics::EXTERNAL_PENDING_SECONDS.to_string()),
            &[
                5.0, 30.0, 60.0, 300.0, 900.0, 3_600.0, 21_600.0, 86_400.0, 259_200.0, 604_800.0,
            ],
        )
        .context("configuring external_pending_seconds histogram buckets")?
        .install()
        .context("installing Prometheus metrics exporter")?;

    info!(bind = %addr, "Prometheus metrics endpoint listening");
    Ok(())
}

// ── Stats sampler ──────────────────────────────────────────────────────────────

/// Periodically calls `repo.fetch_stats()` and publishes queue-state gauges so
/// `outbox_lag_seconds`, `outbox_pending_deliveries`, and
/// `outbox_external_pending_deliveries` are populated for Prometheus scraping.
/// Also samples per-row ages for the `outbox_external_pending_seconds` histogram.
///
/// Runs every `interval` (typically the scheduler's `poll_interval`) and stops when
/// `shutdown` is cancelled.
async fn run_stats_sampler(repo: Arc<dyn Repo>, interval: Duration, shutdown: CancellationToken) {
    // Track callbacks seen on the previous tick so we can zero out stale series
    // when a callback drains to zero and disappears from fetch_stats results.
    let mut last_callbacks: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }

        match repo.fetch_stats().await {
            Ok(stats) => {
                // Use NaN when the queue is empty so alerts on "lag > N" don't
                // conflate "no pending events" with "freshly-enqueued event at age 0".
                metrics::set_lag_seconds(stats.oldest_pending_age_seconds.unwrap_or(f64::NAN));

                let mut now_seen: HashSet<String> = HashSet::new();
                for (cb, s) in &stats.callbacks {
                    now_seen.insert(cb.clone());
                    metrics::set_pending_deliveries(cb, "managed", s.pending_managed as f64);
                    metrics::set_pending_deliveries(cb, "external", s.pending_external as f64);
                    metrics::set_external_pending_deliveries(cb, s.external_pending as f64);
                }

                // Zero out gauges for callbacks that have fully drained so scrapes
                // don't show phantom backlogs from stale label sets.
                for stale in last_callbacks.difference(&now_seen) {
                    metrics::set_pending_deliveries(stale, "managed", 0.0);
                    metrics::set_pending_deliveries(stale, "external", 0.0);
                    metrics::set_external_pending_deliveries(stale, 0.0);
                }
                last_callbacks = now_seen;
            }
            Err(e) => {
                warn!(error = %e, "stats sampler: fetch_stats failed");
            }
        }

        // Populate the outbox_external_pending_seconds histogram with per-row ages
        // for external-mode deliveries currently awaiting completion confirmation.
        //
        // NOTE: snapshot semantics — rows are ordered oldest-first and capped at
        // SAMPLE_LIMIT. When the backlog exceeds the limit the histogram's lower
        // buckets become empty until the backlog drains. A WARN is emitted so
        // operators know the histogram has been truncated.
        const SAMPLE_LIMIT: i64 = 1000;
        match repo.sample_external_pending_ages(SAMPLE_LIMIT).await {
            Ok(samples) => {
                if samples.len() as i64 == SAMPLE_LIMIT {
                    warn!(
                        limit = SAMPLE_LIMIT,
                        "stats sampler: sample_external_pending_ages hit the row limit; \
                         the histogram's lower buckets may be empty until the backlog drains"
                    );
                }
                for (cb, age_secs) in samples {
                    metrics::record_external_pending_seconds(&cb, age_secs);
                }
            }
            Err(e) => {
                warn!(error = %e, "stats sampler: sample_external_pending_ages failed");
            }
        }
    }
}

// ── Signal handling ────────────────────────────────────────────────────────────

/// Wait for SIGINT (Ctrl-C) or SIGTERM and cancel the shutdown token.
///
/// On Unix both signals are caught so that `docker stop` and the Kubernetes
/// kubelet (which send SIGTERM) trigger the same graceful drain path as an
/// interactive Ctrl-C. The `#[cfg(unix)]` / `#[cfg(not(unix))]` split is
/// required because `tokio::select!` does not support `#[cfg(…)]` on individual
/// branches.
///
/// Also exits cleanly when the token is cancelled by any other path (e.g. admin
/// bind failure) so the JoinSet drains uniformly.
async fn wait_for_shutdown_signal(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, requesting shutdown");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, requesting shutdown");
            }
            _ = shutdown.cancelled() => {
                return; // Another path triggered shutdown — exit cleanly.
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received SIGINT, requesting shutdown");
            }
            _ = shutdown.cancelled() => {
                return; // Another path triggered shutdown — exit cleanly.
            }
        }
    }
    shutdown.cancel();
}
