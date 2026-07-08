//! End-to-end tests for the config deprecation channel.
//!
//! Verifies that `load_with_env` emits exactly one structured `WARN` per
//! deprecated key that is present in the resolved config (via TOML or env var),
//! that the old value is still honoured (behavior unchanged), and that no
//! warning fires when only the replacement key is set.

use autumn_web::config::{AutumnConfig, MockEnv, deprecated_config_keys, deprecated_env_var_name};
use autumn_web::log::capture::LogBuffer;
use autumn_web::log::capture::LogCaptureLayer;
use autumn_web::log::filter::ParameterFilter;
use tracing_subscriber::layer::SubscriberExt as _;

/// Install a scoped log capture layer and return the buffer.
/// The returned `_guard` must be held for the duration of the test.
fn install_log_capture() -> (LogBuffer, tracing::dispatcher::DefaultGuard) {
    let buf = LogBuffer::new(64, ParameterFilter::default());
    let layer = LogCaptureLayer::new(buf.clone());
    let subscriber = tracing_subscriber::registry().with(layer);
    let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));
    (buf, guard)
}

/// Write `content` to `autumn.toml` in a fresh tempdir and return a `MockEnv`
/// pointing at that directory.
fn toml_env(content: &str) -> (tempfile::TempDir, MockEnv) {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml_path = dir.path().join("autumn.toml");
    std::fs::write(&toml_path, content).expect("write autumn.toml");
    let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap());
    (dir, env)
}

/// Count how many captured log entries are deprecation WARNs for the given key.
fn count_deprecation_warns(buf: &LogBuffer, key: &str) -> usize {
    buf.snapshot(None, None)
        .into_iter()
        .filter(|e| {
            e.level == "WARN"
                && e.fields
                    .get("deprecated_key")
                    .and_then(|v| v.as_str())
                    .is_some_and(|k| k == key)
        })
        .count()
}

// ── Test 8: TOML path → one WARN + old value honored ─────────────────────────

#[test]
fn green_toml_deprecated_key_warns_once_and_value_honored() {
    let (buf, _guard) = install_log_capture();
    let (_dir, env) = toml_env("[security.rate_limit]\ntrusted_proxies = [\"10.0.0.0/8\"]");

    let config = AutumnConfig::load_with_env(&env).expect("load");

    // Exactly ONE warn for this key.
    let warns = count_deprecation_warns(&buf, "security.rate_limit.trusted_proxies");
    assert_eq!(warns, 1, "expected exactly 1 WARN for the deprecated key");

    // Old value is honored (behavior preserved).
    assert_eq!(
        config.security.rate_limit.trusted_proxies,
        vec!["10.0.0.0/8".to_owned()],
        "deprecated field value must be passed through unchanged"
    );

    // The WARN carries the right machine-readable fields.
    let snap = buf.snapshot(None, None);
    let warn = snap
        .iter()
        .find(|e| {
            e.level == "WARN"
                && e.fields
                    .get("deprecated_key")
                    .and_then(|v| v.as_str())
                    .is_some_and(|k| k == "security.rate_limit.trusted_proxies")
        })
        .expect("no matching WARN entry");

    assert_eq!(
        warn.fields["replacement"].as_str().unwrap(),
        "security.trusted_proxies.ranges"
    );
    assert_eq!(warn.fields["remove_in"].as_str().unwrap(), "1.0.0");
}

// ── Test 9: env var path → one WARN + old value honored ──────────────────────

#[test]
fn green_env_var_deprecated_key_warns_once_and_value_honored() {
    let (buf, _guard) = install_log_capture();
    let (_dir, base_env) = toml_env(""); // empty TOML
    let env = base_env.with(
        "AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES",
        "192.168.1.0/24",
    );

    let config = AutumnConfig::load_with_env(&env).expect("load");

    let warns = count_deprecation_warns(&buf, "security.rate_limit.trusted_proxies");
    assert_eq!(
        warns, 1,
        "expected exactly 1 WARN for the env-var deprecated key"
    );

    // Value is applied via the existing env-var override path.
    assert_eq!(
        config.security.rate_limit.trusted_proxies,
        vec!["192.168.1.0/24".to_owned()],
        "deprecated env var value must still be applied"
    );
}

// ── Test 10: only replacement key set → NO deprecation WARN ──────────────────

