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

pub fn run_sim(provider_str: &str, url: &str, secret: &str, payload: &str) {
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
            req = req.header("X-Webhook-Event", "sim.event");
        }
        WebhookProvider::Github => {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
                .expect("HMAC can take key of any size");
            mac.update(payload_bytes);
            let result = mac.finalize();
            let signature_hex = hex::encode(result.into_bytes());

            req = req.header("X-Hub-Signature-256", format!("sha256={signature_hex}"));
            req = req.header("X-GitHub-Delivery", fresh_sim_delivery_id(provider));
            req = req.header("X-GitHub-Event", "sim.event");
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

    handle_sim_response(req.send());
}

fn handle_sim_response(result: Result<reqwest::blocking::Response, reqwest::Error>) {
    match result {
        Ok(response) => {
            let status = response.status();
            match response.text() {
                Ok(text) => {
                    if status.is_success() {
                        println!("✅ Response Status: {status}");
                        if !text.is_empty() {
                            println!("📝 Response Body: {text}");
                        }
                    } else {
                        eprintln!("❌ Webhook endpoint returned status: {status}");
                        if !text.is_empty() {
                            eprintln!("Response Body: {text}");
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
        run_sim(provider, &url, "secret", r#"{"ok":true}"#);

        handle.join().expect("capture server should finish")
    }

    /// Capture a simulated request whole — headers *and* body — so a test can
    /// check the body-carried delivery ID and verify the signature covers the
    /// bytes actually sent.
    fn capture_request(provider: &str, payload: &'static str) -> String {
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
        run_sim(provider, &url, "secret", payload);

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
