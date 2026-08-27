//! `[shadow]` configuration — traffic mirroring and response diffing (#1653).
//!
//! Off by default and inert until an operator sets both `enabled = true` and a
//! `target`. Every knob here exists to bound the blast radius: mirroring copies
//! production traffic, so the sample rate, the per-request deadline, the
//! in-flight ceiling, and the capture budgets are all first-class config rather
//! than hard-coded constants.
//!
//! ```toml
//! [shadow]
//! enabled        = true
//! target         = "http://127.0.0.1:9091"   # the candidate build
//! sample_rate    = 0.05                      # mirror 5 % of eligible traffic
//! routes         = ["/api/*"]                # empty = every eligible route
//! timeout_ms     = 2000
//! max_in_flight  = 8
//! ```

use serde::Deserialize;

/// Settings for mirroring live `GET`/`HEAD` traffic to a shadow build and
/// diffing its responses against the live ones.
///
/// # Why the method set is not configurable
///
/// This slice mirrors idempotent traffic only. Replaying a `POST` against a
/// candidate build would let the candidate's writes land for real — the effect
/// virtualization that makes that safe is the deliberate follow-up slice. The
/// method allowlist is therefore a compile-time constant
/// ([`crate::shadow::sample::MIRRORABLE_METHODS`]), not a config key an
/// operator can widen by accident.
#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfig {
    /// Master switch. Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Base URL of the candidate build that receives mirrored requests, e.g.
    /// `"http://127.0.0.1:9091"`. Required once [`Self::enabled`] is `true`.
    #[serde(default)]
    pub target: Option<String>,

    /// Fraction of *eligible* requests to mirror, `0.0`–`1.0`. Default: `1.0`.
    ///
    /// Eligibility (method, loop guard, route allowlist) is decided first, so
    /// this samples what is left rather than all traffic.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,

    /// Route patterns opted into mirroring. Empty (the default) means every
    /// eligible route. A trailing `*` matches a prefix (`"/api/*"`); anything
    /// else is an exact path match.
    #[serde(default)]
    pub routes: Vec<String>,

    /// Wall-clock deadline for a single shadow request, in milliseconds.
    /// A shadow that overruns it is abandoned and counted, never awaited by the
    /// live request. Default: `2000`.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Ceiling on concurrently in-flight mirrored requests. Once reached,
    /// further candidates are dropped (and counted) rather than queued, so a
    /// slow shadow can never accumulate work. Default: `8`.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,

    /// Largest response body, in bytes, either side will buffer for comparison.
    /// A larger body is not compared at all (counted as skipped) — it is never
    /// partially buffered. Default: 256 KiB.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    /// How many recent divergences `{actuator-prefix}/shadow` keeps.
    /// Default: `50`.
    #[serde(default = "default_max_records")]
    pub max_records: usize,

    /// Budget, in bytes of the serialized form, for each recorded JSON sample
    /// before it is truncated. Default: `2048`.
    #[serde(default = "default_max_sample_bytes")]
    pub max_sample_bytes: usize,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target: None,
            sample_rate: default_sample_rate(),
            routes: Vec::new(),
            timeout_ms: default_timeout_ms(),
            max_in_flight: default_max_in_flight(),
            max_body_bytes: default_max_body_bytes(),
            max_records: default_max_records(),
            max_sample_bytes: default_max_sample_bytes(),
        }
    }
}

