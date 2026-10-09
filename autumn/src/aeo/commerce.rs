//! Agent commerce: x402 payments, MPP discovery, UCP and ACP documents.
//!
//! | Protocol | What Autumn does |
//! |---|---|
//! | x402 | Priced routes answer `402` with `PAYMENT-REQUIRED`. A retry with `PAYMENT-SIGNATURE` is verified and settled by the facilitator. |
//! | MPP | Priced routes with an `mpp_method` get `x-payment-info` in `/openapi.json`. The app runs the MPP payment itself. |
//! | UCP | [`UcpProfile`] is served at `/.well-known/ucp`. |
//! | ACP | [`AcpDiscovery`] is served at `/.well-known/acp.json`. |
//!
//! ```toml
//! [aeo.x402]
//! facilitator_url = "https://x402.org/facilitator"
//! pay_to = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C"
//! network = "eip155:84532"
//! asset = "0x036CbD53842c5426634e7929541eC2318f3dCF7e"
//!
//! [[aeo.paid_routes]]
//! method = "GET"
//! path = "/api"
//! amount = "10000"
//! description = "Premium data"
//! ```

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

/// Path of the UCP profile.
pub const UCP_PATH: &str = "/.well-known/ucp";
/// Path of the ACP discovery document.
pub const ACP_PATH: &str = "/.well-known/acp.json";

/// `[aeo.x402]`: where and how to get paid.
#[derive(Debug, Clone, Default, Deserialize)]
#[non_exhaustive]
pub struct X402Config {
    /// Facilitator base URL. It verifies and settles payments.
    #[serde(default)]
    pub facilitator_url: Option<String>,
    /// Wallet address that receives payments.
    #[serde(default)]
    pub pay_to: Option<String>,
    /// CAIP-2 network, e.g. `eip155:8453` (Base).
    #[serde(default)]
    pub network: Option<String>,
    /// Token contract address, e.g. USDC.
    #[serde(default)]
    pub asset: Option<String>,
    /// Token name for the EIP-712 domain. Default: `USDC`.
    #[serde(default)]
    pub asset_name: Option<String>,
    /// Token version for the EIP-712 domain. Default: `2`.
    #[serde(default)]
    pub asset_version: Option<String>,
    /// Payment scheme. Default: `exact`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Seconds the client has to pay. Default: 300.
    #[serde(default)]
    pub max_timeout_secs: Option<u64>,
}

/// `[[aeo.paid_routes]]`: one priced route.
#[derive(Debug, Clone, Default, Deserialize)]
#[non_exhaustive]
pub struct PaidRoute {
    /// HTTP method, e.g. `GET`.
    pub method: String,
    /// Route path. `{name}` matches one segment.
    pub path: String,
    /// Price in the asset's smallest unit, as digits.
    pub amount: String,
    /// What the payment buys.
    #[serde(default)]
    pub description: Option<String>,
    /// MPP payment method (`stripe`, `tempo`, `lightning`, `card`). When set,
    /// `/openapi.json` carries `x-payment-info` for this route.
    #[serde(default)]
    pub mpp_method: Option<String>,
    /// MPP currency, e.g. `usd`.
    #[serde(default)]
    pub mpp_currency: Option<String>,
    /// MPP amount in the smallest unit. Default: `amount`.
    #[serde(default)]
    pub mpp_amount: Option<String>,
}

