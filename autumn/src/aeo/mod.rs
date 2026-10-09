//! Agent readiness (AEO) by default.
//!
//! AEO means "Agent Engine Optimization". An Autumn app tells AI
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
pub(crate) mod documents;
pub mod markdown;
pub(crate) mod negotiate;
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
    pub site_name: Option<String>,

    /// One-line site summary. Default: the `OpenAPI` description.
    #[serde(default)]
    pub site_description: Option<String>,

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

    /// Publish the MCP tool list (server card, `WebMCP`, `site-guide`). Set
    /// `false` when a proxy or an app-wide layer guards `/mcp`. A
    /// `secure_mcp` endpoint never publishes its tools.
    #[serde(default = "default_true")]
    pub publish_tools: bool,

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

impl AeoConfig {
    /// Refuse an x402 route that can never match a request: its handler
    /// would run free.
    ///
    /// # Errors
    /// Names the first such route.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        for route in self.paid_routes.iter().filter(|r| r.is_x402()) {
            if let Some(problem) = commerce::unmatchable_route(route) {
                return Err(format!("[[aeo.paid_routes]] {problem}"));
            }
        }
        Ok(())
    }
}

impl Default for AeoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            content_signals: ContentSignalsConfig::default(),
            ai_crawlers: AiCrawlersConfig::default(),
            site_name: None,
            site_description: None,
            markdown: true,
            markdown_max_bytes: default_markdown_max_bytes(),
            link_headers: true,
            llms_txt: true,
            site_guide_skill: true,
            publish_tools: true,
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
    /// Auth.md credential types an agent gets (`api_key`, `access_token`).
    #[serde(default)]
    pub agent_credential_types: Vec<String>,
    /// Auth.md assertion types (`urn:ietf:params:oauth:token-type:id-jag`,
    /// `verified_email`).
    #[serde(default)]
    pub agent_assertion_types: Vec<String>,
    /// Auth.md revocation endpoint.
    #[serde(default)]
    pub agent_revocation_endpoint: Option<String>,
    /// Auth.md events endpoint.
    #[serde(default)]
    pub agent_events_endpoint: Option<String>,
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
#[non_exhaustive]
pub enum CrawlerAccess {
    /// Same rules as `User-agent: *`.
    #[default]
    Allow,
    /// `Disallow: /`.
    Disallow,
}

/// Skills registered with `AppBuilder::agent_skill` (an `AppState`
/// extension).
#[derive(Debug, Clone, Default)]
pub(crate) struct RegisteredAgentSkills(pub Vec<AgentSkill>);

/// UCP and ACP documents registered on the app builder (an `AppState`
/// extension).
#[derive(Debug, Clone, Default)]
pub(crate) struct RegisteredCommerceDocs {
    /// `/.well-known/ucp`.
    pub(crate) ucp: Option<commerce::UcpProfile>,
    /// `/.well-known/acp.json`.
    pub(crate) acp: Option<commerce::AcpDiscovery>,
}

/// What AEO serves for one router: the site facts and the layer settings.
/// Stored as an `AppState` extension at router build time.
#[derive(Debug, Clone, Default)]
pub(crate) struct AeoSite {
    /// `[aeo] enabled`.
    pub(crate) enabled: bool,
    /// `[seo] base_url`.
    pub(crate) base_url: Option<String>,
    /// The facts the documents render from.
    pub(crate) facts: documents::SiteFacts,
    /// Settings for [`negotiate::NegotiateLayer`].
    pub(crate) negotiate: negotiate::NegotiateConfig,
}

