//! The `[fault_injection]` config section (issue #3071).

use serde::Deserialize;

/// The largest injected latency, in milliseconds (5 minutes).
const MAX_LATENCY_MS: u64 = 300_000;

/// The largest injected latency for one request or one dependency call.
pub const MAX_LATENCY: std::time::Duration = std::time::Duration::from_millis(MAX_LATENCY_MS);

/// The largest number of fault rules.
pub const MAX_RULES: usize = 64;

/// The largest `stop.min_requests`.
const MAX_MIN_REQUESTS: u64 = 1_000_000;

/// `[fault_injection]`: faults for a staging environment.
///
/// ```toml
/// [fault_injection]
/// enabled = true
///
/// [fault_injection.stop]
/// objective = 99.0
/// max_burn_rate = 14.4
///
/// [[fault_injection.faults]]
/// routes = ["/api/*"]
/// target = "route"
/// kind = "error"
/// rate = 0.05
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FaultInjectionConfig {
    /// Turns on the faults. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Allow faults when the profile is `prod`. Default: `false`.
    ///
    /// Without it, config validation fails in `prod`, and the router does
    /// not install the layers.
    #[serde(default)]
    pub allow_in_production: bool,
    /// The stop condition.
    #[serde(default)]
    pub stop: FaultStopConfig,
    /// The faults (`[[fault_injection.faults]]`).
    #[serde(default)]
    pub faults: Vec<FaultRule>,
}

/// `[fault_injection.stop]`: the burn-rate stop condition.
///
/// The layer counts the requests and the `5xx` responses in a window. The
/// layer disarms when the error ratio is more than `max_burn_rate` times the
/// error budget of `objective`, and the window has `min_requests` or more.
/// The defaults stop at 14.4% errors: the fast-page burn rate of a 99% SLO.
/// It stays disarmed until an operator calls
/// [`FaultInjection::arm`](super::FaultInjection::arm).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FaultStopConfig {
    /// The availability objective, in percent, as in `[[slo]]`. Default:
    /// `99.0`.
    #[serde(default = "default_objective")]
    pub objective: f64,
    /// The largest burn rate of the error budget. Default: `14.4`.
    #[serde(default = "default_max_burn_rate")]
    pub max_burn_rate: f64,
    /// The window length, in seconds. Default: `60`.
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// The smallest number of requests in a window before the rule applies.
    /// Default: `20`.
    #[serde(default = "default_min_requests")]
    pub min_requests: u64,
}

impl Default for FaultStopConfig {
    fn default() -> Self {
        Self {
            objective: default_objective(),
            max_burn_rate: default_max_burn_rate(),
            window_secs: default_window_secs(),
            min_requests: default_min_requests(),
        }
    }
}

const fn default_objective() -> f64 {
    99.0
}

const fn default_max_burn_rate() -> f64 {
    14.4
}

const fn default_window_secs() -> u64 {
    60
}

const fn default_min_requests() -> u64 {
    20
}

/// One `[[fault_injection.faults]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FaultRule {
    /// Path patterns. A trailing `*` matches a prefix (`"/api/*"`). Other
    /// patterns match the exact path. Empty (the default) matches all paths.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Where the fault occurs. Default: `route`.
    #[serde(default)]
    pub target: FaultTarget,
    /// What the fault does.
    pub kind: FaultKind,
    /// The probability of the fault, from `0.0` to `1.0`.
    pub rate: f64,
    /// The added latency for `kind = "latency"`, in milliseconds.
    #[serde(default)]
    pub latency_ms: u64,
    /// The status for `kind = "error"` with `target = "route"`. Default:
    /// `503`.
    #[serde(default = "default_status")]
    pub status: u16,
}

impl Default for FaultRule {
    fn default() -> Self {
        Self {
            routes: Vec::new(),
            target: FaultTarget::default(),
            kind: FaultKind::Error,
            rate: 0.0,
            latency_ms: 0,
            status: default_status(),
        }
    }
}

const fn default_status() -> u16 {
    503
}

impl FaultRule {
    /// A rule for all paths. Set `routes`, `latency_ms` and `status` on the
    /// result as needed.
    #[must_use]
    pub fn new(target: FaultTarget, kind: FaultKind, rate: f64) -> Self {
        Self {
            target,
            kind,
            rate,
            ..Self::default()
        }
    }
}

/// Where a fault occurs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FaultTarget {
    /// The request, before the handler runs.
    #[default]
    Route,
    /// A database connection checkout: the `Db` extractor, `LazyDb::checkout`,
    /// the shard paths and the generated repositories.
    Database,
    /// A Redis session store operation.
    Redis,
    /// An outbound call through `http_client::Client`.
    Http,
}