impl PaidRoute {
    /// `true` when `method` and `path` match this route.
    #[must_use]
    pub fn matches(&self, method: &str, path: &str) -> bool {
        if !self.method.eq_ignore_ascii_case(method) {
            return false;
        }
        let mut want = self.path.split('/');
        let mut got = path.split('/');
        loop {
            match (want.next(), got.next()) {
                (None, None) => return true,
                (Some(w), Some(g)) => {
                    let is_param = w.starts_with('{') && w.ends_with('}');
                    if (is_param && g.is_empty()) || (!is_param && w != g) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }
}

impl X402Config {
    /// `true` when every field the 402 challenge needs is set.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        [
            &self.facilitator_url,
            &self.pay_to,
            &self.network,
            &self.asset,
        ]
        .iter()
        .all(|v| v.as_deref().is_some_and(|v| !v.trim().is_empty()))
    }

    /// The x402 v2 `PaymentRequirements` for `route`.
    #[must_use]
    pub fn requirements(&self, route: &PaidRoute) -> Value {
        json!({
            "scheme": self.scheme.as_deref().unwrap_or("exact"),
            "network": self.network.as_deref().unwrap_or_default(),
            "amount": route.amount,
            "asset": self.asset.as_deref().unwrap_or_default(),
            "payTo": self.pay_to.as_deref().unwrap_or_default(),
            "maxTimeoutSeconds": self.max_timeout_secs.unwrap_or(300),
            "extra": {
                "name": self.asset_name.as_deref().unwrap_or("USDC"),
                "version": self.asset_version.as_deref().unwrap_or("2"),
            },
        })
    }

    /// The x402 v2 `PaymentRequired` object for `route` at `resource_url`.
    #[must_use]
    pub fn payment_required(&self, route: &PaidRoute, resource_url: &str, error: &str) -> Value {
        json!({
            "x402Version": 2,
            "error": error,
            "resource": {
                "url": resource_url,
                "description": route.description.as_deref().unwrap_or_default(),
                "mimeType": "application/json",
            },
            "accepts": [self.requirements(route)],
        })
    }
}

/// Encode a JSON value for an x402 header: standard padded base64.
#[must_use]
pub fn encode_header(value: &Value) -> String {
    STANDARD.encode(value.to_string())
}

/// Decode an x402 header into JSON. `None` for bad base64 or JSON.
#[must_use]
pub fn decode_header(header: &str) -> Option<Value> {
    let bytes = STANDARD.decode(header.trim()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `true` when the client's `accepted` requirements are the ones offered.
#[must_use]
pub fn accepted_matches(accepted: &Value, offered: &Value) -> bool {
    ["scheme", "network", "amount", "asset", "payTo"]
        .iter()
        .all(|k| accepted.get(k).is_some() && accepted.get(k) == offered.get(k))
}

/// Add MPP `x-payment-info` and a `402` response to each priced operation
/// in an `OpenAPI` document.
pub fn apply_mpp(spec: &mut Value, routes: &[PaidRoute]) {
    for route in routes {
        let Some(method) = route.mpp_method.as_deref() else {
            continue;
        };
        let Some(op) = spec
            .get_mut("paths")
            .and_then(|p| p.get_mut(&route.path))
            .and_then(|p| p.get_mut(route.method.to_ascii_lowercase()))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let mut info = json!({
            "intent": "charge",
            "method": method,
            "amount": route.mpp_amount.as_deref().unwrap_or(&route.amount),
        });
        if let Some(currency) = &route.mpp_currency {
            info["currency"] = json!(currency);
        }
        if let Some(d) = &route.description {
            info["description"] = json!(d);
        }
        op.insert("x-payment-info".to_owned(), info);
        let responses = op.entry("responses").or_insert_with(|| json!({}));
        if let Some(responses) = responses.as_object_mut() {
            responses.insert(
                "402".to_owned(),
                json!({ "description": "Payment Required" }),
            );
        }
    }
}

/// An invalid commerce document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommerceError {
    /// A required field is missing or empty.
    #[error("{doc}: `{field}` is required")]
    Missing {
        /// Document name.
        doc: &'static str,
        /// Field path.
        field: &'static str,
    },
}

/// A UCP business profile, served at `/.well-known/ucp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UcpProfile(Value);

impl UcpProfile {
    /// Check the required UCP fields: `ucp.version`, `ucp.services`,
    /// `ucp.payment_handlers`.
    ///
    /// # Errors
    ///
    /// [`CommerceError::Missing`] for the first missing field.
    pub fn new(profile: Value) -> Result<Self, CommerceError> {
        let missing = |field| CommerceError::Missing {
            doc: "UCP profile",
            field,
        };
        let ucp = profile.get("ucp").ok_or_else(|| missing("ucp"))?;
        if ucp
            .get("version")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(missing("ucp.version"));
        }
        if !ucp.get("services").is_some_and(Value::is_object) {
            return Err(missing("ucp.services"));
        }
        if !ucp.get("payment_handlers").is_some_and(Value::is_object) {
            return Err(missing("ucp.payment_handlers"));
        }
        Ok(Self(profile))
    }

    /// The profile JSON.
    #[must_use]
    pub const fn as_json(&self) -> &Value {
        &self.0
    }
}

/// An ACP discovery document, served at `/.well-known/acp.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpDiscovery(Value);

impl AcpDiscovery {
    /// Check the required ACP fields: `protocol.name = "acp"`,
    /// `protocol.version`, `protocol.supported_versions`, an absolute
    /// `api_base_url`, `transports`, and `capabilities.services`.
    ///
    /// # Errors
    ///
    /// [`CommerceError::Missing`] for the first missing field.
    pub fn new(document: Value) -> Result<Self, CommerceError> {
        let missing = |field| CommerceError::Missing {
            doc: "ACP discovery",
            field,
        };
        let non_empty_str =
            |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        let non_empty_arr =
            |v: Option<&Value>| v.and_then(Value::as_array).is_some_and(|a| !a.is_empty());
        let protocol = document.get("protocol");
        if protocol.and_then(|p| p.get("name")).and_then(Value::as_str) != Some("acp") {
            return Err(missing("protocol.name"));
        }
        if !non_empty_str(protocol.and_then(|p| p.get("version"))) {
            return Err(missing("protocol.version"));
        }
        if !non_empty_arr(protocol.and_then(|p| p.get("supported_versions"))) {
            return Err(missing("protocol.supported_versions"));
        }
        let absolute = document
            .get("api_base_url")
            .and_then(Value::as_str)
            .and_then(|u| url::Url::parse(u).ok())
            .is_some_and(|u| matches!(u.scheme(), "http" | "https"));
        if !absolute {
            return Err(missing("api_base_url"));
        }
        if !non_empty_arr(document.get("transports")) {
            return Err(missing("transports"));
        }
        if !non_empty_arr(document.get("capabilities").and_then(|c| c.get("services"))) {
            return Err(missing("capabilities.services"));
        }
        Ok(Self(document))
    }