impl ShadowConfig {
    /// Validate the section.
    ///
    /// Returns `Ok(())` unmodified while [`Self::enabled`] is `false`: an
    /// operator drafting a `[shadow]` block must be able to leave it
    /// half-finished in the file without failing boot. Every check below is
    /// therefore about a mirror that is actually about to run.
    ///
    /// # Errors
    ///
    /// Returns the human-readable reason the section cannot be honoured.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        let Some(target) = self
            .target
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            return Err(
                "shadow.target is required when shadow.enabled = true — set it to the \
                 candidate build's base URL, e.g. \"http://127.0.0.1:9091\""
                    .to_owned(),
            );
        };
        // Parsed, not prefix-matched: `"http://"` passes a `starts_with` check
        // but has no authority to dial, so the replica would boot reporting an
        // enabled mirror while every selected request failed as a transport
        // error. A target that cannot be dialed must fail boot, which is what
        // the rest of this function promises.
        let parsed = url::Url::parse(target)
            .map_err(|error| format!("shadow.target is not a valid URL ({error}): {target:?}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "shadow.target must be an absolute http(s) URL, got {target:?}"
            ));
        }
        if parsed.host_str().is_none_or(str::is_empty) {
            return Err(format!(
                "shadow.target must name a host to dial, got {target:?}"
            ));
        }
        // A query or fragment on the base is unusable: `shadow_url` appends the
        // live request target to it, so `http://candidate/base?token=x` would
        // produce `…/base?token=x/api/orders?page=2` — the request path folded
        // into the query — and a fragment is never transmitted at all. Every
        // mirrored request would reach the wrong resource on the candidate,
        // quietly. A base *path* (`http://candidate/base`) is fine and is what
        // an operator mounting the candidate under a prefix needs.
        if parsed.query().is_some() {
            return Err(format!(
                "shadow.target must not carry a query string — the request target is \
                 appended to it, so the mirrored path would fold into the query: {target:?}"
            ));
        }
        if parsed.fragment().is_some() {
            return Err(format!(
                "shadow.target must not carry a fragment — a fragment is never sent over \
                 the wire: {target:?}"
            ));
        }
        // Userinfo would be echoed by the actuator endpoint and by the startup
        // log line. Refuse it rather than quietly publishing a credential; the
        // candidate should be reached without one, or behind something that
        // adds it.
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(
                "shadow.target must not embed credentials — the value is published by the \
                 shadow actuator endpoint and logged at startup"
                    .to_owned(),
            );
        }

        for route in &self.routes {
            if route.trim().is_empty() {
                return Err("shadow.routes must not contain a blank pattern".to_owned());
            }
            if !route.starts_with('/') {
                return Err(format!(
                    "shadow.routes patterns must start with '/', got {route:?} — a pattern that \
                     cannot match any request path would silently disable mirroring"
                ));
            }
        }

        if !self.sample_rate.is_finite() || !(0.0..=1.0).contains(&self.sample_rate) {
            return Err(format!(
                "shadow.sample_rate must be between 0.0 and 1.0, got {}",
                self.sample_rate
            ));
        }

        for (name, is_zero) in [
            ("shadow.timeout_ms", self.timeout_ms == 0),
            ("shadow.max_in_flight", self.max_in_flight == 0),
            ("shadow.max_body_bytes", self.max_body_bytes == 0),
            ("shadow.max_records", self.max_records == 0),
            ("shadow.max_sample_bytes", self.max_sample_bytes == 0),
        ] {
            if is_zero {
                return Err(format!("{name} must be greater than zero"));
            }
        }

        Ok(())
    }

    /// The target base URL with any trailing `/` removed, so joining a request
    /// target (which always starts with `/`) cannot produce a doubled slash.
    ///
    /// `None` when no target is configured.
    #[must_use]
    pub fn target_base(&self) -> Option<&str> {
        self.target
            .as_deref()
            .map(str::trim)
            .map(|target| target.strip_suffix('/').unwrap_or(target))
            .filter(|target| !target.is_empty())
    }

    /// Whether mirroring should actually run: switched on *and* pointed
    /// somewhere.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && self.target_base().is_some()
    }
}

const fn default_sample_rate() -> f64 {
    1.0
}

const fn default_timeout_ms() -> u64 {
    2000
}

const fn default_max_in_flight() -> usize {
    8
}

const fn default_max_body_bytes() -> usize {
    256 * 1024
}

const fn default_max_records() -> usize {
    50
}

