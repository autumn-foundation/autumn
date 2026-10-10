//! The AEO documents: `llms.txt`, API catalog, MCP server card, agent
//! skills, ARD manifest, OAuth metadata, and `auth.md`.
//!
//! Each renderer is a pure function of [`SiteFacts`] (what the app has) and
//! [`Origin`] (where the request came in). The router serves them from its
//! fallback, so an application route at the same path always wins.

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

use super::{AuthMdConfig, OAuthConfig};

/// Path of `llms.txt`.
pub const LLMS_TXT_PATH: &str = "/llms.txt";
/// Path of the API catalog (RFC 9727).
pub const API_CATALOG_PATH: &str = "/.well-known/api-catalog";
/// Domain-level path of the MCP server card.
pub const SERVER_CARD_PATH: &str = "/.well-known/mcp/server-card.json";

/// The endpoint-specific card path for an MCP mount at `mcp_path`. A
/// trailing `/` is dropped, so `/` and `/mcp/` give `/server-card` and
/// `/mcp/server-card`.
#[must_use]
pub fn server_card_path(mcp_path: &str) -> String {
    format!("{}/server-card", mcp_path.trim_end_matches('/'))
}
/// Path of the agent skills index.
pub const SKILLS_INDEX_PATH: &str = "/.well-known/agent-skills/index.json";
/// Path of the ARD manifest under its current name.
pub const ARD_PATH: &str = "/.well-known/ard.json";
/// Path of the OAuth protected resource metadata (RFC 9728).
pub const OAUTH_RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";
/// Path of the OAuth authorization server metadata (RFC 8414).
pub const OAUTH_SERVER_PATH: &str = "/.well-known/oauth-authorization-server";
/// Path of `auth.md`.
pub const AUTH_MD_PATH: &str = "/auth.md";
/// Name of the generated agent skill.
pub const SITE_GUIDE_SKILL: &str = "site-guide";

/// What the app has, captured once at router build time.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // independent switches from `[aeo]`
pub struct SiteFacts {
    /// `[aeo] name`, else the `OpenAPI` title. `None` uses the host name.
    pub name: Option<String>,
    /// One-line summary.
    pub description: Option<String>,
    /// The MCP server, when mounted.
    pub mcp: Option<McpFacts>,
    /// The `OpenAPI` document, when mounted.
    pub openapi: Option<OpenApiFacts>,
    /// The health endpoint path, when mounted.
    pub health_path: Option<String>,
    /// Pages for `llms.txt`: `GET` routes that declare a `seo(title)`.
    pub pages: Vec<PageFacts>,
    /// Skills the app registered.
    pub skills: Vec<AgentSkill>,
    /// Publish the generated `site-guide` skill.
    pub site_guide_skill: bool,
    /// Serve `llms.txt`.
    pub llms_txt: bool,
    /// Markdown negotiation is on.
    pub markdown: bool,
    /// `[aeo.auth_md]`.
    pub auth_md: AuthMdConfig,
    /// `[aeo.oauth]`.
    pub oauth: OAuthConfig,
    /// The CSRF header the `WebMCP` script sends.
    pub csrf_header: String,
    /// The Web Bot Auth key, when configured.
    pub web_bot_auth: Option<super::web_bot_auth::WebBotAuthKey>,
    /// UCP and ACP documents.
    pub commerce: super::RegisteredCommerceDocs,
    /// The default `robots.txt`, when AEO serves it (no `[seo]` routes).
    pub robots_txt: Option<String>,
    /// Serve a `/sitemap.xml` of the known pages when no route does, so the
    /// links in `llms.txt` and the site guide always resolve.
    pub sitemap: bool,
    /// The home page paths: `/`, or `/{locale}` for each locale when the
    /// root is locale-prefixed. Empty means `/`.
    pub home_paths: Vec<String>,
    /// The home page says `noindex`: the sitemap leaves it out.
    pub home_noindex: bool,
}

/// The mounted MCP server.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct McpFacts {
    /// Mount path, e.g. `/mcp`.
    pub path: String,
    /// The tools. Empty when [`McpFacts::public_tools`] is `false`.
    pub tools: Vec<ToolFacts>,
    /// `false` when `secure_mcp` gates the endpoint: the tool list is then
    /// not published.
    pub public_tools: bool,
}

/// One MCP tool.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ToolFacts {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: Option<String>,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
    /// MCP annotations (`readOnlyHint`, ...).
    pub annotations: Value,
}

/// The mounted `OpenAPI` document.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct OpenApiFacts {
    /// Path of the JSON document.
    pub json_path: String,
    /// Path of the HTML docs (Swagger UI), when mounted.
    pub docs_path: Option<String>,
    /// API version.
    pub version: String,
}

/// One page for `llms.txt`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PageFacts {
    /// Route path.
    pub path: String,
    /// `seo(title)`.
    pub title: String,
    /// `seo(description)`.
    pub description: Option<String>,
}

/// An agent skill: one `SKILL.md` file.
///
/// ```rust,ignore
/// let skill = AgentSkill::parse(include_str!("../skills/refunds/SKILL.md"))?;
/// autumn_web::app().agent_skill(skill);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkill {
    name: String,
    description: String,
    body: String,
}

/// An invalid [`AgentSkill`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AgentSkillError {
    /// The name is not 1–64 characters of `a-z`, `0-9`, and single hyphens.
    #[error("agent skill name {0:?} must be 1-64 chars of a-z, 0-9 and single inner hyphens")]
    InvalidName(String),
    /// The description is empty or longer than 1024 characters.
    #[error("agent skill {0:?} needs a description of 1-1024 characters")]
    InvalidDescription(String),
    /// The `SKILL.md` text has no `name`/`description` front matter.
    #[error("SKILL.md needs YAML front matter with `name` and `description`")]
    MissingFrontMatter,
}

impl AgentSkill {
    /// Build a skill from its parts. `body` is the Markdown after the front
    /// matter.
    ///
    /// # Errors
    ///
    /// Returns [`AgentSkillError`] for a bad name or description.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Self, AgentSkillError> {
        let name = name.into();
        let description = description.into();
        if !valid_skill_name(&name) {
            return Err(AgentSkillError::InvalidName(name));
        }
        let len = description.chars().count();
        if description.trim().is_empty() || len > 1024 {
            return Err(AgentSkillError::InvalidDescription(name));
        }
        Ok(Self {
            name,
            description,
            body: body.into(),
        })
    }

    /// Parse a complete `SKILL.md` file (front matter and body).
    ///
    /// # Errors
    ///
    /// Returns [`AgentSkillError`] when the front matter is missing or bad.
    pub fn parse(skill_md: &str) -> Result<Self, AgentSkillError> {
        let rest = skill_md
            .strip_prefix("---\n")
            .or_else(|| skill_md.strip_prefix("---\r\n"))
            .ok_or(AgentSkillError::MissingFrontMatter)?;
        let (front, body) = rest
            .split_once("\n---\n")
            .or_else(|| rest.split_once("\r\n---\r\n"))
            .ok_or(AgentSkillError::MissingFrontMatter)?;
        let name = front_matter_field(front, "name").ok_or(AgentSkillError::MissingFrontMatter)?;
        let description =
            front_matter_field(front, "description").ok_or(AgentSkillError::MissingFrontMatter)?;
        let body = body
            .strip_prefix("\r\n")
            .or_else(|| body.strip_prefix('\n'))
            .unwrap_or(body);
        Self::new(name, description, body)
    }

    /// Skill name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Skill description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The `SKILL.md` bytes as served.
    #[must_use]
    pub fn to_skill_md(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}",
            self.name,
            yaml_quote(&self.description),
            self.body
        )
    }

    /// Path where the skill is served.
    #[must_use]
    pub fn path(&self) -> String {
        format!("/.well-known/agent-skills/{}/SKILL.md", self.name)
    }
}

/// Where a request came in: the absolute origin and its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// `scheme://host[:port]`, no trailing slash.
    pub base: String,
    /// Host name without port, lowercase.
    pub host: String,
    /// `true` when `base` comes from `[seo] base_url`, not from the request.
    pub configured: bool,
}