    /// The document JSON.
    #[must_use]
    pub const fn as_json(&self) -> &Value {
        &self.0
    }
}

// ── x402 layer ──────────────────────────────────────────────────────────────

/// Tower layer that charges for [`PaidRoute`]s with x402.
///
/// 1. A request without `PAYMENT-SIGNATURE` gets `402` and `PAYMENT-REQUIRED`.
/// 2. A request with it is verified by the facilitator (`POST /verify`).
/// 3. The handler runs. Only a `2xx` answer is settled (`POST /settle`).
/// 4. The response carries `PAYMENT-RESPONSE`. A failed settlement turns
///    into `402`, and the handler body is not sent.
#[cfg(feature = "http-client")]
#[derive(Clone)]
pub struct X402Layer {
    state: std::sync::Arc<X402State>,
}

#[cfg(feature = "http-client")]
struct X402State {
    config: X402Config,
    routes: Vec<PaidRoute>,
    client: crate::http_client::Client,
    base_url: Option<String>,
}

#[cfg(feature = "http-client")]
impl X402Layer {
    /// The layer for `[aeo.x402]` and `[[aeo.paid_routes]]`, or `None` when
    /// no route is priced or the x402 settings are incomplete.
    #[must_use]
    pub fn from_config(
        config: &crate::config::AutumnConfig,
        state: &crate::state::AppState,
    ) -> Option<Self> {
        let aeo = &config.aeo;
        if !aeo.enabled || aeo.paid_routes.is_empty() {
            return None;
        }
        if !aeo.x402.is_complete() {
            tracing::warn!(
                "aeo: [[aeo.paid_routes]] is set but [aeo.x402] needs facilitator_url, \
                 pay_to, network, and asset; x402 is off"
            );
            return None;
        }
        Some(Self {
            state: std::sync::Arc::new(X402State {
                config: aeo.x402.clone(),
                routes: aeo.paid_routes.clone(),
                client: crate::http_client::Client::from_state(state).named("x402"),
                base_url: config.seo.base_url.clone(),
            }),
        })
    }
}

#[cfg(feature = "http-client")]
impl<S> tower::Layer<S> for X402Layer {
    type Service = X402Service<S>;

