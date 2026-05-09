//! Integration tests for Phase 3 — Scheduler & wake source.
//!
//! Each test gets a fresh Postgres instance via `testcontainers`.
//! Docker must be running locally for these tests to pass.

use std::sync::Arc;
use std::time::Duration;

use outbox_dispatcher_core::{DispatchConfig, PgRepo, Repo, schedule_new_deliveries};
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, postgres::PgListener};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

// ── Test helpers ───────────────────────────────────────────────────────────────

/// Spin up a fresh Postgres container, run migrations, and return a pool.
///
/// Uses Postgres 16 (alpine) because `gen_random_uuid()` is a core built-in
/// since PG 13; the testcontainers default image (`11-alpine`) predates that.
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

    // Run migrations from the workspace-root migrations directory.
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("run migrations");

    (pool, container)
}

/// Build a test `DispatchConfig` that allows unsigned callbacks and private IPs.
fn test_config() -> DispatchConfig {
    DispatchConfig {
        allow_unsigned_callbacks: true,
        allow_private_ip_targets: true,
        ..Default::default()
    }
}

/// Insert an event row directly and return its `event_id`.
async fn insert_event(
    pool: &PgPool,
    callbacks: serde_json::Value,
    payload_bytes: Option<usize>,
) -> Uuid {
    let event_id = Uuid::new_v4();
    let payload = if let Some(size) = payload_bytes {
        let padding = "x".repeat(size.saturating_sub(10));
        json!({"data": padding})
    } else {
        json!({"hello": "world"})
    };

    sqlx::query(
        "INSERT INTO outbox_events \
         (event_id, kind, aggregate_type, aggregate_id, payload, metadata, callbacks) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(event_id)
    .bind("test.event@v1")
    .bind("test")
    .bind(Uuid::new_v4())
    .bind(&payload)
    .bind(json!({}))
    .bind(&callbacks)
    .execute(pool)
    .await
    .expect("insert event");

    event_id
}

// ── Delivery row queries (no compile-time macro, safe in test files) ───────────

struct DeliveryInfo {
    callback_name: String,
    completion_mode: String,
    dead_letter: bool,
    last_error: Option<String>,
}

async fn fetch_deliveries(pool: &PgPool, event_id: Uuid) -> Vec<DeliveryInfo> {
    sqlx::query(
        "SELECT callback_name, completion_mode, dead_letter, last_error \
         FROM outbox_deliveries WHERE event_id = $1 ORDER BY callback_name",
    )
    .bind(event_id)
    .fetch_all(pool)
    .await
    .expect("fetch deliveries")
    .into_iter()
    .map(|row| DeliveryInfo {
        callback_name: row.get("callback_name"),
        completion_mode: row.get("completion_mode"),
        dead_letter: row.get("dead_letter"),
        last_error: row.get("last_error"),
    })
    .collect()
}

async fn count_deliveries(pool: &PgPool, event_id: Uuid) -> i64 {
    sqlx::query("SELECT COUNT(*) FROM outbox_deliveries WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .expect("count deliveries")
        .get::<i64, _>(0)
}

// ── Phase 3 integration tests ──────────────────────────────────────────────────

/// Happy path: valid callbacks → delivery rows created with correct names and mode.
#[tokio::test]
async fn schedule_creates_delivery_rows_for_valid_callbacks() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let event_id = insert_event(
        &pool,
        json!([
            {"name": "notify", "url": "https://example.com/hook"},
            {"name": "log_cb", "url": "https://log.example.com/hook"}
        ]),
        None,
    )
    .await;

    let new_cursor = schedule_new_deliveries(repo.as_ref(), &test_config(), 0)
        .await
        .expect("schedule_new_deliveries");

    assert!(new_cursor > 0, "cursor should advance");

    let rows = fetch_deliveries(&pool, event_id).await;
    assert_eq!(rows.len(), 2, "expected 2 delivery rows");

    let names: Vec<&str> = rows.iter().map(|r| r.callback_name.as_str()).collect();
    assert!(names.contains(&"log_cb"));
    assert!(names.contains(&"notify"));
    for row in &rows {
        assert_eq!(row.completion_mode, "managed");
        assert!(!row.dead_letter, "delivery should not be dead-lettered");
    }
}

/// ensure_deliveries is idempotent: running schedule twice creates no duplicate rows.
#[tokio::test]
async fn schedule_is_idempotent_on_second_call() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let event_id = insert_event(
        &pool,
        json!([{"name": "notify", "url": "https://example.com/hook"}]),
        None,
    )
    .await;

    schedule_new_deliveries(repo.as_ref(), &test_config(), 0)
        .await
        .expect("first schedule");
    schedule_new_deliveries(repo.as_ref(), &test_config(), 0)
        .await
        .expect("second schedule (should be no-op)");

    let count = count_deliveries(&pool, event_id).await;
    assert_eq!(count, 1, "idempotent: only 1 delivery row after 2 calls");
}

