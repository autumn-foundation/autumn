//! Integration tests for `AppBuilder::layer` (Story S-049).
//!
//! Verifies that user-registered Tower middleware integrates with Autumn's
//! built-in stack, sees the generated request ID, and correctly propagates
//! `Service::poll_ready` backpressure.

use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::Duration;

use autumn_web::middleware::RequestId;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::http::{Request, Response, StatusCode};
use tokio::sync::Notify;
use tower::{Service, ServiceBuilder, timeout::TimeoutLayer};

// ── Test 1: TimeoutLayer triggers end-to-end ─────────────────────────────

#[get("/slow")]
async fn slow_handler() -> &'static str {
    tokio::time::sleep(Duration::from_millis(200)).await;
    "done"
}

#[tokio::test]
async fn timeout_layer_one_liner_triggers() {
    let client = TestApp::new()
        .routes(routes![slow_handler])
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_| async {
                    StatusCode::REQUEST_TIMEOUT
                }))
                .layer(TimeoutLayer::new(Duration::from_millis(50))),
        )
        .build();

    client.get("/slow").send().await.assert_status(408);
}

// ── Test 2: poll_ready backpressure propagates ───────────────────────────

#[derive(Clone)]
struct PendingLayer {
    gate: Arc<Notify>,
    ready: Arc<std::sync::atomic::AtomicBool>,
}

impl<S> tower::Layer<S> for PendingLayer {
    type Service = PendingService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        PendingService {
            inner,
            gate: self.gate.clone(),
            ready: self.ready.clone(),
        }
    }
}

#[derive(Clone)]
struct PendingService<S> {
    inner: S,
    gate: Arc<Notify>,
    ready: Arc<std::sync::atomic::AtomicBool>,
}

impl<S> Service<Request<Body>> for PendingService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response<Body>, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.ready.load(std::sync::atomic::Ordering::SeqCst) {
            self.inner.poll_ready(cx)
        } else {
            let gate = self.gate.clone();
            let waker = cx.waker().clone();
            tokio::spawn(async move {
                gate.notified().await;
                waker.wake();
            });
            Poll::Pending
        }
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

#[get("/ping")]
async fn ping_handler() -> &'static str {
    "pong"
}

#[tokio::test]
async fn poll_ready_propagates_backpressure() {
    let gate = Arc::new(Notify::new());
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let client = TestApp::new()
        .routes(routes![ping_handler])
        .layer(PendingLayer {
            gate: gate.clone(),
            ready: ready.clone(),
        })
        .build();

    // Fire a request while the layer is pending. The router must respect
    // `poll_ready` and NOT call the service — assert the request does not
    // complete within a short budget.
    let req_future = client.get("/ping").send();
    let stuck = tokio::time::timeout(Duration::from_millis(100), req_future).await;
    assert!(
        stuck.is_err(),
        "request should be pending while gate is shut"
    );

    // Open the gate; poll_ready must observe the transition and dispatch.
    ready.store(true, std::sync::atomic::Ordering::SeqCst);
    gate.notify_waiters();

    let resp = tokio::time::timeout(Duration::from_secs(1), client.get("/ping").send())
        .await
        .expect("request must complete once the layer is ready");
    resp.assert_status(200);
}

// ── Test 3: custom layer observes the generated request ID ───────────────

#[derive(Clone)]
struct CaptureIdLayer {
    captured: Arc<Mutex<Option<String>>>,
}

impl<S> tower::Layer<S> for CaptureIdLayer {
    type Service = CaptureIdService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        CaptureIdService {
            inner,
            captured: self.captured.clone(),
        }
    }
}

#[derive(Clone)]
struct CaptureIdService<S> {
    inner: S,
    captured: Arc<Mutex<Option<String>>>,
}

impl<S> Service<Request<Body>> for CaptureIdService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response<Body>, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if let Some(id) = req.extensions().get::<RequestId>() {
            *self.captured.lock().unwrap() = Some(id.to_string());
        }
        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

#[tokio::test]
async fn custom_layer_sees_request_id() {
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let client = TestApp::new()
        .routes(routes![ping_handler])
        .layer(CaptureIdLayer {
            captured: captured.clone(),
        })
        .build();

    let resp = client.get("/ping").send().await;
    resp.assert_status(200);

    let response_id = resp
        .header("x-request-id")
        .expect("RequestIdLayer should set X-Request-Id")
        .to_owned();
    let observed = captured
        .lock()
        .unwrap()
        .clone()
        .expect("custom layer should have captured the request ID");

    assert_eq!(
        observed, response_id,
        "custom layer must observe the same request ID that RequestIdLayer wrote to the response"
    );
}
