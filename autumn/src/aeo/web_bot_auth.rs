//! Web Bot Auth: prove that requests from this app's bots come from it.
//!
//! - The app publishes its public keys as a JWKS at
//!   `/.well-known/http-message-signatures-directory`.
//! - The app signs outbound requests (RFC 9421 HTTP Message Signatures) with
//!   the private key, so a receiving site can verify them.
//!
//! Configure the key in `autumn.toml`:
//!
//! ```toml
//! [aeo.web_bot_auth]
//! private_key_env = "WEB_BOT_AUTH_KEY"   # base64url Ed25519 seed (32 bytes)
//! ```
//!
//! Then sign an outbound request:
//!
//! ```rust,ignore
//! let signer = WebBotAuthSigner::from_state(&state).expect("key set");
//! client.get("https://example.com/").sign_web_bot_auth(&signer).send().await?;
//! ```
//!
//! Each attempt, and each redirect to the same origin, gets a fresh
//! signature. A redirect to another origin drops it, and the rest of the
//! chain goes unsigned.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer as _, SigningKey};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// Path of the key directory.
pub const DIRECTORY_PATH: &str = "/.well-known/http-message-signatures-directory";
/// Media type of the key directory.
pub const DIRECTORY_MEDIA_TYPE: &str = "application/http-message-signatures-directory+json";

/// `[aeo.web_bot_auth]` settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[non_exhaustive]
pub struct WebBotAuthConfig {
    /// Name of the environment variable that holds the base64url Ed25519
    /// seed. The key itself never goes in `autumn.toml`.
    #[serde(default)]
    pub private_key_env: Option<String>,
    /// URL sent as `Signature-Agent`: where verifiers find the directory.
    /// Default: `[seo] base_url`.
    #[serde(default)]
    pub signature_agent: Option<String>,
    /// Signature lifetime in seconds. Default: 60.
    #[serde(default)]
    pub expires_secs: Option<u64>,
}

/// A bad Web Bot Auth key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WebBotAuthError {
    /// The seed is not base64 or not 32 bytes.
    #[error("Web Bot Auth key must be a base64url Ed25519 seed of 32 bytes")]
    InvalidKey,
    /// The configured environment variable is not set.
    #[error("Web Bot Auth key variable {0} is not set")]
    MissingKey(String),
}

/// An Ed25519 key for Web Bot Auth. `Debug` shows the key id only.
#[derive(Clone)]
pub struct WebBotAuthKey {
    signing: SigningKey,
    kid: String,
}

impl std::fmt::Debug for WebBotAuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebBotAuthKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl WebBotAuthKey {
    /// Load a key from a base64url (or standard base64) 32-byte seed.
    ///
    /// # Errors
    ///
    /// [`WebBotAuthError::InvalidKey`] for a bad seed.
    pub fn from_seed_b64(seed: &str) -> Result<Self, WebBotAuthError> {
        let seed = seed.trim().trim_end_matches('=');
        let bytes = URL_SAFE_NO_PAD
            .decode(seed)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(seed))
            .map_err(|_| WebBotAuthError::InvalidKey)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| WebBotAuthError::InvalidKey)?;
        let signing = SigningKey::from_bytes(&seed);
        let kid = ed25519_thumbprint(&URL_SAFE_NO_PAD.encode(signing.verifying_key().as_bytes()));
        Ok(Self { signing, kid })
    }

    /// Load the key named by `[aeo.web_bot_auth] private_key_env`. `None`
    /// when no variable is configured.
    ///
    /// # Errors
    ///
    /// [`WebBotAuthError`] when the variable is unset or holds a bad seed.
    pub fn from_config(
        config: &WebBotAuthConfig,
        env: &dyn crate::config::Env,
    ) -> Option<Result<Self, WebBotAuthError>> {
        let var = config.private_key_env.as_deref()?;
        Some(
            env.var(var)
                .map_err(|_| WebBotAuthError::MissingKey(var.to_owned()))
                .and_then(|seed| Self::from_seed_b64(&seed)),
        )
    }

    /// RFC 7638 JWK thumbprint, used as `kid` and `keyid`.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Public key as a JWK (no private part).
    #[must_use]
    pub fn public_jwk(&self) -> Value {
        json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(self.signing.verifying_key().as_bytes()),
            "kid": self.kid,
            "use": "sig",
        })
    }

    /// Sign `data`, as standard base64.
    #[must_use]
    pub(crate) fn sign_b64(&self, data: &[u8]) -> String {
        STANDARD.encode(self.signing.sign(data).to_bytes())
    }
}

