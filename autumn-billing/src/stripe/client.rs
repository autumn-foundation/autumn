//! Stripe REST calls.
//!
//! Every call goes through [`StripeProvider::send`], which adds the bearer
//! token and decodes the reply. Bodies are form-encoded with Stripe's bracket
//! syntax (`line_items[0][price]`), the only encoding the API accepts.
//!
//! # Errors never carry secrets
//!
//! A [`BillingError::Provider`] message names the method, the path, the HTTP
//! status and Stripe's own error summary. It never contains the request body
//! or the `Authorization` header, and the secret key is scrubbed from any text
//! Stripe echoes back.
//!
//! # Idempotency keys
//!
//! Only `retry_invoice_payment` sends an `Idempotency-Key`: the dunning job
//! owns a key that is stable per `(invoice, attempt)`, so a crashed job can
//! repeat the call safely. The `create_*` calls send none. The provider has no
//! access to `AppState`, so it cannot draw a random key, and a key derived from
//! the request fields would be wrong: two checkouts for the same customer and
//! price are two distinct sessions (the first one may have expired), and a
//! derived key would make Stripe return the stale one.

use serde_json::Value;

use super::{PROVIDER_NAME, StripeProvider};
use crate::config::SecretString;
use crate::error::BillingError;
use crate::model::ProviderId;
use crate::provider::{
    CheckoutRequest, CustomerRequest, HostedSession, PaymentAttemptOutcome, PortalRequest,
};

/// Stripe error codes that mean the invoice cannot be collected right now.
/// The outcome is a decline, not a transport failure.
const DECLINE_CODES: &[&str] = &[
    "card_declined",
    "payment_intent_authentication_failure",
    "invoice_payment_intent_requires_action",
];

/// A decoded reply. The body is `Null` when it is empty or not JSON.
struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    fn str_field(&self, name: &str) -> Option<&str> {
        self.body.get(name).and_then(Value::as_str)
    }

    /// The `error` object Stripe returns on a failure.
    fn error(&self) -> ApiError {
        let error = self.body.get("error");
        let text = |name: &str| {
            error
                .and_then(|e| e.get(name))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        ApiError {
            kind: text("type"),
            code: text("code"),
            decline_code: text("decline_code"),
            message: text("message"),
        }
    }
}

/// The parts of a Stripe error object the mapping reads.
struct ApiError {
    kind: Option<String>,
    code: Option<String>,
    decline_code: Option<String>,
    message: Option<String>,
}

impl ApiError {
    /// One line for a provider error message.
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(kind) = &self.kind {
            parts.push(kind.clone());
        }
        if let Some(code) = &self.code {
            parts.push(code.clone());
        }
        let head = parts.join("/");
        match (&self.message, head.is_empty()) {
            (Some(message), false) => format!("{head}: {message}"),
            (Some(message), true) => message.clone(),
            (None, false) => head,
            (None, true) => "no error detail".to_owned(),
        }
    }

    fn is_already_paid(&self) -> bool {
        self.code.as_deref() == Some("invoice_already_paid")
            || self
                .message
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains("already paid"))
    }

    /// The decline reason reported to the dunning job.
    fn decline_reason(&self) -> String {
        if let Some(code) = &self.code {
            return code.clone();
        }
        if let Some(code) = &self.decline_code {
            return code.clone();
        }
        if self.kind.as_deref() == Some("card_error") {
            return "card_declined".to_owned();
        }
        "declined".to_owned()
    }

    fn is_known_decline(&self) -> bool {
        self.kind.as_deref() == Some("card_error")
            || self
                .code
                .as_deref()
                .is_some_and(|code| DECLINE_CODES.contains(&code))
    }
}

/// Form-encode `fields` with Stripe's bracket keys.
fn form(fields: &[(&str, &str)]) -> Result<String, BillingError> {
    serde_urlencoded::to_string(fields)
        .map_err(|e| BillingError::provider(PROVIDER_NAME, format!("form encoding: {e}")))
}

impl StripeProvider {
    fn secret(&self) -> &str {
        self.config
            .secret_key
            .as_ref()
            .map_or("", SecretString::expose)
    }

    /// Scrub the secret key from text Stripe echoed back.
    fn redact(&self, text: &str) -> String {
        let secret = self.secret();
        if secret.is_empty() {
            text.to_owned()
        } else {
            text.replace(secret, "<redacted>")
        }
    }

    fn provider_error(&self, method: &str, path: &str, detail: &str) -> BillingError {
        BillingError::provider(
            PROVIDER_NAME,
            format!("{method} {path}: {}", self.redact(detail)),
        )
    }

    fn http_error(&self, method: &str, path: &str, reply: &Reply) -> BillingError {
        let detail = format!("HTTP {} {}", reply.status, reply.error().summary());
        self.provider_error(method, path, &detail)
    }

    /// Send `request` with the bearer token and decode the reply.
    async fn send(
        &self,
        request: autumn_web::http::RequestBuilder,
        method: &str,
        path: &str,
    ) -> Result<Reply, BillingError> {
        let response = request
            .header("authorization", format!("Bearer {}", self.secret()))
            .send()
            .await
            .map_err(|e| self.provider_error(method, path, &format!("request failed: {e}")))?;
        let status = response.status().as_u16();
        let bytes = response.bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok(Reply { status, body })
    }

