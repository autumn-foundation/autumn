//! Service level objectives declared in `autumn.toml` (issue #3069).
//!
//! Each `[[slo]]` table declares one objective:
//!
//! ```toml
//! [[slo]]
//! name = "availability"
//! objective = 99.9
//! sli = "availability"
//!
//! [[slo]]
//! name = "orders-latency"
//! objective = 99.0
//! sli = "latency"
//! route = "/api/orders"
//! threshold_ms = 250
//! ```
//!
//! The app does not read these tables at run time. `autumn slo generate`
//! reads them to write Prometheus rules, a Grafana dashboard and rollout
//! analysis templates. `autumn deploy` reads them to set the bake limits.
//!
//! [`validate`] converts the raw tables into [`Slo`] values. All objective
//! math uses integer parts per million, so generated thresholds are exact.

use std::fmt;

use serde::Deserialize;

/// One million: the denominator of every ratio in this module.
pub const PPM: u32 = 1_000_000;

/// Latency histogram bucket bounds, in milliseconds.
///
/// These match the `le` labels of `autumn_http_request_duration_seconds`
/// (issue #3064). A latency `threshold_ms` must be one of these values,
/// because Prometheus can only count requests at a bucket bound.
pub const LATENCY_BUCKETS_MS: [u64; 12] =
    [1, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

/// Maximum length of an SLO name.
pub const MAX_NAME_LEN: usize = 40;

/// The kind of service level indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SliKind {
    /// The ratio of responses that are not `5xx`.
    Availability,
    /// The ratio of requests that complete in `threshold_ms` or less.
    Latency,
}

/// One raw `[[slo]]` table from `autumn.toml`.
///
/// Call [`validate`] to get checked [`Slo`] values.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SloConfig {
    /// Unique name: lowercase letters, digits and `-`. It starts with a letter.
    pub name: String,
    /// Target percentage of good events, for example `99.9`.
    pub objective: f64,
    /// The indicator to measure.
    pub sli: SliKind,
    /// The matched route pattern, for example `/api/orders/{id}`.
    /// When unset, the SLO covers all routes.
    #[serde(default)]
    pub route: Option<String>,
    /// Latency limit in milliseconds. Required for `sli = "latency"`.
    /// Not allowed for `sli = "availability"`.
    #[serde(default)]
    pub threshold_ms: Option<u64>,
    /// Text for alert annotations and dashboard panels.
    #[serde(default)]
    pub description: Option<String>,
}

/// A checked service level indicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sli {
    /// Good events are responses that are not `5xx`.
    Availability {
        /// Route filter. `None` covers all routes.
        route: Option<String>,
    },
    /// Good events are requests that complete in `threshold_ms` or less.
    Latency {
        /// Route filter. `None` covers all routes.
        route: Option<String>,
        /// The limit. It is always one of [`LATENCY_BUCKETS_MS`].
        threshold_ms: u64,
    },
}

/// A checked service level objective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slo {
    /// The unique name.
    pub name: String,
    /// The objective in parts per million. `99.9` is `999_000`.
    pub objective_ppm: u32,
    /// The indicator.
    pub sli: Sli,
    /// Optional description.
    pub description: Option<String>,
}

/// A configuration error in one `[[slo]]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SloError {
    /// The SLO name, or its position when the name is not usable.
    pub slo: String,
    /// What is wrong, and how to correct it.
    pub message: String,
}

impl fmt::Display for SloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[[slo]] {}: {}", self.slo, self.message)
    }
}

impl std::error::Error for SloError {}

impl Slo {
    /// The error budget in parts per million. `99.9` gives `1_000`.
    #[must_use]
    pub const fn budget_ppm(&self) -> u32 {
        PPM - self.objective_ppm
    }

    /// The route filter, if any.
    #[must_use]
    pub fn route(&self) -> Option<&str> {
        match &self.sli {
            Sli::Availability { route } | Sli::Latency { route, .. } => route.as_deref(),
        }
    }
}

/// A multiwindow burn-rate alert from the Google SRE workbook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnWindow {
    /// The long window, as a Prometheus duration.
    pub long: &'static str,
    /// The short window, as a Prometheus duration.
    pub short: &'static str,
    /// The burn rate, in tenths. `144` is a burn rate of `14.4`.
    pub factor_tenths: u32,
    /// The alert severity label.
    pub severity: &'static str,
}

/// The three burn-rate alerts for each SLO.
///
/// Page at 14.4× over 1h/5m, page at 6× over 6h/30m, and open a ticket at
/// 1× over 3d/6h.
pub const BURN_WINDOWS: [BurnWindow; 3] = [
    BurnWindow {
        long: "1h",
        short: "5m",
        factor_tenths: 144,
        severity: "page",
    },
    BurnWindow {
        long: "6h",
        short: "30m",
        factor_tenths: 60,
        severity: "page",
    },
    BurnWindow {
        long: "3d",
        short: "6h",
        factor_tenths: 10,
        severity: "ticket",
    },
];

