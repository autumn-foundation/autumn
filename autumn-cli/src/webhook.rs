use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use sha2::Sha256;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookProvider {
    Stripe,
    Github,
    Slack,
    Generic,
}

impl WebhookProvider {
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "stripe" => Ok(Self::Stripe),
            "github" => Ok(Self::Github),
            "slack" => Ok(Self::Slack),
            "generic" => Ok(Self::Generic),
            _ => Err(format!(
                "Unknown provider '{s}'. Supported providers: stripe, github, slack, generic"
            )),
        }
    }

    const fn as_slug(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Github => "github",
            Self::Slack => "slack",
            Self::Generic => "generic",
        }
    }
}

/// The JSON field a provider's replay key is read from, for the providers that
/// carry it in the body instead of a header.
///
/// Stripe keys replay protection on the event's top-level `id`, and Slack's
/// Events API on `event_id` (see `autumn_web::webhook::resolve_delivery_id`).
const fn body_delivery_id_field(provider: WebhookProvider) -> Option<&'static str> {
    match provider {
        WebhookProvider::Stripe => Some("id"),
        WebhookProvider::Slack => Some("event_id"),
        // GitHub and generic carry it in a header, refreshed per request below.
        WebhookProvider::Github | WebhookProvider::Generic => None,
    }
}

