// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
//! Plugin routes, mounted under `config.route_prefix`.
//!
//! | Method | Path             | Purpose                                   |
//! |--------|------------------|-------------------------------------------|
//! | POST   | `/checkout`      | Start hosted checkout for a plan          |
//! | POST   | `/portal`        | Open the hosted billing portal            |
//! | GET    | `/subscription`  | Current subscription + plan (mirror only) |
//! | POST   | `/webhook`       | Signed provider webhook receiver          |
//!
//! `checkout` and `portal` answer `303 See Other` for a browser and JSON
//! `{ "url", "id" }` when the `Accept` header prefers `application/json`.
//! Redirect URLs come from the config only. Body fields with the same names
//! are ignored.

use std::sync::Arc;

use autumn_web::negotiate::{Format, Negotiate};
use autumn_web::reexports::axum::body::Bytes;
use autumn_web::reexports::axum::extract::{
    FromRequest, FromRequestParts, OriginalUri, Request, State,
};
use autumn_web::reexports::axum::response::{IntoResponse, Redirect, Response};
use autumn_web::reexports::axum::routing::{get, post};
use autumn_web::reexports::axum::{Json, Router};
use autumn_web::reexports::http::request::Parts;
use autumn_web::reexports::http::{HeaderMap, header};
use autumn_web::route_listing::{RouteClassification, RouteInfo, RouteSource};
use autumn_web::webhook::SignedWebhook;
use autumn_web::{AppState, AutumnError, AutumnResult};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::BillingService;
use crate::config::BillingConfig;
use crate::error::BillingError;
use crate::gate::{Billing, session_user_id};
use crate::model::{Customer, Subscription};
use crate::plan::{Plan, PlanId};
use crate::provider::{CheckoutRequest, CustomerRequest, HostedSession, PortalRequest};
use crate::reconcile::{self, ReconcileOutcome};
use crate::store::CustomerUpsert;

/// The plugin router (paths relative to the prefix). Every option the
/// routes read comes from the service on `AppState`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/checkout", post(checkout))
        .route("/portal", post(portal))
        .route("/subscription", get(subscription))
        .route("/webhook", post(webhook))
}

/// Route listing for `autumn routes` and the audit gate.
#[must_use]
pub fn route_infos(config: &BillingConfig) -> Vec<RouteInfo> {
    let prefix = config.route_prefix.trim_end_matches('/');
    let info =
        |method: &str, path: &str, handler: &str, classification: RouteClassification| RouteInfo {
            method: method.to_owned(),
            path: format!("{prefix}{path}"),
            handler: format!("autumn_billing::routes::{handler}"),
            source: RouteSource::Plugin("autumn-billing".to_owned()),
            classification,
            ..Default::default()
        };
    vec![
        info("POST", "/checkout", "checkout", RouteClassification::Gated),
        info("POST", "/portal", "portal", RouteClassification::Gated),
        info(
            "GET",
            "/subscription",
            "subscription",
            RouteClassification::Gated,
        ),
        info("POST", "/webhook", "webhook", RouteClassification::Public),
    ]
}

/// The session user id. Rejects with 401 before the body is read.
struct SessionUser(String);

impl FromRequestParts<AppState> for SessionUser {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        session_user_id(parts, state)
            .await
            .map(Self)
            .map_err(BillingError::into_autumn)
    }
}

/// [`SignedWebhook`] under a nested router.
///
/// `nest` strips the prefix from the request URI, but the webhook registry
/// matches the full path. Restore axum's `OriginalUri` first.
struct NestedWebhook(SignedWebhook);

impl FromRequest<AppState> for NestedWebhook {
    type Rejection = AutumnError;

    async fn from_request(mut req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(OriginalUri(uri)) = req.extensions().get::<OriginalUri>().cloned() {
            *req.uri_mut() = uri;
        }
        SignedWebhook::from_request(req, state).await.map(Self)
    }
}

/// `POST /checkout` body. Extra fields are ignored.
#[derive(Debug, Deserialize)]
struct CheckoutBody {
    plan: String,
}

/// `GET /subscription` response.
#[derive(Debug, Serialize)]
struct SubscriptionResponse {
    subscription: Option<Subscription>,
    plan: Option<Plan>,
    entitled: bool,
}

/// Decode a JSON or form-encoded body.
fn parse_body<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, AutumnError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("application/json")
    {
        serde_json::from_slice(body)
            .map_err(|e| AutumnError::bad_request_msg(format!("invalid JSON body: {e}")))
    } else {
        serde_urlencoded::from_bytes(body)
            .map_err(|e| AutumnError::bad_request_msg(format!("invalid form body: {e}")))
    }
}

/// 303 for a browser, JSON for an API client.
fn hosted_response(negotiate: &Negotiate, session: &HostedSession) -> Response {
    match negotiate.format() {
        Format::Json => Json(json!({ "url": session.url, "id": session.id })).into_response(),
        Format::Html => Redirect::to(&session.url).into_response(),
    }
}

