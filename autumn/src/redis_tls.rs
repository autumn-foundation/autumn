//! Process-wide rustls `CryptoProvider` guard for TLS Redis URLs.
//!
//! `redis`'s `tokio-rustls-comp` feature builds its TLS `ClientConfig` through
//! the short-form `rustls::ClientConfig::builder()`, which resolves its crypto
//! provider from process-global state. That call does **not** return an error
//! when it cannot resolve one — it *panics*:
//!
//! ```text
//! Could not automatically determine the process-level CryptoProvider from
//! Rustls crate features. Call CryptoProvider::install_default() before this
//! point ...
//! ```
//!
//! rustls resolves implicitly only while exactly ONE of its `ring` /
//! `aws-lc-rs` features is enabled. Autumn pins `ring` everywhere, but Cargo
//! unifies features across the whole dependency graph, so any dependency that
//! turns on `aws-lc-rs` (`telemetry-otlp` alone is enough) makes the choice
//! ambiguous — and then every `rediss://` connection panics. Not a test
//! artifact: the `azure-container-apps` release target provisions a Redis
//! Cache with `non_ssl_port_enabled = false`, so it only ever hands the app a
//! `rediss://` URL, and the panic lands at startup on a real deployment.
//!
//! See issue #2172.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rusty_fork::rusty_fork_test;

    /// A TLS Redis URL pointed at a port nothing listens on. None of the
    /// scenarios below dial: every constructor under test builds a *lazy*
    /// connection manager, so the URL is only ever parsed and classified.
    const TLS_URL: &str = "rediss://127.0.0.1:16380/";
    /// The plaintext counterpart, for the negative scenario.
    const PLAIN_URL: &str = "redis://127.0.0.1:16379/";

    /// Recursively collect every `.rs` file under `dir`.
    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// Every Redis client in this crate must be built through this module's
    /// guarded constructor, so a `rediss://` URL can never reach rustls with
    /// no process-level `CryptoProvider` installed.
    ///
    /// A source scan rather than only per-call-site runtime assertions,
    /// because the failure mode is a *missing* call: a new Redis-backed
    /// subsystem that opens its own client is exactly the regression #2172
    /// describes, and no runtime test of the subsystems that exist today can
    /// see it. It also covers the two sites a runtime test cannot reach
    /// cheaply — `job::start_redis_runtime` (needs a live `AppState`) and
    /// `session::resolve_backend_plan`'s URL validation.
    #[test]
    fn no_redis_client_is_opened_outside_the_tls_guard() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        assert!(!files.is_empty(), "no sources found under {}", src.display());
        files.sort();

        let mut offenders = Vec::new();
        for file in files {
            // This module *is* the guard: the one place allowed to call it.
            if file.file_name().is_some_and(|name| name == "redis_tls.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&file).expect("read source");
            for (index, line) in source.lines().enumerate() {
                let trimmed = line.trim_start();
                // Prose (doc comments, block-comment continuations) may quote
                // the banned call while explaining it.
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                if line.contains("Client::open(") {
                    offenders.push(format!(
                        "{}:{}: {}",
                        file.strip_prefix(&src).unwrap_or(&file).display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these sites open a Redis client directly instead of through \
             `crate::redis_tls::open_client`, so a `rediss://` URL reaches \
             rustls with no process-level CryptoProvider installed and panics \
             at startup (#2172):\n{}",
            offenders.join("\n")
        );
    }

    /// Run `body` inside a Tokio runtime: the lazy `ConnectionManager`s these
    /// constructors build need a reactor context to spawn their background
    /// reconnect task onto, even though they never dial.
    #[cfg(feature = "redis")]
    fn in_runtime(body: impl FnOnce()) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(async { body() });
    }

    /// Assert that a fresh process starts with no provider — otherwise every
    /// assertion below would pass vacuously.
    #[cfg(feature = "redis")]
    fn assert_no_provider_yet() {
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_none(),
            "this scenario must run in a fresh process (rusty_fork), or the \
             assertions below prove nothing"
        );
    }

    /// The `#[must_use]`-shaped assertion each TLS scenario ends on.
    #[cfg(feature = "redis")]
    fn assert_provider_installed(subsystem: &str) {
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "{subsystem} built a Redis client for a `rediss://` URL without \
             installing a process-level rustls CryptoProvider first; the next \
             rustls `ClientConfig::builder()` call panics instead of \
             connecting (#2172)"
        );
    }

    // Each scenario runs in its own forked process: installing a
    // `CryptoProvider` is a one-shot process-global side effect, so a second
    // in-process assertion would pass no matter which subsystem installed it.
    #[cfg(feature = "redis")]
    rusty_fork_test! {
        #[test]
        fn session_store_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::session::SessionConfig::default();
                config.backend = crate::session::SessionBackend::Redis;
                config.redis.url = Some(TLS_URL.to_owned());
                let _ = crate::session_redis::RedisStore::from_config(&config);
            });
            assert_provider_installed("the Redis session store");
        }

        // `channels` is `ws`-gated, so this scenario only exists in builds
        // that compile the Redis pub/sub backend at all.
        #[cfg(feature = "ws")]
        #[test]
        fn channels_backend_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::config::ChannelConfig::default();
                config.backend = crate::config::ChannelBackend::Redis;
                config.redis.url = Some(TLS_URL.to_owned());
                let _ = crate::channels::Channels::from_config(
                    &config,
                    tokio_util::sync::CancellationToken::new(),
                );
            });
            assert_provider_installed("the Redis channels backend");
        }

        #[test]
        fn idempotency_store_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::config::IdempotencyConfig::default();
                config.redis.url = Some(TLS_URL.to_owned());
                let _ = crate::idempotency::RedisIdempotencyStore::from_config(&config);
            });
            assert_provider_installed("the Redis idempotency store");
        }

        #[test]
        fn webhook_replay_store_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let config = crate::webhook::WebhookReplayRedisConfig {
                    url: Some(TLS_URL.to_owned()),
                    ..Default::default()
                };
                let _ = crate::webhook::RedisWebhookReplayStore::from_config(&config);
            });
            assert_provider_installed("the Redis webhook replay store");
        }

        #[test]
        fn job_tracking_store_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::config::JobConfig::default();
                config.redis.url = Some(TLS_URL.to_owned());
                let _ = crate::job_tracking::build_redis_tracking_store(&config);
            });
            assert_provider_installed("the Redis job tracking store");
        }

        #[test]
        fn rate_limit_backend_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::security::RateLimitConfig::default();
                config.backend = crate::security::RateLimitBackend::Redis;
                config.redis.url = Some(TLS_URL.to_owned());
                let _ = crate::security::rate_limit::RateLimitLayer::from_config(&config);
            });
            assert_provider_installed("the Redis rate-limit backend");
        }

        /// The negative half of the guarantee: a plaintext `redis://` URL
        /// never touches TLS, so claiming the process-wide default for it
        /// could pre-empt a later, unrelated attempt elsewhere in the process
        /// to install a *different* provider (e.g. `aws-lc-rs`) for something
        /// that actually needs one.
        #[test]
        fn a_plaintext_redis_url_leaves_the_process_provider_unset() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::session::SessionConfig::default();
                config.backend = crate::session::SessionBackend::Redis;
                config.redis.url = Some(PLAIN_URL.to_owned());
                let _ = crate::session_redis::RedisStore::from_config(&config);
            });
            assert!(
                rustls::crypto::CryptoProvider::get_default().is_none(),
                "a plaintext `redis://` connection must not claim the \
                 process-wide rustls default provider"
            );
        }
    }
}
