//! TLS setup for the `reqwest` clients that the framework builds.
//!
//! `reqwest` 0.13 uses `aws-lc-rs` and the platform root store by default.
//! The framework keeps one crypto backend (`ring`) and a fixed root store
//! (`webpki-roots`), as the Postgres `verify-full` connector does. So every
//! framework client gets a preconfigured rustls config from this module.
//!
//! This module does not install a process-wide `CryptoProvider`, so the app
//! can install its own provider for its own clients. (With `managed-pg`,
//! `postgresql_embedded` installs `ring` as that provider before its first
//! download.)

use std::sync::{Arc, LazyLock};

/// The rustls client config for framework clients: `ring`, the Mozilla roots
/// and HTTP/1.1 ALPN.
static CLIENT_CONFIG: LazyLock<rustls::ClientConfig> = LazyLock::new(|| {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap_or_else(|err| panic!("ring supports the default TLS versions: {err}"))
    .with_root_certificates(root_store())
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
});

/// The Mozilla roots from `webpki-roots`.
fn root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// The rustls client config that framework clients use.
pub(crate) fn client_config() -> rustls::ClientConfig {
    CLIENT_CONFIG.clone()
}

/// A `reqwest` async client builder with the framework TLS config.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_backend_preconfigured(client_config())
}

/// A `reqwest` blocking client builder with the framework TLS config.
#[cfg(all(feature = "db", feature = "http-client"))]
pub(crate) fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder().tls_backend_preconfigured(client_config())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_uses_ring_and_webpki_roots() {
        let config = client_config();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        // The key-exchange groups tell the providers apart: aws-lc-rs adds
        // X25519MLKEM768, which ring does not have.
        let groups = |provider: &rustls::crypto::CryptoProvider| {
            provider
                .kx_groups
                .iter()
                .map(|group| group.name())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            groups(config.crypto_provider()),
            groups(&rustls::crypto::ring::default_provider()),
            "framework clients must use the ring provider"
        );
        assert_eq!(root_store().len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }

    #[test]
    fn client_builder_builds_without_a_process_provider() {
        // No process-wide provider is installed here. The build must not
        // panic, because the config carries its own provider.
        let client = client_builder().build();
        assert!(client.is_ok(), "{client:?}");
    }

    /// No framework source builds a `reqwest` client outside this module.
    /// Those builds use the process default provider. A framework app may not
    /// install one, and then the build panics. The scan rejects the
    /// fully qualified constructors, and a `use` of `reqwest::Client` that
    /// would hide one.
    #[test]
    fn framework_sources_build_clients_through_this_module() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "bare reqwest client construction: {offenders:#?}"
        );
    }

    fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read src dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, offenders);
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !name.ends_with(".rs") || name == "reqwest_tls.rs" || name == "tests.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            // Only production code: stop at an inline unit test module. A
            // `mod tests;` line declares a file, so the scan goes past it.
            let production = text
                .match_indices("\n#[cfg(test)]\nmod ")
                .find(|(at, needle)| {
                    text[at + needle.len()..]
                        .lines()
                        .next()
                        .is_some_and(|line| line.trim_end().ends_with('{'))
                })
                .map_or(text.as_str(), |(at, _)| &text[..at]);
            for (index, line) in production.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                let imports_client = code.starts_with("use reqwest::")
                    && code
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|token| ["Client", "ClientBuilder", "get"].contains(&token));
                if imports_client
                    || [
                        "reqwest::Client::new()",
                        "reqwest::Client::builder()",
                        "reqwest::Client::default()",
                        "reqwest::ClientBuilder::new()",
                        "reqwest::ClientBuilder::default()",
                        "reqwest::get(",
                        "reqwest::blocking::Client::new()",
                        "reqwest::blocking::Client::builder()",
                        "reqwest::blocking::Client::default()",
                        "reqwest::blocking::ClientBuilder::new()",
                        "reqwest::blocking::get(",
                    ]
                    .iter()
                    .any(|needle| code.contains(needle))
                {
                    offenders.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
}