impl Origin {
    /// Use `[seo] base_url` when set. Else build it from the `Host` header:
    /// `http` for loopback hosts, `https` for the rest. A `base_url` that is
    /// not `http` or `https`, has a query or a fragment, or carries a user
    /// name or password is not used: the documents describe an HTTP site,
    /// paths are appended to the base, and the documents are public.
    #[must_use]
    pub fn resolve(base_url: Option<&str>, host_header: Option<&str>) -> Self {
        if let Some(raw) = base_url.map(|b| b.trim().trim_end_matches('/'))
            && let Ok(url) = url::Url::parse(raw)
            && let Some(host) = url.host_str()
            && matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
        {
            // The scheme as parsed (lowercase), the rest as written: the
            // signing authority strips the default port by scheme.
            let base = format!("{}{}", url.scheme(), &raw[url.scheme().len()..]);
            return Self {
                base,
                host: host.to_ascii_lowercase(),
                configured: true,
            };
        }
        let authority = host_header
            .map(str::trim)
            .filter(|h| valid_authority(h))
            .map_or_else(|| "localhost".to_owned(), str::to_ascii_lowercase);
        let host = authority_host(&authority).to_owned();
        let loopback = host == "localhost"
            || host.ends_with(".localhost")
            || host == "[::1]"
            || host.starts_with("127.");
        let scheme = if loopback { "http" } else { "https" };
        Self {
            base: format!("{scheme}://{authority}"),
            host,
            configured: false,
        }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `host[:port]` of [`Origin::base`], lowercase, without a default port
    /// (the RFC 9421 `@authority` form).
    #[must_use]
    pub fn signing_authority(&self) -> String {
        let authority = self.authority().to_ascii_lowercase();
        let default_port = if self.base.starts_with("https://") {
            ":443"
        } else {
            ":80"
        };
        authority
            .strip_suffix(default_port)
            .map_or_else(|| authority.clone(), str::to_owned)
    }

    /// `true` when a request `Host` names this origin, as the RFC 9421
    /// `@authority` a verifier rebuilds from that request.
    #[must_use]
    pub(crate) fn is_request_authority(&self, host: Option<&str>) -> bool {
        let Some(host) = host.map(|h| h.trim().to_ascii_lowercase()) else {
            return false;
        };
        let default_port = if self.base.starts_with("https://") {
            ":443"
        } else {
            ":80"
        };
        host.strip_suffix(default_port).unwrap_or(&host) == self.signing_authority()
    }

    /// `host[:port]` of [`Origin::base`].
    #[must_use]
    pub fn authority(&self) -> &str {
        let rest = self
            .base
            .split_once("://")
            .map_or(self.base.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }
}

/// A rendered document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// `Content-Type` value.
    pub content_type: &'static str,
    /// Extra response headers.
    pub headers: Vec<(&'static str, String)>,
    /// Body.
    pub body: String,
}

/// Render the document served at `path`, or `None` when the app has none.
#[must_use]
pub fn render(facts: &SiteFacts, origin: &Origin, path: &str) -> Option<Document> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    render_at(facts, origin, path, now)
}

/// [`render`] at Unix time `now` (used to sign the key directory).
///
/// A document built from the request `Host` (no `[seo] base_url`) is never
/// marked for shared caches: a cache could store a forged host for others.
#[must_use]
pub fn render_at(facts: &SiteFacts, origin: &Origin, path: &str, now: u64) -> Option<Document> {
    let mut doc = render_inner(facts, origin, path, now)?;
    if !origin.configured {
        doc.headers.retain(|(name, _)| *name != "cache-control");
        doc.headers.push(("cache-control", "no-store".to_owned()));
    }
    Some(doc)
}

/// `true` when `path` can name a generated document. The fallback checks
/// this before it does any other work.
#[must_use]
pub fn is_document_path(path: &str) -> bool {
    path.starts_with("/.well-known/")
        || path.ends_with("/server-card")
        || matches!(
            path,
            LLMS_TXT_PATH
                | AUTH_MD_PATH
                | ROBOTS_TXT_PATH
                | SITEMAP_PATH
                | super::webmcp::WEBMCP_JS_PATH
        )
}

/// Path of `robots.txt`.
pub const ROBOTS_TXT_PATH: &str = "/robots.txt";

/// Path of the sitemap.
pub const SITEMAP_PATH: &str = "/sitemap.xml";

/// A sitemap of `/` and the pages with a `seo(title)`. The router fallback
/// serves it only when no `[seo]` or app route answers `/sitemap.xml`.
fn sitemap(facts: &SiteFacts, origin: &Origin) -> Document {
    use std::fmt::Write as _;

    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    let mut paths: Vec<&str> = if facts.home_noindex {
        Vec::new()
    } else if facts.home_paths.is_empty() {
        vec!["/"]
    } else {
        facts.home_paths.iter().map(String::as_str).collect()
    };
    for page in &facts.pages {
        if !paths.contains(&page.path.as_str()) {
            paths.push(&page.path);
        }
    }
    for path in paths {
        let loc = origin
            .url(path)
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let _ = writeln!(body, "  <url><loc>{loc}</loc></url>");
    }
    body.push_str("</urlset>\n");
    Document {
        content_type: "application/xml",
        headers: Vec::new(),
        body,
    }
}

fn render_inner(facts: &SiteFacts, origin: &Origin, path: &str, now: u64) -> Option<Document> {
    match path {
        SITEMAP_PATH => facts.sitemap.then(|| sitemap(facts, origin)),
        ROBOTS_TXT_PATH => facts.robots_txt.as_ref().map(|body| Document {
            content_type: "text/plain; charset=utf-8",
            headers: Vec::new(),
            body: body.clone(),
        }),
        super::webmcp::WEBMCP_JS_PATH => Some(Document {
            content_type: "text/javascript; charset=utf-8",
            headers: vec![("cache-control", "public, max-age=300".to_owned())],
            body: super::webmcp::script(facts, &facts.csrf_header),
        }),
        super::commerce::UCP_PATH => facts.commerce.ucp.as_ref().map(|u| cors_json(u.as_json())),
        super::commerce::ACP_PATH => facts.commerce.acp.as_ref().map(|a| cors_json(a.as_json())),
        super::web_bot_auth::DIRECTORY_PATH => {
            let key = facts.web_bot_auth.as_ref()?;
            let body = super::web_bot_auth::directory_json(std::slice::from_ref(key));
            let digest = super::web_bot_auth::content_digest(body.as_bytes());
            let mut headers = vec![
                ("content-digest", digest.clone()),
                ("cache-control", "public, max-age=300".to_owned()),
            ];
            // Sign only for the configured site: a signature over a request
            // `Host` would let anyone get a signature for any name.
            if origin.configured {
                let (input, signature) = super::web_bot_auth::sign_directory(
                    key,
                    &origin.signing_authority(),
                    &digest,
                    now,
                );
                headers.push(("signature-input", input));
                headers.push(("signature", signature));
            }
            Some(Document {
                content_type: super::web_bot_auth::DIRECTORY_MEDIA_TYPE,
                headers,
                body,
            })
        }
        LLMS_TXT_PATH => llms_txt(facts, origin),
        API_CATALOG_PATH => api_catalog(facts, origin),
        SERVER_CARD_PATH => server_card(facts, origin),
        SKILLS_INDEX_PATH => skills_index(facts, origin),
        super::AI_CATALOG_PATH | ARD_PATH => Some(ard_manifest(facts, origin)),
        OAUTH_RESOURCE_PATH => protected_resource(facts, origin),
        OAUTH_SERVER_PATH => authorization_server(facts, origin),
        AUTH_MD_PATH => auth_md(facts, origin),
        _ => {
            if let Some(name) = path
                .strip_prefix("/.well-known/agent-skills/")
                .and_then(|rest| rest.strip_suffix("/SKILL.md"))
            {
                return all_skills(facts, origin)
                    .into_iter()
                    .find(|s| s.name == name)
                    .map(|s| Document {
                        content_type: MARKDOWN,
                        headers: Vec::new(),
                        body: s.to_skill_md(),
                    });
            }
            let mcp = facts.mcp.as_ref()?;
            let mut card = (path == server_card_path(&mcp.path))
                .then(|| server_card(facts, origin))
                .flatten()?;
            // SEP-2127: the media type at the endpoint path. The
            // `.well-known` copy stays `application/json` for scanners.
            card.content_type = "application/mcp-server-card+json";
            Some(card)
        }
    }
}

/// The homepage `Link` header value. It always holds a relation that
/// scanners count (`describedby`).
#[must_use]
pub fn homepage_link_header(facts: &SiteFacts, home: &str) -> String {
    let mut links = Vec::new();
    if has_api(facts) {
        links.push(format!(
            "<{API_CATALOG_PATH}>; rel=\"api-catalog\"; type=\"application/linkset+json\""
        ));
    }
    if let Some(api) = &facts.openapi {
        links.push(format!(
            "<{}>; rel=\"service-desc\"; type=\"application/vnd.oai.openapi+json\"",
            api.json_path
        ));
        if let Some(docs) = &api.docs_path {
            links.push(format!("<{docs}>; rel=\"service-doc\"; type=\"text/html\""));
        }
    }
    if facts.llms_txt {
        links.push(format!(
            "<{LLMS_TXT_PATH}>; rel=\"describedby\"; type=\"text/plain\""
        ));
    } else {
        links.push(format!(
            "<{}>; rel=\"describedby\"; type=\"application/ai-catalog+json\"",
            super::AI_CATALOG_PATH
        ));
    }
    links.push(format!(
        "<{}>; rel=\"ai-catalog\"; type=\"application/ai-catalog+json\"",
        super::AI_CATALOG_PATH
    ));
    links.push(format!(
        "<{ARD_PATH}>; rel=\"ard\"; type=\"application/ai-catalog+json\""
    ));
    if facts.markdown {
        // The Markdown copy of this page: `/`, or `/{locale}` on a localized
        // site.
        links.push(format!(
            "<{home}>; rel=\"alternate\"; type=\"text/markdown\""
        ));
    }
    links.join(", ")
}

const MARKDOWN: &str = "text/markdown; charset=utf-8";
const JSON: &str = "application/json";

const fn has_api(facts: &SiteFacts) -> bool {
    facts.mcp.is_some() || facts.openapi.is_some()
}

fn display_name(facts: &SiteFacts, origin: &Origin) -> String {
    facts
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| origin.host.clone())
}

fn json_body(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
}

fn cors_json(value: &Value) -> Document {
    Document {
        content_type: JSON,
        headers: vec![
            ("access-control-allow-origin", "*".to_owned()),
            ("cache-control", "public, max-age=3600".to_owned()),
        ],
        body: json_body(value),
    }
}

// ── llms.txt ────────────────────────────────────────────────────────────────

