//! Outbox delivery to another service, and the inbox endpoint that takes it.
//!
//! **Producer.** Each topic of `[outbox.destinations.<name>]` gets a relay
//! handler. The handler POSTs the message to the destination as a signed
//! envelope. The relay keeps its rules: retry with jitter, order per
//! aggregate, dead letters and `requeue`.
//!
//! **Receiver.** `POST /_autumn/inbox` verifies the signature. Then, in one
//! transaction, it records the message id with [`Inbox::seen`] and writes one
//! relay row for each `#[listener]` of the topic. A copy commits nothing new.
//! The receiver relay then runs each listener, in order per aggregate.
//!
//! The envelope is the JSON body
//! `{"id", "topic", "aggregate", "created_at", "payload"}`. The
//! `Autumn-Signature: t=<unix>,v1=<hex>` header signs `{t}.{body}` with
//! HMAC-SHA256. The id, topic and aggregate are in the signed body, so a
//! replay cannot change them.

#[cfg(feature = "http-client")]
use std::sync::Arc;

use axum::response::IntoResponse as _;
use chrono::{DateTime, Utc};
use scoped_futures::ScopedFutureExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    CURRENT_MESSAGE_ID, Inbox, OutboxHandlers, OutboxMessage, TOPIC_INBOX, insert_message_with_id,
    is_valid_topic, mail_pool,
};
use crate::config::{OutboxConfig, OutboxInboxConfig};
use crate::{AppState, AutumnError, AutumnResult};

/// The path of the inbox endpoint.
pub const INBOX_PATH: &str = "/_autumn/inbox";
/// The header that signs an inbox request: `t=<unix seconds>,v1=<hex>`.
pub const SIGNATURE_HEADER: &str = "Autumn-Signature";
/// The header that holds the message id. For logs only: the signed body holds
/// the id that counts.
pub const MESSAGE_ID_HEADER: &str = "Autumn-Message-Id";
/// The header that holds the topic. For logs only, as [`MESSAGE_ID_HEADER`].
pub const TOPIC_HEADER: &str = "Autumn-Topic";

/// The [`Inbox`] consumer name of the inbox endpoint.
const INBOX_CONSUMER: &str = "autumn.inbox";

/// The body of an inbox request.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    id: String,
    topic: String,
    aggregate: String,
    created_at: DateTime<Utc>,
    payload: Value,
}

/// The `Autumn-Signature` value for `body`, signed at `timestamp` (Unix
/// seconds): `t=<timestamp>,v1=<hex HMAC-SHA256 of "{timestamp}.{body}">`.
#[must_use]
pub fn inbox_signature(secret: &str, timestamp: i64, body: &str) -> String {
    let signature = crate::security::config::hmac_sha256_hex(
        secret.as_bytes(),
        format!("{timestamp}.{body}").as_bytes(),
    );
    format!("t={timestamp},v1={signature}")
}

// ── Producer ────────────────────────────────────────────────────────────────

/// Add one relay handler for each destination topic.
///
/// # Errors
///
/// Returns an error when a topic also has an `outbox_handler`, a topic is not
/// an `#[event(name = "...")]` in this binary, a secret is not set, or the
/// `http-client` feature is off.
pub(super) fn add_destinations(
    handlers: &mut OutboxHandlers,
    config: &OutboxConfig,
) -> AutumnResult<()> {
    for (name, destination) in &config.destinations {
        for topic in &destination.topics {
            if handlers.handlers.contains_key(topic) {
                return Err(AutumnError::internal_server_error_msg(format!(
                    "outbox topic {topic:?} has an outbox_handler and is in \
                     outbox.destinations.{name}; send each topic to one place"
                )));
            }
            if crate::events::event_contract(topic).is_none() {
                return Err(AutumnError::internal_server_error_msg(format!(
                    "outbox.destinations.{name}: topic {topic:?} is not an \
                     #[event(name = \"...\")] in this binary. A remote topic must be an event \
                     with a descriptor, so that `autumn contracts check` covers it"
                )));
            }
        }
        let secret = destination
            .signing_secret
            .clone()
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| {
                AutumnError::internal_server_error_msg(format!(
                    "outbox.destinations.{name}.signing_secret is required"
                ))
            })?;
        #[cfg(feature = "http-client")]
        add_destination(
            handlers,
            name,
            &destination.url,
            secret,
            &destination.topics,
        );
        #[cfg(not(feature = "http-client"))]
        {
            drop(secret);
            return Err(AutumnError::internal_server_error_msg(format!(
                "outbox.destinations.{name} needs the autumn-web `http-client` feature"
            )));
        }
    }
    Ok(())
}

