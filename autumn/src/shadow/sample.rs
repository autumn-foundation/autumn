//! Which requests get mirrored, and why the rest do not (issue #1653).
//!
//! The decision is a pure function of the request and one `roll` in `[0, 1)`
//! supplied by the caller, so it is fully testable and — when the roll comes
//! from a [`crate::entropy::SeededEntropy`], as it does under
//! [`#[sim_test]`](crate::sim_test) — reproducible.
//!
//! Four gates run before the sample rate is even consulted, cheapest first:
//!
//! 1. **Method.** Only [`MIRRORABLE_METHODS`] (`GET`/`HEAD`). This slice
//!    mirrors idempotent traffic only, and the set is a constant rather than a
//!    config key precisely so it cannot be widened by accident.
//! 2. **Loop guard.** A request already carrying [`SHADOW_HEADER`] is itself a
//!    mirrored request. Mirroring it again — which is what happens the moment
//!    someone points a shadow target at the app itself, or chains two shadows —
//!    would multiply traffic without bound.
//! 3. **Exempt paths.** The actuator prefix and the platform probe paths. A
//!    load balancer's health checks are the highest-rate, least-interesting
//!    traffic an app serves; mirroring them buys nothing and drowns the
//!    candidate.
//! 4. **Route allowlist.** Empty means "every eligible route".

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

use axum::http::{HeaderMap, Method};

use crate::entropy::Entropy;

/// The only methods this slice mirrors.
pub const MIRRORABLE_METHODS: [Method; 2] = [Method::GET, Method::HEAD];

/// Header stamped on every mirrored request.
///
/// Two jobs: it is the loop guard (see the module docs), and it is the seam a
/// candidate build uses to recognise mirrored traffic and refuse to act on it —
/// the hook the follow-up effect-virtualization slice builds on.
pub const SHADOW_HEADER: &str = "x-autumn-shadow";

/// Value sent with [`SHADOW_HEADER`].
pub const SHADOW_HEADER_VALUE: &str = "1";

/// Label used for the route dimension when no route patterns are configured.
const ALL_ROUTES_LABEL: &str = "*";

/// Why a request was not mirrored.
///
/// Each variant is a metric label value, so an operator can see at a glance
/// whether their mirror is quiet because nothing matched, because the sample
/// rate is low, or because they pointed it at itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Not a [`MIRRORABLE_METHODS`] method.
    Method,
    /// The request is itself a mirrored request.
    LoopGuard,
    /// An actuator or probe path.
    ExemptPath,
    /// A route allowlist is configured and this path is not on it.
    RouteNotOptedIn,
    /// Eligible, but the sample rate did not select it.
    NotSampled,
}

impl SkipReason {
    /// Stable snake_case name for metrics and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::LoopGuard => "loop_guard",
            Self::ExemptPath => "exempt_path",
            Self::RouteNotOptedIn => "route_not_opted_in",
            Self::NotSampled => "not_sampled",
        }
    }
}

/// Whether to mirror one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorDecision {
    /// Mirror it.
    Mirror,
    /// Leave it alone, for this reason.
    Skip(SkipReason),
}

/// One entry of the configured route allowlist.
#[derive(Clone, Debug)]
enum RoutePattern {
    /// `"/api/*"` — matches any path starting with `"/api/"`, and `"/api/"`.
    Prefix { pattern: String, prefix: String },
    /// `"/status"` — matches that path exactly.
    Exact(String),
}

impl RoutePattern {
    fn parse(raw: &str) -> Self {
        raw.strip_suffix('*').map_or_else(
            || Self::Exact(raw.to_owned()),
            |prefix| Self::Prefix {
                pattern: raw.to_owned(),
                prefix: prefix.to_owned(),
            },
        )
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Prefix { prefix, .. } => path.starts_with(prefix.as_str()),
            Self::Exact(exact) => path == exact,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Prefix { pattern, .. } => pattern,
            Self::Exact(exact) => exact,
        }
    }
}