    fn layer(&self, inner: S) -> Self::Service {
        X402Service {
            inner,
            state: std::sync::Arc::clone(&self.state),
        }
    }
}

/// Service made by [`X402Layer`].
#[cfg(feature = "http-client")]
#[derive(Clone)]
pub struct X402Service<S> {
    inner: S,
    state: std::sync::Arc<X402State>,
}

#[cfg(feature = "http-client")]
impl<S> tower::Service<axum::http::Request<axum::body::Body>> for X402Service<S>
where
    S: tower::Service<axum::http::Request<axum::body::Body>, Response = axum::response::Response>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = futures::future::Either<
        S::Future,
        std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<axum::response::Response, S::Error>> + Send,
            >,
        >,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        // An unpriced request costs no clone and no box.
        let Some(route) = self
            .state
            .routes
            .iter()
            .find(|r| r.matches(req.method().as_str(), req.uri().path()))
            .cloned()
        else {
            return futures::future::Either::Left(self.inner.call(req));
        };
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let state = std::sync::Arc::clone(&self.state);
        futures::future::Either::Right(Box::pin(async move {
            let resource = resource_url(&state, &req);
            let offered = state.config.requirements(&route);
            let Some(header) = req
                .headers()
                .get("payment-signature")
                .and_then(|v| v.to_str().ok())
            else {
                return Ok(payment_required(
                    &state,
                    &route,
                    &resource,
                    "Payment required",
                ));
            };
            let Some(payload) = decode_header(header) else {
                return Ok(plain(
                    axum::http::StatusCode::BAD_REQUEST,
                    "malformed PAYMENT-SIGNATURE",
                ));
            };
            if !payload
                .get("accepted")
                .is_some_and(|a| accepted_matches(a, &offered))
            {
                return Ok(payment_required(
                    &state,
                    &route,
                    &resource,
                    "payment does not match the requirements",
                ));
            }
            let body = json!({
                "x402Version": 2,
                "paymentPayload": payload,
                "paymentRequirements": offered,
            });
            let verify = match facilitator(&state, "verify", &body).await {
                Ok(v) => v,
                Err(status) => return Ok(plain(status, "payment facilitator unavailable")),
            };
            if verify.get("isValid").and_then(Value::as_bool) != Some(true) {
                let reason = verify
                    .get("invalidReason")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid payment")
                    .to_owned();
                return Ok(payment_required(&state, &route, &resource, &reason));
            }

            let res = inner.call(req).await?;
            if !res.status().is_success() {
                return Ok(res);
            }
            let settled = match facilitator(&state, "settle", &body).await {
                Ok(v) => v,
                Err(status) => return Ok(plain(status, "payment facilitator unavailable")),
            };
            let header = axum::http::HeaderValue::from_str(&encode_header(&settled)).ok();
            if settled.get("success").and_then(Value::as_bool) != Some(true) {
                let mut failed = payment_required(&state, &route, &resource, "settlement failed");
                if let Some(h) = header {
                    failed.headers_mut().insert("payment-response", h);
                }
                return Ok(failed);
            }
            let mut res = res;
            if let Some(h) = header {
                res.headers_mut().insert("payment-response", h);
            }
            Ok(res)
        }))
    }
}