/// The customer linked to `user_id`, created at the provider when missing.
async fn customer_for(
    state: &AppState,
    service: &BillingService,
    user_id: &str,
) -> Result<Customer, BillingError> {
    let store = service.store();
    if let Some(customer) = store.customer_by_user(user_id).await? {
        return Ok(customer);
    }
    let provider = service.provider();
    let local_id = state.entropy().uuid_v4().to_string();
    let provider_customer_id = provider
        .create_customer(CustomerRequest::new(local_id.clone(), user_id))
        .await?;
    let now = state.clock().now();
    store
        .upsert_customer(
            CustomerUpsert::new(local_id, provider.name(), provider_customer_id, now)
                .with_user(user_id),
        )
        .await
}

/// `true` when the customer has a subscription the provider still bills.
async fn has_live_subscription(
    service: &BillingService,
    user_id: &str,
) -> Result<bool, BillingError> {
    let store = service.store();
    let Some(customer) = store.customer_by_user(user_id).await? else {
        return Ok(false);
    };
    let rows = store.subscriptions_for_customer(&customer.id).await?;
    Ok(rows.iter().any(|row| row.status.is_live()))
}

/// `POST {prefix}/checkout` — start hosted checkout for `plan`.
async fn checkout(
    State(state): State<AppState>,
    SessionUser(user_id): SessionUser,
    negotiate: Negotiate,
    headers: HeaderMap,
    body: Bytes,
) -> AutumnResult<Response> {
    let service = BillingService::require(&state)?;
    let body: CheckoutBody = parse_body(&headers, &body)?;
    let plan = service
        .catalog()
        .get(&PlanId::new(body.plan))
        .cloned()
        .ok_or_else(|| BillingError::NotFound("plan").into_autumn())?;
    if has_live_subscription(&service, &user_id)
        .await
        .map_err(BillingError::into_autumn)?
    {
        return Err(
            BillingError::Conflict("the user already has a live subscription".to_owned())
                .into_autumn(),
        );
    }
    let customer = customer_for(&state, &service, &user_id)
        .await
        .map_err(BillingError::into_autumn)?;
    let config = service.config();
    let request = CheckoutRequest::new(
        customer.provider_customer_id,
        customer.id,
        plan.provider_price_id,
        config.success_url.clone(),
        config.cancel_url.clone(),
    );
    let session = service
        .provider()
        .create_checkout(request)
        .await
        .map_err(BillingError::into_autumn)?;
    Ok(hosted_response(&negotiate, &session))
}

/// `POST {prefix}/portal` — open the hosted billing portal.
async fn portal(
    State(state): State<AppState>,
    SessionUser(user_id): SessionUser,
    negotiate: Negotiate,
) -> AutumnResult<Response> {
    let service = BillingService::require(&state)?;
    let customer = service
        .store()
        .customer_by_user(&user_id)
        .await
        .map_err(BillingError::into_autumn)?
        .ok_or_else(|| BillingError::NotFound("customer").into_autumn())?;
    let request = PortalRequest::new(
        customer.provider_customer_id,
        service.config().portal_return_url.clone(),
    );
    let session = service
        .provider()
        .create_portal(request)
        .await
        .map_err(BillingError::into_autumn)?;
    Ok(hosted_response(&negotiate, &session))
}

/// `GET {prefix}/subscription` — the mirror view. Never calls the provider.
async fn subscription(
    State(state): State<AppState>,
    SessionUser(user_id): SessionUser,
) -> AutumnResult<Json<SubscriptionResponse>> {
    let billing = Billing::from_state(&state)
        .ok_or_else(|| AutumnError::service_unavailable_msg("billing plugin is not started"))?;
    let view = billing
        .current_subscription(&user_id)
        .await
        .map_err(BillingError::into_autumn)?;
    let response = view.map_or(
        SubscriptionResponse {
            subscription: None,
            plan: None,
            entitled: false,
        },
        |view| SubscriptionResponse {
            subscription: Some(view.subscription),
            plan: view.plan,
            entitled: view.entitled,
        },
    );
    Ok(Json(response))
}

/// `POST {prefix}/webhook` — verified provider events.
///
/// A body the provider cannot decode answers `500`, so the replay key is
/// released and the provider redelivers. The webhook extractor must stay the
/// last argument: it consumes the body.
async fn webhook(
    State(state): State<AppState>,
    NestedWebhook(webhook): NestedWebhook,
) -> AutumnResult<Json<Value>> {
    let service: Arc<BillingService> = BillingService::require(&state)?;
    let event = match service.provider().parse_event(webhook.raw_body()) {
        Ok(event) => event,
        Err(BillingError::Malformed(message)) => {
            return Err(AutumnError::internal_server_error_msg(format!(
                "malformed billing event: {message}"
            )));
        }
        Err(other) => return Err(other.into_autumn()),
    };
    let event_id = event.id.clone();
    let outcome = reconcile::apply(&state, &service, event)
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
    let label = match outcome {
        ReconcileOutcome::Applied { .. } => "applied",
        ReconcileOutcome::Duplicate => "duplicate",
        ReconcileOutcome::Ignored { .. } => "ignored",
    };
    Ok(Json(json!({
        "accepted": true,
        "outcome": label,
        "event_id": event_id,
    })))
}