/// The mirroring admission decision, resolved once at router-assembly time.
#[derive(Clone, Debug)]
pub struct MirrorSelector {
    sample_rate: f64,
    routes: std::sync::Arc<Vec<RoutePattern>>,
    actuator_prefix: String,
    actuator_prefix_slash: String,
    probe_paths: std::sync::Arc<Vec<String>>,
}

impl MirrorSelector {
    /// Build a selector.
    ///
    /// `actuator_prefix` and `probe_paths` come from the same config the
    /// load-shed layer reads, so the two agree on what platform traffic is.
    #[must_use]
    pub fn new(
        sample_rate: f64,
        routes: &[String],
        actuator_prefix: &str,
        probe_paths: &[String],
    ) -> Self {
        let actuator_prefix = actuator_prefix.trim_end_matches('/').to_owned();
        Self {
            sample_rate,
            routes: std::sync::Arc::new(routes.iter().map(|r| RoutePattern::parse(r)).collect()),
            actuator_prefix_slash: format!("{actuator_prefix}/"),
            actuator_prefix,
            probe_paths: std::sync::Arc::new(probe_paths.to_vec()),
        }
    }

    /// Decide whether to mirror one request.
    ///
    /// `roll` is a **thunk**, not a value: it is called only once the four
    /// cheap gates above have passed. On an app with mirroring enabled this
    /// runs on every inbound request, and the entropy source behind
    /// [`roll_from`] takes a lock — so drawing eagerly would put a lock
    /// acquisition on the path of every `POST`, every health check, and every
    /// request to a route that is not even opted in. It must be in `[0, 1)`.
    #[must_use]
    pub fn decide(
        &self,
        method: &Method,
        target: &str,
        headers: &HeaderMap,
        roll: impl FnOnce() -> f64,
    ) -> MirrorDecision {
        if !MIRRORABLE_METHODS.contains(method) {
            return MirrorDecision::Skip(SkipReason::Method);
        }
        if headers.contains_key(SHADOW_HEADER) {
            return MirrorDecision::Skip(SkipReason::LoopGuard);
        }

        let path = path_of(target);
        if self.is_exempt(path) {
            return MirrorDecision::Skip(SkipReason::ExemptPath);
        }
        if !self.routes.is_empty() && !self.routes.iter().any(|r| r.matches(path)) {
            return MirrorDecision::Skip(SkipReason::RouteNotOptedIn);
        }

        // `roll < rate` — so `rate = 0.0` never fires (no roll is below zero)
        // and `rate = 1.0` always does (every roll is below one).
        if self.sample_rate <= 0.0 {
            return MirrorDecision::Skip(SkipReason::NotSampled);
        }
        if roll() < self.sample_rate {
            MirrorDecision::Mirror
        } else {
            MirrorDecision::Skip(SkipReason::NotSampled)
        }
    }

    /// The bounded metric label for a path: the configured pattern it matched,
    /// or [`ALL_ROUTES_LABEL`] when no allowlist is configured.
    ///
    /// Never the raw path. A global tower layer runs outside axum's route
    /// matching, so [`axum::extract::MatchedPath`] is not available here; using
    /// the URL itself would let an unbounded URL space become unbounded metric
    /// cardinality.
    #[must_use]
    pub fn route_label(&self, target: &str) -> &str {
        let path = path_of(target);
        self.routes
            .iter()
            .find(|r| r.matches(path))
            .map_or(ALL_ROUTES_LABEL, RoutePattern::label)
    }

    /// Platform traffic that is never mirrored.
    fn is_exempt(&self, path: &str) -> bool {
        if !self.actuator_prefix.is_empty()
            && (path == self.actuator_prefix || path.starts_with(&self.actuator_prefix_slash))
        {
            return true;
        }
        self.probe_paths.iter().any(|probe| probe == path)
    }
}

/// The path portion of a request target, with any query string removed.
fn path_of(target: &str) -> &str {
    target.split('?').next().unwrap_or(target)
}

