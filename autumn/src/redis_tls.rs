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

/// Whether `url` needs the rustls `CryptoProvider` installed before a client
/// built from it can connect.
///
/// Answered by asking the `redis` crate to parse the URL and reporting
/// whether it resolved to a TLS address, rather than by matching schemes
/// here. That distinction matters: the first cut of this check lived in
/// `autumn-cache-redis` as `starts_with("rediss://")`, which silently missed
/// Valkey's `valkeys://` and any case variant the URL parser accepts. Reusing
/// `redis`'s own scheme table means a scheme it learns to treat as TLS is
/// covered the day the dependency is bumped, with nothing to keep in sync.
///
/// A plain `redis://`/`valkey://` URL never touches TLS, so it must **not**
/// claim the process-wide default: doing so could pre-empt a later, unrelated
/// attempt elsewhere in the process to install a different provider (e.g.
/// `aws-lc-rs`) for something that actually needs one. A URL `redis` cannot
/// parse at all is likewise not TLS — [`open_client`] is about to reject it
/// with that same parse error.
#[must_use]
pub fn url_needs_tls_crypto_provider(url: &str) -> bool {
    use redis::IntoConnectionInfo as _;

    url.into_connection_info()
        .is_ok_and(|info| matches!(info.addr(), redis::ConnectionAddr::TcpTls { .. }))
}

/// Install `ring` as the process-wide default `CryptoProvider` if `url` is a
/// TLS Redis URL and nothing has installed one yet.
///
/// Call this before handing a URL to any Redis client constructor this module
/// does not own — [`open_client`] already does it for you. Idempotent and
/// safe to race: an existing provider is always kept, because the requirement
/// is only that *a* default exists before rustls looks for one, and silently
/// replacing an application's deliberate choice would be worse than the panic
/// this prevents.
pub fn ensure_tls_crypto_provider(url: &str) {
    if url_needs_tls_crypto_provider(url) && rustls::crypto::CryptoProvider::get_default().is_none()
    {
        // Errors only if another thread won the race to install one, which
        // satisfies the requirement just as well. `ring` matches the backend
        // every other rustls call site in this workspace already pins.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Open a Redis client, installing the rustls `CryptoProvider` first when
/// `url` is a TLS (`rediss://` / `valkeys://`) endpoint.
///
/// The only sanctioned way to build a [`redis::Client`] in this crate — a
/// unit test scans `src/` to keep it that way. See the module docs for why a
/// bare `redis::Client::open` is a latent startup panic (#2172).
///
/// # Errors
///
/// Returns the `redis` crate's parse error when `url` is not a valid Redis
/// connection URL. No connection is attempted here.
pub fn open_client(url: &str) -> redis::RedisResult<redis::Client> {
    ensure_tls_crypto_provider(url);
    redis::Client::open(url)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{open_client, url_needs_tls_crypto_provider};

    use rusty_fork::rusty_fork_test;

    /// A TLS Redis URL pointed at a port nothing listens on. None of the
    /// scenarios below dial: every constructor under test builds a *lazy*
    /// connection manager, so the URL is only ever parsed and classified.
    const TLS_URL: &str = "rediss://127.0.0.1:16380/";
    /// The plaintext counterpart, for the negative scenario.
    const PLAIN_URL: &str = "redis://127.0.0.1:16379/";

    #[test]
    fn plain_redis_urls_do_not_claim_the_process_wide_tls_provider() {
        assert!(!url_needs_tls_crypto_provider("redis://127.0.0.1:6379/"));
        assert!(!url_needs_tls_crypto_provider(
            "redis://user:pass@host:6379/0"
        ));
        assert!(!url_needs_tls_crypto_provider("valkey://127.0.0.1:6379/"));
        assert!(!url_needs_tls_crypto_provider("unix:///run/redis.sock"));
        // Unparseable input is not TLS: `open_client` is about to reject it.
        assert!(!url_needs_tls_crypto_provider(""));
        assert!(!url_needs_tls_crypto_provider("not a redis url"));
        assert!(!url_needs_tls_crypto_provider("https://example.com"));
    }

    #[test]
    fn rediss_urls_claim_the_tls_provider() {
        assert!(url_needs_tls_crypto_provider("rediss://127.0.0.1:6380/"));
        assert!(url_needs_tls_crypto_provider(
            "rediss://user:pass@cache.redis.cache.windows.net:6380/0"
        ));
    }

    #[test]
    fn valkeys_urls_also_claim_the_tls_provider() {
        // Regression: `redis::Client::open` (via the `url` crate, which
        // lowercases the scheme while parsing) treats both `rediss://` and
        // Valkey's `valkeys://` as TLS — a literal `starts_with("rediss://")`
        // check missed `valkeys://` entirely, so such a connection could
        // reach `ClientConfig::builder()` with no provider installed.
        assert!(url_needs_tls_crypto_provider("valkeys://127.0.0.1:6380/"));
    }

    #[test]
    fn tls_scheme_matching_is_case_insensitive() {
        // Regression: a literal `starts_with("rediss://")` also missed a
        // case-variant scheme such as `REDISS://` — valid per URL parsing
        // (schemes are normalized to lowercase) and accepted the same way by
        // `redis::Client::open`, but invisible to a case-sensitive check.
        assert!(url_needs_tls_crypto_provider("REDISS://127.0.0.1:6380/"));
        assert!(url_needs_tls_crypto_provider("Valkeys://127.0.0.1:6380/"));
    }

    #[test]
    fn a_scheme_that_merely_starts_with_a_tls_scheme_is_not_tls() {
        // `redisstore://` shares the `rediss` prefix but is not a Redis
        // scheme at all; a prefix match would have claimed the provider for
        // it. `redis`'s parser rejects it outright, so it is not TLS.
        assert!(!url_needs_tls_crypto_provider("redisstore://host:6379/"));
    }

    #[test]
    fn open_client_rejects_a_malformed_url_without_panicking() {
        assert!(open_client("not a redis url").is_err());
    }

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
        assert!(
            !files.is_empty(),
            "no sources found under {}",
            src.display()
        );
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
    fn in_runtime(body: impl FnOnce()) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(async { body() });
    }

    /// Assert that a fresh process starts with no provider — otherwise every
    /// assertion below would pass vacuously.
    fn assert_no_provider_yet() {
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_none(),
            "this scenario must run in a fresh process (rusty_fork), or the \
             assertions below prove nothing"
        );
    }

    /// The assertion every TLS scenario ends on.
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
    rusty_fork_test! {
        #[test]
        fn session_store_installs_the_tls_provider() {
            assert_no_provider_yet();
            in_runtime(|| {
                let mut config = crate::session::SessionConfig {
                    backend: crate::session::SessionBackend::Redis,
                    ..Default::default()
                };
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
                let mut config = crate::config::ChannelConfig {
                    backend: crate::config::ChannelBackend::Redis,
                    ..Default::default()
                };
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
                let mut config = crate::security::RateLimitConfig {
                    backend: crate::security::RateLimitBackend::Redis,
                    ..Default::default()
                };
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
                let mut config = crate::session::SessionConfig {
                    backend: crate::session::SessionBackend::Redis,
                    ..Default::default()
                };
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