    /// `POST path` with a form body. Fails on any non-2xx reply.
    async fn post_form(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        idempotency_key: Option<&str>,
    ) -> Result<Reply, BillingError> {
        let body = form(fields)?;
        let mut request = self
            .client
            .post(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .text_body(body);
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        self.send(request, "POST", path).await
    }

    /// `POST path`, then read `id` and, when present, `url` from the reply.
    async fn create(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<(ProviderId, Option<String>), BillingError> {
        let reply = self.post_form(path, fields, None).await?;
        if !reply.is_success() {
            return Err(self.http_error("POST", path, &reply));
        }
        let id = reply
            .str_field("id")
            .ok_or_else(|| self.provider_error("POST", path, "reply has no id"))?;
        let url = reply.str_field("url").map(str::to_owned);
        Ok((ProviderId::new(id), url))
    }

    async fn hosted_session(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<HostedSession, BillingError> {
        let (id, url) = self.create(path, fields).await?;
        let url = url.ok_or_else(|| self.provider_error("POST", path, "reply has no url"))?;
        Ok(HostedSession::new(id, url))
    }

    pub(super) async fn customer(
        &self,
        request: CustomerRequest,
    ) -> Result<ProviderId, BillingError> {
        let mut fields = vec![
            (
                "metadata[autumn_customer_id]",
                request.local_customer_id.as_str(),
            ),
            ("metadata[autumn_user_id]", request.user_id.as_str()),
        ];
        if let Some(email) = &request.email {
            fields.push(("email", email.as_str()));
        }
        let (id, _) = self.create("/v1/customers", &fields).await?;
        Ok(id)
    }

    pub(super) async fn checkout(
        &self,
        request: CheckoutRequest,
    ) -> Result<HostedSession, BillingError> {
        let quantity = request.quantity.to_string();
        let fields = [
            ("mode", "subscription"),
            ("customer", request.provider_customer_id.as_str()),
            ("client_reference_id", request.local_customer_id.as_str()),
            ("line_items[0][price]", request.provider_price_id.as_str()),
            ("line_items[0][quantity]", quantity.as_str()),
            ("success_url", request.success_url.as_str()),
            ("cancel_url", request.cancel_url.as_str()),
        ];
        self.hosted_session("/v1/checkout/sessions", &fields).await
    }

    pub(super) async fn portal(
        &self,
        request: PortalRequest,
    ) -> Result<HostedSession, BillingError> {
        let fields = [
            ("customer", request.provider_customer_id.as_str()),
            ("return_url", request.return_url.as_str()),
        ];
        self.hosted_session("/v1/billing_portal/sessions", &fields)
            .await
    }

    pub(super) async fn pay_invoice(
        &self,
        invoice: &ProviderId,
        idempotency_key: &str,
    ) -> Result<PaymentAttemptOutcome, BillingError> {
        let path = format!("/v1/invoices/{invoice}/pay");
        let reply = self.post_form(&path, &[], Some(idempotency_key)).await?;
        if reply.is_success() {
            return Ok(match reply.str_field("status") {
                Some("paid") => PaymentAttemptOutcome::Paid,
                status => PaymentAttemptOutcome::Declined {
                    reason: format!("invoice_{}", status.unwrap_or("unknown")),
                },
            });
        }
        match reply.status {
            // Not a payment outcome: bad credentials, rate limit, or a Stripe
            // failure. The job keeps the attempt and retries later.
            401 | 403 | 429 | 500..=599 => Err(self.http_error("POST", &path, &reply)),
            400..=499 => {
                let error = reply.error();
                if error.is_already_paid() {
                    return Ok(PaymentAttemptOutcome::AlreadyPaid);
                }
                if !error.is_known_decline() {
                    tracing::debug!(
                        invoice = %invoice,
                        status = reply.status,
                        detail = %self.redact(&error.summary()),
                        "stripe invoice pay rejected; treated as a decline"
                    );
                }
                Ok(PaymentAttemptOutcome::Declined {
                    reason: error.decline_reason(),
                })
            }
            _ => Err(self.http_error("POST", &path, &reply)),
        }
    }

    pub(super) async fn cancel(&self, subscription: &ProviderId) -> Result<(), BillingError> {
        let path = format!("/v1/subscriptions/{subscription}");
        let request = self.client.delete(&path);
        let reply = self.send(request, "DELETE", &path).await?;
        // 404: the subscription is already gone, which is the state we want.
        if reply.is_success() || reply.status == 404 {
            return Ok(());
        }
        Err(self.http_error("DELETE", &path, &reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_uses_bracket_keys() {
        let encoded = form(&[
            ("line_items[0][price]", "price_x"),
            ("mode", "subscription"),
        ])
        .unwrap();
        assert_eq!(
            encoded,
            "line_items%5B0%5D%5Bprice%5D=price_x&mode=subscription"
        );
    }

    #[test]
    fn already_paid_is_read_from_code_or_message() {
        let by_code = ApiError {
            kind: None,
            code: Some("invoice_already_paid".into()),
            decline_code: None,
            message: None,
        };
        assert!(by_code.is_already_paid());
        let by_message = ApiError {
            kind: None,
            code: None,
            decline_code: None,
            message: Some("Invoice is Already Paid".into()),
        };
        assert!(by_message.is_already_paid());
    }

    #[test]
    fn decline_reason_prefers_code_then_decline_code_then_kind() {
        let coded = ApiError {
            kind: Some("card_error".into()),
            code: Some("card_declined".into()),
            decline_code: Some("insufficient_funds".into()),
            message: None,
        };
        assert_eq!(coded.decline_reason(), "card_declined");
        let decline_only = ApiError {
            kind: Some("card_error".into()),
            code: None,
            decline_code: Some("insufficient_funds".into()),
            message: None,
        };
        assert_eq!(decline_only.decline_reason(), "insufficient_funds");
        let bare_card_error = ApiError {
            kind: Some("card_error".into()),
            code: None,
            decline_code: None,
            message: None,
        };
        assert_eq!(bare_card_error.decline_reason(), "card_declined");
        let nothing = ApiError {
            kind: None,
            code: None,
            decline_code: None,
            message: None,
        };
        assert_eq!(nothing.decline_reason(), "declined");
    }
}