/// Draw a sampling roll in `[0, 1)` from an entropy source.
///
/// Uses the top 53 bits so every draw is exactly representable as an `f64`, and
/// draws through [`Entropy`] rather than a thread RNG so a seeded source makes
/// the whole mirroring decision reproducible.
#[must_use]
pub fn roll_from(entropy: &dyn Entropy) -> f64 {
    /// `2^-53`, the spacing of the representable values this maps onto.
    const SCALE: f64 = 1.0 / (1u64 << 53) as f64;
    ((entropy.next_u64() >> 11) as f64) * SCALE
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, Method};

    fn selector() -> MirrorSelector {
        MirrorSelector::new(1.0, &[], "/actuator", &["/healthz".to_owned()])
    }

    #[test]
    fn get_and_head_are_mirrored() {
        let selector = selector();
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                selector.decide(&method, "/api/orders", &HeaderMap::new(), || 0.0),
                MirrorDecision::Mirror
            );
        }
    }

    #[test]
    fn mutating_methods_are_never_mirrored() {
        let selector = selector();
        for method in [
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert_eq!(
                selector.decide(&method, "/api/orders", &HeaderMap::new(), || 0.0),
                MirrorDecision::Skip(SkipReason::Method),
                "{method} must not be mirrored"
            );
        }
    }

    #[test]
    fn a_mirrored_request_is_never_mirrored_again() {
        let selector = selector();
        let mut headers = HeaderMap::new();
        headers.insert(SHADOW_HEADER, HeaderValue::from_static(SHADOW_HEADER_VALUE));
        assert_eq!(
            selector.decide(&Method::GET, "/api/orders", &headers, || 0.0),
            MirrorDecision::Skip(SkipReason::LoopGuard)
        );
    }

    #[test]
    fn actuator_and_probe_paths_are_exempt() {
        let selector = selector();
        for path in ["/actuator", "/actuator/health", "/healthz"] {
            assert_eq!(
                selector.decide(&Method::GET, path, &HeaderMap::new(), || 0.0),
                MirrorDecision::Skip(SkipReason::ExemptPath),
                "{path} must be exempt"
            );
        }
        // A route that merely *starts with the same letters* is not exempt.
        assert_eq!(
            selector.decide(
                &Method::GET,
                "/actuatorsomething",
                &HeaderMap::new(),
                || 0.0
            ),
            MirrorDecision::Mirror
        );
    }

    #[test]
    fn an_empty_route_list_opts_every_route_in() {
        let selector = selector();
        assert_eq!(
            selector.decide(&Method::GET, "/anything/at/all", &HeaderMap::new(), || 0.0),
            MirrorDecision::Mirror
        );
    }

    #[test]
    fn route_patterns_gate_which_paths_are_mirrored() {
        let selector = MirrorSelector::new(
            1.0,
            &["/api/*".to_owned(), "/status".to_owned()],
            "/actuator",
            &[],
        );
        for path in ["/api/orders", "/api/", "/status"] {
            assert_eq!(
                selector.decide(&Method::GET, path, &HeaderMap::new(), || 0.0),
                MirrorDecision::Mirror,
                "{path} must match"
            );
        }
        for path in ["/apiary", "/status/detail", "/"] {
            assert_eq!(
                selector.decide(&Method::GET, path, &HeaderMap::new(), || 0.0),
                MirrorDecision::Skip(SkipReason::RouteNotOptedIn),
                "{path} must not match"
            );
        }
    }

    #[test]
    fn a_query_string_does_not_defeat_route_matching() {
        let selector = MirrorSelector::new(1.0, &["/api/*".to_owned()], "/actuator", &[]);
        assert_eq!(
            selector.decide(
                &Method::GET,
                "/api/orders?page=2",
                &HeaderMap::new(),
                || 0.0
            ),
            MirrorDecision::Mirror
        );
    }

    #[test]
    fn sample_rate_zero_mirrors_nothing() {
        let selector = MirrorSelector::new(0.0, &[], "/actuator", &[]);
        for roll in [0.0, 0.5, 0.999] {
            assert_eq!(
                selector.decide(&Method::GET, "/api/orders", &HeaderMap::new(), || roll),
                MirrorDecision::Skip(SkipReason::NotSampled)
            );
        }
    }

    #[test]
    fn sample_rate_is_a_deterministic_function_of_the_roll() {
        let selector = MirrorSelector::new(0.25, &[], "/actuator", &[]);
        assert_eq!(
            selector.decide(&Method::GET, "/api/orders", &HeaderMap::new(), || 0.1),
            MirrorDecision::Mirror
        );
        assert_eq!(
            selector.decide(&Method::GET, "/api/orders", &HeaderMap::new(), || 0.9),
            MirrorDecision::Skip(SkipReason::NotSampled)
        );
    }

    #[test]
    fn route_label_is_the_configured_pattern_not_the_raw_path() {
        let selector = MirrorSelector::new(1.0, &["/api/*".to_owned()], "/actuator", &[]);
        assert_eq!(selector.route_label("/api/orders/42"), "/api/*");
        // With no patterns configured the label collapses to a single bucket so
        // metric cardinality can never follow the URL space.
        let all = MirrorSelector::new(1.0, &[], "/actuator", &[]);
        assert_eq!(all.route_label("/api/orders/42"), "*");
    }

    #[test]
    fn the_sampling_roll_is_only_drawn_once_the_cheap_gates_pass() {
        let selector = MirrorSelector::new(1.0, &["/api/*".to_owned()], "/actuator", &[]);
        let draws = std::cell::Cell::new(0_u32);
        let roll = || {
            draws.set(draws.get() + 1);
            0.0
        };

        // Wrong method, exempt path, and un-opted-in route must all decide
        // without touching the entropy source.
        let _ = selector.decide(&Method::POST, "/api/orders", &HeaderMap::new(), roll);
        assert_eq!(draws.get(), 0);
        let _ = selector.decide(&Method::GET, "/actuator/health", &HeaderMap::new(), roll);
        assert_eq!(draws.get(), 0);
        let _ = selector.decide(&Method::GET, "/elsewhere", &HeaderMap::new(), roll);
        assert_eq!(draws.get(), 0);

        // An eligible request does draw.
        let _ = selector.decide(&Method::GET, "/api/orders", &HeaderMap::new(), roll);
        assert_eq!(draws.get(), 1);
    }

    #[test]
    fn a_zero_sample_rate_never_draws_at_all() {
        let selector = MirrorSelector::new(0.0, &[], "/actuator", &[]);
        let drew = std::cell::Cell::new(false);
        let decision = selector.decide(&Method::GET, "/api/orders", &HeaderMap::new(), || {
            drew.set(true);
            0.0
        });
        assert_eq!(decision, MirrorDecision::Skip(SkipReason::NotSampled));
        assert!(!drew.get(), "a disabled sample rate must not draw entropy");
    }

    #[test]
    fn skip_reasons_have_stable_metric_labels() {
        assert_eq!(SkipReason::Method.as_str(), "method");
        assert_eq!(SkipReason::LoopGuard.as_str(), "loop_guard");
        assert_eq!(SkipReason::ExemptPath.as_str(), "exempt_path");
        assert_eq!(SkipReason::RouteNotOptedIn.as_str(), "route_not_opted_in");
        assert_eq!(SkipReason::NotSampled.as_str(), "not_sampled");
    }

    #[test]
    fn roll_from_entropy_stays_in_range_and_is_reproducible() {
        let seeded = crate::entropy::SeededEntropy::new(42);
        let first: Vec<f64> = (0..8).map(|_| roll_from(&seeded)).collect();
        assert!(first.iter().all(|r| (0.0..1.0).contains(r)), "{first:?}");
        let replay = crate::entropy::SeededEntropy::new(42);
        let second: Vec<f64> = (0..8).map(|_| roll_from(&replay)).collect();
        assert_eq!(first, second, "the same seed must reproduce the same rolls");
    }
}