/// Rewrite a simulated payload's body-carried delivery ID so a second
/// `autumn webhook sim` is a new delivery rather than a replay.
///
/// The header-based providers already get [`fresh_sim_delivery_id`] per
/// invocation — without this, Stripe and Slack sims reused whatever ID the
/// payload hardcoded, so the endpoint's replay protection (correctly) answered
/// `409 Conflict` to every run after the first for the next 24 hours.
///
/// The rewritten body is what gets signed *and* sent: signatures cover the exact
/// bytes on the wire, so this must happen before signing.
///
/// Returns the payload unchanged (and `None`) for a header-based provider, or
/// when the payload is not a JSON object — a body this cannot safely rewrite is
/// left exactly as the user wrote it.
fn with_fresh_body_delivery_id(
    provider: WebhookProvider,
    payload: &str,
) -> (String, Option<(&'static str, String)>) {
    let Some(field) = body_delivery_id_field(provider) else {
        return (payload.to_owned(), None);
    };
    let Ok(mut body) = serde_json::from_str::<serde_json::Value>(payload) else {
        return (payload.to_owned(), None);
    };
    let Some(object) = body.as_object_mut() else {
        return (payload.to_owned(), None);
    };
    let fresh = fresh_sim_delivery_id(provider);
    object.insert(field.to_owned(), serde_json::Value::String(fresh.clone()));
    serde_json::to_string(&body).map_or_else(
        |_| (payload.to_owned(), None),
        |rewritten| (rewritten, Some((field, fresh))),
    )
}

fn fresh_sim_delivery_id(provider: WebhookProvider) -> String {
    let mut random = [0_u8; 16];
    if let Err(error) = getrandom::fill(&mut random) {
        eprintln!("Error: failed to generate webhook delivery ID: {error}");
        std::process::exit(1);
    }

    format!("sim-{}-{}", provider.as_slug(), hex::encode(random))
}

/// The event type a simulated delivery announces when the caller does not pass
/// one. Kept as the historical value so an existing `autumn webhook sim`
/// invocation behaves exactly as before.
const DEFAULT_SIM_EVENT_TYPE: &str = "sim.event";

pub fn run_sim(
    provider_str: &str,
    url: &str,
    secret: &str,
    payload: &str,
    event_type: Option<&str>,
) {
    let provider = match WebhookProvider::from_str(provider_str) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    println!("🌟 Simulating webhook for provider: {provider:?}");
    println!("📡 Sending to URL: {url}");

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to initialize HTTP client");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is set before Unix epoch")
        .as_secs();

    // Stripe and Slack read the event type out of the payload (`type`), not a
    // header, so an --event there would be silently dropped.
    if event_type.is_some() && body_delivery_id_field(provider).is_some() {
        eprintln!(
            "⚠️  Warning: --event is ignored for {provider:?}: its event type comes from the \
             payload's \"type\" field. Set it in --payload instead."
        );
    }

    // Refresh the body-carried delivery ID before signing: the signature covers
    // the exact bytes sent, and a reused ID would come back 409 Conflict.
    let (payload, fresh_body_id) = with_fresh_body_delivery_id(provider, payload);
    if let Some((field, id)) = &fresh_body_id {
        println!("🔁 Fresh delivery ID: {field} = {id}");
    }

    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(payload.clone());

    let payload_bytes = payload.as_bytes();

    match provider {
        WebhookProvider::Generic => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .expect("HMAC can take key of any size");
            mac.update(payload_bytes);
            let result = mac.finalize();
            let signature_hex = hex::encode(result.into_bytes());

            req = req.header("X-Webhook-Signature", format!("sha256={signature_hex}"));
            req = req.header("X-Webhook-Delivery", fresh_sim_delivery_id(provider));
            req = req.header(
                "X-Webhook-Event",
                event_type.unwrap_or(DEFAULT_SIM_EVENT_TYPE),
            );
        }
        WebhookProvider::Github => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .expect("HMAC can take key of any size");
            mac.update(payload_bytes);
            let result = mac.finalize();
            let signature_hex = hex::encode(result.into_bytes());

            req = req.header("X-Hub-Signature-256", format!("sha256={signature_hex}"));
            req = req.header("X-GitHub-Delivery", fresh_sim_delivery_id(provider));
            req = req.header(
                "X-GitHub-Event",
                event_type.unwrap_or(DEFAULT_SIM_EVENT_TYPE),
            );
        }
        WebhookProvider::Stripe => {
            // A payload with no top-level "id" used to be a 400
            // (MissingDeliveryId) here, because Stripe's replay key comes from
            // that field. `with_fresh_body_delivery_id` above now always sets
            // one — unless the payload is not a JSON object, which stays the
            // user's business and is warned about here.
            if fresh_body_id.is_none() {
                eprintln!(
                    "⚠️  Warning: could not set a delivery ID — the Stripe payload is not a \
                     JSON object.\n   The endpoint reads its replay-protection delivery ID \
                     from the top-level \"id\" field and will return 400 MissingDeliveryId."
                );
            }

            let signed_payload = format!("{now}.{payload}");
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .expect("HMAC can take key of any size");
            mac.update(signed_payload.as_bytes());
            let result = mac.finalize();
            let signature_hex = hex::encode(result.into_bytes());

            req = req.header("Stripe-Signature", format!("t={now},v1={signature_hex}"));
        }
        WebhookProvider::Slack => {
            let signed_payload = format!("v0:{now}:{payload}");
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .expect("HMAC can take key of any size");
            mac.update(signed_payload.as_bytes());
            let result = mac.finalize();
            let signature_hex = hex::encode(result.into_bytes());

            req = req.header("X-Slack-Signature", format!("v0={signature_hex}"));
            req = req.header("X-Slack-Request-Timestamp", now.to_string());
        }
    }

    handle_sim_response(req.send(), provider);
}

/// Cap on the response body echoed back on failure.
///
/// A dev-profile app answers with a full HTML error page — several hundred lines
/// of inline CSS for one status code — which buried the actual diagnosis.
const MAX_ECHOED_BODY_BYTES: usize = 600;

/// Trim a response body down to something readable, dropping an HTML error page
/// to its first line rather than dumping the stylesheet.
fn summarize_body(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('<') {
        return format!(
            "<{} bytes of HTML — the app is serving its error page; add `Accept: \
             application/problem+json` or read the server log for detail>",
            trimmed.len()
        );
    }
    if trimmed.len() <= MAX_ECHOED_BODY_BYTES {
        return trimmed.to_owned();
    }
    let cut = trimmed
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= MAX_ECHOED_BODY_BYTES)
        .last()
        .unwrap_or(0);
    format!("{}… ({} bytes total)", &trimmed[..cut], trimmed.len())
}