#[cfg(feature = "http-client")]
fn add_destination(
    handlers: &mut OutboxHandlers,
    name: &str,
    url: &str,
    secret: String,
    topics: &[String],
) {
    let destination = Arc::new(Destination {
        name: name.to_owned(),
        url: url.to_owned(),
        secret,
    });
    for topic in topics {
        let destination = Arc::clone(&destination);
        handlers.insert_unchecked(topic.clone(), move |state, message| {
            deliver(state, Arc::clone(&destination), message)
        });
    }
}

/// One remote destination of the relay.
#[cfg(feature = "http-client")]
struct Destination {
    name: String,
    url: String,
    secret: String,
}

/// POST `message` to `destination`. Only a `200` with the status `accepted`
/// or `duplicate` marks it sent. Any other result is a failure, and the relay
/// sends the message again later.
///
/// The request does not follow redirects: a redirect to a login page must not
/// count as delivered. It does not use the shared circuit breaker: the relay
/// already waits between attempts, and an open breaker would use up attempts
/// without a send.
#[cfg(feature = "http-client")]
async fn deliver(
    state: AppState,
    destination: Arc<Destination>,
    message: OutboxMessage,
) -> AutumnResult<()> {
    let body = serde_json::to_string(&Envelope {
        id: message.id.clone(),
        topic: message.topic.clone(),
        aggregate: message.aggregate.clone(),
        created_at: message.created_at,
        payload: message.payload,
    })
    .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
    // The receiver checks `t=` against its own clock: real time outside a
    // `Sim`, the sim clock inside one.
    let timestamp = crate::time::ambient_now().timestamp();
    let signature = inbox_signature(&destination.secret, timestamp, &body);
    // Named by its URL, as webhook delivery is: a test mocks the destination
    // with `TestApp::http_mock(url)`.
    let response = crate::http_client::Client::from_state(&state)
        .named(&destination.url)
        .post(&destination.url)
        .header("Content-Type", "application/json")
        .header(SIGNATURE_HEADER, signature)
        .header(MESSAGE_ID_HEADER, &message.id)
        .header(TOPIC_HEADER, &message.topic)
        .text_body(body)
        .no_redirect()
        .send()
        .await
        .map_err(|error| {
            AutumnError::service_unavailable_msg(format!(
                "outbox destination {}: {error}",
                destination.name
            ))
        })?;
    let status = response.status();
    if status.as_u16() == 200 && is_acknowledgement(&response.text()) {
        return Ok(());
    }
    Err(AutumnError::service_unavailable_msg(format!(
        "outbox destination {}: the inbox returned {status}, not an acknowledgement",
        destination.name
    )))
}

/// Whether `body` is the JSON acknowledgement of an inbox.
#[cfg(feature = "http-client")]
fn is_acknowledgement(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Ack {
        status: String,
    }
    serde_json::from_str::<Ack>(body)
        .is_ok_and(|ack| matches!(ack.status.as_str(), "accepted" | "duplicate"))
}

// ── Receiver ────────────────────────────────────────────────────────────────

/// The signature check of the inbox endpoint. The app installs it when
/// `outbox.inbox.enabled = true`.
#[derive(Debug)]
pub struct InboxVerifier {
    verifier: crate::webhook::AutumnSignatureVerifier,
    max_body_bytes: usize,
}

