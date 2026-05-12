use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

/// Axum middleware that enforces `Authorization: Bearer <token>` on every request.
///
/// The comparison is performed in constant time via [`subtle::ConstantTimeEq`] to prevent
/// timing-oracle attacks that could leak the expected token one byte at a time.
pub async fn require_bearer_token(request: Request, next: Next) -> Response {
    // Token is injected via axum Extension by the router setup.
    let expected: &ExpectedToken = request
        .extensions()
        .get::<ExpectedToken>()
        .expect("ExpectedToken extension must be set by the router");

    let authorised = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, provided)| {
            // Constant-time compare of the token bytes via `subtle::ConstantTimeEq`.
            // Note: the slice impl short-circuits on length mismatch, so token length
            // is not protected — acceptable here because the bearer token is
            // operator-controlled and its length is not considered secret.
            let provided_bytes = provided.as_bytes();
            let expected_bytes = expected.0.as_bytes();
            bool::from(provided_bytes.ct_eq(expected_bytes))
        })
        .unwrap_or(false);

    if authorised {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static(r#"Bearer realm="outbox-dispatcher""#),
            )],
            "Unauthorized",
        )
            .into_response()
    }
}

/// Newtype wrapper for the expected bearer token, stored as an axum `Extension`.
///
/// Wrapping in a newtype avoids collisions with other `String` extensions and makes
/// the intent explicit at the call site.
#[derive(Clone)]
pub struct ExpectedToken(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Method, Request, StatusCode},
        middleware,
        routing::get,
    };
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn build_app(token: &str) -> Router {
        Router::new()
            .route("/", get(ok_handler))
            .layer(middleware::from_fn(require_bearer_token))
            .layer(axum::Extension(ExpectedToken(token.to_string())))
    }

    async fn send(app: Router, auth: Option<&str>) -> StatusCode {
        let mut req = Request::builder().method(Method::GET).uri("/");
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        let response = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        response.status()
    }

    #[tokio::test]
    async fn valid_token_returns_200() {
        let app = build_app("secret-token");
        assert_eq!(send(app, Some("Bearer secret-token")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = build_app("secret-token");
        assert_eq!(
            send(app, Some("Bearer wrong-token")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn missing_header_returns_401() {
        let app = build_app("secret-token");
        assert_eq!(send(app, None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_bearer_scheme_returns_401() {
        let app = build_app("secret-token");
        assert_eq!(
            send(app, Some("Basic secret-token")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn empty_token_in_config_rejected_by_wrong_bearer() {
        // Even if the configured token is empty, a non-matching Bearer is still rejected.
        let app = build_app("");
        assert_eq!(
            send(app, Some("Bearer something")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn bearer_scheme_accepted_case_insensitively() {
        assert_eq!(
            send(build_app("secret-token"), Some("bearer secret-token")).await,
            StatusCode::OK
        );
        assert_eq!(
            send(build_app("secret-token"), Some("BEARER secret-token")).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn empty_token_with_empty_bearer_accepted() {
        // Degenerate case: both empty strings match. In practice, config validation
        // prevents empty tokens, but the middleware itself does not re-validate.
        let app = build_app("");
        assert_eq!(send(app, Some("Bearer ")).await, StatusCode::OK);
    }
}
