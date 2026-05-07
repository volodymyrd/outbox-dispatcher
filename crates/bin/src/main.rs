use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use outbox_dispatcher_core::config::{AppConfig, DatabaseConfig, LogConfig, LogFormat};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
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
async fn main() -> Result<()> {
    // Parse CLI first so `--help` and `--version` work without a valid config.
    let cli = Cli::parse();

    let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "local".to_string());

    let config = AppConfig::load(&app_env)
        .with_context(|| format!("failed to load app config for env '{app_env}'"))?;

    if let Err(errors) = config.validate() {
        for e in &errors.0 {
            eprintln!("config error: {e}");
        }
        anyhow::bail!("invalid configuration ({} error(s))", errors.0.len());
    }

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

            info!("outbox-dispatcher started (dispatch loop not yet implemented — Phase 3+)");
            // TODO(Phase 3): start scheduler, dispatch loop, admin API.
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
