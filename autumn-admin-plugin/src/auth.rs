//! Role-check and step-up middleware for the admin router.
//!
//! Wraps the nested admin router with `from_fn` layers that inspect the
//! request's [`Session`] and short-circuit with appropriate error responses
//! when authentication or freshness requirements are not met.

use autumn_web::AutumnError;
use autumn_web::session::Session;
use autumn_web::step_up;
use axum::extract::Request;
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

/// Produce an axum middleware that verifies the incoming request has a
/// logged-in session with the given role.
///
/// Returns 401 if `auth_session_key` is absent from the session, 403 if
/// the role doesn't match. The session key matches Autumn's
/// `auth.session_key` config (default `"user_id"`); deployments that
/// changed it (e.g. to `"uid"`) must pass the same string here via
/// [`crate::AdminPlugin::auth_session_key`].
///
/// Errors are produced via `AutumnError::*_msg` so the framework's
/// error-page filter renders them as branded HTML for browser clients
/// and JSON for API clients.
pub async fn check_role(
    role: String,
    auth_session_key: String,
    req: Request,
    next: Next,
) -> Response {
    let Some(session) = req.extensions().get::<Session>().cloned() else {
        return AutumnError::internal_server_error_msg(
            "SessionLayer not installed; admin plugin requires sessions",
        )
        .into_response();
    };

    let Some(user_id) = session.get(&auth_session_key).await else {
        return AutumnError::unauthorized_msg("authentication required").into_response();
    };

    // Publish the request's current actor (#1383) the way the framework's own
    // auth seams do. Without this the admin router resolves no principal at
    // all, so a `#[repository(versioned)]` write made from an admin page lands
    // as `SYSTEM_ACTOR` — and, while impersonating (#1394), would lose the
    // operator's identity exactly where it matters most. `audit_actor_id`
    // returns the real impersonator when the session carries one, and the
    // effective user otherwise. Seeded only when nothing stronger (an outer
    // bearer layer, an explicit `with_actor` scope) already resolved a
    // principal, matching the "first resolver wins" rule in `autumn_web::auth`.
    if autumn_web::current::Current::actor().is_none() {
        autumn_web::current::Current::set_actor(
            autumn_web::auth::impersonation::audit_actor_id(&session, &user_id).await,
        );
    }
    autumn_web::log::context::set_user_id(user_id);

    let current = session.get("role").await.unwrap_or_default();
    if current != role {
        return AutumnError::forbidden_msg(format!("'{role}' role required")).into_response();
    }
    next.run(req).await
}

