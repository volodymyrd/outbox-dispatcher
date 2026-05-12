use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use outbox_dispatcher_core::{PageParams, Repo, RetryOutcome};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{ExpectedToken, require_bearer_token};

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AdminState {
    /// Shared repository handle used by all route handlers.
    pub repo: Arc<dyn Repo>,
    /// Reflects whether the LISTEN/NOTIFY connection is up.
    pub listener_status: Arc<AtomicBool>,
    /// Tracks when the last successful scheduler cycle completed.
    ///
    /// Stores milliseconds since the Unix epoch, or `0` if no cycle has completed yet.
    pub last_cycle_at: Arc<AtomicI64>,
    /// `2 × poll_interval` — the readiness deadline for the last-cycle check.
    pub ready_deadline: Duration,
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the admin [`Router`] with all routes, auth middleware, and shared state.
///
/// `/health` and `/ready` are public (no auth required) so that liveness/readiness
/// probes from load balancers and orchestrators work without a bearer token.
/// All other endpoints require `Authorization: Bearer <token>`.
pub fn build_router(state: AdminState, auth_token: String) -> Router {
    let public = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .with_state(state.clone());

    let protected = Router::new()
        .route("/v1/dead-letters", get(list_dead_letters))
        .route("/v1/external-pending", get(list_external_pending))
        .route("/v1/events/{event_id}", get(get_event))
        .route("/v1/deliveries/{id}/retry", post(retry_delivery))
        .route("/v1/deliveries/{id}/complete", post(complete_delivery))
        .route("/v1/deliveries/{id}/abandon", post(abandon_delivery))
        .route("/v1/stats", get(get_stats))
        .layer(middleware::from_fn(require_bearer_token))
        .layer(Extension(ExpectedToken(auth_token)))
        .with_state(state);

    public.merge(protected)
}

// ── /health ───────────────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

// ── /ready ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    db_reachable: bool,
    listener_up: bool,
    last_cycle_ok: bool,
}

async fn ready(State(state): State<AdminState>) -> Response {
    let db_reachable = state.repo.ping().await.is_ok();
    let listener_up = state.listener_status.load(Ordering::Relaxed);
    let last_cycle_ok = {
        let stored = state.last_cycle_at.load(Ordering::Acquire);
        stored != 0
            && chrono::Utc::now().timestamp_millis().saturating_sub(stored)
                <= state.ready_deadline.as_millis() as i64
    };

    let all_ok = db_reachable && listener_up && last_cycle_ok;
    let status_code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status_code,
        Json(ReadyResponse {
            status: if all_ok { "ready" } else { "not_ready" },
            db_reachable,
            listener_up,
            last_cycle_ok,
        }),
    )
        .into_response()
}

// ── /v1/dead-letters ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DeadLettersQuery {
    callback_name: Option<String>,
    /// Keyset pagination: return items with `delivery_id < before`.
    before: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct DeadLettersResponse {
    items: Vec<outbox_dispatcher_core::DeadLetterRow>,
    next_cursor: Option<i64>,
}

async fn list_dead_letters(
    State(state): State<AdminState>,
    Query(q): Query<DeadLettersQuery>,
) -> Response {
    let limit = q.limit.clamp(1, 200);
    let page = PageParams {
        limit,
        cursor: q.before,
        callback_name: q.callback_name,
    };
    match state.repo.list_dead_letters(page).await {
        Ok(items) => {
            let next_cursor = if items.len() as i64 == limit {
                items.last().map(|r| r.delivery_id)
            } else {
                None
            };
            Json(DeadLettersResponse { items, next_cursor }).into_response()
        }
        Err(e) => internal_error(e),
    }
}

// ── /v1/external-pending ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ExternalPendingQuery {
    callback_name: Option<String>,
    /// Only include rows where `now() - dispatched_at > older_than_secs` seconds.
    older_than_secs: Option<f64>,
    /// Keyset pagination cursor (delivery_id).
    before: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct ExternalPendingResponse {
    items: Vec<outbox_dispatcher_core::ExternalPendingRow>,
    next_cursor: Option<i64>,
}