impl AeoSite {
    /// Build the site from config and the facts the router collected.
    #[must_use]
    pub(crate) fn new(
        config: &crate::config::AutumnConfig,
        mut facts: documents::SiteFacts,
    ) -> Self {
        let aeo = &config.aeo;
        if aeo.site_name.is_some() {
            facts.name.clone_from(&aeo.site_name);
        }
        if aeo.site_description.is_some() {
            facts.description.clone_from(&aeo.site_description);
        }
        facts.site_guide_skill = aeo.site_guide_skill;
        facts.llms_txt = aeo.llms_txt;
        facts.markdown = aeo.markdown;
        facts.auth_md = aeo.auth_md.clone();
        facts.oauth = aeo.oauth.clone();
        facts
            .csrf_header
            .clone_from(&config.security.csrf.token_header);
        if !aeo.publish_tools
            && let Some(mcp) = facts.mcp.as_mut()
        {
            mcp.public_tools = false;
            mcp.tools.clear();
        }
        facts.robots_txt = aeo.enabled.then(|| default_robots_txt(config));
        facts.sitemap = aeo.enabled;
        warn_on_config(config);
        // The master switch covers signing too: with AEO off the key
        // directory is not served, so a signature could not be verified.
        let wba_key = aeo
            .enabled
            .then(|| {
                web_bot_auth::WebBotAuthKey::from_config(&aeo.web_bot_auth, &crate::config::OsEnv)
            })
            .flatten();
        facts.web_bot_auth = match wba_key {
            Some(Ok(key)) => Some(key),
            Some(Err(err)) => {
                tracing::warn!(error = %err, "aeo: Web Bot Auth key not loaded");
                None
            }
            None => None,
        };
        // One `Link` value per home page: each names its own Markdown copy.
        let home_links: Vec<(String, HeaderValue)> = if aeo.link_headers {
            let homes = if facts.home_paths.is_empty() {
                vec!["/".to_owned()]
            } else {
                facts.home_paths.clone()
            };
            homes
                .into_iter()
                .filter_map(|home| {
                    let link = documents::homepage_link_header(&facts, &home);
                    HeaderValue::from_str(&link).ok().map(|v| (home, v))
                })
                .collect()
        } else {
            Vec::new()
        };
        let content_signal = aeo
            .content_signals
            .enabled
            .then(|| HeaderValue::from_str(&robots::content_signal_value(aeo.content_signals)).ok())
            .flatten();
        // RFC 9728 §5.1: a `401` points at the protected resource metadata.
        let resource_metadata = (!aeo.oauth.authorization_servers.is_empty())
            .then_some(config.seo.base_url.as_deref())
            .flatten()
            .and_then(|base| {
                HeaderValue::from_str(&format!(
                    "Bearer resource_metadata=\"{}{}\"",
                    base.trim_end_matches('/'),
                    documents::OAUTH_RESOURCE_PATH
                ))
                .ok()
            });
        Self {
            enabled: aeo.enabled,
            base_url: config.seo.base_url.clone(),
            facts,
            negotiate: negotiate::NegotiateConfig {
                markdown: aeo.markdown,
                max_bytes: aeo.markdown_max_bytes,
                home_links,
                content_signal,
                resource_metadata_from_host: !aeo.oauth.authorization_servers.is_empty()
                    && resource_metadata.is_none(),
                resource_metadata,
            },
        }
    }

    /// A signer for outbound requests, when AEO is on and a Web Bot Auth
    /// key is set.
    ///
    /// `Signature-Agent` is `[aeo.web_bot_auth] signature_agent`, else
    /// `[seo] base_url`.
    #[must_use]
    pub(crate) fn web_bot_auth_signer(
        &self,
        config: &crate::config::AutumnConfig,
    ) -> Option<web_bot_auth::WebBotAuthSigner> {
        if !self.enabled {
            return None;
        }
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
    pub(crate) fn layer(&self) -> Option<negotiate::NegotiateLayer> {
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
    let readable = method == Method::GET || method == Method::HEAD;
    if let Some(site) = site.filter(|s| s.enabled)
        && (readable || method == Method::OPTIONS)
        && documents::is_document_path(uri.path())
    {
        let host = req
            .headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .or_else(|| uri.authority().map(axum::http::uri::Authority::as_str));
        let origin = documents::Origin::resolve(site.base_url.as_deref(), host);
        if let Some(doc) = documents::render(&site.facts, &origin, uri.path()) {
            return document_response(doc, &method, req.headers());
        }
    }
    crate::middleware::error_page_filter::fallback_404_handler(method, uri).await
}

/// Marks a generated document, so the response layer removes any
/// `Set-Cookie` that an inner layer (CSRF, session) added: a public
/// document must not carry one visitor's cookie to the next.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AeoDocument;

/// With locale-prefixed routing, each page that is not excluded from it is
/// served at `/{locale}{path}` (the bare path only redirects), so it is
/// listed once per locale, as the `[seo]` sitemap lists it.
#[cfg_attr(not(feature = "i18n"), allow(dead_code))]
pub(crate) fn localize_pages(
    pages: Vec<documents::PageFacts>,
    locales: &[String],
    exclude_prefixes: &[String],
    exclude_exact: &[String],
) -> Vec<documents::PageFacts> {
    if locales.is_empty() {
        return pages;
    }
    let mut out = Vec::with_capacity(pages.len() * locales.len());
    for page in pages {
        if exclude_exact.contains(&page.path)
            || crate::seo::matches_locale_exclude_prefix(&page.path, exclude_prefixes)
        {
            out.push(page);
            continue;
        }
        for locale in locales {
            let path = if page.path == "/" {
                format!("/{locale}")
            } else {
                format!("/{locale}{}", page.path)
            };
            out.push(documents::PageFacts {
                path,
                ..page.clone()
            });
        }
    }
    out
}

/// Turn a rendered document into a response: `OPTIONS` preflight for CORS
/// documents, a strong `ETag`, and `304` for a matching `If-None-Match`.
fn document_response(
    doc: documents::Document,
    method: &Method,
    request_headers: &axum::http::HeaderMap,
) -> Response {
    use axum::http::{StatusCode, header};

    let etag = format!(
        "\"{}\"",
        &documents::sha256_digest(doc.body.as_bytes())["sha256:".len()..][..32]
    );
    // Weak comparison (RFC 9110 §13.1.2): a proxy may send `W/` back.
    let not_modified = request_headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().trim_start_matches("W/"))
        .any(|t| t == etag || t == "*");
    let mut res = if *method == Method::OPTIONS {
        if !doc
            .headers
            .iter()
            .any(|(k, _)| *k == "access-control-allow-origin")
        {
            return StatusCode::NOT_FOUND.into_response();
        }
        StatusCode::NO_CONTENT.into_response()
    } else if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let len = doc.body.len();
        // HEAD gets the headers GET gets, with no body.
        let mut res = if *method == Method::HEAD {
            axum::body::Body::empty().into_response()
        } else {
            doc.body.into_response()
        };
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(doc.content_type),
        );
        res.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        res
    };
    let headers = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, v);
    }
    for (name, value) in doc.headers {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.append(name, value);
        }
    }
    res.extensions_mut().insert(AeoDocument);
    res
}