fn llms_txt(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    if !facts.llms_txt {
        return None;
    }
    // Titles and descriptions are text: Markdown in them must not make a
    // link or emphasis of its own.
    let text = |s: &str| super::markdown::escape_inline(&one_line(s));
    let mut out = format!("# {}\n", text(&display_name(facts, origin)));
    if let Some(d) = facts
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        let _ = writeln!(out, "\n> {}", text(d));
    }
    if facts.markdown {
        out.push_str("\nSend `Accept: text/markdown` to get any page of this site as Markdown.\n");
    }
    if !facts.pages.is_empty() {
        out.push_str("\n## Pages\n\n");
        for page in &facts.pages {
            let _ = write!(out, "- [{}]({})", text(&page.title), origin.url(&page.path));
            if let Some(d) = page.description.as_deref().filter(|d| !d.trim().is_empty()) {
                let _ = write!(out, ": {}", text(d));
            }
            out.push('\n');
        }
    }
    out.push_str("\n## Agents\n\n");
    if let Some(mcp) = &facts.mcp {
        let _ = writeln!(
            out,
            "- [MCP server]({}): Streamable HTTP endpoint.",
            origin.url(&mcp.path)
        );
    }
    if has_api(facts) {
        let _ = writeln!(
            out,
            "- [API catalog]({}): RFC 9727 list of APIs.",
            origin.url(API_CATALOG_PATH)
        );
    }
    if let Some(api) = &facts.openapi {
        let _ = writeln!(
            out,
            "- [OpenAPI]({}): API description.",
            origin.url(&api.json_path)
        );
    }
    if !all_skills(facts, origin).is_empty() {
        let _ = writeln!(
            out,
            "- [Agent skills]({}): skills for this site.",
            origin.url(SKILLS_INDEX_PATH)
        );
    }
    if auth_md_applies(facts, origin) {
        let _ = writeln!(
            out,
            "- [auth.md]({}): how agents get credentials.",
            origin.url(AUTH_MD_PATH)
        );
    }
    let _ = writeln!(
        out,
        "- [Resource manifest]({}): ARD list of agent resources.",
        origin.url(super::AI_CATALOG_PATH)
    );
    let _ = writeln!(
        out,
        "- [Sitemap]({}): all pages.",
        origin.url("/sitemap.xml")
    );
    Some(Document {
        content_type: "text/plain; charset=utf-8",
        headers: Vec::new(),
        body: out,
    })
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── API catalog (RFC 9727) ──────────────────────────────────────────────────

fn api_catalog(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    if !has_api(facts) {
        return None;
    }
    let status = facts
        .health_path
        .as_ref()
        .map(|p| json!([{ "href": origin.url(p), "type": "application/json" }]));
    let mut linkset = Vec::new();
    if let Some(api) = &facts.openapi {
        let mut entry = json!({
            "anchor": origin.url("/"),
            "service-desc": [{
                "href": origin.url(&api.json_path),
                "type": "application/vnd.oai.openapi+json",
            }],
        });
        if let Some(docs) = &api.docs_path {
            entry["service-doc"] = json!([{ "href": origin.url(docs), "type": "text/html" }]);
        }
        if let Some(status) = &status {
            entry["status"] = status.clone();
        }
        linkset.push(entry);
    }
    if let Some(mcp) = &facts.mcp {
        let mut entry = json!({
            "anchor": origin.url(&mcp.path),
            "service-desc": [{
                "href": origin.url(&server_card_path(&mcp.path)),
                "type": "application/mcp-server-card+json",
            }],
        });
        if let Some(status) = &status {
            entry["status"] = status.clone();
        }
        linkset.push(entry);
    }
    Some(Document {
        content_type: "application/linkset+json; profile=\"https://www.rfc-editor.org/info/rfc9727\"",
        headers: vec![("link", format!("<{API_CATALOG_PATH}>; rel=\"api-catalog\""))],
        body: json_body(&json!({ "linkset": linkset })),
    })
}

// ── MCP server card ─────────────────────────────────────────────────────────

/// MCP protocol versions the endpoint speaks. Keep in step with `mcp.rs`.
const MCP_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

fn server_card(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    let mcp = facts.mcp.as_ref()?;
    let display = display_name(facts, origin);
    let name = card_name(&origin.host, &display);
    let version = facts
        .openapi
        .as_ref()
        .map_or_else(|| "1.0.0".to_owned(), |api| api.version.clone());
    let description = truncate_chars(
        facts
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map_or_else(|| format!("MCP server for {display}"), one_line)
            .as_str(),
        100,
    );
    let endpoint = origin.url(&mcp.path);
    let mut card = json!({
        "$schema": "https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json",
        "name": name,
        "title": truncate_chars(&display, 100),
        "version": version,
        "description": description,
        "websiteUrl": origin.url("/"),
        "remotes": [{
            "type": "streamable-http",
            "url": endpoint,
            "supportedProtocolVersions": MCP_PROTOCOL_VERSIONS,
        }],
        // SEP-1649 fields, which the isitagentready.com scanner reads.
        "serverInfo": { "name": name, "title": display, "version": version },
        "protocolVersion": MCP_PROTOCOL_VERSIONS[0],
        "transport": { "type": "streamable-http", "endpoint": endpoint },
        "capabilities": { "tools": { "listChanged": false } },
    });
    if mcp.public_tools {
        card["tools"] = mcp
            .tools
            .iter()
            .map(|t| {
                let mut tool = json!({
                    "name": t.name,
                    "inputSchema": t.input_schema,
                    "annotations": t.annotations,
                });
                if let Some(d) = &t.description {
                    tool["description"] = json!(d);
                }
                tool
            })
            .collect();
    } else {
        // The gate is any Tower layer (or a proxy): its scheme is unknown.
        card["authentication"] = json!({ "required": true });
    }
    let mut doc = cors_json(&card);
    doc.headers
        .push(("access-control-allow-methods", "GET".to_owned()));
    doc.headers.push((
        "access-control-allow-headers",
        "Content-Type, If-None-Match".to_owned(),
    ));
    doc.headers
        .push(("access-control-expose-headers", "ETag".to_owned()));
    Some(doc)
}

/// Reverse-DNS card name: `com.example.shop/shop`.
fn card_name(host: &str, display: &str) -> String {
    let mut labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    labels.reverse();
    let mut ns: String = labels
        .join(".")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        .collect();
    if ns.is_empty() {
        "localhost".clone_into(&mut ns);
    }
    let mut slug: String = display
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        "app".clone_into(&mut slug);
    }
    // The name must stay ASCII, so cut it without an ellipsis.
    let mut name = format!("{ns}/{slug}");
    name.truncate(200);
    name
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ── Agent skills ────────────────────────────────────────────────────────────

/// The registered skills and the `site-guide` skill. A name is published
/// once: the first skill with that name wins, as its path serves it.
fn all_skills(facts: &SiteFacts, origin: &Origin) -> Vec<AgentSkill> {
    let mut skills: Vec<AgentSkill> = Vec::with_capacity(facts.skills.len() + 1);
    for skill in &facts.skills {
        if !skills.iter().any(|s| s.name == skill.name) {
            skills.push(skill.clone());
        }
    }
    if facts.site_guide_skill && !skills.iter().any(|s| s.name == SITE_GUIDE_SKILL) {
        skills.push(site_guide(facts, origin));
    }
    skills
}

fn skills_index(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    let skills = all_skills(facts, origin);
    if skills.is_empty() {
        return None;
    }
    let entries: Vec<Value> = skills
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "type": "skill-md",
                "description": s.description,
                "url": s.path(),
                "digest": sha256_digest(s.to_skill_md().as_bytes()),
            })
        })
        .collect();
    Some(cors_json(&json!({
        "$schema": "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
        "skills": entries,
    })))
}

fn site_guide(facts: &SiteFacts, origin: &Origin) -> AgentSkill {
    let name = one_line(&display_name(facts, origin));
    let mut body = format!("# Use {name}\n\n");
    if let Some(d) = facts
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        let _ = write!(body, "{}\n\n", one_line(d));
    }
    body.push_str("## Read\n\n");
    if facts.markdown {
        body.push_str("- Send `Accept: text/markdown` to get any page as Markdown.\n");
    }
    let _ = writeln!(body, "- Page list: {}", origin.url("/sitemap.xml"));
    if facts.llms_txt {
        let _ = writeln!(body, "- Site overview: {}", origin.url(LLMS_TXT_PATH));
    }
    let _ = writeln!(
        body,
        "- Content rules: {} (`Content-Signal`). Obey them.",
        origin.url("/robots.txt")
    );
    if has_api(facts) {
        body.push_str("\n## Act\n\n");
    }
    if let Some(mcp) = &facts.mcp {
        let _ = writeln!(
            body,
            "- MCP server (Streamable HTTP): {}",
            origin.url(&mcp.path)
        );
        for tool in mcp.tools.iter().filter(|_| mcp.public_tools) {
            match &tool.description {
                Some(d) => {
                    let _ = writeln!(body, "  - `{}`: {}", tool.name, one_line(d));
                }
                None => {
                    let _ = writeln!(body, "  - `{}`", tool.name);
                }
            }
        }
    }
    if let Some(api) = &facts.openapi {
        let _ = writeln!(
            body,
            "- API description (OpenAPI): {}",
            origin.url(&api.json_path)
        );
    }
    if auth_md_applies(facts, origin) {
        let _ = writeln!(
            body,
            "\n## Authenticate\n\n- Read {} before you call the API.",
            origin.url(AUTH_MD_PATH)
        );
    }
    let description = truncate_chars(
        &format!("Read and use {name} as an AI agent: Markdown pages, tools, API, and auth."),
        1024,
    );
    AgentSkill {
        name: SITE_GUIDE_SKILL.to_owned(),
        description,
        body,
    }
}