async fn list_external_pending(
    State(state): State<AdminState>,
    Query(q): Query<ExternalPendingQuery>,
) -> Response {
    let limit = q.limit.clamp(1, 200);
    let page = PageParams {
        limit,
        cursor: q.before,
        callback_name: q.callback_name,
    };
    let older_than = q.older_than_secs.map(Duration::from_secs_f64);
    match state.repo.list_external_pending(page, older_than).await {
        Ok(items) => {
            let next_cursor = if items.len() as i64 == limit {
                items.last().map(|r| r.delivery_id)
            } else {
                None
            };
            Json(ExternalPendingResponse { items, next_cursor }).into_response()
        }
        Err(e) => internal_error(e),
    }
}

// ── /v1/events/{event_id} ─────────────────────────────────────────────────────

async fn get_event(State(state): State<AdminState>, Path(event_id): Path<Uuid>) -> Response {
    match state.repo.fetch_event_with_deliveries(event_id).await {
        Ok(detail) => Json(detail).into_response(),
        Err(outbox_dispatcher_core::Error::Database(sqlx::Error::RowNotFound)) => {
            (StatusCode::NOT_FOUND, "event not found").into_response()
        }
        Err(e) => internal_error(e),
    }
}

// ── /v1/deliveries/{id}/retry ─────────────────────────────────────────────────

#[derive(Serialize)]
struct MutationResponse {
    ok: bool,
}

async fn retry_delivery(State(state): State<AdminState>, Path(id): Path<i64>) -> Response {
    match state.repo.retry_delivery(id).await {
        Ok(RetryOutcome::Reset) => Json(MutationResponse { ok: true }).into_response(),
        Ok(RetryOutcome::NotFound) => (StatusCode::NOT_FOUND, "delivery not found").into_response(),
        Ok(RetryOutcome::Locked) => (
            StatusCode::CONFLICT,
            "delivery is currently locked by a dispatcher; retry again shortly",
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}

// ── /v1/deliveries/{id}/complete ──────────────────────────────────────────────

async fn complete_delivery(State(state): State<AdminState>, Path(id): Path<i64>) -> Response {
    match state.repo.complete_delivery(id).await {
        Ok(true) => Json(MutationResponse { ok: true }).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "delivery not found").into_response(),
        Err(e) => internal_error(e),
    }
}

// ── /v1/deliveries/{id}/abandon ───────────────────────────────────────────────

async fn abandon_delivery(State(state): State<AdminState>, Path(id): Path<i64>) -> Response {
    match state.repo.abandon_delivery(id).await {
        Ok(true) => Json(MutationResponse { ok: true }).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "delivery not found").into_response(),
        Err(e) => internal_error(e),
    }
}

// ── /v1/stats ─────────────────────────────────────────────────────────────────