/// RFC 7638 thumbprint of an Ed25519 public key in base64url form.
#[must_use]
pub(crate) fn ed25519_thumbprint(x_b64url: &str) -> String {
    // RFC 7638: required members only, sorted, no whitespace.
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x_b64url}"}}"#);
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

/// One HTTP message component for an RFC 9421 signature base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Component<'a> {
    /// Component name, e.g. `@authority` or `content-type`.
    pub(crate) name: &'a str,
    /// Parameters written after the name, e.g. `;req`.
    pub(crate) params: &'a str,
    /// Component value.
    pub(crate) value: &'a str,
}

/// Build an RFC 9421 signature base and the matching `@signature-params`
/// value (the `Signature-Input` member value).
#[must_use]
pub(crate) fn signature_base(components: &[Component<'_>], params: &str) -> (String, String) {
    let list: Vec<String> = components
        .iter()
        .map(|c| format!("\"{}\"{}", c.name, c.params))
        .collect();
    let signature_params = format!("({}){params}", list.join(" "));
    let mut base = String::new();
    for (c, id) in components.iter().zip(&list) {
        base.push_str(id);
        base.push_str(": ");
        base.push_str(c.value);
        base.push('\n');
    }
    base.push_str("\"@signature-params\": ");
    base.push_str(&signature_params);
    (base, signature_params)
}

/// Headers for a signed outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignedHeaders {
    /// `Signature-Agent`.
    pub signature_agent: String,
    /// `Signature-Input`.
    pub signature_input: String,
    /// `Signature`.
    pub signature: String,
}

/// Signs outbound requests with a [`WebBotAuthKey`].
#[derive(Debug, Clone)]
pub struct WebBotAuthSigner {
    key: WebBotAuthKey,
    signature_agent: String,
    expires_secs: u64,
}

impl WebBotAuthSigner {
    /// Build a signer. `signature_agent` is the origin that serves the key
    /// directory.
    ///
    /// A path or query on `signature_agent` is dropped: verifiers look the
    /// directory up at the origin.
    #[must_use]
    pub fn new(key: WebBotAuthKey, signature_agent: impl Into<String>) -> Self {
        let agent = signature_agent.into();
        let signature_agent = url::Url::parse(&agent)
            .ok()
            .filter(url::Url::has_host)
            .map_or(agent, |u| u.origin().ascii_serialization());
        Self {
            key,
            signature_agent,
            expires_secs: 60,
        }
    }

    /// Set the signature lifetime.
    #[must_use]
    pub const fn expires_secs(mut self, secs: u64) -> Self {
        self.expires_secs = secs;
        self
    }

    /// The key.
    #[must_use]
    pub const fn key(&self) -> &WebBotAuthKey {
        &self.key
    }

    /// The signer for this app: its key and `Signature-Agent`. `None` when
    /// no key is configured or no `Signature-Agent` URL is known.
    #[must_use]
    pub fn from_state(state: &crate::state::AppState) -> Option<Self> {
        let site = state.extension::<super::AeoSite>()?;
        let config = state.extension::<crate::config::AutumnConfig>()?;
        site.web_bot_auth_signer(&config)
    }

    /// Sign a request to `url` now. `None` when `url` has no host.
    #[must_use]
    pub fn sign_url(&self, url: &str) -> Option<SignedHeaders> {
        let authority = request_authority(&url::Url::parse(url).ok()?)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Some(self.sign(&authority, now))
    }

