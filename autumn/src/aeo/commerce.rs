//! Agent commerce: x402 payments, MPP discovery, UCP and ACP documents.
//!
//! | Protocol | What Autumn does |
//! |---|---|
//! | x402 | Priced routes answer `402` with `PAYMENT-REQUIRED`. The facilitator verifies and settles a retry that has `PAYMENT-SIGNATURE`. |
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
#[cfg(feature = "http-client")]
use sha2::Digest as _;

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
    /// A priced route. `amount` is in the asset's smallest unit.
    #[must_use]
    pub fn new(
        method: impl Into<String>,
        path: impl Into<String>,
        amount: impl Into<String>,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            amount: amount.into(),
            ..Self::default()
        }
    }

    /// `true` when `method` and `path` match this route.
    ///
    /// `HEAD` matches a `GET` route (axum runs the `GET` handler for it). One
    /// trailing `/` is ignored, as the static page layer does. `{name}`
    /// matches one segment; `{*name}` matches the rest of the path.
    #[must_use]
    pub fn matches(&self, method: &str, path: &str) -> bool {
        self.method_matches(method) && template_matches(&self.path, path)
    }

    fn method_matches(&self, method: &str) -> bool {
        self.method.eq_ignore_ascii_case(method)
            || (method.eq_ignore_ascii_case("HEAD") && self.method.eq_ignore_ascii_case("GET"))
    }

    /// `true` for a route x402 charges: an MPP route is paid through the
    /// app's own MPP flow instead.
    #[must_use]
    pub const fn is_x402(&self) -> bool {
        self.mpp_method.is_none()
    }
}

/// Strip one trailing `/` (but keep `/`).
fn normalize(path: &str) -> &str {
    if path.len() > 1 {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    }
}