const fn default_max_sample_bytes() -> usize {
    2048
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_with_no_target() {
        let config = ShadowConfig::default();
        assert!(!config.enabled);
        assert!(config.target.is_none());
        assert!(config.routes.is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn defaults_are_bounded() {
        let config = ShadowConfig::default();
        assert!(config.timeout_ms > 0);
        assert!(config.max_in_flight > 0);
        assert!(config.max_body_bytes > 0);
        assert!(config.max_records > 0);
        assert!(config.max_sample_bytes > 0);
        assert!((config.sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn enabling_without_a_target_is_rejected() {
        let config = ShadowConfig {
            enabled: true,
            ..ShadowConfig::default()
        };
        let error = config.validate().expect_err("must reject");
        assert!(error.contains("shadow.target"), "{error}");
    }

    #[test]
    fn target_must_be_an_absolute_http_url() {
        for bad in ["localhost:9091", "ftp://host", "/relative", "  "] {
            let config = ShadowConfig {
                enabled: true,
                target: Some(bad.to_owned()),
                ..ShadowConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "{bad} must be rejected as a shadow target"
            );
        }
        let config = ShadowConfig {
            enabled: true,
            target: Some("http://127.0.0.1:9091".to_owned()),
            ..ShadowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sample_rate_must_be_a_probability() {
        for bad in [-0.1, 1.1, f64::NAN] {
            let config = ShadowConfig {
                enabled: true,
                target: Some("http://127.0.0.1:9091".to_owned()),
                sample_rate: bad,
                ..ShadowConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "sample_rate {bad} must be rejected"
            );
        }
    }

    #[test]
    fn zero_bounds_are_rejected() {
        let base = ShadowConfig {
            enabled: true,
            target: Some("http://127.0.0.1:9091".to_owned()),
            ..ShadowConfig::default()
        };
        for mutate in [
            (|c: &mut ShadowConfig| c.timeout_ms = 0) as fn(&mut ShadowConfig),
            |c: &mut ShadowConfig| c.max_in_flight = 0,
            |c: &mut ShadowConfig| c.max_body_bytes = 0,
            |c: &mut ShadowConfig| c.max_records = 0,
            |c: &mut ShadowConfig| c.max_sample_bytes = 0,
        ] {
            let mut config = base.clone();
            mutate(&mut config);
            assert!(config.validate().is_err(), "a zero bound must be rejected");
        }
    }

    #[test]
    fn validation_is_skipped_while_disabled() {
        // An operator may leave a half-finished `[shadow]` block in the file;
        // it must not fail boot until they actually switch it on.
        let config = ShadowConfig {
            enabled: false,
            target: Some("not-a-url".to_owned()),
            sample_rate: 9.0,
            ..ShadowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn deserializes_from_toml_with_partial_tables() {
        let config: ShadowConfig = toml::from_str(
            r#"
            enabled = true
            target = "http://127.0.0.1:9091"
            sample_rate = 0.25
            routes = ["/api/*"]
            "#,
        )
        .expect("must parse");
        assert!(config.enabled);
        assert_eq!(config.target.as_deref(), Some("http://127.0.0.1:9091"));
        assert!((config.sample_rate - 0.25).abs() < f64::EPSILON);
        assert_eq!(config.routes, vec!["/api/*".to_owned()]);
        // Unspecified keys still carry their bounded defaults.
        assert_eq!(config.timeout_ms, ShadowConfig::default().timeout_ms);
    }

    #[test]
    fn a_target_with_no_host_to_dial_is_rejected() {
        // `"http://"` passes a `starts_with` check but has nothing to dial, so
        // the replica would boot reporting an enabled mirror while every
        // request failed as a transport error.
        // NB `"http:///path"` is deliberately absent: WHATWG normalizes it to
        // host `path`, which is dialable, so rejecting it would be wrong.
        for bad in ["http://", "https://"] {
            let config = ShadowConfig {
                enabled: true,
                target: Some(bad.to_owned()),
                ..ShadowConfig::default()
            };
            let error = config
                .validate()
                .expect_err(&format!("{bad} must be rejected"));
            assert!(error.contains("shadow.target"), "{error}");
        }
    }

    #[test]
    fn a_target_carrying_a_query_or_fragment_is_rejected() {
        // The request target is appended to the base, so a query on the base
        // would swallow the mirrored path (`…?token=x/api/orders`) and a
        // fragment is never sent at all — either way every mirrored request
        // would silently reach the wrong resource.
        for bad in [
            "http://candidate/base?token=x",
            "http://candidate#fragment",
            "http://candidate/base?a=1#f",
        ] {
            let config = ShadowConfig {
                enabled: true,
                target: Some(bad.to_owned()),
                ..ShadowConfig::default()
            };
            let error = config
                .validate()
                .expect_err(&format!("{bad} must be rejected"));
            assert!(error.contains("shadow.target"), "{error}");
        }

        // A base *path* is legitimate — mounting the candidate under a prefix.
        let config = ShadowConfig {
            enabled: true,
            target: Some("http://candidate/base".to_owned()),
            ..ShadowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_target_carrying_credentials_is_rejected() {
        // The value is echoed by the shadow actuator endpoint and logged at
        // startup, so it must not be a place to put a password.
        let config = ShadowConfig {
            enabled: true,
            target: Some("http://user:pass@candidate.internal".to_owned()),
            ..ShadowConfig::default()
        };
        let error = config.validate().expect_err("must reject");
        assert!(error.contains("credentials"), "{error}");
    }

    #[test]
    fn route_patterns_that_can_never_match_are_rejected() {
        // A pattern without a leading slash matches no request path, so the
        // allowlist would silently mirror nothing at all.
        for bad in ["api/*", "  ", ""] {
            let config = ShadowConfig {
                enabled: true,
                target: Some("http://127.0.0.1:9091".to_owned()),
                routes: vec![bad.to_owned()],
                ..ShadowConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "{bad:?} must be rejected as a route pattern"
            );
        }
        let config = ShadowConfig {
            enabled: true,
            target: Some("http://127.0.0.1:9091".to_owned()),
            routes: vec!["/api/*".to_owned(), "/status".to_owned()],
            ..ShadowConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validation_errors_name_the_key_that_is_wrong() {
        let base = ShadowConfig {
            enabled: true,
            target: Some("http://127.0.0.1:9091".to_owned()),
            ..ShadowConfig::default()
        };
        for (name, mutate) in [
            (
                "shadow.timeout_ms",
                (|c: &mut ShadowConfig| c.timeout_ms = 0) as fn(&mut ShadowConfig),
            ),
            ("shadow.max_in_flight", |c: &mut ShadowConfig| {
                c.max_in_flight = 0;
            }),
            ("shadow.max_body_bytes", |c: &mut ShadowConfig| {
                c.max_body_bytes = 0;
            }),
            ("shadow.max_records", |c: &mut ShadowConfig| {
                c.max_records = 0;
            }),
            ("shadow.max_sample_bytes", |c: &mut ShadowConfig| {
                c.max_sample_bytes = 0;
            }),
            ("shadow.sample_rate", |c: &mut ShadowConfig| {
                c.sample_rate = 2.0;
            }),
        ] {
            let mut config = base.clone();
            mutate(&mut config);
            let error = config.validate().expect_err("must reject");
            assert!(
                error.contains(name),
                "the error for {name} must name it, got: {error}"
            );
        }
    }

    #[test]
    fn target_base_trims_a_trailing_slash() {
        let config = ShadowConfig {
            enabled: true,
            target: Some("http://127.0.0.1:9091/".to_owned()),
            ..ShadowConfig::default()
        };
        assert_eq!(config.target_base(), Some("http://127.0.0.1:9091"));
    }
}