fn valid_skill_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

pub(super) fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            // Any other control character as a YAML escape, so the value
            // reads back as written.
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The value of the top-level `key` in YAML front matter. It reads what a
/// `SKILL.md` writes: a plain or quoted scalar, with indented continuation
/// lines folded in; a block scalar (`|` or `>`, with `-` or `+` chomping);
/// and a ` #` comment after a plain value.
fn front_matter_field(front: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = front.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.strip_prefix(key).is_some_and(|r| r.starts_with(':')))?;
    let value = lines[at][key.len() + 1..].trim();
    // A value goes on over indented and blank lines, up to the next key.
    let mut more: Vec<&str> = lines[at + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
        .copied()
        .collect();
    if let Some((folded, chomp, indent)) = block_scalar_header(value) {
        return Some(block_scalar(&more, folded, chomp, indent));
    }
    while more.last().is_some_and(|l| l.trim().is_empty()) {
        more.pop();
    }
    // In a double-quoted value, a `\` at the end of a line escapes the break:
    // the lines join with no space.
    let mut lines: Vec<String> = Vec::new();
    let mut glue = false;
    for line in std::iter::once(value).chain(more.iter().map(|l| l.trim())) {
        match lines.last_mut() {
            Some(last) if glue => last.push_str(line),
            _ => lines.push(line.to_owned()),
        }
        glue = false;
        if value.starts_with('"')
            && let Some(last) = lines.last_mut()
            && (last.len() - last.trim_end_matches('\\').len()) % 2 == 1
        {
            last.pop();
            glue = true;
        }
    }
    let joined = fold_lines(lines.iter().map(String::as_str));
    if joined.starts_with(['"', '\'']) {
        // Up to the closing quote: a comment may follow it.
        return Some(yaml_unquote(quoted_scalar(&joined)));
    }
    // A plain value ends at a comment.
    let plain = joined.find(" #").map_or(joined.as_str(), |n| &joined[..n]);
    Some(plain.trim_end().to_owned())
}

/// The quoted scalar at the start of `s`, quotes included. In `"..."` a
/// `\"` is not the end; in `'...'` a `''` is not.
fn quoted_scalar(s: &str) -> &str {
    let bytes = s.as_bytes();
    let quote = bytes[0];
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote == b'"' => i += 2,
            b'\'' if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b if b == quote => return &s[..=i],
            _ => i += 1,
        }
    }
    s
}

/// `|` or `>` with an optional chomping (`-`, `+`) and indentation digit:
/// whether it folds, its chomping (`' '` to clip), and the indent it
/// declares (a top-level key's content is indented by exactly that).
fn block_scalar_header(value: &str) -> Option<(bool, char, Option<usize>)> {
    let value = value.split(" #").next().unwrap_or(value).trim_end();
    let mut chars = value.chars();
    let folded = match chars.next()? {
        '>' => true,
        '|' => false,
        _ => return None,
    };
    let mut chomp = ' ';
    let mut indent = None;
    for c in chars {
        match c {
            '-' | '+' if chomp == ' ' => chomp = c,
            '1'..='9' if indent.is_none() => indent = c.to_digit(10).map(|d| d as usize),
            _ => return None,
        }
    }
    Some((folded, chomp, indent))
}

/// A block scalar's lines, without their common indent, kept (`|`) or
/// folded (`>`), then chomped.
fn block_scalar(lines: &[&str], folded: bool, chomp: char, declared: Option<usize>) -> String {
    let indent = declared.unwrap_or_else(|| {
        lines
            .iter()
            .find(|l| !l.trim().is_empty())
            .map_or(0, |l| l.len() - l.trim_start().len())
    });
    let mut body: Vec<&str> = lines
        .iter()
        .map(|l| l.get(indent..).unwrap_or("").trim_end_matches('\r'))
        .collect();
    let mut trailing = 0;
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
        trailing += 1;
    }
    let mut text = if folded {
        fold_block(&body)
    } else {
        body.join("\n")
    };
    if !text.is_empty() {
        match chomp {
            '-' => {}
            '+' => text.push_str(&"\n".repeat(trailing + 1)),
            _ => text.push('\n'),
        }
    }
    text
}

/// A folded (`>`) block's lines, joined. A break between two plain lines
/// is a space and each blank line a newline; a more-indented line keeps the
/// breaks around it.
fn fold_block(lines: &[&str]) -> String {
    let mut out = String::new();
    let mut prev_more: Option<bool> = None;
    let mut blanks = 0;
    for line in lines {
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        let more = line.starts_with([' ', '\t']);
        match prev_more {
            Some(prev) if prev || more => out.push_str(&"\n".repeat(blanks + 1)),
            Some(_) if blanks == 0 => out.push(' '),
            // The first line, or a plain line after plain lines and blanks.
            _ => out.push_str(&"\n".repeat(blanks)),
        }
        out.push_str(line);
        prev_more = Some(more);
        blanks = 0;
    }
    out
}

/// YAML line folding: a break between two lines is a space, and each blank
/// line is a newline.
fn fold_lines<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    let mut after_text = false;
    for line in lines {
        if line.is_empty() {
            out.push('\n');
            after_text = false;
        } else {
            if after_text {
                out.push(' ');
            }
            out.push_str(line);
            after_text = true;
        }
    }
    out
}

fn yaml_unquote(value: &str) -> String {
    let quoted = value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if !quoted {
        return value.to_owned();
    }
    let inner = &value[1..value.len() - 1];
    if value.starts_with('\'') {
        return inner.replace("''", "'");
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // The YAML 1.2 double-quoted escapes.
        let decoded = match chars.next() {
            None => None,
            Some('0') => Some('\0'),
            Some('a') => Some('\u{7}'),
            Some('b') => Some('\u{8}'),
            Some('t' | '\t') => Some('\t'),
            Some('n') => Some('\n'),
            Some('v') => Some('\u{b}'),
            Some('f') => Some('\u{c}'),
            Some('r') => Some('\r'),
            Some('e') => Some('\u{1b}'),
            Some('N') => Some('\u{85}'),
            Some('_') => Some('\u{a0}'),
            Some('L') => Some('\u{2028}'),
            Some('P') => Some('\u{2029}'),
            Some(e @ ('x' | 'u' | 'U')) => {
                let digits = match e {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let hex: String = chars.by_ref().take(digits).collect();
                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
            }
            // `\"`, `\\`, `\/`, `\ ` stand for themselves.
            Some(other) => Some(other),
        };
        out.extend(decoded);
    }
    out
}

// ── ARD manifest ────────────────────────────────────────────────────────────

fn ard_manifest(facts: &SiteFacts, origin: &Origin) -> Document {
    let display = display_name(facts, origin);
    let urn = |ns: &str, name: &str| format!("urn:air:{}:{ns}:{name}", origin.host);
    let mut entries = Vec::new();
    if let Some(mcp) = &facts.mcp {
        entries.push(json!({
            "identifier": urn("mcp", "server"),
            "displayName": format!("{display} MCP server"),
            "type": "application/mcp-server-card+json",
            "url": origin.url(&server_card_path(&mcp.path)),
        }));
    }
    for skill in all_skills(facts, origin) {
        entries.push(json!({
            "identifier": urn("skill", &skill.name),
            "displayName": skill.name,
            "description": skill.description,
            "type": "application/agent-skills+md",
            "url": origin.url(&skill.path()),
        }));
    }
    if has_api(facts) {
        entries.push(json!({
            "identifier": urn("api", "catalog"),
            "displayName": format!("{display} API catalog"),
            "type": "application/linkset+json",
            "url": origin.url(API_CATALOG_PATH),
        }));
    }
    if let Some(api) = &facts.openapi {
        entries.push(json!({
            "identifier": urn("api", "openapi"),
            "displayName": format!("{display} OpenAPI"),
            "type": "application/vnd.oai.openapi+json",
            "url": origin.url(&api.json_path),
        }));
    }
    if facts.llms_txt {
        entries.push(json!({
            "identifier": urn("doc", "llms-txt"),
            "displayName": format!("{display} llms.txt"),
            "type": "text/plain",
            "url": origin.url(LLMS_TXT_PATH),
        }));
    }
    if entries.is_empty() {
        entries.push(json!({
            "identifier": urn("doc", "sitemap"),
            "displayName": format!("{display} sitemap"),
            "type": "application/xml",
            "url": origin.url("/sitemap.xml"),
        }));
    }
    cors_json(&json!({
        "specVersion": "1.0",
        "host": {
            "displayName": display,
            "identifier": format!("did:web:{}", origin.host),
        },
        "entries": entries,
    }))
}

// ── OAuth and auth.md ───────────────────────────────────────────────────────

fn protected_resource(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    let oauth = &facts.oauth;
    if oauth.authorization_servers.is_empty() {
        return None;
    }
    let mut prm = json!({
        "resource": origin.base,
        "resource_name": display_name(facts, origin),
        "authorization_servers": oauth.authorization_servers,
        "bearer_methods_supported": ["header"],
    });
    if !oauth.scopes_supported.is_empty() {
        prm["scopes_supported"] = json!(oauth.scopes_supported);
    }
    if auth_md_applies(facts, origin) {
        prm["resource_documentation"] = json!(origin.url(AUTH_MD_PATH));
    }
    Some(cors_json(&prm))
}

/// The configured issuer, when this origin may publish its metadata: the
/// issuer has no path, and its origin is the site origin (RFC 8414 §3).
fn local_issuer<'a>(facts: &'a SiteFacts, origin: &Origin) -> Option<&'a str> {
    let issuer = facts
        .oauth
        .authorization_server
        .issuer
        .as_deref()
        .filter(|i| !i.trim().is_empty())?;
    let url = url::Url::parse(issuer).ok()?;
    // RFC 8414 §2: an `https` URL with no query or fragment. Credentials in
    // it would be published in the metadata, so they are refused too.
    let valid = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    let rooted = valid && matches!(url.path(), "" | "/");
    let same_origin = url::Url::parse(&origin.base).is_ok_and(|base| base.origin() == url.origin());
    (rooted && same_origin).then_some(issuer)
}