/// The burn rate, in tenths, above which a rollout fails.
///
/// This is the fast page rate (14.4×). The deploy bake, the Argo Rollouts
/// analysis and the Flagger analysis all use it.
pub const ROLLBACK_BURN_TENTHS: u32 = 144;

/// The largest error ratio, in ppm, that stays at or below `burn_tenths`.
///
/// The result is `budget_ppm * burn_tenths / 10`, rounded down and capped at
/// [`PPM`].
#[must_use]
pub fn max_error_ppm(budget_ppm: u32, burn_tenths: u32) -> u32 {
    let scaled = u64::from(budget_ppm) * u64::from(burn_tenths) / 10;
    u32::try_from(scaled).map_or(PPM, |v| v.min(PPM))
}

/// Write `numerator / 10^scale` as an exact decimal, with no trailing zeros.
///
/// `format_decimal(144, 4)` is `"0.0144"`. `format_decimal(10, 1)` is `"1"`.
#[must_use]
pub fn format_decimal(numerator: u64, scale: u32) -> String {
    let digits = numerator.to_string();
    let scale = scale as usize;
    if scale == 0 {
        return digits;
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// Check every `[[slo]]` table.
///
/// # Errors
///
/// Returns the first [`SloError`]: a bad name, a duplicate name, an
/// objective out of range or with more than four decimals, a bad route, or a
/// `threshold_ms` that is missing, not allowed, or not a bucket bound.
pub fn validate(configs: &[SloConfig]) -> Result<Vec<Slo>, SloError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut slos = Vec::with_capacity(configs.len());
    for (index, config) in configs.iter().enumerate() {
        let label = if config.name.is_empty() {
            format!("#{}", index + 1)
        } else {
            config.name.clone()
        };
        let fail = |message: String| SloError {
            slo: label.clone(),
            message,
        };
        check_name(&config.name).map_err(fail)?;
        if !seen.insert(config.name.as_str()) {
            return Err(fail(
                "duplicate name; each SLO needs a unique name".to_owned(),
            ));
        }
        let objective_ppm = objective_to_ppm(config.objective).map_err(fail)?;
        if let Some(description) = &config.description {
            check_description(description).map_err(fail)?;
        }
        let route = match config.route.as_deref() {
            None => None,
            Some(route) => Some(check_route(route).map_err(fail)?.to_owned()),
        };
        let sli = match (config.sli, config.threshold_ms) {
            (SliKind::Availability, None) => Sli::Availability { route },
            (SliKind::Availability, Some(_)) => {
                return Err(fail(
                    "threshold_ms is only for sli = \"latency\"; remove it".to_owned(),
                ));
            }
            (SliKind::Latency, None) => {
                return Err(fail(format!(
                    "sli = \"latency\" needs threshold_ms, one of {}",
                    bucket_list()
                )));
            }
            (SliKind::Latency, Some(threshold_ms)) => {
                if !LATENCY_BUCKETS_MS.contains(&threshold_ms) {
                    return Err(fail(format!(
                        "threshold_ms = {threshold_ms} is not a histogram bucket bound; \
                         use one of {}",
                        bucket_list()
                    )));
                }
                Sli::Latency {
                    route,
                    threshold_ms,
                }
            }
        };
        slos.push(Slo {
            name: config.name.clone(),
            objective_ppm,
            sli,
            description: config.description.clone(),
        });
    }
    Ok(slos)
}

fn check_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "name {name:?} is not valid; use 1 to {MAX_NAME_LEN} lowercase letters, \
             digits or '-', and start with a letter"
        ))
    }
}

/// A route goes into Prometheus label matchers, YAML block scalars, and
/// Argo Rollouts and Flagger templates. Allow printable ASCII only, and no
/// character or pair that one of them reads as syntax.
fn check_route(route: &str) -> Result<&str, String> {
    if !route.starts_with('/') {
        return Err(format!("route {route:?} must start with '/'"));
    }
    let bad_char = route
        .chars()
        .any(|c| !c.is_ascii_graphic() || matches!(c, '"' | '\\' | '`'));
    if bad_char || route.contains("{{") || route.contains("}}") {
        return Err(format!(
            "route {route:?} must hold printable ASCII only, with no space, '\"', '\\', \
             '`', '{{{{' or '}}}}'"
        ));
    }
    Ok(route)
}

/// A description goes into alert annotations, which Prometheus reads as Go
/// templates. Reject template braces and control characters.
fn check_description(description: &str) -> Result<(), String> {
    if description.contains("{{") || description.contains("}}") {
        return Err("description must not contain '{{' or '}}'".to_owned());
    }
    if description.chars().any(char::is_control) {
        return Err("description must not contain control characters".to_owned());
    }
    Ok(())
}