/// The `robots.txt` AEO serves when no `[seo]` route does. Only the `dev`
/// and `test` profiles close the site: a custom profile name (`staging`,
/// `live`) must not hide a public site from search engines by accident.
fn default_robots_txt(config: &crate::config::AutumnConfig) -> String {
    let profile = match config.profile.as_deref() {
        Some("dev" | "test") | None => "dev",
        Some(_) => "prod",
    };
    robots::robots_txt_with_policy(profile, None, &[], &robots_policy(config))
}

/// Log the `[aeo]` settings that cannot work as written.
fn warn_on_config(config: &crate::config::AutumnConfig) {
    let aeo = &config.aeo;
    if !aeo.enabled {
        return;
    }
    let prod = matches!(config.profile.as_deref(), Some("prod" | "production"));
    if prod && config.seo.base_url.is_none() {
        tracing::warn!(
            "aeo: set [seo] base_url in production; without it the agent documents use \
             the request Host and are not cached"
        );
    }
    for problem in commerce::paid_route_problems(&aeo.paid_routes) {
        tracing::warn!(
            "aeo: [[aeo.paid_routes]] {problem}; an x402 route answers 503, and the route \
             is not in the OpenAPI payment info"
        );
    }
    if let Some(issuer) = aeo.oauth.authorization_server.issuer.as_deref() {
        // With no `base_url` the origin comes from each request; check only
        // the issuer path here.
        let origin =
            documents::Origin::resolve(config.seo.base_url.as_deref().or(Some(issuer)), None);
        let facts = documents::SiteFacts {
            oauth: aeo.oauth.clone(),
            ..documents::SiteFacts::default()
        };
        if documents::render(&facts, &origin, documents::OAUTH_SERVER_PATH).is_none() {
            tracing::warn!(
                issuer,
                "aeo: [aeo.oauth.authorization_server] issuer must be this site's origin \
                 with no path; Autumn does not publish its metadata"
            );
        }
    }
}

/// Path of the ARD manifest.
pub const AI_CATALOG_PATH: &str = "/.well-known/ai-catalog.json";

/// The agent documents a static build writes. The key directory is not
/// here: its signature expires.
const STATIC_DOCUMENT_PATHS: &[&str] = &[
    documents::LLMS_TXT_PATH,
    documents::SKILLS_INDEX_PATH,
    AI_CATALOG_PATH,
    documents::ARD_PATH,
    documents::API_CATALOG_PATH,
    documents::SERVER_CARD_PATH,
    documents::OAUTH_RESOURCE_PATH,
    documents::OAUTH_SERVER_PATH,
    documents::AUTH_MD_PATH,
    commerce::UCP_PATH,
    commerce::ACP_PATH,
];