/// Match a concrete `path` against a route `template`.
fn template_matches(template: &str, path: &str) -> bool {
    let mut want = normalize(template).split('/');
    let mut got = normalize(path).split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some(w), Some(g)) => {
                if w.starts_with("{*") && w.ends_with('}') {
                    return !g.is_empty();
                }
                let is_param = w.starts_with('{') && w.ends_with('}');
                if (is_param && g.is_empty()) || (!is_param && w != g) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// `true` for an amount in the smallest unit: ASCII digits, no leading zero.
#[must_use]
pub fn valid_amount(amount: &str) -> bool {
    !amount.is_empty()
        && amount.bytes().all(|b| b.is_ascii_digit())
        && (amount == "0" || !amount.starts_with('0'))
}

/// Why `route` can never match a request, if it cannot.
///
/// That is a method that is not an HTTP method, or a path that does not
/// start with `/`. Such an x402 route would serve its handler free, so the
/// config is refused at startup.
#[must_use]
pub fn unmatchable_route(route: &PaidRoute) -> Option<String> {
    const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
    // Matching compares the method as written, so no trimming here.
    if !METHODS
        .iter()
        .any(|m| m.eq_ignore_ascii_case(&route.method))
    {
        return Some(format!(
            "{:?} {}: method must be one of {}",
            route.method,
            route.path,
            METHODS.join(", ")
        ));
    }
    // Matching reads only the request path: a query or a fragment in the
    // template never matches.
    if !route.path.starts_with('/')
        || route
            .path
            .chars()
            .any(|c| c.is_whitespace() || c == '?' || c == '#')
    {
        return Some(format!(
            "{} {:?}: path must start with `/`, with no spaces, query, or fragment",
            route.method, route.path
        ));
    }
    None
}

/// Problems in `[[aeo.paid_routes]]`, one message each. A priced x402 route
/// with a problem answers `503`.
#[must_use]
pub fn paid_route_problems(routes: &[PaidRoute]) -> Vec<String> {
    let mut problems = Vec::new();
    for r in routes {
        problems.extend(unmatchable_route(r));
        if !valid_amount(&r.amount) {
            problems.push(format!(
                "{} {}: amount {:?} is not digits",
                r.method, r.path, r.amount
            ));
        }
        if let Some(m) = &r.mpp_method {
            if m.is_empty() || m.bytes().any(|b| !(b.is_ascii_lowercase() || b == b'-')) {
                problems.push(format!(
                    "{} {}: mpp_method {m:?} must be lowercase",
                    r.method, r.path
                ));
            }
            if r.mpp_amount.as_deref().is_some_and(|a| !valid_amount(a)) {
                problems.push(format!("{} {}: mpp_amount is not digits", r.method, r.path));
            }
        }
    }
    problems
}

fn route_is_valid(r: &PaidRoute) -> bool {
    paid_route_problems(std::slice::from_ref(r)).is_empty()
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
#[cfg_attr(not(feature = "http-client"), allow(dead_code))]
pub(crate) fn accepted_matches(accepted: &Value, offered: &Value) -> bool {
    ["scheme", "network", "amount", "asset", "payTo"]
        .iter()
        .all(|k| accepted.get(k).is_some() && accepted.get(k) == offered.get(k))
}

/// Add MPP `x-payment-info` and a `402` response to each priced operation
/// in an `OpenAPI` document.
pub(crate) fn apply_mpp(spec: &mut Value, routes: &[PaidRoute]) {
    for route in routes.iter().filter(|r| route_is_valid(r)) {
        let Some(method) = route.mpp_method.as_deref() else {
            continue;
        };
        // Match the operation as `PaidRoute` matches a request: one trailing
        // `/` does not count.
        let Some(op) = spec
            .get_mut("paths")
            .and_then(Value::as_object_mut)
            .and_then(|paths| {
                paths
                    .iter_mut()
                    .find(|(k, _)| normalize(k) == normalize(&route.path))
                    .map(|(_, v)| v)
            })
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

/// The `OpenAPI` JSON for `spec`, with MPP `x-payment-info` when a priced
/// route has an `mpp_method`. A spec with no MPP route keeps its key order
/// (`serde_json` sorts the keys of a `Value` map).
///
/// # Errors
///
/// A `serde_json` error when the spec does not serialize.
pub fn spec_json_with_mpp(
    spec: &impl serde::Serialize,
    config: &crate::config::AutumnConfig,
) -> serde_json::Result<String> {
    if !has_mpp(config) {
        return serde_json::to_string_pretty(spec);
    }
    let mut value = serde_json::to_value(spec)?;
    apply_mpp(&mut value, &config.aeo.paid_routes);
    serde_json::to_string_pretty(&value)
}

/// Rewrite `dist/openapi.json` and `dist/openapi.yaml` with MPP
/// `x-payment-info`, when a priced route has an `mpp_method`.
///
/// # Errors
///
/// An I/O error when a file cannot be written.
#[cfg(feature = "openapi")]
pub fn write_mpp_spec(
    spec: &crate::openapi::OpenApiSpec,
    config: &crate::config::AutumnConfig,
    dist_dir: &std::path::Path,
) -> std::io::Result<()> {
    if !has_mpp(config) {
        return Ok(());
    }
    let mut value = serde_json::to_value(spec).map_err(std::io::Error::other)?;
    apply_mpp(&mut value, &config.aeo.paid_routes);
    let json = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
    std::fs::write(dist_dir.join("openapi.json"), json)?;
    let yaml = serde_yaml::to_string(&value).map_err(std::io::Error::other)?;
    std::fs::write(dist_dir.join("openapi.yaml"), yaml)
}

fn has_mpp(config: &crate::config::AutumnConfig) -> bool {
    config.aeo.enabled
        && config
            .aeo
            .paid_routes
            .iter()
            .any(|r| r.mpp_method.is_some())
}

/// An invalid commerce document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommerceError {
    /// A required field is missing, empty, or has a wrong value.
    #[error("{doc}: `{field}` is missing or not valid")]
    Invalid {
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
    /// [`CommerceError::Invalid`] for the first missing field.
    pub fn new(profile: Value) -> Result<Self, CommerceError> {
        let missing = |field| CommerceError::Invalid {
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
    /// [`CommerceError::Invalid`] for the first missing field.
    pub fn new(document: Value) -> Result<Self, CommerceError> {
        let missing = |field| CommerceError::Invalid {
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
/// 2. The facilitator verifies a request that has it (`POST /verify`).
/// 3. `GET` and `HEAD`: the handler runs, then the facilitator settles a
///    `2xx` answer. Other methods: the facilitator settles first, then the
///    handler runs.
/// 4. The response carries `PAYMENT-RESPONSE` and `Cache-Control: private,
///    no-store`. If settlement fails, Autumn sends `402` and no handler body.
///
/// One payment header works once per process: a second use gets `402`.
#[cfg(feature = "http-client")]
#[derive(Clone)]
pub(crate) struct X402Layer {
    state: std::sync::Arc<X402State>,
}

#[cfg(feature = "http-client")]
struct X402State {
    config: X402Config,
    priced: PricedRoutes,
    /// `false` when `[aeo.x402]` cannot work as written: priced routes then
    /// answer `503`, never free.
    available: bool,
    client: crate::http_client::Client,
    base_url: Option<String>,
    /// SHA-256 of each payment header already used.
    used: std::sync::Mutex<lru::LruCache<[u8; 32], ()>>,
}

/// Marks a request that one x402 layer already handled, so the second copy
/// (SSG path: one outside the static layer, one inside) does not charge
/// again.
#[cfg(feature = "http-client")]
#[derive(Debug, Clone, Copy)]
struct X402Handled;

/// The valid x402 routes, and the locale prefixes to strip.
#[derive(Debug, Clone)]
struct PricedRoutes {
    routes: Vec<PaidRoute>,
    /// Supported locales when locale-prefixed routing is on: `/en/x` is the
    /// route `/x`.
    locales: Vec<String>,
}

impl PricedRoutes {
    /// The priced routes, or `None` when AEO is off or no route is priced.
    fn from_config(config: &crate::config::AutumnConfig) -> Option<Self> {
        let aeo = &config.aeo;
        let routes: Vec<PaidRoute> = aeo
            .paid_routes
            .iter()
            // An invalid x402 route stays matched, so it answers 503 and
            // never runs free.
            .filter(|r| r.is_x402())
            .cloned()
            .collect();
        if !aeo.enabled || routes.is_empty() {
            return None;
        }
        #[cfg(feature = "i18n")]
        let locales = if config.i18n.locale_prefix_enabled {
            crate::router::validated_locale_prefix_locales(&config.i18n)
        } else {
            Vec::new()
        };
        #[cfg(not(feature = "i18n"))]
        let locales = Vec::new();
        Some(Self { routes, locales })
    }

    /// The priced route for `req`, if any. Uses the route template axum
    /// matched when it is known.
    fn route_for(&self, req: &axum::http::Request<axum::body::Body>) -> Option<&PaidRoute> {
        let method = req.method().as_str();
        let template = req
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .map(axum::extract::MatchedPath::as_str);
        let path = req.uri().path();
        // A route outside the locale nests (a scoped group, an excluded
        // path) can itself start with a locale segment, so the path is
        // matched as it is and with the locale stripped. Both cannot name two
        // different routes: a localized `/x` is mounted at `/{locale}/x`.
        let same = |r: &PaidRoute, t: &str| {
            normalize(&r.path) == normalize(t)
                || normalize(&r.path) == normalize(self.strip_locale(t))
        };
        self.routes.iter().find(|r| {
            r.method_matches(method)
                && (template.is_some_and(|t| same(r, t))
                    || template_matches(&r.path, path)
                    || template_matches(&r.path, self.strip_locale(path)))
        })
    }

    fn strip_locale<'a>(&self, path: &'a str) -> &'a str {
        for locale in &self.locales {
            if let Some(rest) = path
                .strip_prefix('/')
                .and_then(|p| p.strip_prefix(locale.as_str()))
            {
                if rest.is_empty() {
                    return "/";
                }
                if rest.starts_with('/') {
                    return rest;
                }
            }
        }
        path
    }
}

#[cfg(feature = "http-client")]
impl X402Layer {
    /// The layer for `[aeo.x402]` and `[[aeo.paid_routes]]`, or `None` when
    /// no route is priced. With incomplete or unsafe x402 settings, priced
    /// routes answer `503`.
    #[must_use]
    pub(crate) fn from_config(
        config: &crate::config::AutumnConfig,
        state: &crate::state::AppState,
    ) -> Option<Self> {
        let aeo = &config.aeo;
        let priced = PricedRoutes::from_config(config)?;
        let complete = aeo.x402.is_complete();
        let safe = facilitator_url_is_safe(aeo.x402.facilitator_url.as_deref().unwrap_or_default());
        if !complete {
            tracing::error!(
                "aeo: [[aeo.paid_routes]] has x402 routes but [aeo.x402] needs \
                 facilitator_url, pay_to, network, and asset; those routes answer 503"
            );
        } else if !safe {
            tracing::error!(
                "aeo: [aeo.x402] facilitator_url must be https (http only for localhost); \
                 priced routes answer 503"
            );
        }
        Some(Self {
            state: std::sync::Arc::new(X402State {
                config: aeo.x402.clone(),
                priced,
                available: complete && safe,
                client: crate::http_client::Client::from_state(state).named("x402"),
                base_url: config.seo.base_url.clone(),
                used: std::sync::Mutex::new(lru::LruCache::new(
                    std::num::NonZeroUsize::new(10_000).unwrap_or(std::num::NonZeroUsize::MIN),
                )),
            }),
        })
    }
}

#[cfg(feature = "http-client")]
impl X402Layer {
    /// [`X402Layer::from_config`], built once per app state. Every copy of
    /// the layer then shares one used-payment cache.
    #[must_use]
    pub(crate) fn shared(
        config: &crate::config::AutumnConfig,
        state: &crate::state::AppState,
    ) -> Option<Self> {
        if let Some(shared) = state.extension::<SharedX402>() {
            return shared.0.clone();
        }
        let layer = Self::from_config(config, state);
        state.insert_extension(SharedX402(layer.clone()));
        layer
    }
}

/// Without the `http-client` feature Autumn cannot reach a facilitator, so
/// a priced route fails closed: it answers `503` and is never served free.
#[cfg(not(feature = "http-client"))]
#[derive(Clone)]
pub(crate) struct X402Unavailable {
    priced: std::sync::Arc<PricedRoutes>,
}

#[cfg(not(feature = "http-client"))]
impl X402Unavailable {
    /// The layer, or `None` when no route is priced.
    #[must_use]
    pub(crate) fn from_config(config: &crate::config::AutumnConfig) -> Option<Self> {
        let priced = PricedRoutes::from_config(config)?;
        tracing::error!(
            "aeo: [[aeo.paid_routes]] has x402 routes, but x402 needs the `http-client` \
             feature; those routes answer 503"
        );
        Some(Self {
            priced: std::sync::Arc::new(priced),
        })
    }
}

#[cfg(not(feature = "http-client"))]
impl<S> tower::Layer<S> for X402Unavailable {
    type Service = X402UnavailableService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        X402UnavailableService {
            inner,
            priced: std::sync::Arc::clone(&self.priced),
        }
    }
}

/// Service made by [`X402Unavailable`].
#[cfg(not(feature = "http-client"))]
#[derive(Clone)]
pub(crate) struct X402UnavailableService<S> {
    inner: S,
    priced: std::sync::Arc<PricedRoutes>,
}

#[cfg(not(feature = "http-client"))]
impl<S> tower::Service<axum::http::Request<axum::body::Body>> for X402UnavailableService<S>
where
    S: tower::Service<axum::http::Request<axum::body::Body>, Response = axum::response::Response>,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = futures::future::Either<
        std::future::Ready<Result<axum::response::Response, S::Error>>,
        S::Future,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        use axum::response::IntoResponse as _;

        if !is_internal_render(&req) && self.priced.route_for(&req).is_some() {
            let res = (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "payments are not available",
            )
                .into_response();
            return futures::future::Either::Left(std::future::ready(Ok(res)));
        }
        futures::future::Either::Right(self.inner.call(req))
    }
}

/// `true` for a build or ISR render: Autumn makes it, no client does.
fn is_internal_render(req: &axum::http::Request<axum::body::Body>) -> bool {
    req.extensions()
        .get::<crate::static_gen::RenderDeadlineExempt>()
        .is_some()
}

/// The app's one [`X402Layer`] (an `AppState` extension).
#[cfg(feature = "http-client")]
struct SharedX402(Option<X402Layer>);

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
pub(crate) struct X402Service<S> {
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

    #[allow(clippy::too_many_lines)]
    fn call(&mut self, mut req: axum::http::Request<axum::body::Body>) -> Self::Future {
        // An unpriced request costs no clone and no box. A build or ISR
        // render is internal: the outer copy of this layer charges the live
        // request that reads the page.
        let route = if req.extensions().get::<X402Handled>().is_some() || is_internal_render(&req) {
            None
        } else {
            self.state.priced.route_for(&req).cloned()
        };
        let Some(route) = route else {
            return futures::future::Either::Left(self.inner.call(req));
        };
        req.extensions_mut().insert(X402Handled);
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let state = std::sync::Arc::clone(&self.state);
        futures::future::Either::Right(Box::pin(async move {
            if !state.available || !route_is_valid(&route) {
                return Ok(plain(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "payments are not available",
                ));
            }
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
            // One payment header works once. Claim it before any call.
            let key: [u8; 32] = sha2::Sha256::digest(header.as_bytes()).into();
            {
                let mut used = state
                    .used
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if used.put(key, ()).is_some() {
                    return Ok(payment_required(
                        &state,
                        &route,
                        &resource,
                        "payment already used",
                    ));
                }
            }
            let verify = match facilitator(&state, "verify", &body).await {
                Ok(v) => v,
                Err(status) => {
                    // Not verified, so not used: the client may retry it.
                    state
                        .used
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop(&key);
                    return Ok(plain(status, "payment facilitator unavailable"));
                }
            };
            if verify.get("isValid").and_then(Value::as_bool) != Some(true) {
                let reason = verify
                    .get("invalidReason")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid payment")
                    .to_owned();
                return Ok(payment_required(&state, &route, &resource, &reason));
            }

            // A safe method settles after a successful answer. Any other
            // method settles first, so its side effects are always paid.
            let safe = matches!(
                *req.method(),
                axum::http::Method::GET | axum::http::Method::HEAD
            );
            let early = if safe {
                None
            } else {
                match settle(&state, &route, &resource, &body).await {
                    Ok(receipt) => Some(receipt),
                    Err(failed) => return Ok(*failed),
                }
            };
            let mut res = inner.call(req).await?;
            let receipt = match early {
                Some(receipt) => receipt,
                None if res.status().is_success() => {
                    match settle(&state, &route, &resource, &body).await {
                        Ok(receipt) => receipt,
                        Err(failed) => return Ok(*failed),
                    }
                }
                None => return Ok(res),
            };
            let headers = res.headers_mut();
            if let Some(h) = receipt {
                headers.insert("payment-response", h);
            }
            headers.insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("private, no-store"),
            );
            Ok(res)
        }))
    }
}

/// Settle a verified payment. `Ok` carries the `PAYMENT-RESPONSE` value;
/// `Err` is the response to send instead (boxed: the error path is rare).
#[cfg(feature = "http-client")]
async fn settle(
    state: &X402State,
    route: &PaidRoute,
    resource: &str,
    body: &Value,
) -> Result<Option<axum::http::HeaderValue>, Box<axum::response::Response>> {
    let settled = facilitator(state, "settle", body)
        .await
        .map_err(|status| Box::new(plain(status, "payment facilitator unavailable")))?;
    let header = axum::http::HeaderValue::from_str(&encode_header(&settled)).ok();
    if settled.get("success").and_then(Value::as_bool) == Some(true) {
        return Ok(header);
    }
    let mut failed = payment_required(state, route, resource, "settlement failed");
    if let Some(h) = header {
        failed.headers_mut().insert("payment-response", h);
    }
    Err(Box::new(failed))
}

/// `true` for an `https` facilitator, or `http` on a loopback host.
#[cfg(feature = "http-client")]
fn facilitator_url_is_safe(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|u| match u.scheme() {
        "https" => true,
        "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    })
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
    // An error answer is an unavailable facilitator, whatever its body says.
    if !res.is_success() {
        tracing::warn!(
            status = res.status().as_u16(),
            op,
            "aeo: x402 facilitator answered an error"
        );
        return Err(axum::http::StatusCode::BAD_GATEWAY);
    }
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

    #[cfg(not(feature = "http-client"))]
    #[tokio::test]
    async fn without_the_http_client_priced_routes_fail_closed() {
        use tower::{Layer as _, ServiceExt as _};

        let mut config = crate::config::AutumnConfig::default();
        config.aeo.x402 = x402();
        config.aeo.paid_routes.push(route());
        let layer = X402Unavailable::from_config(&config).expect("a route is priced");
        let inner = tower::service_fn(|_req: axum::http::Request<axum::body::Body>| async {
            Ok::<_, std::convert::Infallible>(axum::response::Response::new(
                axum::body::Body::empty(),
            ))
        });
        let call = |path: &str| {
            let req = axum::http::Request::get(path)
                .body(axum::body::Body::empty())
                .unwrap();
            layer.layer(inner).oneshot(req)
        };
        assert_eq!(call("/api/reports/7").await.unwrap().status(), 503);
        assert_eq!(call("/free").await.unwrap().status(), 200);
    }

    #[test]
    fn a_route_outside_the_locale_nests_keeps_its_locale_segment() {
        let priced = PricedRoutes {
            routes: vec![
                PaidRoute::new("GET", "/en/admin", "1"),
                PaidRoute::new("GET", "/api", "2"),
            ],
            locales: vec!["en".to_owned()],
        };
        let get = |path: &str| {
            axum::http::Request::get(path)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let amount = |path: &str| priced.route_for(&get(path)).map(|r| r.amount.clone());
        assert_eq!(amount("/en/admin").as_deref(), Some("1"), "unlocalized");
        assert_eq!(amount("/en/api").as_deref(), Some("2"), "localized");
        assert_eq!(amount("/api").as_deref(), Some("2"));
        assert_eq!(amount("/admin"), None);
    }

    #[test]
    fn route_matching_normalizes_head_slash_and_wildcards() {
        let r = route();
        assert!(r.matches("HEAD", "/api/reports/7"));
        assert!(r.matches("GET", "/api/reports/7/"));
        let files = PaidRoute::new("GET", "/files/{*path}", "1");
        assert!(files.matches("GET", "/files/a/b"));
        assert!(!files.matches("GET", "/files/"));
        assert!(!PaidRoute::new("POST", "/x", "1").matches("HEAD", "/x"));
    }

    #[test]
    fn paid_route_problems_flag_bad_amounts_and_methods() {
        assert!(valid_amount("10000") && valid_amount("0"));
        assert!(!valid_amount("010") && !valid_amount("1.5") && !valid_amount(""));
        let mut r = PaidRoute::new("GET", "/x", "01");
        r.mpp_method = Some("Stripe".to_owned());
        assert_eq!(paid_route_problems(&[r]).len(), 2);
    }

    #[test]
    fn a_route_that_can_never_match_is_a_problem() {
        assert!(unmatchable_route(&PaidRoute::new("get", "/x", "1")).is_none());
        for r in [
            PaidRoute::new("", "/x", "1"),
            PaidRoute::new("FETCH", "/x", "1"),
            PaidRoute::new(" GET ", "/x", "1"),
            PaidRoute::new("GET", "/x ", "1"),
            PaidRoute::new("GET", "/api?plan=pro", "1"),
            PaidRoute::new("GET", "/api#top", "1"),
            PaidRoute::new("GET", "api", "1"),
            PaidRoute::new("GET", "", "1"),
        ] {
            assert!(unmatchable_route(&r).is_some(), "{r:?}");
        }
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn facilitator_must_be_https_or_loopback() {
        assert!(facilitator_url_is_safe("https://x402.org/facilitator"));
        assert!(facilitator_url_is_safe("http://localhost:8080"));
        assert!(!facilitator_url_is_safe("http://facilitator.example"));
        assert!(!facilitator_url_is_safe("not a url"));
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

        // A trailing `/` in the config still finds the operation.
        let mut slashed = route();
        slashed.path = "/api/free/".to_owned();
        apply_mpp(&mut spec, &[slashed]);
        assert!(
            spec["paths"]["/api/free"]["get"]
                .get("x-payment-info")
                .is_some()
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
            CommerceError::Invalid {
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
                matches!(AcpDiscovery::new(doc), Err(CommerceError::Invalid { field: f, .. }) if f == field),
                "{field}"
            );
        }
    }
}