impl FaultTarget {
    /// The config name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Database => "database",
            Self::Redis => "redis",
            Self::Http => "http",
        }
    }
}

/// What a fault does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FaultKind {
    /// Wait `latency_ms`, then continue.
    Latency,
    /// Fail. A route fault returns `status`. A dependency fault returns an
    /// error.
    Error,
}

impl FaultInjectionConfig {
    /// Validate the section for `profile`.
    ///
    /// A disabled section is not checked, so a draft can stay in the file.
    ///
    /// # Errors
    ///
    /// Returns the reason when the profile is `prod` and
    /// `allow_in_production` is not set, or when a value is out of range.
    pub fn validate(&self, profile: Option<&str>) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if is_production(profile) && !self.allow_in_production {
            return Err(
                "fault_injection.enabled = true is refused in prod (or with no profile); \
                 set fault_injection.allow_in_production = true to allow it"
                    .to_owned(),
            );
        }
        if self.faults.len() > MAX_RULES {
            return Err(format!(
                "fault_injection.faults has {} rules; the limit is {MAX_RULES}",
                self.faults.len()
            ));
        }
        self.stop.validate()?;
        for (index, rule) in self.faults.iter().enumerate() {
            rule.validate()
                .map_err(|error| format!("fault_injection.faults[{index}]: {error}"))?;
        }
        Ok(())
    }
}

impl FaultStopConfig {
    fn validate(&self) -> Result<(), String> {
        crate::slo::objective_to_ppm(self.objective)
            .map_err(|error| format!("fault_injection.stop.{error}"))?;
        if !self.max_burn_rate.is_finite()
            || self.max_burn_rate <= 0.0
            || self.max_burn_rate > 1_000.0
        {
            return Err(format!(
                "fault_injection.stop.max_burn_rate = {} must be greater than 0 and at most 1000",
                self.max_burn_rate
            ));
        }
        if self.window_secs == 0 {
            return Err("fault_injection.stop.window_secs must be greater than 0".to_owned());
        }
        if self.min_requests == 0 || self.min_requests > MAX_MIN_REQUESTS {
            return Err(format!(
                "fault_injection.stop.min_requests = {} must be from 1 to {MAX_MIN_REQUESTS}",
                self.min_requests
            ));
        }
        if self.max_error_ppm() >= crate::slo::PPM {
            return Err(
                "fault_injection.stop: objective and max_burn_rate allow 100% errors, so the \
                 stop condition can never trip; lower max_burn_rate"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// The largest error ratio, in ppm, that does not stop the faults:
    /// `budget * max_burn_rate`, to the nearest ppm, at most 100%.
    pub(crate) fn max_error_ppm(&self) -> u32 {
        let budget = crate::slo::objective_to_ppm(self.objective)
            .map_or(0, |objective| crate::slo::PPM.saturating_sub(objective));
        let limit = (f64::from(budget) * self.max_burn_rate)
            .round()
            .clamp(0.0, f64::from(crate::slo::PPM));
        // The clamp keeps the value in [0, PPM], so the cast cannot truncate.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to [0, PPM]"
        )]
        let limit = limit as u32;
        limit
    }
}

impl FaultRule {
    fn validate(&self) -> Result<(), String> {
        if !self.rate.is_finite() || !(0.0..=1.0).contains(&self.rate) {
            return Err(format!("rate = {} must be from 0.0 to 1.0", self.rate));
        }
        for route in &self.routes {
            if !route.starts_with('/') {
                return Err(format!("routes pattern {route:?} must start with '/'"));
            }
        }
        match self.kind {
            FaultKind::Latency => {
                if self.latency_ms == 0 || self.latency_ms > MAX_LATENCY_MS {
                    return Err(format!(
                        "latency_ms = {} must be from 1 to {MAX_LATENCY_MS} for kind = \"latency\"",
                        self.latency_ms
                    ));
                }
            }
            FaultKind::Error => {
                if self.target == FaultTarget::Route && !(400..=599).contains(&self.status) {
                    return Err(format!("status = {} must be from 400 to 599", self.status));
                }
            }
        }
        Ok(())
    }

    /// The rate in parts per million.
    pub(crate) fn rate_ppm(&self) -> u32 {
        // Validation keeps the rate in [0, 1], so the cast cannot truncate.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rate is in [0, 1]"
        )]
        let ppm = (self.rate.clamp(0.0, 1.0) * f64::from(crate::slo::PPM)).round() as u32;
        ppm
    }
}