    /// Sign a request to `authority` (`host[:port]`) at Unix time `now`.
    ///
    /// Covers `@authority` and `signature-agent`, with `tag="web-bot-auth"`.
    /// `Signature-Agent` uses the quoted-string form that Cloudflare verifies.
    #[must_use]
    pub fn sign(&self, authority: &str, now: u64) -> SignedHeaders {
        let agent = format!("\"{}\"", self.signature_agent.replace(['"', '\\'], ""));
        let params = format!(
            ";created={now};expires={};keyid=\"{}\";alg=\"ed25519\";tag=\"web-bot-auth\"",
            now.saturating_add(self.expires_secs),
            self.key.kid()
        );
        let (base, signature_params) = signature_base(
            &[
                Component {
                    name: "@authority",
                    params: "",
                    value: authority,
                },
                Component {
                    name: "signature-agent",
                    params: "",
                    value: &agent,
                },
            ],
            &params,
        );
        SignedHeaders {
            signature_agent: agent,
            signature_input: format!("sig1={signature_params}"),
            signature: format!("sig1=:{}:", self.key.sign_b64(base.as_bytes())),
        }
    }
}

/// The key directory body: a JWKS with the public key.
#[must_use]
pub(crate) fn directory_json(keys: &[WebBotAuthKey]) -> String {
    let keys: Vec<Value> = keys.iter().map(WebBotAuthKey::public_jwk).collect();
    serde_json::to_string_pretty(&json!({ "keys": keys })).unwrap_or_else(|_| "{}".to_owned())
}

/// `Content-Digest` (RFC 9530) of `body`: `sha-256=:<base64>:`.
#[must_use]
pub(crate) fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(body)))
}

/// `Signature-Input` and `Signature` for a directory response served to
/// `authority`, as the directory draft recommends.
///
/// The signature covers `@authority` (as the verifier requested it) and
/// `content-digest`, as the directory draft requires.
#[must_use]
pub(crate) fn sign_directory(
    key: &WebBotAuthKey,
    authority: &str,
    content_digest: &str,
    now: u64,
) -> (String, String) {
    let params = format!(
        ";created={now};expires={};keyid=\"{}\";alg=\"ed25519\";\
         tag=\"http-message-signatures-directory\"",
        now.saturating_add(300),
        key.kid()
    );
    let (base, signature_params) = signature_base(
        &[
            Component {
                name: "@authority",
                params: ";req",
                value: authority,
            },
            Component {
                name: "content-digest",
                params: "",
                value: content_digest,
            },
        ],
        &params,
    );
    (
        format!("binding0={signature_params}"),
        format!("binding0=:{}:", key.sign_b64(base.as_bytes())),
    )
}

