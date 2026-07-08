//!
//! Integration tests for middleware pipeline.
//!
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use autumn_web::error::AutumnError;
use autumn_web::middleware::{AutumnErrorInfo, ExceptionFilter};
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use axum::response::Response;

struct MarkCalledFilter {
    called: Arc<AtomicBool>,
}

impl ExceptionFilter for MarkCalledFilter {
    fn filter(&self, _error: &AutumnErrorInfo, response: Response) -> Response {
        self.called.store(true, Ordering::SeqCst);
        response
    }
}

#[get("/ok")]
async fn ok_handler() -> &'static str {
    "ok"
}

#[get("/fail")]
async fn fail_handler() -> Result<String, AutumnError> {
    Err(AutumnError::not_found_msg("gone"))
}

#[tokio::test]
async fn exception_filter_on_error_response() {
    let called = Arc::new(AtomicBool::new(false));

    let layer =
        autumn_web::middleware::ExceptionFilterLayer::new(vec![Arc::new(MarkCalledFilter {
            called: called.clone(),
        })]);
    let state = autumn_web::AppState::for_test().with_profile("test");

    let router = axum::Router::new()
        .route("/ok", axum::routing::get(ok_handler))
        .route("/fail", axum::routing::get(fail_handler))
        .layer(layer)
        .with_state(state.clone());

    let app = TestApp::from_router(router, state);

    let resp = app.get("/fail").send().await;
    resp.assert_status(404);
    assert!(called.load(Ordering::SeqCst));

    called.store(false, Ordering::SeqCst);
    let resp = app.get("/ok").send().await;
    resp.assert_status(200);
    assert!(!called.load(Ordering::SeqCst));
}

#[derive(Clone)]
struct AddHeaderLayer;

impl<S> tower::Layer<S> for AddHeaderLayer {
    type Service = AddHeaderService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        AddHeaderService { inner }
    }
}

#[derive(Clone)]
struct AddHeaderService<S> {
    inner: S,
}

impl<S, B> tower::Service<axum::http::Request<B>> for AddHeaderService<S>
where
    S: tower::Service<axum::http::Request<B>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<std::convert::Infallible>,
    B: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        tower::Service::<axum::http::Request<B>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: axum::http::Request<B>) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let mut resp = inner.call(req).await?;
            resp.headers_mut()
                .insert("x-scoped", "true".parse().unwrap());
            Ok(resp)
        })
    }
}

#[get("/users")]
async fn list_users() -> &'static str {
    "users"
}

#[get("/public")]
async fn public_page() -> &'static str {
    "public"
}

#[tokio::test]
async fn scoped_middleware_applies_only_to_group() {
    let state = autumn_web::AppState::for_test().with_profile("test");

    let sub_router = axum::Router::new()
        .route("/users", axum::routing::get(list_users))
        .layer(AddHeaderLayer);
    let router = axum::Router::new()
        .route("/public", axum::routing::get(public_page))
        .nest("/api", sub_router)
        .with_state(state.clone());

    let app = TestApp::from_router(router, state);

    let resp = app.get("/api/users").send().await;
    resp.assert_status(200);
    assert_eq!(resp.header("x-scoped").unwrap(), "true");

    let resp = app.get("/public").send().await;
    resp.assert_status(200);
    assert!(resp.header("x-scoped").is_none());
}

#[test]
fn app_builder_scoped_compiles() {
    let _builder = autumn_web::app::app().routes(routes![ok_handler]).scoped(
        "/api",
        AddHeaderLayer,
        routes![list_users],
    );
}

#[test]
fn app_builder_exception_filter_compiles() {
    struct NoopFilter;
    impl ExceptionFilter for NoopFilter {
        fn filter(&self, _error: &AutumnErrorInfo, response: Response) -> Response {
            response
        }
    }

    let _builder = autumn_web::app::app()
        .exception_filter(NoopFilter)
        .routes(routes![ok_handler]);
}
