//! Signed inbound webhook intake — Stripe-Signature is verified before the
//! handler runs. See `autumn.toml` `[[security.webhooks.endpoints]]` for the
//! endpoint registration and secret configuration.
//!
//! Currently handles Stripe events for Reddit Gold / Premium subscriptions.
//! Add more `[[security.webhooks.endpoints]]` blocks for GitHub, Slack, etc.

use autumn_web::prelude::*;
use autumn_web::webhook::SignedWebhook;

/// POST /webhooks/stripe
///
/// The `SignedWebhook` extractor rejects the request with 401 before this
/// handler runs if the `Stripe-Signature` header is absent, expired (outside
/// the 5-minute tolerance), or the HMAC doesn't match the configured secret.
///
/// In a real app the handler would:
/// 1. Deserialise the typed Stripe event.
/// 2. Look up the user by `customer.id` in your database.
/// 3. Grant/revoke the Reddit Gold subscription in a transaction.
/// 4. Enqueue a confirmation email via `Mailer::deliver_later`.
///
/// `#[public]`: Stripe calls this route, not a signed-in user, so no session
/// guard fits. The `SignedWebhook` signature check is the guard.
#[public]
#[post("/webhooks/stripe")]
pub async fn stripe_webhook(webhook: SignedWebhook) -> AutumnResult<Json<serde_json::Value>> {
    // Deserialise the full event payload — in production, use a typed Stripe SDK
    // struct (e.g. `stripe::Event`) to access `data.object` for customer/subscription IDs.
    let _event: serde_json::Value = webhook
        .json::<serde_json::Value>()
        .map_err(|e| AutumnError::bad_request_msg(format!("invalid Stripe payload: {e}")))?;

    let event_type = webhook.event_type().unwrap_or("unknown");

    tracing::info!(
        provider     = %webhook.provider(),
        event_type,
        delivery_id  = webhook.delivery_id().unwrap_or("-"),
        "received Stripe webhook"
    );

    match event_type {
        "customer.subscription.created" | "customer.subscription.updated" => {
            // Grant or update Reddit Gold for the subscriber.
            tracing::info!("Reddit Gold subscription event — grant premium to user");
        }
        "customer.subscription.deleted" => {
            tracing::info!("Reddit Gold subscription cancelled — revoke premium");
        }
        _ => {
            tracing::debug!(event_type, "unhandled Stripe event type");
        }
    }

    Ok(Json(serde_json::json!({
        "accepted":    true,
        "event_type":  event_type,
        "delivery_id": webhook.delivery_id(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::config::{AutumnConfig, MockEnv};
    use autumn_web::test::TestApp;
    use autumn_web::webhook::{
        WebhookConfig, WebhookEndpointConfig, WebhookProvider, hmac_sha256_hex,
    };

    const DEV_SECRET: &str = "dev-stripe-webhook-secret-32-bytes";

    fn test_config() -> AutumnConfig {
        AutumnConfig {
            security: autumn_web::security::SecurityConfig {
                csrf: autumn_web::security::CsrfConfig {
                    enabled: false,
                    ..Default::default()
                },
                webhooks: WebhookConfig {
                    endpoints: vec![
                        WebhookEndpointConfig::new(
                            "stripe",
                            "/webhooks/stripe",
                            WebhookProvider::Stripe,
                            DEV_SECRET,
                        )
                        .with_timestamp_tolerance_secs(300)
                        .with_replay_window_secs(86400)
                        .without_replay_protection(),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn stripe_sig(secret: &str, ts: u64, body: &str) -> String {
        let signed = format!("{ts}.{body}");
        let sig = hmac_sha256_hex(secret.as_bytes(), signed.as_bytes());
        format!("t={ts},v1={sig}")
    }

    #[tokio::test]
    async fn valid_signature_is_accepted() {
        let _env = MockEnv::new().with("AUTUMN_PROFILE", "test");
        let app = TestApp::new()
            .config(test_config())
            .routes(routes![stripe_webhook])
            .build();

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let body = r#"{"type":"customer.subscription.created"}"#;
        let sig = stripe_sig(DEV_SECRET, ts, body);

        let resp = app
            .post("/webhooks/stripe")
            .header("stripe-signature", &sig)
            .body(body)
            .send()
            .await;

        resp.assert_status(200);
    }

    #[tokio::test]
    async fn missing_signature_header_is_rejected() {
        let app = TestApp::new()
            .config(test_config())
            .routes(routes![stripe_webhook])
            .build();

        let resp = app
            .post("/webhooks/stripe")
            .body(r#"{"type":"test"}"#)
            .send()
            .await;

        resp.assert_status(400);
    }
}