/// Cursor recovery: `recover_cursor` returns 0 initially, then the max scheduled event id.
#[tokio::test]
async fn recover_cursor_returns_max_scheduled_event_id() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    let cursor = repo.recover_cursor().await.expect("recover_cursor");
    assert_eq!(cursor, 0, "cursor must be 0 before any events");

    insert_event(
        &pool,
        json!([{"name": "abc", "url": "https://example.com/"}]),
        None,
    )
    .await;
    schedule_new_deliveries(repo.as_ref(), &test_config(), 0)
        .await
        .expect("schedule");

    let cursor_after = repo.recover_cursor().await.expect("recover_cursor after");
    assert!(cursor_after > 0, "cursor should be > 0 after scheduling");
}

/// Payload-size guard: oversized payload → all callbacks dead-lettered
/// with `source_payload_too_large:` prefix.
#[tokio::test]
async fn oversized_payload_dead_letters_all_callbacks_with_correct_prefix() {
    let (pool, _container) = setup_db().await;

    let config = DispatchConfig {
        allow_unsigned_callbacks: true,
        allow_private_ip_targets: true,
        payload_size_limit_bytes: 50,
        ..Default::default()
    };
    let repo = Arc::new(PgRepo::new(pool.clone(), config.clone()));

    let event_id = insert_event(
        &pool,
        json!([
            {"name": "notify", "url": "https://example.com/hook"},
            {"name": "log_cb", "url": "https://log.example.com/hook"}
        ]),
        Some(200), // 200 bytes >> 50-byte limit
    )
    .await;

    schedule_new_deliveries(repo.as_ref(), &config, 0)
        .await
        .expect("schedule");

    let rows = fetch_deliveries(&pool, event_id).await;
    assert_eq!(rows.len(), 2, "expected 2 dead-lettered rows");
    for row in &rows {
        assert!(row.dead_letter, "delivery must be dead-lettered");
        let err = row.last_error.as_deref().unwrap_or("");
        assert!(
            err.starts_with("source_payload_too_large:"),
            "last_error must start with 'source_payload_too_large:'; got: {err}"
        );
    }
    let names: Vec<&str> = rows.iter().map(|r| r.callback_name.as_str()).collect();
    assert!(names.contains(&"notify"));
    assert!(names.contains(&"log_cb"));
}

/// Invalid callback dead-letter: missing `signing_key_id` with default config →
/// immediately dead-lettered with `invalid_callback:` prefix.
#[tokio::test]
async fn invalid_callback_is_dead_lettered_at_schedule_time() {
    let (pool, _container) = setup_db().await;
    // Default config: allow_unsigned_callbacks = false.
    let config = DispatchConfig::default();
    let repo = Arc::new(PgRepo::new(pool.clone(), config.clone()));

    let event_id = insert_event(
        &pool,
        json!([{"name": "bad", "url": "https://example.com/hook"}]),
        None,
    )
    .await;

    schedule_new_deliveries(repo.as_ref(), &config, 0)
        .await
        .expect("schedule");

    let rows = fetch_deliveries(&pool, event_id).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].dead_letter, "should be dead-lettered");
    let err = rows[0].last_error.as_deref().unwrap_or("");
    assert!(
        err.starts_with("invalid_callback:"),
        "last_error must start with 'invalid_callback:'; got: {err}"
    );
}

/// Poison-pill handling: an event with a non-object array element is skipped
/// for that element (treated as an invalid callback and dead-lettered), while
/// subsequent valid events in the same batch are still processed normally.
///
/// Note: the DB CHECK constraint requires `callbacks` to be a non-empty *array*,
/// so a non-array value is rejected at INSERT time. The real poison-pill case
/// is an array with un-parseable or non-object elements, which the scheduler
/// handles by dead-lettering that callback.
#[tokio::test]
async fn malformed_callbacks_jsonb_is_skipped_without_crash() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    // Poison-pill: the callbacks array contains a string element (not an object).
    // This passes the DB CHECK (it's a non-empty array) but fails structural parsing.
    let poison_id = insert_event(&pool, json!(["not_an_object"]), None).await;

    // Valid event after the poison pill.
    let valid_id = insert_event(
        &pool,
        json!([{"name": "abc", "url": "https://example.com/hook"}]),
        None,
    )
    .await;

    // Must not panic, must return Ok.
    let new_cursor = schedule_new_deliveries(repo.as_ref(), &test_config(), 0)
        .await
        .expect("schedule_new_deliveries must not fail on poison pill");

    assert!(new_cursor > 0, "cursor should advance past both events");

    // Poison pill element → dead-lettered delivery row (not skipped at the event level,
    // but treated as an invalid callback by `parse_callbacks`).
    let poison_rows = fetch_deliveries(&pool, poison_id).await;
    assert_eq!(
        poison_rows.len(),
        1,
        "invalid element gets a dead-lettered row"
    );
    assert!(
        poison_rows[0].dead_letter,
        "element-level parse failure must produce a dead-lettered row"
    );

    // Valid event → 1 normal delivery row.
    let valid_count = count_deliveries(&pool, valid_id).await;
    assert_eq!(valid_count, 1, "valid event should have one delivery row");
}