impl InboxVerifier {
    /// A verifier for `config`.
    ///
    /// # Errors
    ///
    /// Returns an error when the signing secret is not set.
    pub(super) fn from_config(config: &OutboxInboxConfig) -> AutumnResult<Self> {
        let secret = config
            .signing_secret
            .as_deref()
            .filter(|secret| !secret.is_empty())
            .ok_or_else(|| {
                AutumnError::internal_server_error_msg("outbox.inbox.signing_secret is required")
            })?;
        Ok(Self {
            verifier: crate::webhook::AutumnSignatureVerifier::new(
                "outbox-inbox",
                INBOX_PATH,
                secret,
                &config.previous_signing_secrets,
                config.timestamp_tolerance_secs,
            ),
            max_body_bytes: config.max_body_bytes,
        })
    }
}

/// The router of the inbox endpoint.
pub fn inbox_router() -> axum::Router<AppState> {
    axum::Router::new().route(INBOX_PATH, axum::routing::post(receive))
}

/// `200 {"status": "accepted" | "duplicate"}`, or a problem response.
async fn receive(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    match accept(&state, &headers, body).await {
        Ok(status) => axum::Json(serde_json::json!({ "status": status })).into_response(),
        Err(error) => error.into_response(),
    }
}

/// Verify the request and hand the message to the listeners of its topic.
///
/// Status codes: `400` for a bad header or body, `401` for a bad signature,
/// `413` for a body that is too large, and `503` when this release has no
/// listener for the topic. The relay of the producer sends again on each of
/// them, so a topic that arrives before the release that handles it waits.
async fn accept(
    state: &AppState,
    headers: &http::HeaderMap,
    body: axum::body::Body,
) -> AutumnResult<&'static str> {
    let verifier = state
        .extension::<InboxVerifier>()
        .ok_or_else(|| AutumnError::service_unavailable_msg("the outbox inbox is not installed"))?;
    let received_at = crate::time::ambient_system_time();
    // Refuse a request with no header or a stale time before reading its body.
    verifier.verifier.precheck(headers, received_at)?;
    let bytes = axum::body::to_bytes(body, verifier.max_body_bytes)
        .await
        .map_err(|_| {
            AutumnError::bad_request_msg("inbox body is larger than outbox.inbox.max_body_bytes")
                .with_status(http::StatusCode::PAYLOAD_TOO_LARGE)
        })?;
    verifier.verifier.verify(headers, &bytes, received_at)?;
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|error| {
        // The serde text can quote payload values, so give only its kind.
        AutumnError::bad_request_msg(format!(
            "inbox body is not a message envelope ({:?})",
            error.classify()
        ))
    })?;
    if envelope.id.is_empty() || !is_valid_topic(&envelope.topic) {
        return Err(AutumnError::bad_request_msg(
            "inbox message needs an id and a topic of A-Z a-z 0-9 . _ : -",
        ));
    }
    let listeners: Vec<String> = state
        .extension::<crate::events::EventRegistry>()
        .map(|registry| {
            registry
                .listeners_for(&envelope.topic)
                .iter()
                .map(|listener| listener.listener_name.clone())
                .collect()
        })
        .unwrap_or_default();
    let pool = mail_pool(state)
        .ok_or_else(|| AutumnError::service_unavailable_msg("the outbox inbox needs a database"))?;
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::service_unavailable_msg(format!("outbox inbox: no connection: {error}"))
    })?;
    let clock = state.clock_arc();
    let message_id = envelope.id.clone();
    let status = crate::db::scoped_transaction(&mut *conn, |conn| {
        async move {
            // A copy is acknowledged even after a deploy that dropped the
            // listener. A new message with no listener rolls back the record.
            if Inbox::new(INBOX_CONSUMER).seen(conn, &envelope.id).await? {
                return Ok::<_, AutumnError>("duplicate");
            }
            if listeners.is_empty() {
                return Err(AutumnError::service_unavailable_msg(format!(
                    "this release has no #[listener] for topic {}; send it again later",
                    envelope.topic
                )));
            }
            let message = serde_json::to_value(&envelope)
                .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
            for listener in &listeners {
                let body = serde_json::json!({ "listener": listener, "message": message });
                // One lane per listener and aggregate: the order of the
                // producer holds, and one failing listener does not stop
                // the others.
                insert_message_with_id(
                    conn,
                    format!("{}:{listener}", envelope.id),
                    clock.as_ref(),
                    Some(&format!("{listener}|{}", envelope.aggregate)),
                    TOPIC_INBOX,
                    &body,
                )
                .await?;
            }
            Ok("accepted")
        }
        .scope_boxed()
    })
    .await?;
    if status == "duplicate" {
        tracing::debug!(%message_id, "outbox inbox dropped a copy");
    }
    Ok(status)
}

