//! Structured logging initialization via `tracing-subscriber`.
//!
//! Call [`init`] once, early in application startup (after loading config),
//! to install the global tracing subscriber. The subscriber respects
//! [`LogConfig`] to choose between human-readable and JSON output, and
//! uses [`tracing_subscriber::EnvFilter`] to parse the configured log level directive.
//!
//! In normal usage, [`AppBuilder::run`](crate::app::AppBuilder::run) calls
//! [`init`] automatically. You only need to call it directly in test
//! harnesses or custom entry points.

use crate::config::{LogConfig, TelemetryConfig};

/// Initialize the tracing subscriber based on logging configuration alone.
///
/// Must be called **once**, early in application startup -- before any
/// `tracing::info!` / `tracing::debug!` calls. In normal usage,
/// [`AppBuilder::run`](crate::app::AppBuilder::run) calls this
/// automatically.
///
/// # Panics
///
/// Panics if called a second time. The global tracing subscriber can
/// only be set once per process.
///
#[allow(dead_code)]
pub fn init(config: &LogConfig) {
    let profile = crate::config::resolve_profile(&crate::config::OsEnv);
    let _ = init_with_telemetry(config, &TelemetryConfig::default(), Some(profile.as_str()))
        .unwrap_or_else(|error| panic!("failed to initialize logging: {error}"));
}

/// Initialize logging plus optional framework-managed telemetry.
///
/// Returns a guard that must stay alive for as long as OTLP export should stay
/// active so batched spans can flush cleanly on shutdown.
///
/// # Errors
///
/// Returns [`crate::telemetry::TelemetryInitError`] when the telemetry plan is
/// invalid or the tracing subscriber/exporter fails to initialize.
pub fn init_with_telemetry(
    config: &LogConfig,
    telemetry: &TelemetryConfig,
    profile: Option<&str>,
) -> Result<crate::telemetry::TelemetryGuard, crate::telemetry::TelemetryInitError> {
    crate::telemetry::init(config, telemetry, profile)
}

/// Returns `true` when `AUTUMN_ENV` is set to `"production"`
/// (case-insensitive).
#[cfg(test)]
fn is_production() -> bool {
    std::env::var("AUTUMN_ENV").is_ok_and(|v| v.eq_ignore_ascii_case("production"))
}

#[cfg(test)]
mod tests {

    // We cannot call `init()` in standard unit tests because the global subscriber
    // can only be set once per process and other tests may have already set it.
    // Instead, we use `rusty_fork_test` to run tests that call `init()` in a separate process.
    use rusty_fork::rusty_fork_test;

    rusty_fork_test! {
        #[test]
        fn init_succeeds_first_time() {
            let config = LogConfig {
                level: "debug".to_owned(),
                format: LogFormat::Pretty,
                ..LogConfig::default()
            };
            init(&config);
        }

        #[test]
        fn init_panics_on_second_call() {
            let config = LogConfig {
                level: "debug".to_owned(),
                format: LogFormat::Pretty,
                ..LogConfig::default()
            };
            init(&config); // Sets it successfully

            let result = std::panic::catch_unwind(|| {
                init(&config); // Should definitely panic
            });

            assert!(result.is_err(), "init did not panic on second call");

            let err = result.unwrap_err();
            let msg = err.downcast_ref::<&str>().map_or_else(|| err.downcast_ref::<String>().map_or("unknown", |s| s.as_str()), |s| *s);
            assert!(msg.contains("failed to initialize logging"), "Unexpected panic message: {msg}");
        }

        #[test]
        fn init_with_json_format_succeeds() {
            let config = LogConfig {
                level: "debug".to_owned(),
                format: LogFormat::Json,
                ..LogConfig::default()
            };
            init(&config);
        }

        #[test]
        fn init_with_auto_format_succeeds() {
            let config = LogConfig {
                level: "debug".to_owned(),
                format: LogFormat::Auto,
                ..LogConfig::default()
            };
            init(&config);
        }

        #[test]
        fn init_with_invalid_level_falls_back_without_panic() {
            let config = LogConfig {
                level: "invalid_level_format_[without_equal]".to_owned(),
                format: LogFormat::Pretty,
                ..LogConfig::default()
            };
            init(&config);
        }

    }

    use super::*;
    use crate::config::{LogConfig, LogFormat};

    #[test]
    fn is_production_false_by_default() {
        // In tests AUTUMN_ENV is not normally set, so this should be false.
        // (We don't mutate env vars here to avoid cross-test interference.)
        assert!(!is_production());
    }

    // We cannot call `init()` in unit tests because the global subscriber
    // can only be set once per process and other tests may have already
    // set it. Instead, test the format selection logic directly.

    #[test]
    fn auto_format_is_not_json_in_non_production() {
        // Without AUTUMN_ENV=production, Auto should resolve to pretty (not json).
        let use_json = match LogFormat::Auto {
            LogFormat::Auto => is_production(),
            LogFormat::Pretty => false,
            LogFormat::Json => true,
        };
        assert!(!use_json);
    }

    #[test]
    fn pretty_format_is_never_json() {
        let use_json = match LogFormat::Pretty {
            LogFormat::Auto => is_production(),
            LogFormat::Pretty => false,
            LogFormat::Json => true,
        };
        assert!(!use_json);
    }

    #[test]
    fn json_format_is_always_json() {
        let use_json = match LogFormat::Json {
            LogFormat::Auto => is_production(),
            LogFormat::Pretty => false,
            LogFormat::Json => true,
        };
        assert!(use_json);
    }

    #[test]
    fn valid_filter_parses() {
        // Ensure that valid EnvFilter directives don't trigger the fallback.
        let config = LogConfig {
            level: "debug".to_owned(),
            format: LogFormat::Auto,
            ..LogConfig::default()
        };
        let filter = tracing_subscriber::EnvFilter::try_new(&config.level);
        assert!(filter.is_ok());
    }

    #[test]
    fn invalid_filter_falls_back() {
        // An invalid directive should fail to parse (triggering the
        // eprintln fallback in init).
        let filter = tracing_subscriber::EnvFilter::try_new("not_a_valid_[directive");
        assert!(filter.is_err());
    }

    #[test]
    fn test_init_with_telemetry_forwards_to_telemetry_init() {
        use crate::config::{TelemetryConfig, TelemetryProtocol};

        // We know that if we pass an invalid strict telemetry config without an endpoint,
        // it will return an error without touching global subscriber state.
        // Let's use this to verify `init_with_telemetry` forwards correctly.
        let log_config = LogConfig {
            level: "debug".to_owned(),
            format: LogFormat::Pretty,
            ..LogConfig::default()
        };
        let telemetry_config = TelemetryConfig {
            enabled: true,
            strict: true, // strict mode to ensure we get an error
            service_name: "test".to_owned(),
            service_namespace: None,
            service_version: "1.0.0".to_owned(),
            environment: "test".to_owned(),
            otlp_endpoint: None,
            protocol: TelemetryProtocol::Grpc,
            sample_ratio: 1.0,
        };

        // This should return an error because it's enabled but no endpoint is provided (assuming OTLP feature).
        // If OTLP feature is not enabled, it returns `FeatureDisabled`.
        // If OTLP feature is enabled, it returns `MissingEndpoint`.
        // In either case, it's an error.
        let result = init_with_telemetry(&log_config, &telemetry_config, None);
        assert!(result.is_err());
    }
}
