//! Spec for the `jwt` and `push` features.
//!
//! Both are default-on. A build without one must refuse the config that needs
//! it, rather than run with the feature silently dead. Each test asserts the
//! behaviour of the build it runs in, so the default `test` job covers the
//! feature-on side and the `jwt-push` lane of `test-features` covers the
//! feature-off side.

use autumn_web::config::{AutumnConfig, MockEnv};

/// Write `content` to `autumn.toml` in a fresh tempdir and return a `MockEnv`
/// pointing at that directory.
fn toml_env(content: &str) -> (tempfile::TempDir, MockEnv) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("autumn.toml"), content).expect("write autumn.toml");
    let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", dir.path().to_str().expect("utf-8"));
    (dir, env)
}

fn jwt_tenancy() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_owned();
    config.tenancy.jwt_secret = Some("secret".to_owned().into());
    config
}

/// A `[push]` table parses in every build, under strict config too, so a
/// config file that is valid today stays valid without the feature.
#[test]
fn push_section_parses_in_every_build() {
    let (_dir, env) = toml_env("[server]\nstrict_config = true\n\n[push]\nttl_secs = 600\n");
    // A custom profile has no profile defaults, so strict mode judges only
    // the file above (the `dev` defaults name `[storage]`, which a build
    // without `storage` reports as unknown).
    let env = env.with("AUTUMN_PROFILE", "inert-section-spec");
    let config = AutumnConfig::load_with_env(&env).expect("a [push] table must parse");

    let result = config.validate_push();
    if cfg!(feature = "push") {
        assert!(result.is_ok(), "push build: {result:?}");
    } else {
        let message = result
            .expect_err("a [push] table must stop the boot")
            .to_string();
        assert!(
            message.contains("`push` feature") && message.contains("--features push"),
            "the error must name the feature and the fix: {message}"
        );
    }
}

/// An `AUTUMN_PUSH__*` variable is never dropped without a word.
#[test]
fn push_env_var_is_refused_without_the_feature() {
    let mut config = AutumnConfig::default();
    config.apply_env_overrides_with_env(
        &MockEnv::new().with("AUTUMN_PUSH__SUBJECT", "mailto:ops@example.com"),
    );

    let result = config.validate_push();
    if cfg!(feature = "push") {
        assert!(result.is_ok(), "push build: {result:?}");
    } else {
        let message = result
            .expect_err("an AUTUMN_PUSH__* variable must stop the boot")
            .to_string();
        assert!(message.contains("AUTUMN_PUSH__*"), "{message}");
    }
}

/// The private key never shows in `Debug`, from the file or from the
/// environment, with or without the feature.
#[test]
fn push_private_key_is_redacted_in_every_build() {
    let (_dir, env) = toml_env("[push]\nprivate_key = \"file-held-private-key\"\n");
    let env = env.with("AUTUMN_PUSH__PUBLIC_KEY", "env-held-public-key");
    let from_file = AutumnConfig::load_with_env(&env).expect("load");
    let debugged = format!("{:?}", from_file.push);
    assert!(!debugged.contains("file-held-private-key"), "{debugged}");

    let mut from_env = AutumnConfig::default();
    from_env.apply_env_overrides_with_env(
        &MockEnv::new().with("AUTUMN_PUSH__PRIVATE_KEY", "env-held-private-key"),
    );
    let debugged = format!("{:?}", from_env.push);
    assert!(!debugged.contains("env-held-private-key"), "{debugged}");
}

/// No `[push]` and no variable: every build boots.
#[test]
fn no_push_config_boots_in_every_build() {
    let config = AutumnConfig::default();
    assert!(config.validate_push().is_ok());
}

/// `tenancy.source = "jwt"` boots only in a build that can verify the token.
#[test]
fn jwt_tenancy_source_needs_the_jwt_feature() {
    let result = jwt_tenancy().validate_tenancy_source();
    if cfg!(feature = "jwt") {
        assert!(result.is_ok(), "jwt build: {result:?}");
    } else {
        let message = result
            .expect_err("jwt tenancy must stop the boot")
            .to_string();
        assert!(
            message.contains("`jwt` feature") && message.contains("--features jwt"),
            "the error must name the feature and the fix: {message}"
        );
    }
}

/// The check reads only enabled tenancy, and only the `jwt` source.
#[test]
fn other_tenancy_configs_boot_in_every_build() {
    let mut disabled = jwt_tenancy();
    disabled.tenancy.enabled = false;
    assert!(disabled.validate_tenancy_source().is_ok());

    for source in ["header", "subdomain", "session"] {
        let mut config = jwt_tenancy();
        config.tenancy.source = source.to_owned();
        assert!(
            config.validate_tenancy_source().is_ok(),
            "source {source:?} needs no feature"
        );
    }
}

/// A router built without `run` skips the boot check. Tenant resolution must
/// then fail closed: a 500, never a tenant read from an unverified token.
#[cfg(not(feature = "jwt"))]
#[tokio::test]
async fn jwt_tenancy_fails_closed_without_the_feature() {
    // An unsigned token (`alg: none`) whose claim names a tenant.
    let token = "eyJhbGciOiJub25lIn0.eyJ0ZW5hbnRfaWQiOiJhY21lIn0.";
    let request = axum::http::Request::builder()
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .expect("request");
    let (mut parts, ()) = request.into_parts();

    let err = autumn_web::tenancy::extract_tenant_from_parts(&mut parts, &jwt_tenancy())
        .await
        .expect_err("no tenant may resolve without the jwt feature");
    assert_eq!(err.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(err.to_string().contains("--features jwt"), "{err}");
}
