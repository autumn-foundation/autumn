//! Agent readiness (AEO) by default.
//!
//! AEO means "Answer/Agent Engine Optimization". An Autumn app tells AI
//! agents what it is, how to read it, and what they can do, with no code.
//! The checks follow <https://isitagentready.com>.
//!
//! | Part | Default |
//! |---|---|
//! | `robots.txt` content signals and AI crawler groups | On |
//! | `Accept: text/markdown` negotiation | On |
//! | Homepage `Link` headers | On |
//!
//! Turn each part off in `[aeo]`:
//!
//! ```toml
//! [aeo]
//! enabled = true
//! markdown = true
//!
//! [aeo.content_signals]
//! search = true
//! ai_input = true
//! ai_train = false
//!
//! [aeo.ai_crawlers]
//! training = "disallow"
//! ```
//!
//! See `docs/guide/aeo.md`.

pub mod commerce;
pub mod dns_aid;
pub mod documents;
pub mod markdown;
pub mod negotiate;
pub mod robots;
pub mod web_bot_auth;
pub mod webmcp;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request};
use axum::response::{IntoResponse as _, Response};
use serde::Deserialize;

pub use documents::{AgentSkill, AgentSkillError};
pub use robots::{AI_SEARCH_CRAWLERS, AI_TRAINING_CRAWLERS, AI_USER_FETCHERS, BotPolicy};

/// `[aeo]` settings.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // independent on/off switches
pub struct AeoConfig {
    /// Master switch. `false` turns every AEO part off.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Content signals in `robots.txt`.
    #[serde(default)]
    pub content_signals: ContentSignalsConfig,

    /// AI crawler groups in `robots.txt`.
    #[serde(default)]
    pub ai_crawlers: AiCrawlersConfig,

    /// Site name for `llms.txt`, the server card, and the ARD manifest.
    /// Default: the `OpenAPI` title, else the host name.
    #[serde(default)]
    pub name: Option<String>,

    /// One-line site summary. Default: the `OpenAPI` description.
    #[serde(default)]
    pub description: Option<String>,

    /// Answer `Accept: text/markdown` with a Markdown copy of HTML pages.
    #[serde(default = "default_true")]
    pub markdown: bool,

    /// Largest HTML body (bytes) to convert. A larger page stays HTML.
    #[serde(default = "default_markdown_max_bytes")]
    pub markdown_max_bytes: usize,

    /// Send agent `Link` headers on the homepage.
    #[serde(default = "default_true")]
    pub link_headers: bool,

    /// Serve `/llms.txt`.
    #[serde(default = "default_true")]
    pub llms_txt: bool,

    /// Publish the generated `site-guide` agent skill.
    #[serde(default = "default_true")]
    pub site_guide_skill: bool,

    /// `/auth.md` settings.
    #[serde(default)]
    pub auth_md: AuthMdConfig,

    /// OAuth discovery settings.
    #[serde(default)]
    pub oauth: OAuthConfig,

    /// Web Bot Auth key settings.
    #[serde(default)]
    pub web_bot_auth: web_bot_auth::WebBotAuthConfig,

    /// x402 payment settings for [`AeoConfig::paid_routes`].
    #[serde(default)]
    pub x402: commerce::X402Config,

    /// Priced routes (x402 challenge, MPP discovery).
    #[serde(default)]
    pub paid_routes: Vec<commerce::PaidRoute>,
}

impl Default for AeoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            content_signals: ContentSignalsConfig::default(),
            ai_crawlers: AiCrawlersConfig::default(),
            name: None,
            description: None,
            markdown: true,
            markdown_max_bytes: default_markdown_max_bytes(),
            link_headers: true,
            llms_txt: true,
            site_guide_skill: true,
            auth_md: AuthMdConfig::default(),
            oauth: OAuthConfig::default(),
            web_bot_auth: web_bot_auth::WebBotAuthConfig::default(),
            x402: commerce::X402Config::default(),
            paid_routes: Vec::new(),
        }
    }
}

/// `[aeo.auth_md]`: the `/auth.md` page for agents.
///
/// Autumn serves it when the app has an MCP server, an `OpenAPI` document, or
/// OAuth settings.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct AuthMdConfig {
    /// Serve `/auth.md`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Page where an agent or its user gets a credential.
    #[serde(default)]
    pub registration_url: Option<String>,
    /// Extra Markdown added under "Register".
    #[serde(default)]
    pub instructions: Option<String>,
}

impl Default for AuthMdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            registration_url: None,
            instructions: None,
        }
    }
}