#[cfg(feature = "http-client")]
fn resource_url(state: &X402State, req: &axum::http::Request<axum::body::Body>) -> String {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let origin = super::documents::Origin::resolve(state.base_url.as_deref(), host);
    let path = req
        .uri()
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    format!("{}{path}", origin.base)
}

#[cfg(feature = "http-client")]
fn payment_required(
    state: &X402State,
    route: &PaidRoute,
    resource: &str,
    error: &str,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let required = state.config.payment_required(route, resource, error);
    let mut res = (
        axum::http::StatusCode::PAYMENT_REQUIRED,
        axum::Json(required.clone()),
    )
        .into_response();
    let headers = res.headers_mut();
    if let Ok(v) = axum::http::HeaderValue::from_str(&encode_header(&required)) {
        headers.insert("payment-required", v);
    }
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

#[cfg(feature = "http-client")]
fn plain(status: axum::http::StatusCode, msg: &'static str) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (status, msg).into_response()
}

/// POST `body` to `<facilitator>/<op>`. A transport error or a non-JSON
/// answer is a `502`.
#[cfg(feature = "http-client")]
async fn facilitator(
    state: &X402State,
    op: &str,
    body: &Value,
) -> Result<Value, axum::http::StatusCode> {
    let base = state
        .config
        .facilitator_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let res = state
        .client
        .post(format!("{base}/{op}"))
        .json(body)
        .send()
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, op, "aeo: x402 facilitator call failed");
            axum::http::StatusCode::BAD_GATEWAY
        })?;
    res.json::<Value>().map_err(|err| {
        tracing::warn!(error = %err, op, "aeo: x402 facilitator sent no JSON");
        axum::http::StatusCode::BAD_GATEWAY
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x402() -> X402Config {
        X402Config {
            facilitator_url: Some("https://facilitator.example".to_owned()),
            pay_to: Some("0xabc".to_owned()),
            network: Some("eip155:84532".to_owned()),
            asset: Some("0xusdc".to_owned()),
            ..X402Config::default()
        }
    }

    fn route() -> PaidRoute {
        PaidRoute {
            method: "GET".to_owned(),
            path: "/api/reports/{id}".to_owned(),
            amount: "10000".to_owned(),
            description: Some("A report".to_owned()),
            mpp_method: Some("stripe".to_owned()),
            mpp_currency: Some("usd".to_owned()),
            ..PaidRoute::default()
        }
    }

    #[test]
    fn route_matching() {
        let r = route();
        assert!(r.matches("GET", "/api/reports/7"));
        assert!(r.matches("get", "/api/reports/7"));
        assert!(!r.matches("POST", "/api/reports/7"));
        assert!(!r.matches("GET", "/api/reports"));
        assert!(!r.matches("GET", "/api/reports/7/x"));
        assert!(!r.matches("GET", "/api/reports/"));
    }

    #[test]
    fn config_completeness() {
        assert!(x402().is_complete());
        assert!(!X402Config::default().is_complete());
    }

    #[test]
    fn payment_required_is_x402_v2() {
        let pr = x402().payment_required(
            &route(),
            "https://shop.example/api/reports/7",
            "Payment required",
        );
        assert_eq!(pr["x402Version"], 2);
        assert_eq!(pr["error"], "Payment required");
        assert_eq!(pr["resource"]["url"], "https://shop.example/api/reports/7");
        assert_eq!(pr["resource"]["description"], "A report");
        let req = &pr["accepts"][0];
        assert_eq!(req["scheme"], "exact");
        assert_eq!(req["network"], "eip155:84532");
        assert_eq!(req["amount"], "10000");
        assert_eq!(req["asset"], "0xusdc");
        assert_eq!(req["payTo"], "0xabc");
        assert_eq!(req["maxTimeoutSeconds"], 300);
        assert_eq!(req["extra"], json!({"name": "USDC", "version": "2"}));
    }

    #[test]
    fn header_round_trip_uses_padded_standard_base64() {
        let v = json!({"a": "?>"});
        let h = encode_header(&v);
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)),
            "{h}"
        );
        assert_eq!(decode_header(&h), Some(v));
        assert_eq!(decode_header("not base64!"), None);
        assert_eq!(decode_header(&STANDARD.encode("not json")), None);
    }

    #[test]
    fn accepted_must_match_the_offer() {
        let offered = x402().requirements(&route());
        assert!(accepted_matches(&offered.clone(), &offered));
        let mut cheaper = offered.clone();
        cheaper["amount"] = json!("1");
        assert!(!accepted_matches(&cheaper, &offered));
        let mut other_wallet = offered.clone();
        other_wallet["payTo"] = json!("0xevil");
        assert!(!accepted_matches(&other_wallet, &offered));
        assert!(!accepted_matches(&Value::Null, &offered));
    }

    #[test]
    fn mpp_extends_matching_operations() {
        let mut spec = json!({
            "openapi": "3.1.0",
            "paths": {
                "/api/reports/{id}": { "get": { "responses": { "200": { "description": "OK" } } } },
                "/api/free": { "get": { "responses": {} } }
            }
        });
        let mut no_mpp = route();
        no_mpp.path = "/api/free".to_owned();
        no_mpp.mpp_method = None;
        apply_mpp(&mut spec, &[route(), no_mpp]);
        let op = &spec["paths"]["/api/reports/{id}"]["get"];
        assert_eq!(
            op["x-payment-info"],
            json!({"intent": "charge", "method": "stripe", "amount": "10000", "currency": "usd", "description": "A report"})
        );
        assert_eq!(op["responses"]["402"]["description"], "Payment Required");
        assert_eq!(op["responses"]["200"]["description"], "OK");
        assert!(
            spec["paths"]["/api/free"]["get"]
                .get("x-payment-info")
                .is_none()
        );
    }

    #[test]
    fn ucp_requires_core_fields() {
        let ok = json!({"ucp": {"version": "2026-08-25", "services": {}, "payment_handlers": {}}});
        assert!(UcpProfile::new(ok).is_ok());
        let err =
            UcpProfile::new(json!({"ucp": {"version": "2026-08-25", "services": {}}})).unwrap_err();
        assert_eq!(
            err,
            CommerceError::Missing {
                doc: "UCP profile",
                field: "ucp.payment_handlers"
            }
        );
        assert!(UcpProfile::new(json!({})).is_err());
    }

    #[test]
    fn acp_requires_core_fields() {
        let ok = json!({
            "protocol": {"name": "acp", "version": "2025-09-29", "supported_versions": ["2025-09-29"]},
            "api_base_url": "https://shop.example/api",
            "transports": ["rest"],
            "capabilities": {"services": ["checkout"]}
        });
        assert!(AcpDiscovery::new(ok.clone()).is_ok());
        for (field, patch) in [
            (
                "protocol.name",
                json!({"protocol": {"name": "x", "version": "1", "supported_versions": ["1"]}}),
            ),
            ("api_base_url", json!({"api_base_url": "/relative"})),
            ("transports", json!({"transports": []})),
            (
                "capabilities.services",
                json!({"capabilities": {"services": []}}),
            ),
        ] {
            let mut doc = ok.clone();
            for (k, v) in patch.as_object().unwrap() {
                doc[k] = v.clone();
            }
            assert!(
                matches!(AcpDiscovery::new(doc), Err(CommerceError::Missing { field: f, .. }) if f == field),
                "{field}"
            );
        }
    }
}