fn authorization_server(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    let c = &facts.oauth.authorization_server;
    let issuer = local_issuer(facts, origin)?;
    let mut meta = serde_json::Map::new();
    meta.insert("issuer".into(), json!(issuer));
    let mut opt = |key: &str, value: &Option<String>| {
        if let Some(v) = value.as_deref().filter(|v| !v.trim().is_empty()) {
            meta.insert(key.into(), json!(v));
        }
    };
    opt("authorization_endpoint", &c.authorization_endpoint);
    opt("token_endpoint", &c.token_endpoint);
    opt("jwks_uri", &c.jwks_uri);
    opt("registration_endpoint", &c.registration_endpoint);
    // `code` is the default only when there is an authorization endpoint
    // for it.
    let response_types = if !c.response_types_supported.is_empty() {
        c.response_types_supported.clone()
    } else if c.authorization_endpoint.is_some() {
        vec!["code".to_owned()]
    } else {
        Vec::new()
    };
    if !response_types.is_empty() {
        meta.insert("response_types_supported".into(), json!(response_types));
    }
    for (key, list) in [
        ("scopes_supported", &c.scopes_supported),
        ("grant_types_supported", &c.grant_types_supported),
        (
            "code_challenge_methods_supported",
            &c.code_challenge_methods_supported,
        ),
    ] {
        if !list.is_empty() {
            meta.insert(key.into(), json!(list));
        }
    }
    if let Some(identity) = c.agent_identity_endpoint.as_deref() {
        // Auth.md names (WorkOS) and the scanner names, side by side.
        let mut agent = json!({
            "skill": origin.url(AUTH_MD_PATH),
            "identity_endpoint": identity,
            "register_uri": identity,
        });
        if let Some(claim) = c.agent_claim_endpoint.as_deref() {
            agent["claim_endpoint"] = json!(claim);
            agent["claim_uri"] = json!(claim);
        }
        if !c.agent_identity_types.is_empty() {
            agent["identity_types_supported"] = json!(c.agent_identity_types);
        }
        if !c.agent_credential_types.is_empty() {
            agent["credential_types_supported"] = json!(c.agent_credential_types);
            if c.agent_identity_types.iter().any(|t| t == "anonymous") {
                agent["anonymous"] =
                    json!({ "credential_types_supported": c.agent_credential_types });
            }
        }
        if !c.agent_assertion_types.is_empty() {
            agent["identity_assertion"] = json!({
                "assertion_types_supported": c.agent_assertion_types,
                "credential_types_supported": c.agent_credential_types,
            });
        }
        if let Some(revoke) = c.agent_revocation_endpoint.as_deref() {
            agent["revocation_uri"] = json!(revoke);
        }
        if let Some(events) = c.agent_events_endpoint.as_deref() {
            agent["events_endpoint"] = json!(events);
        }
        meta.insert("agent_auth".into(), agent);
    }
    Some(cors_json(&Value::Object(meta)))
}

/// `/auth.md` needs a concrete way to get a credential: a registration
/// page, or OAuth metadata.
fn auth_md_applies(facts: &SiteFacts, origin: &Origin) -> bool {
    facts.auth_md.enabled
        && (facts.auth_md.registration_url.is_some()
            || !facts.oauth.authorization_servers.is_empty()
            || local_issuer(facts, origin).is_some())
}

fn auth_md(facts: &SiteFacts, origin: &Origin) -> Option<Document> {
    if !auth_md_applies(facts, origin) {
        return None;
    }
    let name = one_line(&display_name(facts, origin));
    let oauth = &facts.oauth;
    let mut md = format!(
        "# {name} auth.md\n\nThis page tells AI agents how to get and use a credential for {name}.\n"
    );
    let _ = write!(
        md,
        "\n## Audience\n\nAI agents and scripts that call the {name} API"
    );
    if facts.mcp.is_some() {
        md.push_str(" or its MCP server");
    }
    md.push_str(".\n\n## Discover\n\n");
    if let Some(mcp) = &facts.mcp {
        let _ = writeln!(md, "- MCP server: {}", origin.url(&mcp.path));
    }
    if has_api(facts) {
        let _ = writeln!(md, "- API catalog: {}", origin.url(API_CATALOG_PATH));
    }
    if !oauth.authorization_servers.is_empty() {
        let _ = writeln!(
            md,
            "- OAuth protected resource metadata: {}",
            origin.url(OAUTH_RESOURCE_PATH)
        );
    }
    if local_issuer(facts, origin).is_some() {
        let _ = writeln!(
            md,
            "- OAuth authorization server metadata: {}",
            origin.url(OAUTH_SERVER_PATH)
        );
    }
    md.push_str("\n## Pick a method\n\n- `api_key`: a bearer API token.\n");
    if !oauth.authorization_servers.is_empty() {
        let _ = write!(
            md,
            "- `oauth2`: an access token from {}.",
            oauth.authorization_servers.join(", ")
        );
        if !oauth.scopes_supported.is_empty() {
            let _ = write!(md, " Scopes: `{}`.", oauth.scopes_supported.join("`, `"));
        }
        md.push('\n');
    }
    md.push_str("\n## Register\n\n");
    match &facts.auth_md.registration_url {
        Some(url) => {
            let _ = writeln!(md, "Get a credential at {url}.");
        }
        None => md.push_str("Ask the site operator for an API token.\n"),
    }
    if let Some(extra) = facts.auth_md.instructions.as_deref() {
        let _ = writeln!(md, "\n{}", extra.trim());
    }
    md.push_str(
        "\n## Use the credential\n\nSend `Authorization: Bearer <token>` on each API request",
    );
    if facts.mcp.is_some() {
        md.push_str(" and on each MCP request");
    }
    md.push_str(
        ".\n\n## Errors\n\n- `401`: the credential is missing, expired, or not valid.\n- `403`: the credential is valid but does not allow the action.\n",
    );
    Some(Document {
        content_type: MARKDOWN,
        headers: Vec::new(),
        body: md,
    })
}

// ── Host header checks ──────────────────────────────────────────────────────

/// A `Host` value made of host characters and an optional port.
fn valid_authority(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 255
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        && !h.contains("..")
}

/// Host part of `host[:port]` or `[v6]:port`.
fn authority_host(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map_or(authority, |end| &authority[..=end]);
    }
    authority.split(':').next().unwrap_or(authority)
}