/// `[aeo.oauth]`: OAuth discovery documents.
///
/// Autumn publishes only the values set here. It does not run an
/// authorization server.
#[derive(Debug, Clone, Default, Deserialize)]
#[non_exhaustive]
pub struct OAuthConfig {
    /// Issuer URLs of the authorization servers for this API. When set,
    /// Autumn serves `/.well-known/oauth-protected-resource` (RFC 9728).
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    /// Scopes the API accepts.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    /// Metadata for an authorization server at this origin. When `issuer`
    /// is set, Autumn serves `/.well-known/oauth-authorization-server`
    /// (RFC 8414).
    #[serde(default)]
    pub authorization_server: AuthorizationServerConfig,
}

/// `[aeo.oauth.authorization_server]`: RFC 8414 metadata fields.
#[derive(Debug, Clone, Default, Deserialize)]
#[non_exhaustive]
pub struct AuthorizationServerConfig {
    /// `issuer`. Required to publish the document.
    #[serde(default)]
    pub issuer: Option<String>,
    /// `authorization_endpoint`.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// `token_endpoint`.
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// `jwks_uri`.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// `registration_endpoint`.
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    /// `scopes_supported`.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    /// `response_types_supported`. Default: `["code"]`.
    #[serde(default)]
    pub response_types_supported: Vec<String>,
    /// `grant_types_supported`.
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    /// `code_challenge_methods_supported`.
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// Auth.md agent registration endpoint (`agent_auth`).
    #[serde(default)]
    pub agent_identity_endpoint: Option<String>,
    /// Auth.md claim endpoint (`agent_auth`).
    #[serde(default)]
    pub agent_claim_endpoint: Option<String>,
    /// Auth.md identity types (`anonymous`, `identity_assertion`, ...).
    #[serde(default)]
    pub agent_identity_types: Vec<String>,
}

/// `[aeo.content_signals]`: the `Content-Signal` line in `robots.txt`.
///
/// See <https://contentsignals.org/>.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // one bool per signal
pub struct ContentSignalsConfig {
    /// Write the `Content-Signal` line.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `search`: use the content for a search index.
    #[serde(default = "default_true")]
    pub search: bool,
    /// `ai-input`: use the content as input to AI answers.
    #[serde(default = "default_true")]
    pub ai_input: bool,
    /// `ai-train`: use the content to train AI models.
    #[serde(default)]
    pub ai_train: bool,
}

impl Default for ContentSignalsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            search: true,
            ai_input: true,
            ai_train: false,
        }
    }
}

/// `[aeo.ai_crawlers]`: access for each class of AI crawler.
///
/// `allow` puts the class in the `User-agent: *` group, so it gets the same
/// rules as every crawler. `disallow` gives the class its own group with
/// `Disallow: /`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[non_exhaustive]
pub struct AiCrawlersConfig {
    /// AI search crawlers ([`AI_SEARCH_CRAWLERS`]).
    #[serde(default)]
    pub search: CrawlerAccess,
    /// Fetchers that run when a user asks an AI ([`AI_USER_FETCHERS`]).
    #[serde(default)]
    pub user_fetch: CrawlerAccess,
    /// AI training crawlers ([`AI_TRAINING_CRAWLERS`]).
    #[serde(default)]
    pub training: CrawlerAccess,
}

/// Access for one class of AI crawler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrawlerAccess {
    /// Same rules as `User-agent: *`.
    #[default]
    Allow,
    /// `Disallow: /`.
    Disallow,
}

/// Skills registered with `AppBuilder::agent_skill` (an `AppState`
/// extension).
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct RegisteredAgentSkills(pub Vec<AgentSkill>);

/// UCP and ACP documents registered on the app builder (an `AppState`
/// extension).
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct RegisteredCommerceDocs {
    /// `/.well-known/ucp`.
    pub ucp: Option<commerce::UcpProfile>,
    /// `/.well-known/acp.json`.
    pub acp: Option<commerce::AcpDiscovery>,
}

/// What AEO serves for one router: the site facts and the layer settings.
/// Stored as an `AppState` extension at router build time.
#[derive(Debug, Clone, Default)]
#[doc(hidden)]
pub struct AeoSite {
    /// `[aeo] enabled`.
    pub enabled: bool,
    /// `[seo] base_url`.
    pub base_url: Option<String>,
    /// The facts the documents render from.
    pub facts: documents::SiteFacts,
    /// Settings for [`negotiate::NegotiateLayer`].
    pub negotiate: negotiate::NegotiateConfig,
}