/// The extra line a `409 Conflict` deserves: it is replay protection working, and
/// what to do about it depends on where the provider carries its delivery ID.
const fn replay_conflict_hint(provider: WebhookProvider) -> &'static str {
    if body_delivery_id_field(provider).is_some() {
        // These get a fresh body ID per invocation, so a 409 means the endpoint
        // really has seen this delivery — most likely a hand-set ID.
        "   409 is replay protection: this delivery ID was already accepted. The simulator          mints a fresh one per run, so check for a hand-set ID in the payload."
    } else {
        // The framework also keys replay on the signature for the header-signed
        // providers, so a byte-identical signed body is itself a duplicate.
        "   409 is replay protection: for header-signed providers the endpoint also keys on          the signature, so re-sending a byte-identical payload is a duplicate delivery. Vary          --payload (or restart the app to clear an in-memory replay store)."
    }
}

fn handle_sim_response(
    result: Result<reqwest::blocking::Response, reqwest::Error>,
    provider: WebhookProvider,
) {
    match result {
        Ok(response) => {
            let status = response.status();
            match response.text() {
                Ok(text) => {
                    if status.is_success() {
                        println!("✅ Response Status: {status}");
                        if !text.is_empty() {
                            println!("📝 Response Body: {}", summarize_body(&text));
                        }
                    } else {
                        eprintln!("❌ Webhook endpoint returned status: {status}");
                        if status == reqwest::StatusCode::CONFLICT {
                            eprintln!("{}", replay_conflict_hint(provider));
                        }
                        if !text.is_empty() {
                            eprintln!("Response Body: {}", summarize_body(&text));
                        }
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("❌ Failed to read webhook response body: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("❌ Failed to send webhook: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn test_provider_from_str() {
        assert_eq!(
            WebhookProvider::from_str("stripe").unwrap(),
            WebhookProvider::Stripe
        );
        assert_eq!(
            WebhookProvider::from_str("STRIPE").unwrap(),
            WebhookProvider::Stripe
        );
        assert_eq!(
            WebhookProvider::from_str("github").unwrap(),
            WebhookProvider::Github
        );
        assert_eq!(
            WebhookProvider::from_str("slack").unwrap(),
            WebhookProvider::Slack
        );
        assert_eq!(
            WebhookProvider::from_str("generic").unwrap(),
            WebhookProvider::Generic
        );
        assert!(WebhookProvider::from_str("unknown").is_err());
    }

    fn capture_delivery_header(provider: &str, header: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind webhook capture server");
        let addr = listener.local_addr().expect("capture server local addr");

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept simulated webhook");
            let mut raw_request = Vec::new();
            let mut buffer = [0_u8; 1024];

            loop {
                let bytes_read = stream
                    .read(&mut buffer)
                    .expect("read simulated webhook request");
                if bytes_read == 0 {
                    break;
                }

                raw_request.extend_from_slice(&buffer[..bytes_read]);
                if raw_request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .expect("write simulated webhook response");

            let request = String::from_utf8_lossy(&raw_request);
            request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case(header)
                        .then(|| value.trim().to_owned())
                })
                .unwrap_or_else(|| panic!("missing {header} header in request:\n{request}"))
        });

        let url = format!("http://{addr}/webhook");
        run_sim(provider, &url, "secret", r#"{"ok":true}"#, None);

        handle.join().expect("capture server should finish")
    }

    /// Capture a simulated request whole — headers *and* body — so a test can
    /// check the body-carried delivery ID and verify the signature covers the
    /// bytes actually sent.
    fn capture_request(provider: &str, payload: &'static str) -> String {
        capture_request_with_event(provider, payload, None)
    }

    fn capture_request_with_event(
        provider: &str,
        payload: &'static str,
        event_type: Option<&str>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind webhook capture server");
        let addr = listener.local_addr().expect("capture server local addr");

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept simulated webhook");
            let mut raw_request = Vec::new();
            let mut buffer = [0_u8; 1024];

            loop {
                let bytes_read = stream
                    .read(&mut buffer)
                    .expect("read simulated webhook request");
                if bytes_read == 0 {
                    break;
                }
                raw_request.extend_from_slice(&buffer[..bytes_read]);

                // Keep reading until the declared body has arrived in full.
                let text = String::from_utf8_lossy(&raw_request).into_owned();
                if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= content_length {
                        break;
                    }
                }
            }

            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .expect("write simulated webhook response");

            String::from_utf8_lossy(&raw_request).into_owned()
        });

        let url = format!("http://{addr}/webhook");
        run_sim(provider, &url, "secret", payload, event_type);

        handle.join().expect("capture server should finish")
    }

    fn request_body(request: &str) -> &str {
        request
            .split_once("\r\n\r\n")
            .expect("request has a body")
            .1
    }

    fn header_value(request: &str, header: &str) -> String {
        request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(header)
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("missing {header} header in request:\n{request}"))
    }

    #[test]
    fn a_dev_error_page_is_summarized_rather_than_dumped() {
        let page = format!(
            "<!DOCTYPE html><html><style>{}</style></html>",
            "a".repeat(5_000)
        );
        let summary = summarize_body(&page);
        assert!(summary.contains("bytes of HTML"), "{summary}");
        assert!(
            summary.len() < 300,
            "the stylesheet must not be echoed: {summary}"
        );

        // A short JSON body is passed through untouched.
        assert_eq!(
            summarize_body(r#"{"received":true}"#),
            r#"{"received":true}"#
        );
    }

    #[test]
    fn the_replay_conflict_hint_matches_where_the_delivery_id_lives() {
        assert!(
            replay_conflict_hint(WebhookProvider::Stripe).contains("fresh one per run"),
            "body-keyed providers get a fresh ID, so a 409 means something else"
        );
        assert!(
            replay_conflict_hint(WebhookProvider::Github).contains("signature"),
            "header-signed providers also key replay on the signature"
        );
    }

    #[test]
    fn header_based_providers_announce_the_requested_event_type() {
        // Without this the simulator only ever announced `sim.event`, which no
        // real handler dispatches on — so a simulated delivery fell through to
        // acknowledge-and-ignore and proved nothing about the user's handler.
        let request =
            capture_request_with_event("github", r#"{"ref":"refs/heads/main"}"#, Some("push"));
        assert_eq!(header_value(&request, "X-GitHub-Event"), "push");

        let request =
            capture_request_with_event("generic", r#"{"ok":true}"#, Some("example.created"));
        assert_eq!(header_value(&request, "X-Webhook-Event"), "example.created");
    }

    #[test]
    fn an_omitted_event_type_keeps_the_historical_default() {
        let request = capture_request("github", r#"{"ok":true}"#);
        assert_eq!(
            header_value(&request, "X-GitHub-Event"),
            DEFAULT_SIM_EVENT_TYPE
        );
    }

    #[test]
    fn stripe_sim_uses_a_fresh_body_delivery_id_per_invocation() {
        // Stripe's replay key is the payload's top-level `id`, so a fixed one
        // makes every simulation after the first a 409 for the next 24 hours.
        const PAYLOAD: &str = r#"{"id":"evt_1","type":"payment_intent.succeeded"}"#;
        let first = capture_request("stripe", PAYLOAD);
        let second = capture_request("stripe", PAYLOAD);

        let first_id = json_field(request_body(&first), "id");
        let second_id = json_field(request_body(&second), "id");
        assert_ne!(
            first_id, second_id,
            "stripe simulator reused a delivery ID, poisoning replay protection"
        );
        assert_ne!(first_id, "evt_1", "the hardcoded ID must be replaced");
    }

    #[test]
    fn slack_sim_uses_a_fresh_body_delivery_id_per_invocation() {
        const PAYLOAD: &str = r#"{"event_id":"Ev1","type":"event_callback"}"#;
        let first = capture_request("slack", PAYLOAD);
        let second = capture_request("slack", PAYLOAD);

        let first_id = json_field(request_body(&first), "event_id");
        let second_id = json_field(request_body(&second), "event_id");
        assert_ne!(
            first_id, second_id,
            "slack simulator reused a delivery ID, poisoning replay protection"
        );
        assert_ne!(first_id, "Ev1", "the hardcoded ID must be replaced");
    }

    #[test]
    fn stripe_sim_signs_the_bytes_it_sends_after_rewriting_the_delivery_id() {
        // The rewrite has to happen before signing: the extractor verifies the
        // HMAC against the exact request bytes, so a signature over the original
        // payload would be a 401 rather than a working simulation.
        let request = capture_request("stripe", r#"{"id":"evt_1","type":"x"}"#);
        let body = request_body(&request);
        let signature = header_value(&request, "Stripe-Signature");
        let (timestamp, sent_signature) = signature
            .split_once(',')
            .expect("stripe signature has t= and v1= parts");
        let timestamp = timestamp.trim_start_matches("t=");
        let sent_signature = sent_signature.trim_start_matches("v1=");

        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").expect("hmac key");
        mac.update(format!("{timestamp}.{body}").as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());

        assert_eq!(
            sent_signature, expected,
            "the signature must cover the rewritten body that was actually sent"
        );
    }

    #[test]
    fn a_non_object_payload_is_left_exactly_as_written() {
        let (payload, rewritten) = with_fresh_body_delivery_id(WebhookProvider::Stripe, "[1,2]");
        assert_eq!(payload, "[1,2]");
        assert!(rewritten.is_none());

        let (payload, rewritten) = with_fresh_body_delivery_id(WebhookProvider::Stripe, "not json");
        assert_eq!(payload, "not json");
        assert!(rewritten.is_none());
    }

    #[test]
    fn header_based_providers_keep_their_body_untouched() {
        for provider in [WebhookProvider::Github, WebhookProvider::Generic] {
            let (payload, rewritten) = with_fresh_body_delivery_id(provider, r#"{"id":"keep"}"#);
            assert_eq!(payload, r#"{"id":"keep"}"#);
            assert!(
                rewritten.is_none(),
                "{provider:?} carries its delivery ID in a header"
            );
        }
    }

    fn json_field(body: &str, field: &str) -> String {
        serde_json::from_str::<serde_json::Value>(body)
            .expect("body is JSON")
            .get(field)
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("missing {field} in body: {body}"))
            .to_owned()
    }

    #[test]
    fn generic_sim_uses_fresh_delivery_id_per_invocation() {
        let first = capture_delivery_header("generic", "X-Webhook-Delivery");
        let second = capture_delivery_header("generic", "X-Webhook-Delivery");

        assert_ne!(
            first, second,
            "generic simulator reused a delivery ID, poisoning replay protection"
        );
        assert_ne!(first, "sim-delivery-123");
        assert_ne!(second, "sim-delivery-123");
    }

    #[test]
    fn github_sim_uses_fresh_delivery_id_per_invocation() {
        let first = capture_delivery_header("github", "X-GitHub-Delivery");
        let second = capture_delivery_header("github", "X-GitHub-Delivery");

        assert_ne!(
            first, second,
            "github simulator reused a delivery ID, poisoning replay protection"
        );
        assert_ne!(first, "sim-delivery-123");
        assert_ne!(second, "sim-delivery-123");
    }
}