/// LISTEN/NOTIFY: inserting an event fires `pg_notify('outbox_events_new', event_id)`.
#[tokio::test]
async fn pg_notify_fires_on_event_insert() {
    let (pool, _container) = setup_db().await;

    let mut listener = PgListener::connect_with(&pool)
        .await
        .expect("create PgListener");

    listener
        .listen("outbox_events_new")
        .await
        .expect("LISTEN outbox_events_new");

    // Insert an event; the trigger should NOTIFY.
    let event_id = insert_event(
        &pool,
        json!([{"name": "abc", "url": "https://example.com/hook"}]),
        None,
    )
    .await;

    // Expect a notification within 2 seconds.
    let notification = tokio::time::timeout(Duration::from_secs(2), listener.recv())
        .await
        .expect("timeout waiting for NOTIFY")
        .expect("recv notification");

    assert_eq!(notification.channel(), "outbox_events_new");
    assert_eq!(
        notification.payload(),
        event_id.to_string(),
        "notification payload should be the event_id"
    );
}

/// Mixed batch: valid, oversized, and invalid-element events all processed in one pass.
#[tokio::test]
async fn batch_of_mixed_events_processed_correctly() {
    let (pool, _container) = setup_db().await;

    let config = DispatchConfig {
        allow_unsigned_callbacks: true,
        allow_private_ip_targets: true,
        payload_size_limit_bytes: 50,
        ..Default::default()
    };
    let repo = Arc::new(PgRepo::new(pool.clone(), config.clone()));

    // 1. Valid event.
    let valid_id = insert_event(
        &pool,
        json!([{"name": "notify", "url": "https://example.com/hook"}]),
        None,
    )
    .await;

    // 2. Oversized payload event.
    let oversized_id = insert_event(
        &pool,
        json!([{"name": "big", "url": "https://example.com/hook"}]),
        Some(200),
    )
    .await;

    // 3. Event with a non-object element in the callbacks array (passes DB CHECK,
    //    fails structural parsing — dead-lettered at schedule time).
    let invalid_elem_id = insert_event(&pool, json!(["not_an_object_element"]), None).await;

    schedule_new_deliveries(repo.as_ref(), &config, 0)
        .await
        .expect("schedule");

    // Valid → 1 normal (non-dead-lettered) delivery row.
    let valid_rows = fetch_deliveries(&pool, valid_id).await;
    assert_eq!(valid_rows.len(), 1);
    assert!(!valid_rows[0].dead_letter);

    // Oversized → 1 dead-lettered row with source_payload_too_large prefix.
    let oversized_rows = fetch_deliveries(&pool, oversized_id).await;
    assert_eq!(oversized_rows.len(), 1);
    assert!(oversized_rows[0].dead_letter);
    assert!(
        oversized_rows[0]
            .last_error
            .as_deref()
            .unwrap_or("")
            .starts_with("source_payload_too_large:")
    );

    // Invalid element → dead-lettered row with invalid_callback prefix.
    let invalid_rows = fetch_deliveries(&pool, invalid_elem_id).await;
    assert_eq!(invalid_rows.len(), 1);
    assert!(invalid_rows[0].dead_letter);
    assert!(
        invalid_rows[0]
            .last_error
            .as_deref()
            .unwrap_or("")
            .starts_with("invalid_callback:"),
        "got: {}",
        invalid_rows[0].last_error.as_deref().unwrap_or("")
    );
}

/// Multiple events inserted sequentially; cursor advances monotonically so that
/// a second call with the updated cursor skips already-processed events.
#[tokio::test]
async fn cursor_prevents_reprocessing_already_scheduled_events() {
    let (pool, _container) = setup_db().await;
    let repo = Arc::new(PgRepo::new(pool.clone(), test_config()));

    // Insert and schedule first event.
    let first_id = insert_event(
        &pool,
        json!([{"name": "abc", "url": "https://example.com/"}]),
        None,
    )
    .await;

    let cursor_after_first = schedule_new_deliveries(repo.as_ref(), &test_config(), 0)
        .await
        .expect("schedule first batch");

    // Insert second event.
    let second_id = insert_event(
        &pool,
        json!([{"name": "def", "url": "https://example.com/"}]),
        None,
    )
    .await;

    // Schedule with the updated cursor — only the second event should be processed.
    let cursor_after_second =
        schedule_new_deliveries(repo.as_ref(), &test_config(), cursor_after_first)
            .await
            .expect("schedule second batch");

    assert!(
        cursor_after_second > cursor_after_first,
        "cursor must advance for the second event"
    );

    // Both events have exactly 1 delivery row (no duplicates from double-scheduling).
    assert_eq!(count_deliveries(&pool, first_id).await, 1);
    assert_eq!(count_deliveries(&pool, second_id).await, 1);
}