/// The RFC 9421 `@authority` of `url`: the lowercase host, with brackets
/// for IPv6, and the port only when it is not the default.
fn request_authority(url: &url::Url) -> Option<String> {
    let host = match url.host()? {
        url::Host::Domain(d) => d.to_ascii_lowercase(),
        url::Host::Ipv4(a) => a.to_string(),
        url::Host::Ipv6(a) => format!("[{a}]"),
    };
    Some(
        url.port()
            .map_or_else(|| host.clone(), |port| format!("{host}:{port}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

    /// RFC 8037 A.1 private key.
    const RFC8037_D: &str = "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A";

    fn verify(key: &WebBotAuthKey, base: &str, sig_b64: &str) -> bool {
        let x = URL_SAFE_NO_PAD
            .decode(key.public_jwk()["x"].as_str().unwrap())
            .unwrap();
        let vk = VerifyingKey::from_bytes(&x.try_into().unwrap()).unwrap();
        let sig = Signature::from_slice(&STANDARD.decode(sig_b64).unwrap()).unwrap();
        vk.verify(base.as_bytes(), &sig).is_ok()
    }

    #[test]
    fn a_disabled_site_has_no_signer() {
        let config = crate::config::AutumnConfig::default();
        let mut site = super::super::AeoSite {
            enabled: true,
            base_url: Some("https://example.com".to_owned()),
            ..Default::default()
        };
        site.facts.web_bot_auth = Some(WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap());
        assert!(site.web_bot_auth_signer(&config).is_some());
        site.enabled = false;
        assert!(site.web_bot_auth_signer(&config).is_none());
    }

    #[test]
    fn a_non_http_signature_agent_signs_nothing() {
        let mut config = crate::config::AutumnConfig::default();
        let mut site = super::super::AeoSite {
            enabled: true,
            ..Default::default()
        };
        site.facts.web_bot_auth = Some(WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap());
        for agent in ["ftp://keys.example.com", "keys.example.com", "/agents"] {
            config.aeo.web_bot_auth.signature_agent = Some(agent.to_owned());
            assert!(site.web_bot_auth_signer(&config).is_none(), "{agent}");
        }
        config.aeo.web_bot_auth.signature_agent = Some("https://keys.example.com/x".to_owned());
        let signer = site
            .web_bot_auth_signer(&config)
            .expect("an https agent signs");
        assert_eq!(
            signer.sign("a.b", 1).signature_agent,
            "\"https://keys.example.com\""
        );
    }

    #[test]
    fn rfc8037_key_and_thumbprint() {
        let key = WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap();
        let jwk = key.public_jwk();
        assert_eq!(jwk["kty"], "OKP");
        assert_eq!(jwk["crv"], "Ed25519");
        assert_eq!(jwk["x"], "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo");
        assert!(jwk.get("d").is_none(), "never publish the private part");
        assert_eq!(key.kid(), "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k");
        assert_eq!(jwk["kid"], key.kid());
        assert!(!format!("{key:?}").contains(RFC8037_D));
    }

    #[test]
    fn cloudflare_directory_thumbprint() {
        assert_eq!(
            ed25519_thumbprint("JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs"),
            "poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U"
        );
    }

    #[test]
    fn bad_seeds_are_rejected() {
        assert_eq!(
            WebBotAuthKey::from_seed_b64("short").unwrap_err(),
            WebBotAuthError::InvalidKey
        );
        assert_eq!(
            WebBotAuthKey::from_seed_b64("!!!!").unwrap_err(),
            WebBotAuthError::InvalidKey
        );
    }

    /// RFC 9421 B.2.6: signing with Ed25519.
    #[test]
    fn rfc9421_ed25519_vector() {
        let der = STANDARD
            .decode("MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF")
            .unwrap();
        let key = WebBotAuthKey::from_seed_b64(&URL_SAFE_NO_PAD.encode(&der[16..])).unwrap();
        let (base, params) = signature_base(
            &[
                Component {
                    name: "date",
                    params: "",
                    value: "Tue, 20 Apr 2021 02:07:55 GMT",
                },
                Component {
                    name: "@method",
                    params: "",
                    value: "POST",
                },
                Component {
                    name: "@path",
                    params: "",
                    value: "/foo",
                },
                Component {
                    name: "@authority",
                    params: "",
                    value: "example.com",
                },
                Component {
                    name: "content-type",
                    params: "",
                    value: "application/json",
                },
                Component {
                    name: "content-length",
                    params: "",
                    value: "18",
                },
            ],
            ";created=1618884473;keyid=\"test-key-ed25519\"",
        );
        assert_eq!(
            params,
            "(\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \
             \"content-length\");created=1618884473;keyid=\"test-key-ed25519\""
        );
        assert_eq!(
            base,
            "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\
             \"@method\": POST\n\
             \"@path\": /foo\n\
             \"@authority\": example.com\n\
             \"content-type\": application/json\n\
             \"content-length\": 18\n\
             \"@signature-params\": (\"date\" \"@method\" \"@path\" \"@authority\" \
             \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\""
        );
        assert_eq!(
            key.sign_b64(base.as_bytes()),
            "wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw=="
        );
    }

    #[test]
    fn outbound_signature_verifies() {
        let key = WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap();
        let signer = WebBotAuthSigner::new(key.clone(), "https://bot.example.com");
        let h = signer.sign("example.org", 1_700_000_000);
        assert_eq!(h.signature_agent, "\"https://bot.example.com\"");
        let expected_input = format!(
            "sig1=(\"@authority\" \"signature-agent\");created=1700000000;expires=1700000060;\
             keyid=\"{}\";alg=\"ed25519\";tag=\"web-bot-auth\"",
            key.kid()
        );
        assert_eq!(h.signature_input, expected_input);
        let sig = h
            .signature
            .strip_prefix("sig1=:")
            .and_then(|s| s.strip_suffix(':'))
            .unwrap();
        let (base, _) = signature_base(
            &[
                Component {
                    name: "@authority",
                    params: "",
                    value: "example.org",
                },
                Component {
                    name: "signature-agent",
                    params: "",
                    value: "\"https://bot.example.com\"",
                },
            ],
            &expected_input[expected_input.find(')').unwrap() + 1..],
        );
        assert!(verify(&key, &base, sig), "{base}");
    }

    #[test]
    fn signature_agent_is_an_origin() {
        let key = WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap();
        let signer = WebBotAuthSigner::new(key, "https://Bot.Example.com/bots/x?y=1");
        assert_eq!(
            signer.sign("a.b", 1).signature_agent,
            "\"https://bot.example.com\""
        );
    }

    #[test]
    fn request_authority_keeps_ipv6_brackets() {
        let at = |u: &str| request_authority(&url::Url::parse(u).unwrap());
        assert_eq!(
            at("https://[2001:db8::1]:8443/x").as_deref(),
            Some("[2001:db8::1]:8443")
        );
        assert_eq!(at("https://[::1]/").as_deref(), Some("[::1]"));
        assert_eq!(
            at("https://Example.org:443/a").as_deref(),
            Some("example.org")
        );
        assert_eq!(
            at("http://example.org:8080").as_deref(),
            Some("example.org:8080")
        );
        assert_eq!(at("mailto:a@b.example"), None);
    }

    #[test]
    fn sign_url_uses_the_authority() {
        let key = WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap();
        let signer = WebBotAuthSigner::new(key, "https://bot.example.com").expires_secs(5);
        let h = signer.sign_url("https://Example.org:8443/a?b").unwrap();
        assert!(h.signature_input.contains(";expires="), "{h:?}");
        assert!(signer.sign_url("/relative").is_none());
    }

    #[test]
    fn directory_is_a_jwks_and_its_signature_verifies() {
        let key = WebBotAuthKey::from_seed_b64(RFC8037_D).unwrap();
        let dir: Value = serde_json::from_str(&directory_json(std::slice::from_ref(&key))).unwrap();
        assert_eq!(dir["keys"][0]["kid"], key.kid());
        assert!(dir["keys"][0].get("d").is_none());

        let digest = content_digest(b"{}");
        let (input, signature) = sign_directory(&key, "shop.example.com", &digest, 1_700_000_000);
        assert!(
            input.starts_with(
                "binding0=(\"@authority\";req \"content-digest\");created=1700000000;"
            ),
            "{input}"
        );
        assert!(
            input.ends_with("tag=\"http-message-signatures-directory\""),
            "{input}"
        );
        let params = &input[input.find(')').unwrap() + 1..];
        let (base, _) = signature_base(
            &[
                Component {
                    name: "@authority",
                    params: ";req",
                    value: "shop.example.com",
                },
                Component {
                    name: "content-digest",
                    params: "",
                    value: &digest,
                },
            ],
            params,
        );
        let sig = signature
            .strip_prefix("binding0=:")
            .and_then(|s| s.strip_suffix(':'))
            .unwrap();
        assert!(verify(&key, &base, sig), "{base}");
    }
}