/// Convert a percentage to ppm. Reject values outside `(0, 100)` and values
/// with more than four decimals.
pub(crate) fn objective_to_ppm(objective: f64) -> Result<u32, String> {
    if !objective.is_finite() || objective <= 0.0 || objective >= 100.0 {
        return Err(format!(
            "objective = {objective} must be greater than 0 and less than 100"
        ));
    }
    let scaled = objective * 10_000.0;
    let rounded = scaled.round();
    if (scaled - rounded).abs() > 1e-6 {
        return Err(format!(
            "objective = {objective} has more than four decimal places"
        ));
    }
    // The range check above keeps `rounded` in 1..=999_999.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ppm = rounded as u32;
    if ppm == 0 || ppm >= PPM {
        return Err(format!(
            "objective = {objective} must be greater than 0 and less than 100"
        ));
    }
    Ok(ppm)
}

fn bucket_list() -> String {
    LATENCY_BUCKETS_MS
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn availability(name: &str, objective: f64) -> SloConfig {
        SloConfig {
            name: name.to_owned(),
            objective,
            sli: SliKind::Availability,
            route: None,
            threshold_ms: None,
            description: None,
        }
    }

    fn latency(name: &str, objective: f64, threshold_ms: Option<u64>) -> SloConfig {
        SloConfig {
            name: name.to_owned(),
            objective,
            sli: SliKind::Latency,
            route: Some("/api/orders".to_owned()),
            threshold_ms,
            description: None,
        }
    }

    fn error_of(configs: &[SloConfig]) -> String {
        validate(configs)
            .expect_err("the config must be rejected")
            .to_string()
    }

    #[test]
    fn a_valid_set_converts_to_ppm() {
        let slos = validate(&[
            availability("availability", 99.9),
            latency("orders-latency", 99.0, Some(250)),
        ])
        .expect("valid");
        assert_eq!(slos[0].objective_ppm, 999_000);
        assert_eq!(slos[0].budget_ppm(), 1_000);
        assert_eq!(slos[0].sli, Sli::Availability { route: None });
        assert_eq!(slos[1].objective_ppm, 990_000);
        assert_eq!(
            slos[1].sli,
            Sli::Latency {
                route: Some("/api/orders".to_owned()),
                threshold_ms: 250,
            }
        );
        assert_eq!(slos[1].route(), Some("/api/orders"));
    }

    #[test]
    fn an_empty_list_is_valid() {
        assert_eq!(validate(&[]), Ok(Vec::new()));
    }

    #[test]
    fn four_decimals_are_allowed_and_five_are_not() {
        let slos = validate(&[availability("a", 99.9995)]).expect("4 decimals");
        assert_eq!(slos[0].objective_ppm, 999_995);
        assert!(error_of(&[availability("a", 99.99995)]).contains("four decimal"));
    }

    #[test]
    fn objective_bounds_are_exclusive() {
        for bad in [0.0, 100.0, -1.0, 100.5, f64::NAN, f64::INFINITY] {
            let message = error_of(&[availability("a", bad)]);
            assert!(message.contains("objective"), "{bad}: {message}");
        }
        assert!(validate(&[availability("a", 0.0001)]).is_ok());
        assert!(validate(&[availability("a", 99.9999)]).is_ok());
    }

    #[test]
    fn names_are_checked() {
        for bad in ["", "Avail", "1st", "a_b", "a b", "-a", &"a".repeat(41)] {
            let message = error_of(&[availability(bad, 99.0)]);
            assert!(message.contains("name"), "{bad:?}: {message}");
        }
        assert!(validate(&[availability(&"a".repeat(40), 99.0)]).is_ok());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let message = error_of(&[availability("a", 99.0), availability("a", 99.5)]);
        assert!(message.contains("duplicate"), "{message}");
    }

    #[test]
    fn descriptions_are_checked() {
        for bad in ["See {{ the runbook }}", "a }} b", "line\nbreak"] {
            let mut config = availability("a", 99.0);
            config.description = Some(bad.to_owned());
            let message = error_of(&[config]);
            assert!(message.contains("description"), "{bad:?}: {message}");
        }
        let mut config = availability("a", 99.0);
        config.description = Some("Customers see \"orders\" {quickly}.".to_owned());
        assert!(validate(&[config]).is_ok());
    }

    #[test]
    fn route_patterns_with_parameters_are_allowed() {
        let mut config = availability("a", 99.0);
        config.route = Some("/api/orders/{id}".to_owned());
        assert!(validate(&[config]).is_ok());
    }

    #[test]
    fn routes_are_checked() {
        for bad in [
            "api",
            "/a\"b",
            "/a\\b",
            "/a\nb",
            "",
            "/a b",
            "/x/{{ query `up` }}",
            "/a}}",
            "/a`b",
            "/a\u{2028}b",
            "/caf\u{e9}",
        ] {
            let mut config = availability("a", 99.0);
            config.route = Some(bad.to_owned());
            let message = error_of(&[config]);
            assert!(message.contains("route"), "{bad:?}: {message}");
        }
    }

    #[test]
    fn latency_needs_a_bucket_threshold() {
        assert!(error_of(&[latency("l", 99.0, None)]).contains("threshold_ms"));
        let message = error_of(&[latency("l", 99.0, Some(300))]);
        assert!(message.contains("250, 500"), "{message}");
        for bound in LATENCY_BUCKETS_MS {
            assert!(
                validate(&[latency("l", 99.0, Some(bound))]).is_ok(),
                "{bound}"
            );
        }
    }

    #[test]
    fn availability_rejects_a_threshold() {
        let mut config = availability("a", 99.0);
        config.threshold_ms = Some(250);
        assert!(error_of(&[config]).contains("threshold_ms"));
    }

    #[test]
    fn the_error_names_the_slo() {
        let message = error_of(&[availability("checkout", 100.0)]);
        assert!(message.starts_with("[[slo]] checkout:"), "{message}");
    }

    #[test]
    fn max_error_ppm_scales_and_caps() {
        assert_eq!(max_error_ppm(1_000, ROLLBACK_BURN_TENTHS), 14_400);
        assert_eq!(max_error_ppm(1_000, 10), 1_000);
        assert_eq!(max_error_ppm(5, 144), 72);
        assert_eq!(max_error_ppm(100_000, 144), PPM);
        assert_eq!(max_error_ppm(PPM, 10), PPM);
    }

    #[test]
    fn format_decimal_is_exact() {
        assert_eq!(format_decimal(144, 4), "0.0144");
        assert_eq!(format_decimal(10, 1), "1");
        assert_eq!(format_decimal(144, 1), "14.4");
        assert_eq!(format_decimal(0, 3), "0");
        assert_eq!(format_decimal(1_000, 6), "0.001");
        assert_eq!(format_decimal(7, 0), "7");
        assert_eq!(format_decimal(1_440_000, 7), "0.144");
    }

    #[test]
    fn the_burn_windows_match_the_sre_workbook() {
        let rows: Vec<_> = BURN_WINDOWS
            .iter()
            .map(|w| (w.long, w.short, w.factor_tenths, w.severity))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("1h", "5m", 144, "page"),
                ("6h", "30m", 60, "page"),
                ("3d", "6h", 10, "ticket"),
            ]
        );
    }

    #[test]
    fn toml_tables_parse() {
        #[derive(Deserialize)]
        struct Root {
            slo: Vec<SloConfig>,
        }
        let root: Root = toml::from_str(
            "[[slo]]\nname = \"a\"\nobjective = 99.9\nsli = \"availability\"\n\n\
             [[slo]]\nname = \"l\"\nobjective = 99\nsli = \"latency\"\nroute = \"/x\"\nthreshold_ms = 100\n",
        )
        .expect("parse");
        assert_eq!(root.slo.len(), 2);
        assert_eq!(root.slo[1].sli, SliKind::Latency);
        assert!((root.slo[1].objective - 99.0).abs() < f64::EPSILON);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Every ppm objective converts back to the same ppm.
            #[test]
            fn ppm_round_trips(ppm in 1u32..PPM) {
                let objective = f64::from(ppm) / 10_000.0;
                let slos = validate(&[availability("a", objective)]).expect("valid");
                prop_assert_eq!(slos[0].objective_ppm, ppm);
                prop_assert_eq!(slos[0].budget_ppm() + ppm, PPM);
            }

            /// The cap holds and the result never falls when the burn rises.
            #[test]
            fn max_error_ppm_is_capped_and_monotonic(
                budget in 1u32..PPM,
                burn in 1u32..1_000,
            ) {
                let low = max_error_ppm(budget, burn);
                let high = max_error_ppm(budget, burn + 1);
                prop_assert!(low <= PPM);
                prop_assert!(low <= high);
            }

            /// `format_decimal` output parses back to the same value.
            #[test]
            fn format_decimal_round_trips(numerator in 0u64..10_000_000_000, scale in 0u32..9) {
                let text = format_decimal(numerator, scale);
                let parsed: f64 = text.parse().expect("number");
                #[allow(clippy::cast_precision_loss)]
                let expected = numerator as f64 / 10f64.powi(i32::try_from(scale).unwrap_or(0));
                prop_assert!((parsed - expected).abs() <= expected.abs().mul_add(1e-12, 1e-15));
                prop_assert!(!text.ends_with('.'));
                prop_assert!(!(text.contains('.') && text.ends_with('0')));
            }
        }
    }
}