/// `true` for the `prod` profile and its `production` alias, in any case.
/// Also `true` when no profile is set: the check fails closed.
pub fn is_production(profile: Option<&str>) -> bool {
    profile.is_none_or(|profile| {
        profile.eq_ignore_ascii_case("prod") || profile.eq_ignore_ascii_case("production")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(rule: FaultRule) -> FaultInjectionConfig {
        FaultInjectionConfig {
            enabled: true,
            faults: vec![rule],
            ..FaultInjectionConfig::default()
        }
    }

    fn error_rule() -> FaultRule {
        FaultRule {
            rate: 0.5,
            ..FaultRule::default()
        }
    }

    #[test]
    fn prod_is_refused_without_the_override() {
        let config = enabled(error_rule());
        for profile in [Some("prod"), Some("production"), Some("Prod"), None] {
            let error = config.validate(profile).unwrap_err();
            assert!(
                error.contains("allow_in_production"),
                "{profile:?}: {error}"
            );
        }
    }

    #[test]
    fn prod_is_allowed_with_the_override() {
        let mut config = enabled(error_rule());
        config.allow_in_production = true;
        config.validate(Some("prod")).unwrap();
    }

    #[test]
    fn other_profiles_are_allowed() {
        let config = enabled(error_rule());
        for profile in [Some("dev"), Some("staging"), Some("test")] {
            config.validate(profile).unwrap();
        }
    }

    #[test]
    fn a_disabled_section_is_not_checked() {
        let mut config = enabled(FaultRule {
            rate: 7.0,
            ..FaultRule::default()
        });
        config.enabled = false;
        config.validate(Some("prod")).unwrap();
    }

    #[test]
    fn bad_values_are_refused() {
        let cases = [
            FaultRule {
                rate: 1.5,
                ..FaultRule::default()
            },
            FaultRule {
                rate: f64::NAN,
                ..FaultRule::default()
            },
            FaultRule {
                routes: vec!["api/*".to_owned()],
                ..error_rule()
            },
            FaultRule {
                kind: FaultKind::Latency,
                ..error_rule()
            },
            FaultRule {
                status: 200,
                ..error_rule()
            },
        ];
        for rule in cases {
            let config = enabled(rule.clone());
            assert!(config.validate(Some("staging")).is_err(), "{rule:?}");
        }
    }

    #[test]
    fn bad_stop_values_are_refused() {
        for stop in [
            FaultStopConfig {
                objective: 100.0,
                ..FaultStopConfig::default()
            },
            FaultStopConfig {
                max_burn_rate: 0.0,
                ..FaultStopConfig::default()
            },
            FaultStopConfig {
                window_secs: 0,
                ..FaultStopConfig::default()
            },
            FaultStopConfig {
                min_requests: 0,
                ..FaultStopConfig::default()
            },
            FaultStopConfig {
                min_requests: MAX_MIN_REQUESTS + 1,
                ..FaultStopConfig::default()
            },
            // A 1% budget at 100x allows 100% errors: it never trips.
            FaultStopConfig {
                max_burn_rate: 100.0,
                ..FaultStopConfig::default()
            },
        ] {
            let config = FaultInjectionConfig {
                stop: stop.clone(),
                ..enabled(error_rule())
            };
            assert!(config.validate(Some("staging")).is_err(), "{stop:?}");
        }
    }

    #[test]
    fn too_many_rules_are_refused() {
        let config = FaultInjectionConfig {
            faults: vec![error_rule(); MAX_RULES + 1],
            ..enabled(error_rule())
        };
        assert!(config.validate(Some("staging")).is_err());
    }

    #[test]
    fn the_burn_rate_keeps_its_precision() {
        let stop = FaultStopConfig {
            max_burn_rate: 0.05,
            ..FaultStopConfig::default()
        };
        // A 1% budget at 0.05x is 0.05% errors, not 0.1%.
        assert_eq!(stop.max_error_ppm(), 500);
    }

    #[test]
    fn the_default_stop_is_the_fast_page_burn_of_a_99_slo() {
        // 1% budget at 14.4x is 14.4% errors.
        assert_eq!(FaultStopConfig::default().max_error_ppm(), 144_000);
    }

    #[test]
    fn rates_convert_to_ppm() {
        let rule = |rate| FaultRule {
            rate,
            ..FaultRule::default()
        };
        assert_eq!(rule(0.0).rate_ppm(), 0);
        assert_eq!(rule(0.05).rate_ppm(), 50_000);
        assert_eq!(rule(1.0).rate_ppm(), 1_000_000);
    }

    #[test]
    fn the_section_parses_from_toml() {
        let config: FaultInjectionConfig = toml::from_str(
            r#"
            enabled = true
            [stop]
            objective = 99.9
            [[faults]]
            routes = ["/api/*"]
            target = "database"
            kind = "latency"
            rate = 0.1
            latency_ms = 200
            "#,
        )
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.faults[0].target, FaultTarget::Database);
        assert_eq!(config.faults[0].kind, FaultKind::Latency);
        config.validate(Some("staging")).unwrap();
    }
}