#[derive(Deserialize)]
struct InboxRow {
    listener: String,
    message: Envelope,
}

/// The relay handler of [`TOPIC_INBOX`]: run one listener for one message
/// that the inbox took.
///
/// The listener runs with [`current_message_id`](super::current_message_id)
/// set to the id of the producer.
pub(super) async fn run_inbox_listener(
    state: AppState,
    message: OutboxMessage,
) -> AutumnResult<()> {
    let row: InboxRow = message.payload_as()?;
    let registry = state
        .extension::<crate::events::EventRegistry>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("no event listeners installed"))?;
    let handler = registry
        .listeners_for(&row.message.topic)
        .iter()
        .find(|listener| listener.listener_name == row.listener)
        .map(|listener| listener.handler)
        .ok_or_else(|| {
            AutumnError::internal_server_error_msg(format!(
                "listener {} of topic {} is not in this release",
                row.listener, row.message.topic
            ))
        })?;
    // The listener runs as a job does: a free `events::publish` in it goes to
    // this app, not to the process-global bus.
    let run = handler(state.clone(), row.message.payload);
    let run = crate::events::scope_event_app(state, run);
    CURRENT_MESSAGE_ID.scope(row.message.id, run).await
}

#[cfg(test)]
mod endpoint_tests {
    use super::{INBOX_PATH, InboxVerifier, SIGNATURE_HEADER, accept, inbox_signature};
    use crate::config::OutboxInboxConfig;
    use crate::{AppState, AutumnResult};

    const SECRET: &str = "endpoint-test-secret-0123456789abcdef";

    fn state(max_body_bytes: usize) -> AppState {
        let state = AppState::for_test();
        let verifier = InboxVerifier::from_config(&OutboxInboxConfig {
            enabled: true,
            signing_secret: Some(SECRET.to_owned()),
            max_body_bytes,
            ..OutboxInboxConfig::default()
        })
        .expect("verifier");
        state.insert_extension(verifier);
        state
    }

    fn now() -> i64 {
        crate::time::ambient_now().timestamp()
    }

