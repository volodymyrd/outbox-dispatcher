use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use outbox_dispatcher_admin_api::{AdminState, build_router};
use outbox_dispatcher_core::{
    AppConfig, DatabaseConfig, DispatchConfig, KeyRing, LogConfig, LogFormat, PgRepo,
    run_scheduler_with_cycle_tracker,
};
use outbox_dispatcher_http_callback::HttpCallback;
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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

    init_tracing(&config.log).context("initialising tracing subscriber")?;
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

        None => {
            let pool = connect_pool(&config.database).await?;

            if !cli.skip_migrations {
                run_migrations(&pool).await?;
            }

            validate_schema(&pool).await?;

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
            let last_cycle_at = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));

            // Graceful-shutdown token — wired to SIGTERM/SIGINT.
            let shutdown = CancellationToken::new();
            let shutdown_clone = shutdown.clone();
            tokio::spawn(async move {
                if let Ok(()) = tokio::signal::ctrl_c().await {
                    info!("received SIGINT/SIGTERM, requesting shutdown");
                    shutdown_clone.cancel();
                }
            });

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
            let admin_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let addr: std::net::SocketAddr = admin_bind
                    .parse()
                    .expect("admin.bind was validated at startup");
                info!(bind = %addr, "admin API listening");
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .expect("failed to bind admin API listener");
                axum::serve(listener, admin_router)
                    .with_graceful_shutdown(async move {
                        admin_shutdown.cancelled().await;
                    })
                    .await
                    .expect("admin API server error");
            });

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

fn init_tracing(log: &LogConfig) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| log.filter.clone()),
    )
    .context("invalid log filter directive")?;
    match log.format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .try_init()
                .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .try_init()
                .map_err(|e| anyhow::anyhow!("failed to install tracing subscriber: {e}"))?;
        }
    }
    Ok(())
}