async fn get_stats(State(state): State<AdminState>) -> Response {
    match state.repo.fetch_stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => internal_error(e),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn default_limit() -> i64 {
    50
}

fn internal_error(e: outbox_dispatcher_core::Error) -> Response {
    tracing::error!(error = %e, "admin API internal error");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use chrono::{DateTime, Utc};
    use outbox_dispatcher_core::{
        CallbackTarget, DeadLetterRow, DueDelivery, Error, EventWithDeliveries, ExternalPendingRow,
        PageParams, RawEvent, RetryOutcome, Stats, SweepReport,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI64;
    use tower::ServiceExt;
    use uuid::Uuid;

    // ── Mock repo ─────────────────────────────────────────────────────────────

    #[derive(Default)]
    struct MockRepo {
        dead_letters: Vec<DeadLetterRow>,
        external_pending: Vec<ExternalPendingRow>,
        event: Option<EventWithDeliveries>,
        retry_result: Option<RetryOutcome>,
        complete_result: Option<bool>,
        abandon_result: Option<bool>,
        stats: Option<Stats>,
        stats_error: bool,
    }

    #[async_trait]
    impl Repo for MockRepo {
        async fn fetch_new_events(
            &self,
            _after_id: i64,
            _limit: i64,
        ) -> outbox_dispatcher_core::error::Result<Vec<RawEvent>> {
            Ok(vec![])
        }

        async fn ensure_deliveries(
            &self,
            _event_id: Uuid,
            _callbacks: &[CallbackTarget],
        ) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn create_invalid_delivery(
            &self,
            _event_id: Uuid,
            _callback_name: &str,
            _reason: &str,
        ) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn create_invalid_deliveries(
            &self,
            _event_id: Uuid,
            _entries: &[(String, String)],
        ) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn fetch_due_deliveries(
            &self,
            _batch_size: i64,
        ) -> outbox_dispatcher_core::error::Result<Vec<DueDelivery>> {
            Ok(vec![])
        }

        async fn lock_delivery(
            &self,
            _id: i64,
            _until: DateTime<Utc>,
        ) -> outbox_dispatcher_core::error::Result<bool> {
            Ok(true)
        }

        async fn mark_dispatched_managed(
            &self,
            _id: i64,
        ) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn mark_dispatched_external(
            &self,
            _id: i64,
        ) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn mark_failure(
            &self,
            _id: i64,
            _error: &str,
            _available_at: DateTime<Utc>,
            _dead_letter: bool,
        ) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn reset_hung_external(
            &self,
            _now: DateTime<Utc>,
            _max_completion_cycles: i32,
        ) -> outbox_dispatcher_core::error::Result<SweepReport> {
            Ok(SweepReport::default())
        }

        async fn recover_cursor(&self) -> outbox_dispatcher_core::error::Result<i64> {
            Ok(0)
        }

        async fn list_dead_letters(
            &self,
            _page: PageParams,
        ) -> outbox_dispatcher_core::error::Result<Vec<DeadLetterRow>> {
            Ok(self.dead_letters.clone())
        }

        async fn list_external_pending(
            &self,
            _page: PageParams,
            _older_than: Option<Duration>,
        ) -> outbox_dispatcher_core::error::Result<Vec<ExternalPendingRow>> {
            Ok(self.external_pending.clone())
        }

        async fn retry_delivery(
            &self,
            _id: i64,
        ) -> outbox_dispatcher_core::error::Result<RetryOutcome> {
            Ok(self.retry_result.unwrap_or(RetryOutcome::NotFound))
        }

        async fn complete_delivery(&self, _id: i64) -> outbox_dispatcher_core::error::Result<bool> {
            Ok(self.complete_result.unwrap_or(false))
        }

        async fn abandon_delivery(&self, _id: i64) -> outbox_dispatcher_core::error::Result<bool> {
            Ok(self.abandon_result.unwrap_or(false))
        }

        async fn fetch_event_with_deliveries(
            &self,
            _event_id: Uuid,
        ) -> outbox_dispatcher_core::error::Result<EventWithDeliveries> {
            self.event
                .clone()
                .ok_or(Error::Database(sqlx::Error::RowNotFound))
        }

        async fn ping(&self) -> outbox_dispatcher_core::error::Result<()> {
            Ok(())
        }

        async fn fetch_stats(&self) -> outbox_dispatcher_core::error::Result<Stats> {
            if self.stats_error {
                return Err(Error::Database(sqlx::Error::RowNotFound));
            }
            Ok(self.stats.clone().unwrap_or_else(|| Stats {
                events_total: 0,
                deliveries_pending: 0,
                deliveries_external_pending: 0,
                deliveries_dead_lettered: 0,
                oldest_pending_age_seconds: None,
                callbacks: HashMap::new(),
            }))
        }
    }

    fn build_test_app(repo: MockRepo) -> Router {
        let state = AdminState {
            repo: Arc::new(repo),
            listener_status: Arc::new(AtomicBool::new(true)),
            last_cycle_at: Arc::new(AtomicI64::new(chrono::Utc::now().timestamp_millis())),
            ready_deadline: Duration::from_secs(60),
        };
        build_router(state, "test-token".to_string())
    }

    async fn send_request(
        app: Router,
        method: Method,
        uri: &str,
        auth: Option<&str>,
    ) -> axum::http::Response<Body> {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(token) = auth {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    // ── Auth tests ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn health_returns_200_without_auth() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(app, Method::GET, "/health", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_returns_200_with_valid_token() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(app, Method::GET, "/health", Some("test-token")).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_endpoint_rejected_without_auth() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(app, Method::GET, "/v1/dead-letters", None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_rejected() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(app, Method::GET, "/v1/dead-letters", Some("wrong-token")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ── /ready ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn ready_ok_when_all_checks_pass() {
        let app = build_test_app(MockRepo::default());
        // /ready is public — no token needed.
        let resp = send_request(app, Method::GET, "/ready", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_503_when_no_cycle_yet() {
        let state = AdminState {
            repo: Arc::new(MockRepo::default()),
            listener_status: Arc::new(AtomicBool::new(true)),
            last_cycle_at: Arc::new(AtomicI64::new(0)), // no cycle yet
            ready_deadline: Duration::from_secs(60),
        };
        let app = build_router(state, "test-token".to_string());
        let resp = send_request(app, Method::GET, "/ready", None).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn ready_503_when_listener_down() {
        let state = AdminState {
            repo: Arc::new(MockRepo::default()),
            listener_status: Arc::new(AtomicBool::new(false)), // listener down
            last_cycle_at: Arc::new(AtomicI64::new(chrono::Utc::now().timestamp_millis())),
            ready_deadline: Duration::from_secs(60),
        };
        let app = build_router(state, "test-token".to_string());
        let resp = send_request(app, Method::GET, "/ready", None).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── /v1/dead-letters ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn dead_letters_returns_empty_list() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(app, Method::GET, "/v1/dead-letters", Some("test-token")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["items"], serde_json::json!([]));
        assert!(json["next_cursor"].is_null());
    }

    // ── /v1/events/{event_id} ─────────────────────────────────────────────────

    #[tokio::test]
    async fn get_event_not_found_returns_404() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(
            app,
            Method::GET,
            &format!("/v1/events/{}", Uuid::new_v4()),
            Some("test-token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /v1/deliveries/{id}/retry ─────────────────────────────────────────────

    #[tokio::test]
    async fn retry_delivery_not_found_returns_404() {
        let app = build_test_app(MockRepo {
            retry_result: Some(RetryOutcome::NotFound),
            ..MockRepo::default()
        });
        let resp = send_request(
            app,
            Method::POST,
            "/v1/deliveries/999/retry",
            Some("test-token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn retry_delivery_found_returns_200() {
        let app = build_test_app(MockRepo {
            retry_result: Some(RetryOutcome::Reset),
            ..MockRepo::default()
        });
        let resp = send_request(
            app,
            Method::POST,
            "/v1/deliveries/1/retry",
            Some("test-token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn retry_delivery_locked_returns_409() {
        let app = build_test_app(MockRepo {
            retry_result: Some(RetryOutcome::Locked),
            ..MockRepo::default()
        });
        let resp = send_request(
            app,
            Method::POST,
            "/v1/deliveries/1/retry",
            Some("test-token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    // ── /v1/deliveries/{id}/complete ──────────────────────────────────────────

    #[tokio::test]
    async fn complete_delivery_found_returns_200() {
        let app = build_test_app(MockRepo {
            complete_result: Some(true),
            ..MockRepo::default()
        });
        let resp = send_request(
            app,
            Method::POST,
            "/v1/deliveries/1/complete",
            Some("test-token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── /v1/deliveries/{id}/abandon ───────────────────────────────────────────

    #[tokio::test]
    async fn abandon_delivery_not_found_returns_404() {
        let app = build_test_app(MockRepo {
            abandon_result: Some(false),
            ..MockRepo::default()
        });
        let resp = send_request(
            app,
            Method::POST,
            "/v1/deliveries/999/abandon",
            Some("test-token"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ── /v1/stats ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stats_returns_expected_shape() {
        let app = build_test_app(MockRepo::default());
        let resp = send_request(app, Method::GET, "/v1/stats", Some("test-token")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["events_total"].is_number());
        assert!(json["deliveries_pending"].is_number());
        assert!(json["callbacks"].is_object());
    }

    #[tokio::test]
    async fn stats_returns_500_on_repo_error() {
        let app = build_test_app(MockRepo {
            stats_error: true,
            ..MockRepo::default()
        });
        let resp = send_request(app, Method::GET, "/v1/stats", Some("test-token")).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
