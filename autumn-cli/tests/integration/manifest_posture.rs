//! Security-posture manifest (#1627): dump-contract tests for the `declared`
//! CSRF / security-headers dimensions.
//!
//! `autumn routes audit` builds those dimensions from the resolved security
//! configuration the app emits across the `AUTUMN_DUMP_ROUTES` /
//! `AUTUMN_DUMP_SECURITY` process boundary. The manifest-envelope assembly lives
//! in the (binary-only) `autumn-cli` crate and is covered by unit tests next to
//! `build_manifest`; these integration tests exercise the *other* half — the
//! `autumn_web` side that snapshots the config into the wire
//! [`SecurityDump`](autumn_web::route_listing::SecurityDump) and guarantees it is
//! sorted and byte-deterministic (the property the whole manifest's determinism
//! rests on).

use autumn_web::config::AutumnConfig;
use autumn_web::route_listing::SecurityDump;

/// Serialize a dump the same way the app does before writing the marker line.
fn wire(dump: &SecurityDump) -> String {
    serde_json::to_string(dump).unwrap()
}

/// Baseline (AC-1/AC-3): a default config snapshots to the expected declared
/// posture — CSRF disabled, DENY frame options, a non-empty resolved CSP.
#[test]
fn default_config_snapshots_expected_declared_posture() {
    let dump = SecurityDump::from_config(&AutumnConfig::default());

    assert!(!dump.csrf.enabled, "CSRF is off by default");
    assert_eq!(dump.headers.x_frame_options, "DENY");
    assert!(
        dump.headers
            .content_security_policy
            .contains("default-src 'self'"),
        "CSP must be the resolved template, not a sentinel: {:?}",
        dump.headers.content_security_policy
    );
    // Default safe methods are present and sorted.
    assert_eq!(
        dump.csrf.safe_methods,
        vec!["GET", "HEAD", "OPTIONS", "TRACE"]
    );
    assert!(dump.csrf.exempt_paths.is_empty());
}

/// AC-6 determinism: the wire snapshot is byte-identical across repeated builds
/// and independent of config source ordering (lists are sorted on the way out).
#[test]
fn wire_snapshot_is_byte_deterministic_and_sorted() {
    let mut a = AutumnConfig::default();
    a.security.csrf.exempt_paths = vec!["/webhooks/".to_owned(), "/api/".to_owned()];
    let mut b = AutumnConfig::default();
    // Same set, opposite source order.
    b.security.csrf.exempt_paths = vec!["/api/".to_owned(), "/webhooks/".to_owned()];

    let da = SecurityDump::from_config(&a);
    let db = SecurityDump::from_config(&b);

    assert_eq!(da.csrf.exempt_paths, vec!["/api/", "/webhooks/"], "sorted");
    assert_eq!(
        wire(&da),
        wire(&db),
        "source ordering must not affect the wire snapshot"
    );
    // Repeated snapshot of the same config is identical.
    assert_eq!(wire(&SecurityDump::from_config(&a)), wire(&da));
}

/// AC-2 falsifiability at the dump boundary: enabling CSRF and adding an exempt
/// prefix are both visible, isolated changes in the wire snapshot.
#[test]
fn csrf_config_changes_are_visible_in_the_wire_snapshot() {
    let base = SecurityDump::from_config(&AutumnConfig::default());

    let mut enabled = AutumnConfig::default();
    enabled.security.csrf.enabled = true;
    let enabled = SecurityDump::from_config(&enabled);
    assert!(!base.csrf.enabled && enabled.csrf.enabled);
    // Only the flag moved; headers untouched.
    assert_eq!(wire(&base.headers_only()), wire(&enabled.headers_only()));

    let mut exempt = AutumnConfig::default();
    exempt.security.csrf.exempt_paths = vec!["/api/".to_owned()];
    let exempt = SecurityDump::from_config(&exempt);
    assert_eq!(exempt.csrf.exempt_paths, vec!["/api/"]);
    assert!(base.csrf.exempt_paths.is_empty());
}

/// AC-3 falsifiability at the dump boundary: weakening `x_frame_options` and
/// emptying the CSP both surface in the wire snapshot without disturbing CSRF.
#[test]
fn header_config_changes_are_visible_in_the_wire_snapshot() {
    let base = SecurityDump::from_config(&AutumnConfig::default());

    let mut weak = AutumnConfig::default();
    weak.security.headers.x_frame_options = "SAMEORIGIN".to_owned();
    let weak = SecurityDump::from_config(&weak);
    assert_eq!(base.headers.x_frame_options, "DENY");
    assert_eq!(weak.headers.x_frame_options, "SAMEORIGIN");
    // CSRF section unaffected.
    assert_eq!(base.csrf, weak.csrf);

    let mut no_csp = AutumnConfig::default();
    no_csp.security.headers.content_security_policy = String::new();
    let no_csp = SecurityDump::from_config(&no_csp);
    assert!(!base.headers.content_security_policy.is_empty());
    assert!(no_csp.headers.content_security_policy.is_empty());
}

/// Finding B: runtime's `apply_csrf_middleware` exempts every configured webhook
/// endpoint path, so the wire snapshot must fold those paths into
/// `csrf.exempt_paths` (sorted + deduped) even when they are absent from
/// `security.csrf.exempt_paths` — otherwise the manifest claims CSRF on a route
/// runtime never enforces.
#[test]
fn webhook_endpoint_paths_are_folded_into_csrf_exempt_paths() {
    let mut config = AutumnConfig::default();
    config.security.webhooks.endpoints = vec![autumn_web::webhook::WebhookEndpointConfig {
        path: "/webhooks/stripe".to_owned(),
        ..Default::default()
    }];
    // The webhook path is deliberately NOT duplicated in csrf.exempt_paths.
    assert!(config.security.csrf.exempt_paths.is_empty());

    let dump = SecurityDump::from_config(&config);
    assert!(
        dump.csrf
            .exempt_paths
            .iter()
            .any(|p| p == "/webhooks/stripe"),
        "webhook endpoint path must be a CSRF exempt path: {:?}",
        dump.csrf.exempt_paths
    );
    let mut sorted = dump.csrf.exempt_paths.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(dump.csrf.exempt_paths, sorted, "sorted + deduped");
}

/// Small local helper so the CSRF-isolation assertion above can compare just the
/// headers half of two snapshots.
trait HeadersOnly {
    fn headers_only(&self) -> SecurityDump;
}

impl HeadersOnly for SecurityDump {
    fn headers_only(&self) -> SecurityDump {
        Self {
            csrf: autumn_web::route_listing::CsrfDump {
                enabled: false,
                safe_methods: Vec::new(),
                exempt_paths: Vec::new(),
            },
            headers: self.headers.clone(),
        }
    }
}