/// Write the agent documents of `router` into the static build `dist`.
///
/// The documents hold absolute URLs, so this writes nothing without
/// `[seo] base_url` (`base_url` is `None`). A file that is already in
/// `dist` stays. Returns the paths it wrote.
///
/// # Errors
///
/// Returns an I/O error when a file cannot be written.
pub(crate) async fn write_static_documents(
    router: axum::Router,
    base_url: Option<&str>,
    dist: &std::path::Path,
) -> std::io::Result<Vec<String>> {
    use tower::ServiceExt as _;

    if base_url.is_none_or(|b| b.trim().is_empty()) {
        return Ok(Vec::new());
    }
    let mut queue: Vec<String> = STATIC_DOCUMENT_PATHS
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
    let mut written = Vec::new();
    while let Some(path) = queue.pop() {
        // A build render: x402 charges the live request for these bytes.
        let Ok(req) = Request::get(path.as_str())
            .extension(crate::static_gen::RenderDeadlineExempt)
            .body(Body::empty())
        else {
            continue;
        };
        let Ok(res) = router.clone().oneshot(req).await;
        if res.status() != axum::http::StatusCode::OK
            || res.extensions().get::<AeoDocument>().is_none()
        {
            continue;
        }
        let Ok(body) = axum::body::to_bytes(res.into_body(), usize::MAX).await else {
            continue;
        };
        if path == documents::SKILLS_INDEX_PATH {
            queue.extend(skill_paths(&body));
        }
        let file = dist.join(path.trim_start_matches('/'));
        if tokio::fs::try_exists(&file).await.unwrap_or(false) {
            continue;
        }
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&file, &body).await?;
        written.push(path);
    }
    written.sort();
    Ok(written)
}

/// The local `SKILL.md` paths that a skills index names.
fn skill_paths(index: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(index)
        .ok()
        .and_then(|v| v.get("skills")?.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.get("url")?.as_str())
        .filter(|u| u.starts_with('/') && !u.starts_with("//") && !u.contains(".."))
        .map(str::to_owned)
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn site_router(config: &crate::config::AutumnConfig) -> axum::Router {
        let site = Arc::new(AeoSite::new(config, documents::SiteFacts::default()));
        axum::Router::new().fallback(move |req| fallback(Some(Arc::clone(&site)), req))
    }

    #[tokio::test]
    async fn a_static_build_writes_the_agent_documents() {
        let mut config = crate::config::AutumnConfig::default();
        config.seo.base_url = Some("https://example.com".to_owned());
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("llms.txt"), "mine").unwrap();

        let written = write_static_documents(
            site_router(&config),
            config.seo.base_url.as_deref(),
            dist.path(),
        )
        .await
        .unwrap();

        let read = |p: &str| std::fs::read_to_string(dist.path().join(p)).unwrap();
        assert_eq!(read("llms.txt"), "mine", "a file in dist stays");
        assert!(!written.iter().any(|p| p == "/llms.txt"), "{written:?}");
        assert!(read(".well-known/ai-catalog.json").contains("https://example.com"));
        let index: serde_json::Value =
            serde_json::from_str(&read(".well-known/agent-skills/index.json")).unwrap();
        let skill = index["skills"][0]["url"].as_str().unwrap();
        assert!(read(skill.trim_start_matches('/')).starts_with("---\n"));
        assert!(!dist.path().join(".well-known/api-catalog").exists());
    }

    #[test]
    fn localized_pages_get_one_entry_per_locale() {
        let page = |path: &str| documents::PageFacts {
            path: path.to_owned(),
            title: "T".to_owned(),
            description: None,
        };
        let locales = ["en".to_owned(), "fr".to_owned()];
        let out = localize_pages(
            vec![page("/"), page("/about"), page("/api/docs"), page("/legal")],
            &locales,
            &["/api".to_owned()],
            &["/legal".to_owned()],
        );
        let paths: Vec<&str> = out.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/en",
                "/fr",
                "/en/about",
                "/fr/about",
                "/api/docs",
                "/legal"
            ]
        );
        assert_eq!(localize_pages(vec![page("/x")], &[], &[], &[]).len(), 1);
    }

    #[tokio::test]
    async fn a_static_build_needs_a_base_url() {
        let config = crate::config::AutumnConfig::default();
        let dist = tempfile::tempdir().unwrap();
        let written = write_static_documents(site_router(&config), None, dist.path())
            .await
            .unwrap();
        assert!(written.is_empty(), "{written:?}");
    }
}