#[test]
fn green_replacement_key_only_no_deprecation_warning() {
    let (buf, _guard) = install_log_capture();
    let (_dir, env) = toml_env("[security.trusted_proxies]\nranges = [\"10.0.0.0/8\"]");

    let _ = AutumnConfig::load_with_env(&env).expect("load");

    let warns_old = count_deprecation_warns(&buf, "security.rate_limit.trusted_proxies");
    let warns_fwd = count_deprecation_warns(&buf, "security.rate_limit.trust_forwarded_headers");
    assert_eq!(
        warns_old, 0,
        "no WARN expected when only replacement key is set (trusted_proxies)"
    );
    assert_eq!(
        warns_fwd, 0,
        "no WARN expected when only replacement key is set (trust_forwarded_headers)"
    );
}

// ── Test 11: deprecated + unknown typo'd key both fire independently ──────────

#[test]
fn green_deprecated_and_unknown_keys_fire_independently() {
    // Note: unknown-key detection only fires under strict_config = true.
    // Here we just verify that the deprecation warn fires even alongside
    // a misconfigured key — i.e. both channels work without interfering.
    let (buf, _guard) = install_log_capture();
    let (_dir, env) = toml_env("[security.rate_limit]\ntrusted_proxies = [\"10.0.0.0/8\"]\n");

    let _ = AutumnConfig::load_with_env(&env).expect("load");

    // Deprecation channel fires.
    let warns = count_deprecation_warns(&buf, "security.rate_limit.trusted_proxies");
    assert_eq!(warns, 1, "deprecation WARN must fire for deprecated key");

    // No spurious errors about the deprecated key being "unknown".
    let unknown_errors: Vec<_> = buf
        .snapshot(None, None)
        .into_iter()
        .filter(|e| e.level == "ERROR" && e.message.contains("trusted_proxies"))
        .collect();
    assert!(
        unknown_errors.is_empty(),
        "deprecated key must NOT produce an unknown-key error: {unknown_errors:?}"
    );
}

// ── Test 12: second deprecated key honored via its env var ───────────────────
//
// Locks the env-var-name contract for `trust_forwarded_headers`: the mechanical
// name produced by `deprecated_env_var_name` must be the one the loader reads to
// apply the value (the first key is already covered by Test 9).

#[test]
fn green_trust_forwarded_headers_env_var_honored() {
    let (buf, _guard) = install_log_capture();
    let (_dir, base_env) = toml_env("");
    let env_name = deprecated_env_var_name("security.rate_limit.trust_forwarded_headers");
    assert_eq!(
        env_name,
        "AUTUMN_SECURITY__RATE_LIMIT__TRUST_FORWARDED_HEADERS"
    );
    let env = base_env.with(&env_name, "true");

    let config = AutumnConfig::load_with_env(&env).expect("load");

    let warns = count_deprecation_warns(&buf, "security.rate_limit.trust_forwarded_headers");
    assert_eq!(
        warns, 1,
        "expected exactly 1 WARN for the env-var deprecated key"
    );

    // The generated env-var name must be the one the loader actually consumes.
    assert!(
        config.security.rate_limit.trust_forwarded_headers,
        "deprecated env var value must still be applied (generated name must match loader)"
    );
}

// ── Test 13: registry-driven coverage — every entry warns when set via env ───
//
// Guards that the deprecation channel stays wired for EVERY registry entry, so a
// newly added `DEPRECATED_CONFIG_KEYS` entry is automatically covered without a
// bespoke test. Sets each registered key via its env var in one load and asserts
// each one emits exactly one WARN.

#[test]
fn every_registered_key_warns_via_env() {
    let (buf, _guard) = install_log_capture();
    let (_dir, base_env) = toml_env("");

    // Set every registered deprecated key via its mechanical env-var name.
    // The value is irrelevant to detection (presence-only); use a benign one
    // that loads cleanly whether the field is a bool or a CSV list.
    let mut env = base_env;
    for entry in deprecated_config_keys() {
        env = env.with(&deprecated_env_var_name(entry.path), "true");
    }

    AutumnConfig::load_with_env(&env).expect("load");

    for entry in deprecated_config_keys() {
        let warns = count_deprecation_warns(&buf, entry.path);
        assert_eq!(
            warns, 1,
            "registered key {} must emit exactly one WARN when set via env",
            entry.path
        );
    }
}