/// Middleware that requires step-up (fresh) authentication for mutating
/// HTTP methods (POST, PUT, PATCH, DELETE).
///
/// When the session's `last_strong_auth_at` claim is missing or stale:
/// - Browser clients are redirected to `/reauth?return_to=<path>`.
/// - JSON/API clients receive a `401` with `WWW-Authenticate: StepUp`.
///
/// GET requests are passed through unconditionally.
///
/// `max_age_secs` is the freshness window configured at admin plugin build
/// time via [`AdminPlugin::with_step_up_mutations`] or
/// [`AdminPlugin::with_step_up_max_age`]. It is captured in the closure
/// registered in `routes::admin_router` and passed directly here so this
/// function does not need access to `AppState` at request time.
///
/// **Known limitation**: this middleware calls `check_step_up` directly and
/// therefore does **not** emit `auth.step_up.success` / `auth.step_up.failure`
/// audit events. `AppState` (needed to resolve the `AuditLogger`) is not
/// available to `from_fn` middleware — that would require `from_fn_with_state`,
/// which needs the state value at layer-registration time, before the framework
/// finalises `AppState`. Route-level `#[step_up]` attributes do emit full audit
/// events via `__check_step_up_with_config`.
pub async fn check_step_up_mutations(max_age_secs: u64, req: Request, next: Next) -> Response {
    // Only guard mutating methods.
    if !matches!(
        req.method(),
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    ) {
        return next.run(req).await;
    }

    let Some(session) = req.extensions().get::<Session>().cloned() else {
        return AutumnError::internal_server_error_msg(
            "SessionLayer not installed; admin step-up requires sessions",
        )
        .into_response();
    };

    if step_up::check_step_up(&session, max_age_secs)
        .await
        .is_err()
    {
        // Detect JSON clients via Accept header.
        let wants_json = req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.contains("application/json"));

        if wants_json {
            return step_up::__step_up_json_response(max_age_secs);
        }

        // For mutating requests, prefer the Referer (the page with the action
        // button) over the current URI so that after reauth the user lands back
        // on a GET page — not a POST/DELETE-only endpoint with no GET handler.
        let path = req
            .headers()
            .get(header::REFERER)
            .and_then(|v| v.to_str().ok())
            .and_then(step_up::referer_path)
            .unwrap_or_else(|| {
                req.uri()
                    .path_and_query()
                    .map_or_else(|| req.uri().path(), axum::http::uri::PathAndQuery::as_str)
                    .to_owned()
            });
        let encoded = step_up::encode_return_to(&path);
        return Redirect::to(&format!("/reauth?return_to={encoded}")).into_response();
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::session::Session;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use std::collections::HashMap;
    use tower::ServiceExt;

    fn app_with_role_gate(session: Session) -> Router {
        app_with_role_gate_and_key(session, "user_id")
    }

    fn app_with_role_gate_and_key(session: Session, auth_session_key: &'static str) -> Router {
        async fn ok() -> &'static str {
            "ok"
        }
        let role = "admin".to_owned();
        let key = auth_session_key.to_owned();
        Router::new()
            .route("/", get(ok))
            .layer(from_fn(move |mut req: Request, next: Next| {
                let session = session.clone();
                let role = role.clone();
                let key = key.clone();
                async move {
                    req.extensions_mut().insert(session);
                    check_role(role, key, req, next).await
                }
            }))
    }

    #[tokio::test]
    async fn no_session_returns_401() {
        let session = Session::new_for_test("sid".into(), HashMap::new());
        let app = app_with_role_gate(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_role_returns_403() {
        let session = Session::new_for_test(
            "sid".into(),
            HashMap::from([
                ("user_id".into(), "1".into()),
                ("role".into(), "viewer".into()),
            ]),
        );
        let app = app_with_role_gate(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn correct_role_passes_through() {
        let session = Session::new_for_test(
            "sid".into(),
            HashMap::from([
                ("user_id".into(), "1".into()),
                ("role".into(), "admin".into()),
            ]),
        );
        let app = app_with_role_gate(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn custom_auth_session_key_is_honored() {
        // Deployment configured `auth.session_key = "uid"`. A session with
        // "uid" populated must authenticate; a session with only the
        // default "user_id" must NOT (because the deployment's auth
        // pipeline never writes "user_id").
        let with_uid = Session::new_for_test(
            "sid".into(),
            HashMap::from([("uid".into(), "42".into()), ("role".into(), "admin".into())]),
        );
        let app = app_with_role_gate_and_key(with_uid, "uid");
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "uid-keyed session should pass"
        );

        // Inverse: only the default "user_id" present, but the deployment
        // is configured for "uid" — must reject as unauthenticated.
        let with_user_id = Session::new_for_test(
            "sid".into(),
            HashMap::from([
                ("user_id".into(), "42".into()),
                ("role".into(), "admin".into()),
            ]),
        );
        let app = app_with_role_gate_and_key(with_user_id, "uid");
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "wrong-key session must NOT authenticate"
        );
    }

    #[tokio::test]
    async fn missing_session_extension_returns_500() {
        async fn ok() -> &'static str {
            "ok"
        }
        let role = "admin".to_owned();
        let key = "user_id".to_owned();
        let app =
            Router::new()
                .route("/", get(ok))
                .layer(from_fn(move |req: Request, next: Next| {
                    let role = role.clone();
                    let key = key.clone();
                    async move { check_role(role, key, req, next).await }
                }));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ── check_step_up_mutations ───────────────────────────────────────────────

    fn step_up_app(session: Session) -> Router {
        step_up_app_with_max_age(session, autumn_web::step_up::DEFAULT_MAX_AGE_SECS)
    }

    fn step_up_app_with_max_age(session: Session, max_age_secs: u64) -> Router {
        async fn ok() -> &'static str {
            "ok"
        }
        Router::new()
            .route("/resource", get(ok).post(ok).delete(ok))
            .layer(from_fn(move |mut req: Request, next: Next| {
                let session = session.clone();
                async move {
                    req.extensions_mut().insert(session);
                    check_step_up_mutations(max_age_secs, req, next).await
                }
            }))
    }

    #[tokio::test]
    async fn step_up_allows_get_without_claim() {
        let session = Session::new_for_test("sid".into(), HashMap::new());
        let app = step_up_app(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "GET should pass without step-up"
        );
    }

    #[tokio::test]
    async fn step_up_blocks_post_without_claim_html_client() {
        let session = Session::new_for_test("sid".into(), HashMap::new());
        let app = step_up_app(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // HTML clients get a redirect (302/303)
        assert!(
            res.status().is_redirection(),
            "POST without step-up should redirect HTML client: {}",
            res.status()
        );
        let location = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(
            location.contains("/reauth"),
            "redirect should go to /reauth: {location}"
        );
    }

    #[tokio::test]
    async fn step_up_blocks_post_without_claim_json_client() {
        let session = Session::new_for_test("sid".into(), HashMap::new());
        let app = step_up_app(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/resource")
                    .header("Accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "JSON POST without step-up should return 401"
        );
        let www_auth = res
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            www_auth.contains("StepUp"),
            "should include WWW-Authenticate: StepUp header: {www_auth}"
        );
    }

    #[tokio::test]
    async fn step_up_allows_post_with_fresh_claim() {
        use autumn_web::step_up::STEP_UP_SESSION_KEY;
        let now_ts = chrono::Utc::now().timestamp().to_string();
        let session = Session::new_for_test(
            "sid".into(),
            HashMap::from([(STEP_UP_SESSION_KEY.to_string(), now_ts)]),
        );
        let app = step_up_app(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "POST with fresh step-up claim should pass"
        );
    }

    #[tokio::test]
    async fn step_up_blocks_delete_without_claim() {
        let session = Session::new_for_test("sid".into(), HashMap::new());
        let app = step_up_app(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.status().is_redirection(),
            "DELETE without step-up should redirect: {}",
            res.status()
        );
    }

    #[tokio::test]
    async fn step_up_uses_referer_as_return_to_for_post() {
        let session = Session::new_for_test("sid".into(), HashMap::new());
        let app = step_up_app(session);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/resource")
                    .header("Referer", "https://example.com/admin/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(res.status().is_redirection(), "should redirect");
        let location = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(
            location.contains("/admin/users"),
            "return_to should use Referer path, not POST URI: {location}"
        );
    }

    #[tokio::test]
    async fn step_up_custom_max_age_is_honored() {
        use autumn_web::step_up::STEP_UP_SESSION_KEY;
        // Claim set 10 seconds ago — within the default 5-min window but outside
        // a tighter 5-second window. Verifies that the captured max_age_secs is
        // actually used rather than a hard-coded constant.
        let stale_ts = (chrono::Utc::now() - chrono::Duration::seconds(10))
            .timestamp()
            .to_string();
        let session = Session::new_for_test(
            "sid".into(),
            HashMap::from([(STEP_UP_SESSION_KEY.to_string(), stale_ts)]),
        );
        let app = step_up_app_with_max_age(session, 5);
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.status().is_redirection(),
            "10-second-old claim should be blocked by max_age=5s"
        );
    }
}