/// `sha256:<hex>` of `bytes`.
#[must_use]
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::resolve(Some("https://shop.example.com/"), None)
    }

    fn content_site() -> SiteFacts {
        SiteFacts {
            name: Some("Shop".to_owned()),
            description: Some("Hand-made things.".to_owned()),
            health_path: Some("/health".to_owned()),
            pages: vec![PageFacts {
                path: "/about".to_owned(),
                title: "About".to_owned(),
                description: Some("Who we are".to_owned()),
            }],
            site_guide_skill: true,
            llms_txt: true,
            markdown: true,
            auth_md: AuthMdConfig::default(),
            ..SiteFacts::default()
        }
    }

    fn api_site() -> SiteFacts {
        SiteFacts {
            mcp: Some(McpFacts {
                path: "/mcp".to_owned(),
                tools: vec![ToolFacts {
                    name: "list_todos".to_owned(),
                    description: Some("List todos".to_owned()),
                    input_schema: json!({"type": "object"}),
                    annotations: json!({"readOnlyHint": true}),
                }],
                public_tools: true,
            }),
            openapi: Some(OpenApiFacts {
                json_path: "/openapi.json".to_owned(),
                docs_path: Some("/swagger-ui".to_owned()),
                version: "2.1.0".to_owned(),
            }),
            ..content_site()
        }
    }

    fn json_doc(facts: &SiteFacts, path: &str) -> (Document, Value) {
        let doc = render(facts, &origin(), path).unwrap_or_else(|| panic!("no {path}"));
        let value = serde_json::from_str(&doc.body).expect("valid JSON");
        (doc, value)
    }

    // ── Origin ──────────────────────────────────────────────────────────

    #[test]
    fn origin_prefers_base_url() {
        let o = Origin::resolve(Some("https://Shop.Example.com:8443/"), Some("evil.test"));
        assert_eq!(o.base, "https://Shop.Example.com:8443");
        assert_eq!(o.host, "shop.example.com");
    }

    #[test]
    fn a_base_url_with_a_query_or_fragment_is_not_used() {
        for base in [
            "https://example.com?tenant=a",
            "https://example.com/#top",
            "ftp://example.com",
            "https://deploy-token@example.com",
            "https://user:pass@example.com",
        ] {
            let o = Origin::resolve(Some(base), Some("app.example.com"));
            assert!(!o.configured, "{base}");
            assert_eq!(o.url("/llms.txt"), "https://app.example.com/llms.txt");
        }
    }

    #[test]
    fn origin_falls_back_to_host_header() {
        assert_eq!(
            Origin::resolve(None, Some("localhost:3000")).base,
            "http://localhost:3000"
        );
        assert_eq!(
            Origin::resolve(None, Some("127.0.0.1:3000")).base,
            "http://127.0.0.1:3000"
        );
        let o = Origin::resolve(None, Some("App.Example.com"));
        assert_eq!(o.base, "https://app.example.com");
        assert_eq!(o.host, "app.example.com");
    }

    #[test]
    fn origin_rejects_a_hostile_host_header() {
        for bad in ["a b", "x/../y", "<script>", "a\"b", ""] {
            assert_eq!(
                Origin::resolve(None, Some(bad)).base,
                "http://localhost",
                "{bad}"
            );
        }
        assert_eq!(Origin::resolve(None, None).host, "localhost");
    }

    // ── Agent skills ────────────────────────────────────────────────────

    #[test]
    fn skill_round_trips_through_skill_md() {
        let skill = AgentSkill::new("refunds", "Draft refunds.", "# Refunds\n\nSteps.\n").unwrap();
        let md = skill.to_skill_md();
        assert_eq!(
            md,
            "---\nname: refunds\ndescription: \"Draft refunds.\"\n---\n\n# Refunds\n\nSteps.\n"
        );
        assert_eq!(AgentSkill::parse(&md).unwrap(), skill);
    }

    #[test]
    fn skill_parse_reads_plain_and_quoted_front_matter() {
        let skill = AgentSkill::parse(
            "---\nname: a-b\ndescription: Does a thing: well\nlicense: MIT\n---\nBody\n",
        )
        .unwrap();
        assert_eq!(skill.name(), "a-b");
        assert_eq!(skill.description(), "Does a thing: well");
        assert!(AgentSkill::parse("no front matter").is_err());
    }

    #[test]
    fn skill_parse_reads_block_and_multi_line_scalars() {
        let parse = |front: &str| {
            AgentSkill::parse(&format!("---\nname: a\n{front}\nlicense: MIT\n---\nBody\n"))
                .unwrap()
                .description()
                .to_owned()
        };
        assert_eq!(
            parse("description: >-\n  Folded\n  text.\n\n  Next."),
            "Folded text.\nNext."
        );
        assert_eq!(parse("description: |\n  one\n  two"), "one\ntwo\n");
        assert_eq!(parse("description: |- # note\n  kept"), "kept");
        assert_eq!(
            parse("description: >-\n  first\n    indented\n  last"),
            "first\n  indented\nlast"
        );
        // An indentation digit wins over the first line's indent.
        assert_eq!(
            parse("description: |2-\n    deep\n  shallow"),
            "  deep\nshallow"
        );
        assert_eq!(
            parse("description: A long\n  plain value # note"),
            "A long plain value"
        );
        assert_eq!(
            parse("description: \"A long\n  quoted one\""),
            "A long quoted one"
        );
        assert_eq!(parse("description: Plain\n\n"), "Plain");
        assert_eq!(
            parse("description: \"Draft refunds.\" # note"),
            "Draft refunds."
        );
        assert_eq!(
            parse("description: \"Say \\\"hi\\\"\" # note"),
            "Say \"hi\""
        );
        assert_eq!(parse("description: 'It''s here' # note"), "It's here");
        assert_eq!(parse(r#"description: "First\tSecond""#), "First\tSecond");
        assert_eq!(
            parse("description: \"Draft\\\n  refunds.\""),
            "Draftrefunds."
        );
        assert_eq!(parse("description: \"Ends \\\\\n  here\""), "Ends \\ here");
        assert_eq!(
            parse(r#"description: "\u263A \x41\U0001F600 \\ \/""#),
            "\u{263A} A\u{1F600} \\ /"
        );
        // A tab, a carriage return or another control character survives a
        // round trip through the served `SKILL.md`.
        for text in ["One\tTwo", "First\rSecond", "A\u{8}B\u{1b}C"] {
            let skill = AgentSkill::new("a", text, "Body").unwrap();
            assert_eq!(
                AgentSkill::parse(&skill.to_skill_md()).unwrap(),
                skill,
                "{text:?}"
            );
        }
    }

    #[test]
    fn skill_names_follow_the_rfc() {
        for bad in ["", "A", "a_b", "-a", "a-", "a--b", &"a".repeat(65)] {
            assert!(
                matches!(
                    AgentSkill::new(bad, "d", ""),
                    Err(AgentSkillError::InvalidName(_))
                ),
                "{bad:?}"
            );
        }
        assert!(AgentSkill::new("a1-b2", "d", "").is_ok());
        assert!(matches!(
            AgentSkill::new("a", "", ""),
            Err(AgentSkillError::InvalidDescription(_))
        ));
    }

    #[test]
    fn skills_index_lists_digests_of_the_served_bytes() {
        let mut facts = content_site();
        facts.skills = vec![AgentSkill::new("refunds", "Draft refunds.", "Body").unwrap()];
        let (doc, index) = json_doc(&facts, SKILLS_INDEX_PATH);
        assert_eq!(doc.content_type, "application/json");
        assert_eq!(
            index["$schema"],
            "https://schemas.agentskills.io/discovery/0.2.0/schema.json"
        );
        let skills = index["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2, "user skill + site-guide: {index}");
        for entry in skills {
            let url = entry["url"].as_str().unwrap();
            assert_eq!(entry["type"], "skill-md");
            let served = render(&facts, &origin(), url).expect("artifact served");
            assert_eq!(served.content_type, "text/markdown; charset=utf-8");
            assert_eq!(entry["digest"], sha256_digest(served.body.as_bytes()));
        }
    }

    #[test]
    fn a_duplicate_skill_name_is_published_once() {
        let mut facts = content_site();
        facts.skills = vec![
            AgentSkill::new("refunds", "First.", "One").unwrap(),
            AgentSkill::new("refunds", "Second.", "Two").unwrap(),
        ];
        let (_, index) = json_doc(&facts, SKILLS_INDEX_PATH);
        let refunds: Vec<&Value> = index["skills"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "refunds")
            .collect();
        assert_eq!(refunds.len(), 1, "{index}");
        let served = render(&facts, &origin(), refunds[0]["url"].as_str().unwrap()).unwrap();
        assert_eq!(refunds[0]["digest"], sha256_digest(served.body.as_bytes()));
    }

    #[test]
    fn a_trailing_slash_mcp_mount_gets_a_clean_card_path() {
        assert_eq!(server_card_path("/"), "/server-card");
        assert_eq!(server_card_path("/mcp/"), "/mcp/server-card");
        let mut facts = api_site();
        facts.mcp.as_mut().unwrap().path = "/mcp/".to_owned();
        assert!(render(&facts, &origin(), "/mcp/server-card").is_some());
        let (_, catalog) = json_doc(&facts, API_CATALOG_PATH);
        assert!(!catalog.to_string().contains("//server-card"), "{catalog}");
    }

    #[test]
    fn the_fallback_sitemap_lists_home_and_the_titled_pages() {
        let mut facts = content_site();
        assert!(render(&facts, &origin(), SITEMAP_PATH).is_none());
        facts.sitemap = true;
        let doc = render(&facts, &origin(), SITEMAP_PATH).unwrap();
        assert_eq!(doc.content_type, "application/xml");
        assert!(
            doc.body.contains("<loc>https://shop.example.com/</loc>"),
            "{}",
            doc.body
        );
        assert!(
            doc.body
                .contains("<loc>https://shop.example.com/about</loc>"),
            "{}",
            doc.body
        );
    }

    #[test]
    fn a_locale_prefixed_sitemap_lists_each_locale_home() {
        let mut facts = content_site();
        facts.sitemap = true;
        facts.home_paths = vec!["/en".to_owned(), "/fr".to_owned()];
        let body = render(&facts, &origin(), SITEMAP_PATH).unwrap().body;
        assert!(
            body.contains("<loc>https://shop.example.com/en</loc>"),
            "{body}"
        );
        assert!(
            body.contains("<loc>https://shop.example.com/fr</loc>"),
            "{body}"
        );
        assert!(
            !body.contains("<loc>https://shop.example.com/</loc>"),
            "{body}"
        );
    }

    #[test]
    fn site_guide_points_at_what_the_app_has() {
        let mut facts = api_site();
        facts.auth_md.registration_url = Some("https://shop.example.com/tokens".to_owned());
        let doc = render(
            &facts,
            &origin(),
            "/.well-known/agent-skills/site-guide/SKILL.md",
        )
        .unwrap();
        let skill = AgentSkill::parse(&doc.body).unwrap();
        assert_eq!(skill.name(), SITE_GUIDE_SKILL);
        for want in [
            "Accept: text/markdown",
            "https://shop.example.com/mcp",
            "`list_todos`",
            "https://shop.example.com/openapi.json",
            "https://shop.example.com/auth.md",
        ] {
            assert!(doc.body.contains(want), "missing {want}:\n{}", doc.body);
        }
    }

    #[test]
    fn no_skills_means_no_index() {
        let mut facts = content_site();
        facts.site_guide_skill = false;
        assert!(render(&facts, &origin(), SKILLS_INDEX_PATH).is_none());
    }

    // ── llms.txt ────────────────────────────────────────────────────────

    #[test]
    fn llms_txt_names_the_site_and_lists_pages_and_agent_entry_points() {
        let doc = render(&api_site(), &origin(), LLMS_TXT_PATH).unwrap();
        assert_eq!(doc.content_type, "text/plain; charset=utf-8");
        assert!(
            doc.body.starts_with("# Shop\n\n> Hand-made things.\n"),
            "{}",
            doc.body
        );
        for want in [
            "- [About](https://shop.example.com/about): Who we are",
            "(https://shop.example.com/mcp)",
            "(https://shop.example.com/.well-known/api-catalog)",
            "(https://shop.example.com/openapi.json)",
            "Accept: text/markdown",
        ] {
            assert!(doc.body.contains(want), "missing {want}:\n{}", doc.body);
        }
    }

    #[test]
    fn llms_txt_titles_are_text() {
        let mut facts = api_site();
        facts.pages[0].title = "Docs](https://evil.example) [More".to_owned();
        let doc = render(&facts, &origin(), LLMS_TXT_PATH).unwrap();
        assert!(
            doc.body
                .contains(r"- [Docs\](https://evil.example) \[More](https://shop.example.com/"),
            "{}",
            doc.body
        );
    }

    #[test]
    fn llms_txt_uses_the_host_without_a_name_and_can_be_disabled() {
        let mut facts = content_site();
        facts.name = None;
        let doc = render(&facts, &origin(), LLMS_TXT_PATH).unwrap();
        assert!(doc.body.starts_with("# shop.example.com\n"), "{}", doc.body);
        facts.llms_txt = false;
        assert!(render(&facts, &origin(), LLMS_TXT_PATH).is_none());
    }

    // ── API catalog ─────────────────────────────────────────────────────

    #[test]
    fn api_catalog_is_a_linkset_for_openapi_and_mcp() {
        let (doc, catalog) = json_doc(&api_site(), API_CATALOG_PATH);
        assert_eq!(
            doc.content_type,
            "application/linkset+json; profile=\"https://www.rfc-editor.org/info/rfc9727\""
        );
        assert!(
            doc.headers
                .iter()
                .any(|(k, v)| *k == "link" && v.contains("rel=\"api-catalog\"")),
            "HEAD needs the api-catalog Link: {:?}",
            doc.headers
        );
        let set = catalog["linkset"].as_array().unwrap();
        let api = set
            .iter()
            .find(|e| e["service-desc"][0]["href"] == "https://shop.example.com/openapi.json")
            .unwrap_or_else(|| panic!("no OpenAPI entry: {catalog}"));
        assert_eq!(
            api["service-doc"][0]["href"],
            "https://shop.example.com/swagger-ui"
        );
        assert_eq!(api["status"][0]["href"], "https://shop.example.com/health");
        let mcp = set
            .iter()
            .find(|e| e["anchor"] == "https://shop.example.com/mcp")
            .unwrap_or_else(|| panic!("no MCP entry: {catalog}"));
        assert_eq!(
            mcp["service-desc"][0]["href"],
            "https://shop.example.com/mcp/server-card"
        );
    }

    #[test]
    fn content_site_has_no_api_catalog() {
        assert!(render(&content_site(), &origin(), API_CATALOG_PATH).is_none());
    }

    // ── MCP server card ─────────────────────────────────────────────────

    #[test]
    fn server_card_is_served_at_both_paths_in_hybrid_form() {
        for (path, media) in [
            (SERVER_CARD_PATH, "application/json"),
            ("/mcp/server-card", "application/mcp-server-card+json"),
        ] {
            let (doc, card) = json_doc(&api_site(), path);
            assert_eq!(doc.content_type, media, "{path}");
            assert!(
                doc.headers
                    .contains(&("access-control-allow-origin", "*".to_owned()))
            );
            assert_eq!(card["name"], "com.example.shop/shop");
            assert_eq!(card["serverInfo"]["name"], "com.example.shop/shop");
            assert_eq!(card["version"], "2.1.0");
            assert_eq!(card["remotes"][0]["type"], "streamable-http");
            assert_eq!(card["remotes"][0]["url"], "https://shop.example.com/mcp");
            assert_eq!(
                card["transport"]["endpoint"],
                "https://shop.example.com/mcp"
            );
            assert_eq!(card["tools"][0]["name"], "list_todos");
            assert!(card["description"].as_str().unwrap().chars().count() <= 100);
        }
    }

    #[test]
    fn server_card_hides_tools_behind_secure_mcp() {
        let mut facts = api_site();
        let mcp = facts.mcp.as_mut().unwrap();
        mcp.public_tools = false;
        mcp.tools.clear();
        let (_, card) = json_doc(&facts, SERVER_CARD_PATH);
        assert!(card.get("tools").is_none(), "{card}");
        assert_eq!(card["authentication"]["required"], true);
        assert!(card["authentication"].get("schemes").is_none(), "{card}");
    }

    #[test]
    fn no_mcp_means_no_server_card() {
        assert!(render(&content_site(), &origin(), SERVER_CARD_PATH).is_none());
    }

    // ── ARD ─────────────────────────────────────────────────────────────

    #[test]
    fn ard_manifest_lists_every_resource_at_both_names() {
        for path in [super::super::AI_CATALOG_PATH, ARD_PATH] {
            let (doc, ard) = json_doc(&api_site(), path);
            assert_eq!(doc.content_type, "application/json");
            assert!(
                doc.headers
                    .contains(&("access-control-allow-origin", "*".to_owned()))
            );
            assert_eq!(ard["specVersion"], "1.0");
            assert_eq!(ard["host"]["displayName"], "Shop");
            assert_eq!(ard["host"]["identifier"], "did:web:shop.example.com");
            let entries = ard["entries"].as_array().unwrap();
            let types: Vec<&str> = entries
                .iter()
                .map(|e| e["type"].as_str().unwrap())
                .collect();
            for want in [
                "application/mcp-server-card+json",
                "application/agent-skills+md",
                "application/linkset+json",
                "application/vnd.oai.openapi+json",
            ] {
                assert!(types.contains(&want), "missing {want}: {types:?}");
            }
            for e in entries {
                assert!(
                    e["identifier"]
                        .as_str()
                        .unwrap()
                        .starts_with("urn:air:shop.example.com:"),
                    "{e}"
                );
                assert!(e["displayName"].is_string(), "{e}");
                assert!(e["url"].is_string() != e.get("data").is_some(), "{e}");
            }
        }
    }

    #[test]
    fn content_site_ard_still_has_entries() {
        let (_, ard) = json_doc(&content_site(), ARD_PATH);
        assert!(!ard["entries"].as_array().unwrap().is_empty(), "{ard}");
    }

    // ── OAuth and auth.md ───────────────────────────────────────────────

    #[test]
    fn protected_resource_metadata_needs_authorization_servers() {
        let mut facts = api_site();
        assert!(render(&facts, &origin(), OAUTH_RESOURCE_PATH).is_none());
        facts.oauth.authorization_servers = vec!["https://auth.example.com".to_owned()];
        let (_, prm) = json_doc(&facts, OAUTH_RESOURCE_PATH);
        assert_eq!(prm["resource"], "https://shop.example.com");
        assert_eq!(prm["authorization_servers"][0], "https://auth.example.com");
        assert_eq!(prm["bearer_methods_supported"][0], "header");
        assert!(
            prm.get("scopes_supported").is_none(),
            "empty arrays are omitted"
        );
    }

    #[test]
    fn authorization_server_metadata_needs_an_issuer() {
        let mut facts = api_site();
        assert!(render(&facts, &origin(), OAUTH_SERVER_PATH).is_none());
        let as_cfg = &mut facts.oauth.authorization_server;
        as_cfg.issuer = Some("https://shop.example.com".to_owned());
        as_cfg.token_endpoint = Some("https://shop.example.com/oauth/token".to_owned());
        as_cfg.authorization_endpoint = Some("https://shop.example.com/oauth/authorize".to_owned());
        as_cfg.agent_credential_types = vec!["api_key".to_owned()];
        as_cfg.agent_identity_endpoint = Some("https://shop.example.com/agent/identity".to_owned());
        as_cfg.agent_identity_types = vec!["anonymous".to_owned()];
        let (_, meta) = json_doc(&facts, OAUTH_SERVER_PATH);
        assert_eq!(meta["issuer"], "https://shop.example.com");
        assert_eq!(meta["response_types_supported"][0], "code");
        assert_eq!(
            meta["agent_auth"]["skill"],
            "https://shop.example.com/auth.md"
        );
        assert_eq!(
            meta["agent_auth"]["identity_endpoint"],
            "https://shop.example.com/agent/identity"
        );
        assert_eq!(
            meta["agent_auth"]["register_uri"],
            meta["agent_auth"]["identity_endpoint"]
        );
        assert_eq!(
            meta["agent_auth"]["anonymous"]["credential_types_supported"][0],
            "api_key"
        );
        assert!(meta.get("jwks_uri").is_none());

        // RFC 8414 §2: https, no query, no fragment. No credentials either.
        for bad in [
            "https://shop.example.com?tenant=a",
            "https://shop.example.com#x",
            "https://token@shop.example.com",
            "https://user:pass@shop.example.com",
        ] {
            facts.oauth.authorization_server.issuer = Some(bad.to_owned());
            assert!(
                render(&facts, &origin(), OAUTH_SERVER_PATH).is_none(),
                "{bad}"
            );
        }
        facts.oauth.authorization_server.issuer = Some("http://localhost:3000".to_owned());
        let local = Origin::resolve(Some("http://localhost:3000"), None);
        assert!(render(&facts, &local, OAUTH_SERVER_PATH).is_none());
    }

    #[test]
    fn the_request_authority_must_match_the_signed_one() {
        let o = Origin::resolve(Some("https://Example.com"), None);
        assert!(o.is_request_authority(Some("example.com")));
        assert!(o.is_request_authority(Some("Example.com:443")));
        assert!(!o.is_request_authority(Some("www.example.com")));
        assert!(!o.is_request_authority(Some("example.com:8443")));
        assert!(!o.is_request_authority(None));

        // A scheme in capitals still names the default port.
        let o = Origin::resolve(Some("HTTPS://example.com:443"), None);
        assert_eq!(o.signing_authority(), "example.com");
        assert!(o.is_request_authority(Some("example.com")));
    }

    #[test]
    fn a_noindex_home_is_not_in_the_fallback_sitemap() {
        let mut facts = content_site();
        facts.sitemap = true;
        facts.home_noindex = true;
        let doc = render(&facts, &origin(), SITEMAP_PATH).unwrap();
        assert!(
            !doc.body.contains("<loc>https://shop.example.com/</loc>"),
            "{}",
            doc.body
        );
        assert!(doc.body.contains("/about</loc>"), "{}", doc.body);
    }

    #[test]
    fn auth_md_needs_an_issuer_this_site_can_publish() {
        let mut facts = api_site();
        facts.oauth.authorization_server.issuer = Some("https://auth.elsewhere.example".to_owned());
        assert!(render(&facts, &origin(), AUTH_MD_PATH).is_none());
        facts.oauth.authorization_server.issuer = Some("https://shop.example.com".to_owned());
        let doc = render(&facts, &origin(), AUTH_MD_PATH).unwrap();
        assert!(
            doc.body.contains("OAuth authorization server metadata"),
            "{}",
            doc.body
        );
    }

    #[test]
    fn auth_md_is_served_for_api_sites_with_an_auth_md_heading() {
        let mut facts = api_site();
        facts.oauth.authorization_servers = vec!["https://auth.example.com".to_owned()];
        facts.auth_md.registration_url =
            Some("https://shop.example.com/settings/tokens".to_owned());
        let doc = render(&facts, &origin(), AUTH_MD_PATH).unwrap();
        assert_eq!(doc.content_type, "text/markdown; charset=utf-8");
        assert!(doc.body.starts_with("# Shop auth.md\n"), "{}", doc.body);
        for want in [
            "Authorization: Bearer",
            "https://shop.example.com/settings/tokens",
            "https://shop.example.com/.well-known/oauth-protected-resource",
            "https://auth.example.com",
            "https://shop.example.com/mcp",
        ] {
            assert!(doc.body.contains(want), "missing {want}:\n{}", doc.body);
        }
    }

    #[test]
    fn content_site_has_no_auth_md() {
        assert!(render(&content_site(), &origin(), AUTH_MD_PATH).is_none());
        let mut facts = api_site();
        facts.auth_md.enabled = false;
        assert!(render(&facts, &origin(), AUTH_MD_PATH).is_none());
    }

    #[test]
    fn authorization_server_metadata_stays_honest() {
        let mut facts = api_site();
        let c = &mut facts.oauth.authorization_server;
        c.issuer = Some("https://shop.example.com".to_owned());
        let (_, meta) = json_doc(&facts, OAUTH_SERVER_PATH);
        assert!(
            meta.get("response_types_supported").is_none(),
            "no `code` without an authorization endpoint: {meta}"
        );
        for foreign in ["https://auth.other.com", "https://shop.example.com/tenant"] {
            facts.oauth.authorization_server.issuer = Some(foreign.to_owned());
            assert!(
                render(&facts, &origin(), OAUTH_SERVER_PATH).is_none(),
                "{foreign}"
            );
        }
        // With no `base_url`, the request `Host` must match the issuer too.
        facts.oauth.authorization_server.issuer = Some("https://auth.example".to_owned());
        let api_host = Origin::resolve(None, Some("api.example"));
        assert!(render(&facts, &api_host, OAUTH_SERVER_PATH).is_none());
        let auth_host = Origin::resolve(None, Some("auth.example"));
        assert!(render(&facts, &auth_host, OAUTH_SERVER_PATH).is_some());
    }

    #[test]
    fn auth_md_needs_a_concrete_way_to_get_a_credential() {
        assert!(render(&api_site(), &origin(), AUTH_MD_PATH).is_none());
    }

    #[test]
    fn host_derived_documents_are_never_publicly_cached() {
        let host_origin = Origin::resolve(None, Some("evil.example"));
        assert!(!host_origin.configured);
        // A document with no cache policy of its own gets `no-store` too.
        for path in [ARD_PATH, LLMS_TXT_PATH] {
            let doc = render(&api_site(), &host_origin, path).unwrap();
            let policies: Vec<&String> = doc
                .headers
                .iter()
                .filter(|(n, _)| *n == "cache-control")
                .map(|(_, v)| v)
                .collect();
            assert_eq!(policies, ["no-store"], "{path}: {:?}", doc.headers);
        }
        let doc = render(&api_site(), &origin(), ARD_PATH).unwrap();
        assert!(
            doc.headers
                .contains(&("cache-control", "public, max-age=3600".to_owned()))
        );
    }

    #[test]
    fn robots_txt_is_served_only_when_aeo_owns_it() {
        let mut facts = content_site();
        assert!(render(&facts, &origin(), ROBOTS_TXT_PATH).is_none());
        facts.robots_txt = Some("User-agent: *\nAllow: /\n".to_owned());
        let doc = render(&facts, &origin(), ROBOTS_TXT_PATH).unwrap();
        assert_eq!(doc.content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn link_header_keeps_a_counted_relation_without_llms_txt() {
        let mut facts = content_site();
        facts.llms_txt = false;
        let link = homepage_link_header(&facts, "/");
        assert!(
            link.contains("</.well-known/ai-catalog.json>; rel=\"describedby\""),
            "{link}"
        );
    }

    #[test]
    fn document_paths_are_recognized_cheaply() {
        for p in [
            "/llms.txt",
            "/auth.md",
            "/robots.txt",
            "/.well-known/ard.json",
            "/mcp/server-card",
        ] {
            assert!(is_document_path(p), "{p}");
        }
        for p in ["/", "/favicon.ico", "/about"] {
            assert!(!is_document_path(p), "{p}");
        }
    }

    // ── WebMCP and Web Bot Auth ─────────────────────────────────────────

    #[test]
    fn webmcp_script_is_always_served() {
        let doc = render(&content_site(), &origin(), "/_autumn/webmcp.js").unwrap();
        assert_eq!(doc.content_type, "text/javascript; charset=utf-8");
        let doc = render(&api_site(), &origin(), "/_autumn/webmcp.js").unwrap();
        assert!(doc.body.contains("\"list_todos\""), "{}", doc.body);
    }

    #[test]
    fn key_directory_needs_a_key_and_is_signed() {
        let path = super::super::web_bot_auth::DIRECTORY_PATH;
        let mut facts = content_site();
        assert!(render(&facts, &origin(), path).is_none());
        facts.web_bot_auth = Some(
            super::super::web_bot_auth::WebBotAuthKey::from_seed_b64(
                "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A",
            )
            .unwrap(),
        );
        let unsigned = render_at(&facts, &Origin::resolve(None, Some("x.test")), path, 1).unwrap();
        assert!(
            !unsigned.headers.iter().any(|(k, _)| *k == "signature"),
            "no signature for a request-derived host"
        );
        let doc = render_at(&facts, &origin(), path, 1_700_000_000).unwrap();
        assert!(
            doc.headers
                .iter()
                .any(|(k, v)| *k == "content-digest" && v.starts_with("sha-256=:"))
        );
        assert_eq!(
            doc.content_type,
            "application/http-message-signatures-directory+json"
        );
        let input = &doc
            .headers
            .iter()
            .find(|(k, _)| *k == "signature-input")
            .unwrap()
            .1;
        assert!(input.contains("created=1700000000"), "{input}");
        assert!(doc.body.contains("\"OKP\""));
        assert_eq!(origin().authority(), "shop.example.com");
    }

    // ── Link header and unknown paths ───────────────────────────────────

    #[test]
    fn homepage_link_header_lists_agent_relations() {
        let link = homepage_link_header(&api_site(), "/");
        for want in [
            "</.well-known/api-catalog>; rel=\"api-catalog\"",
            "</openapi.json>; rel=\"service-desc\"",
            "</swagger-ui>; rel=\"service-doc\"",
            "</llms.txt>; rel=\"describedby\"",
            "</.well-known/ai-catalog.json>; rel=\"ai-catalog\"",
            "</>; rel=\"alternate\"; type=\"text/markdown\"",
        ] {
            assert!(link.contains(want), "missing {want}: {link}");
        }
        let content = homepage_link_header(&content_site(), "/");
        assert!(content.contains("rel=\"describedby\""), "{content}");
        assert!(!content.contains("api-catalog"), "{content}");
        let localized = homepage_link_header(&content_site(), "/fr");
        assert!(
            localized.contains("</fr>; rel=\"alternate\"; type=\"text/markdown\""),
            "{localized}"
        );
    }

    #[test]
    fn unknown_paths_render_nothing() {
        for path in [
            "/",
            "/.well-known/agent-skills/nope/SKILL.md",
            "/mcpx/server-card",
        ] {
            assert!(render(&api_site(), &origin(), path).is_none(), "{path}");
        }
    }
}