    async fn post(
        state: &AppState,
        signature: Option<String>,
        body: &str,
    ) -> AutumnResult<&'static str> {
        let mut headers = http::HeaderMap::new();
        if let Some(signature) = signature {
            headers.insert(SIGNATURE_HEADER, signature.parse().expect("header value"));
        }
        accept(state, &headers, axum::body::Body::from(body.to_owned())).await
    }

    fn status(result: AutumnResult<&'static str>) -> u16 {
        result.expect_err("refused").status().as_u16()
    }

    #[test]
    fn the_endpoint_needs_a_secret() {
        assert!(InboxVerifier::from_config(&OutboxInboxConfig::default()).is_err());
        assert_eq!(INBOX_PATH, "/_autumn/inbox");
    }

    #[tokio::test]
    async fn without_a_verifier_the_endpoint_is_unavailable() {
        assert_eq!(status(post(&AppState::for_test(), None, "{}").await), 503);
    }

    #[tokio::test]
    async fn header_and_time_are_checked_before_the_body() {
        let state = state(1024);
        assert_eq!(status(post(&state, None, "{}").await), 400);
        let stale = inbox_signature(SECRET, now() - 3_600, "{}");
        assert_eq!(status(post(&state, Some(stale), "{}").await), 401);
        let malformed = format!("t={},v9=00", now());
        assert_eq!(status(post(&state, Some(malformed), "{}").await), 400);
    }

    #[tokio::test]
    async fn a_large_body_is_refused() {
        let state = state(8);
        let body = "{\"id\":\"0123456789\"}";
        let signature = inbox_signature(SECRET, now(), body);
        assert_eq!(status(post(&state, Some(signature), body).await), 413);
    }

    #[tokio::test]
    async fn the_signature_and_the_envelope_are_checked() {
        let state = state(4096);
        let wrong = inbox_signature("another-secret-0123456789abcdef", now(), "{}");
        assert_eq!(status(post(&state, Some(wrong), "{}").await), 401);

        let not_an_envelope = "{\"id\":1}";
        let signed = inbox_signature(SECRET, now(), not_an_envelope);
        assert_eq!(
            status(post(&state, Some(signed), not_an_envelope).await),
            400
        );

        let bad_topic = r#"{"id":"m","topic":"a b","aggregate":"x","created_at":"2026-01-01T00:00:00Z","payload":null}"#;
        let signed = inbox_signature(SECRET, now(), bad_topic);
        assert_eq!(status(post(&state, Some(signed), bad_topic).await), 400);

        // A good request on an app with no database: 503, so the relay retries.
        let good = r#"{"id":"m","topic":"deal.won","aggregate":"x","created_at":"2026-01-01T00:00:00Z","payload":{}}"#;
        let signed = inbox_signature(SECRET, now(), good);
        assert_eq!(status(post(&state, Some(signed), good).await), 503);
    }
}

#[cfg(all(test, feature = "http-client"))]
mod tests {
    use std::sync::Arc;

    use super::{Destination, deliver, is_acknowledgement};
    use crate::outbox::OutboxMessage;

    async fn send_to(status: u16, body: serde_json::Value) -> crate::AutumnResult<()> {
        let mut app = crate::test::TestApp::new();
        let mock = app
            .http_mock("http://marketing/_autumn/inbox")
            .post("/_autumn/inbox")
            .respond_with(status, body);
        let client = app.build();
        let destination = Arc::new(Destination {
            name: "marketing".to_owned(),
            url: "http://marketing/_autumn/inbox".to_owned(),
            secret: "deliver-test-secret-0123456789abcdef".to_owned(),
        });
        let message = OutboxMessage::new(
            "m-1",
            "deal:1",
            "deal.won",
            serde_json::json!({ "deal_id": 1 }),
            chrono::Utc::now(),
        );
        let result = deliver(client.state().clone(), destination, message).await;
        mock.expect_called(1);
        result
    }

    #[tokio::test]
    async fn only_an_acknowledged_200_marks_a_message_sent() {
        send_to(200, serde_json::json!({ "status": "accepted" }))
            .await
            .expect("accepted");
        send_to(200, serde_json::json!({ "status": "duplicate" }))
            .await
            .expect("duplicate");
        let error = send_to(200, serde_json::json!({ "ok": true }))
            .await
            .expect_err("not an acknowledgement");
        assert!(error.to_string().contains("marketing"), "{error}");
        let error = send_to(409, serde_json::json!({}))
            .await
            .expect_err("a failure");
        assert!(error.to_string().contains("409"), "{error}");
    }

    #[test]
    fn only_an_inbox_status_is_an_acknowledgement() {
        assert!(is_acknowledgement(r#"{"status":"accepted"}"#));
        assert!(is_acknowledgement(r#"{"status":"duplicate"}"#));
        for body in ["", "<html>login</html>", "{}", r#"{"status":"ok"}"#] {
            assert!(!is_acknowledgement(body), "{body:?}");
        }
    }
}
