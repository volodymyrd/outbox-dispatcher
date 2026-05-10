//! Integration tests for Phase 4 — dispatch lock and recovery via `locked_until` expiry.
//!
//! Each test gets a fresh Postgres instance via `testcontainers`.
//! Docker must be running locally for these tests to pass.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use outbox_dispatcher_core::{DispatchConfig, PgRepo, Repo, schedule_new_deliveries};
use serde_json::json;
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

// ── Test helpers ───────────────────────────────────────────────────────────────

/// Spin up a fresh Postgres container, run migrations, and return a pool.
async fn setup_db() -> (PgPool, testcontainers::ContainerAsync<Postgres>) {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("failed to start Postgres container");

    let host = container.get_host().await.expect("get_host");
    let port = container.get_host_port_ipv4(5432).await.expect("get_port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&url)
        .await
        .expect("connect to test Postgres");

    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("run migrations");

    (pool, container)
}

/// Build a test `DispatchConfig` with relaxed security settings.
fn test_config() -> DispatchConfig {
    DispatchConfig {
        allow_unsigned_callbacks: true,
        allow_private_ip_targets: true,
        ..Default::default()
    }
}

/// Insert an event row and schedule its deliveries; returns the delivery id.
async fn insert_and_schedule_event(pool: &PgPool) -> i64 {
    let event_id = Uuid::new_v4();
    let config = test_config();

    sqlx::query(
        "INSERT INTO outbox_events \
         (event_id, kind, aggregate_type, aggregate_id, payload, metadata, callbacks) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(event_id)
    .bind("test.event@v1")
    .bind("test")
    .bind(Uuid::new_v4())
    .bind(json!({"hello": "world"}))
    .bind(json!({}))
    .bind(json!([{"name": "notify", "url": "https://example.com/hook"}]))
    .execute(pool)
    .await
    .expect("insert event");

    let repo = Arc::new(PgRepo::new(pool.clone(), config.clone()));
    schedule_new_deliveries(repo.as_ref(), &config, 0)
        .await
        .expect("schedule_new_deliveries");

    // Return the delivery id just created.
    sqlx::query("SELECT id FROM outbox_deliveries WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .expect("fetch delivery id")
        .get::<i64, _>(0)
}

// ── Phase 4 dispatch lock+recovery integration tests ──────────────────────────

/// Happy path: `lock_delivery` succeeds and the row is removed from
/// `fetch_due_deliveries` while the lock is active.
#[tokio::test]
async fn lock_delivery_hides_row_until_lock_expires() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let delivery_id = insert_and_schedule_event(&pool).await;

    // The row should be visible before locking.
    let due_before = repo
        .fetch_due_deliveries(10)
        .await
        .expect("fetch_due_deliveries before lock");
    assert_eq!(
        due_before.len(),
        1,
        "delivery must be visible before locking"
    );

    // Lock it for 200ms in the future.
    let lock_until = Utc::now() + chrono::Duration::milliseconds(200);
    let locked = repo
        .lock_delivery(delivery_id, lock_until)
        .await
        .expect("lock_delivery");
    assert!(locked, "lock_delivery must return true for an unlocked row");

    // While the lock is active the row must NOT appear in fetch_due_deliveries.
    let due_during = repo
        .fetch_due_deliveries(10)
        .await
        .expect("fetch_due_deliveries during lock");
    assert!(
        due_during.is_empty(),
        "delivery must be hidden while lock is active"
    );

    // Wait for the lock to expire, then verify the row reappears.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let due_after = repo
        .fetch_due_deliveries(10)
        .await
        .expect("fetch_due_deliveries after lock expiry");
    assert_eq!(
        due_after.len(),
        1,
        "delivery must reappear after locked_until expiry"
    );
}

/// `lock_delivery` increments `attempts` each time it succeeds.
#[tokio::test]
async fn lock_delivery_increments_attempts() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let delivery_id = insert_and_schedule_event(&pool).await;

    // Lock and let expire, then lock again.
    let lock1 = Utc::now() + chrono::Duration::milliseconds(100);
    repo.lock_delivery(delivery_id, lock1)
        .await
        .expect("first lock");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let lock2 = Utc::now() + chrono::Duration::milliseconds(100);
    repo.lock_delivery(delivery_id, lock2)
        .await
        .expect("second lock");

    // attempts should now be 2 in the DB.
    let attempts: i32 = sqlx::query("SELECT attempts FROM outbox_deliveries WHERE id = $1")
        .bind(delivery_id)
        .fetch_one(&pool)
        .await
        .expect("fetch attempts")
        .get(0);

    assert_eq!(attempts, 2, "attempts must increment with each lock");
}

/// `lock_delivery` returns `false` when the row is currently locked by another
/// replica (concurrent dispatch protection).
#[tokio::test]
async fn lock_delivery_returns_false_when_already_locked() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let delivery_id = insert_and_schedule_event(&pool).await;

    // First lock — succeeds.
    let lock_until = Utc::now() + chrono::Duration::seconds(60);
    let first = repo
        .lock_delivery(delivery_id, lock_until)
        .await
        .expect("first lock");
    assert!(first, "first lock must succeed");

    // Second lock attempt on the same row while still locked — must return false.
    let second = repo
        .lock_delivery(delivery_id, lock_until)
        .await
        .expect("second lock attempt");
    assert!(
        !second,
        "second lock attempt must return false while row is still locked"
    );
}

/// After `mark_dispatched_managed` the row no longer appears in `fetch_due_deliveries`
/// even after the original `locked_until` would have expired.
#[tokio::test]
async fn dispatched_row_does_not_reappear_after_lock_expiry() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let delivery_id = insert_and_schedule_event(&pool).await;

    // Lock for a short window then immediately mark as dispatched.
    let lock_until = Utc::now() + chrono::Duration::milliseconds(100);
    repo.lock_delivery(delivery_id, lock_until)
        .await
        .expect("lock_delivery");

    repo.mark_dispatched_managed(delivery_id)
        .await
        .expect("mark_dispatched_managed");

    // Wait for what would have been the lock expiry.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let due_after = repo
        .fetch_due_deliveries(10)
        .await
        .expect("fetch_due_deliveries after dispatch");
    assert!(
        due_after.is_empty(),
        "dispatched delivery must not reappear after lock expiry"
    );
}