impl AeoSite {
    /// Build the site from config and the facts the router collected.
    #[must_use]
    pub fn new(config: &crate::config::AutumnConfig, mut facts: documents::SiteFacts) -> Self {
        let aeo = &config.aeo;
        if aeo.name.is_some() {
            facts.name.clone_from(&aeo.name);
        }
        if aeo.description.is_some() {
            facts.description.clone_from(&aeo.description);
        }
        facts.site_guide_skill = aeo.site_guide_skill;
        facts.llms_txt = aeo.llms_txt;
        facts.markdown = aeo.markdown;
        facts.auth_md = aeo.auth_md.clone();
        facts.oauth = aeo.oauth.clone();
        facts
            .csrf_header
            .clone_from(&config.security.csrf.token_header);
        facts.web_bot_auth = match web_bot_auth::WebBotAuthKey::from_config(
            &aeo.web_bot_auth,
            &crate::config::OsEnv,
        ) {
            Some(Ok(key)) => Some(key),
            Some(Err(err)) => {
                tracing::warn!(error = %err, "aeo: Web Bot Auth key not loaded");
                None
            }
            None => None,
        };
        let home_link = aeo
            .link_headers
            .then(|| documents::homepage_link_header(&facts))
            .flatten()
            .and_then(|v| HeaderValue::from_str(&v).ok());
        let content_signal = aeo
            .content_signals
            .enabled
            .then(|| HeaderValue::from_str(&robots::content_signal_value(aeo.content_signals)).ok())
            .flatten();
        Self {
            enabled: aeo.enabled,
            base_url: config.seo.base_url.clone(),
            facts,
            negotiate: negotiate::NegotiateConfig {
                markdown: aeo.markdown,
                max_bytes: aeo.markdown_max_bytes,
                home_link,
                content_signal,
            },
        }
    }

    /// A signer for outbound requests, when a Web Bot Auth key is set.
    ///
    /// `Signature-Agent` is `[aeo.web_bot_auth] signature_agent`, else
    /// `[seo] base_url`.
    #[must_use]
    pub fn web_bot_auth_signer(
        &self,
        config: &crate::config::AutumnConfig,
    ) -> Option<web_bot_auth::WebBotAuthSigner> {
        let key = self.facts.web_bot_auth.clone()?;
        let wba = &config.aeo.web_bot_auth;
        let agent = wba
            .signature_agent
            .clone()
            .or_else(|| self.base_url.clone())?;
        let signer = web_bot_auth::WebBotAuthSigner::new(key, agent);
        Some(match wba.expires_secs {
            Some(secs) => signer.expires_secs(secs),
            None => signer,
        })
    }

    /// The response layer, or `None` when AEO is off.
    #[must_use]
    pub fn layer(&self) -> Option<negotiate::NegotiateLayer> {
        self.enabled
            .then(|| negotiate::NegotiateLayer::new(self.negotiate.clone()))
    }
}

/// Router fallback: serve an AEO document for an unmatched `GET`/`HEAD`,
/// else the framework 404.
///
/// Serving from the fallback means an application route at the same path
/// always wins, and a generated path can never collide with one.
pub(crate) async fn fallback(site: Option<Arc<AeoSite>>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    if let Some(site) = site.filter(|s| s.enabled)
        && (method == Method::GET || method == Method::HEAD)
    {
        let host = req
            .headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| uri.authority().map(axum::http::uri::Authority::as_str));
        let origin = documents::Origin::resolve(site.base_url.as_deref(), host);
        if let Some(doc) = documents::render(&site.facts, &origin, uri.path()) {
            let mut res = doc.body.into_response();
            let headers = res.headers_mut();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static(doc.content_type),
            );
            for (name, value) in doc.headers {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    headers.append(name, value);
                }
            }
            return res;
        }
    }
    crate::middleware::error_page_filter::fallback_404_handler(method, uri).await
}

/// Path of the ARD manifest.
pub const AI_CATALOG_PATH: &str = "/.well-known/ai-catalog.json";

/// The `robots.txt` [`BotPolicy`] for `config`, with `Agentmap:` when
/// `[seo] base_url` is set.
#[must_use]
pub fn robots_policy(config: &crate::config::AutumnConfig) -> BotPolicy {
    let agentmap = config
        .aeo
        .enabled
        .then_some(config.seo.base_url.as_deref())
        .flatten()
        .map(|base| format!("{}{AI_CATALOG_PATH}", base.trim_end_matches('/')));
    BotPolicy::from_config(&config.aeo).agentmap(agentmap)
}

const fn default_markdown_max_bytes() -> usize {
    2 * 1024 * 1024
}

const fn default_true() -> bool {
    true
}
