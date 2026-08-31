//! The capability manifest that accompanies a sandboxed plugin artifact.
//!
//! The manifest is the *whole* review surface. An operator who reads it knows
//! everything the plugin may do, because the runtime refuses to do anything the
//! manifest does not name:
//!
//! ```toml
//! name = "autumn-plugin-hello"
//! version = "0.1.0"
//! wire_version = 1
//! prefix = "/hello"
//! capabilities = ["http-request"]
//! sha256 = "…64 hex chars, the module's digest…"
//!
//! [[routes]]
//! method = "GET"
//! path = "/hello/greet"
//!
//! [limits]
//! fuel = 200_000_000
//! memory_bytes = 33_554_432
//! ```
//!
//! # Everything here fails closed
//!
//! Parsing is not "read what you recognise and ignore the rest". An unknown
//! key, an unknown capability name, a future `wire_version`, a declared route
//! outside the declared prefix, a zero or oversized limit, a digest that is not
//! 64 lowercase hex characters — each is a hard error, because every one of
//! them is a case where the operator's reading of the manifest and the
//! runtime's would differ. A manifest an older build cannot fully understand is
//! a manifest it must refuse to run.
//!
//! # The routes are enforced, not documented
//!
//! [`SandboxManifest::routes`] is not advisory metadata. The host builds its
//! router from exactly these `(method, path)` pairs, so a request to an
//! undeclared path under the prefix is a 404 the guest never sees. That is what
//! makes "which routes it mounts" a property of the manifest rather than a
//! promise about the artifact.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::route_listing::{RouteClassification, RouteInfo, RouteSource};

/// The sandbox wire-protocol version this build speaks.
///
/// A manifest declaring any other value is refused at load, so a host and an
/// artifact built from different Autumn versions never guess at each other.
pub const WIRE_VERSION: u32 = 1;

/// Longest accepted plugin name, in bytes.
const MAX_NAME_LEN: usize = 64;

/// Upper bound on a manifest's declared fuel budget.
///
/// Fuel is the CPU ceiling: roughly one unit per executed instruction, so this
/// bounds a runaway guest to seconds of a core, not forever. The ceiling exists
/// so a manifest cannot ask for an *unbounded* budget and call it a limit.
pub const MAX_FUEL: u64 = 100_000_000_000;

/// Upper bound on a manifest's declared linear-memory ceiling (1 GiB).
pub const MAX_MEMORY_BYTES: usize = 1024 * 1024 * 1024;

/// Upper bound on a manifest's declared request-body ceiling (64 MiB).
pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound on a manifest's declared response ceiling (64 MiB).
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound on the memory a plugin may hold across all in-flight requests
/// (1 GiB).
///
/// Bounding the factors separately does not bound the product: 1 GiB × 1024 is
/// two valid factors and a terabyte. And linear memory is not the only thing an
/// in-flight request pins — the buffered request body, the pending stdout frame
/// and the decoded response all live in *host* memory, outside the guest's
/// limiter, so a manifest with a tiny `memory_bytes` and 64 MiB body/response
/// ceilings would pass a memory-only product check and still allocate hundreds
/// of gigabytes. See [`ResourceLimits::request_footprint_bytes`].
pub const MAX_FOOTPRINT_BYTES: u128 = 1024 * 1024 * 1024;

/// Upper bound on a manifest's declared request-body deadline (60 s).
pub const MAX_REQUEST_BODY_TIMEOUT_MS: u64 = 60_000;

/// Upper bound on a manifest's declared concurrency ceiling.
///
/// Each in-flight request holds its own instance, and each instance may hold up
/// to [`ResourceLimits::memory_bytes`], so concurrency × memory is the real
/// host exposure. Bounding it keeps that product reviewable.
pub const MAX_CONCURRENCY: usize = 1024;

/// HTTP methods a declared route may use.
///
/// `CONNECT` and `TRACE` are absent deliberately: neither is a thing a plugin
/// serving its own prefix has any business answering.
const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

// ── Capabilities ─────────────────────────────────────────────────────────

/// A capability a sandboxed plugin may be granted.
///
/// The vocabulary is deliberately tiny in this slice: a plugin may handle HTTP
/// requests under its own prefix, and that is all. Filesystem, network,
/// environment and database access are not "not granted by default" — they do
/// not exist as grantable capabilities at all, so there is no manifest a plugin
/// author can write that asks for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SandboxCapability {
    /// Serve HTTP requests routed to the plugin's declared prefix.
    HttpRequest,
}

impl SandboxCapability {
    /// Every capability this build understands, in manifest spelling.
    pub const ALL: &'static [Self] = &[Self::HttpRequest];

    /// The manifest spelling of this capability.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HttpRequest => "http-request",
        }
    }

    /// One line an operator can read on a consent screen.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::HttpRequest => {
                "handle HTTP requests routed to this plugin's own prefix (no other authority)"
            }
        }
    }

    /// Parse a manifest capability name.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::UnknownCapability`] for any name this build
    /// does not understand — an older host must refuse a newer grant, never
    /// silently drop it.
    pub fn parse(raw: &str) -> Result<Self, ManifestError> {
        match raw {
            "http-request" => Ok(Self::HttpRequest),
            other => Err(ManifestError::UnknownCapability(other.to_owned())),
        }
    }
}

impl fmt::Display for SandboxCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SandboxCapability {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ── Declared routes ──────────────────────────────────────────────────────

/// One `(method, path)` pair the plugin mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredRoute {
    /// HTTP method, upper-cased during parsing.
    pub method: String,
    /// Full mounted path, which must be the prefix or live under it.
    pub path: String,
}

// ── Resource limits ──────────────────────────────────────────────────────

/// The per-request resource ceilings the host enforces for this plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResourceLimits {
    /// CPU ceiling for one request, in wasm fuel units (~one per instruction).
    pub fuel: u64,
    /// Linear-memory ceiling for one request's instance, in bytes.
    pub memory_bytes: usize,
    /// Largest request body forwarded into the guest, in bytes. A larger body
    /// is refused with 413 and the guest is never started.
    pub max_request_body_bytes: usize,
    /// Largest response frame accepted from the guest, in bytes.
    pub max_response_bytes: usize,
    /// Largest number of requests this plugin may execute concurrently.
    pub max_concurrency: usize,
    /// How long the host will wait for a request body before giving up, in
    /// milliseconds.
    ///
    /// A request holds its concurrency permit from the moment it is admitted —
    /// that is what makes the footprint below a bound on the whole request and
    /// not just on the part a guest is running. Without a deadline, a client
    /// that dribbles a body could hold a permit indefinitely without ever
    /// starting a guest, and `max_concurrency` such clients would shut the
    /// plugin's prefix with 503s while no sandbox was executing anything.
    pub request_body_timeout_ms: u64,
}

impl Default for ResourceLimits {
    /// Defaults sized for "a plugin renders a page", not for "a plugin does
    /// arbitrary work": generous enough that an honest handler never notices,
    /// small enough that a hostile one is stopped in milliseconds.
    fn default() -> Self {
        Self {
            fuel: 200_000_000,
            memory_bytes: 32 * 1024 * 1024,
            max_request_body_bytes: 1024 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_concurrency: 8,
            request_body_timeout_ms: 5_000,
        }
    }
}

impl ResourceLimits {
    /// The host memory one in-flight request may hold at once.
    ///
    /// Every term is a buffer that exists while a request is being served:
    ///
    /// | Term | What holds it |
    /// | --- | --- |
    /// | `memory_bytes` | the guest instance's linear memory |
    /// | `4 × max_request_body_bytes` | the body is buffered, cloned into the frame, and base64-expanded (≈4/3) into the NDJSON line that becomes the guest's stdin — all live at once |
    /// | `5 × max_response_bytes` | the response side peaks while the answer is *parsed*, not after: the raw NDJSON line is still live (up to `2 ×`), the base64 field may be copied out of it (`~1.34 ×`, when a guest escapes it), and the decoded body is allocated while both are held |
    /// | table storage | bounded per instance by `MAX_TABLE_ELEMENTS`, at 16 bytes a reference |
    ///
    /// The request terms are deliberately counted at their *simultaneous* peak
    /// rather than at what any one of them costs: a ceiling that assumed the
    /// buffers took turns would be a number nobody could rely on.
    ///
    /// Multiplied by `max_concurrency`, this is what the plugin can cost the
    /// host at any instant — the number a reviewer should actually look at, so
    /// it is the number the validator checks.
    #[must_use]
    pub const fn request_footprint_bytes(&self) -> u128 {
        (self.memory_bytes as u128)
            .saturating_add((self.max_request_body_bytes as u128).saturating_mul(4))
            .saturating_add((self.max_response_bytes as u128).saturating_mul(5))
            // The instance's tables, bounded by `MAX_TABLE_ELEMENTS` at a
            // generous 16 bytes a reference. Small, but per-instance storage
            // the footprint would otherwise not know about at all.
            .saturating_add(crate::plugin_sandbox::host::MAX_TABLE_ELEMENTS as u128 * 16)
            // The request's metadata, at the same factor its bytes are walked
            // building the frame. The ceiling that bounds it is the host's
            // rather than this manifest's, but it is per-request storage all
            // the same: leaving it out is what made this product understate a
            // near-maximum-concurrency plugin by hundreds of megabytes.
            .saturating_add(crate::plugin_sandbox::host::MAX_REQUEST_METADATA_BYTES as u128 * 4)
            // The instance's globals, at a generous 16 bytes each. Per-instance
            // storage the footprint would otherwise not know about at all — the
            // same omission the tables term above exists to correct.
            .saturating_add(crate::plugin_sandbox::host::MAX_GLOBALS as u128 * 16)
            .saturating_add(4096)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let checks: [(&str, u128, u128); 6] = [
            ("fuel", u128::from(self.fuel), u128::from(MAX_FUEL)),
            (
                "memory_bytes",
                self.memory_bytes as u128,
                MAX_MEMORY_BYTES as u128,
            ),
            (
                "max_request_body_bytes",
                self.max_request_body_bytes as u128,
                MAX_REQUEST_BODY_BYTES as u128,
            ),
            (
                "max_response_bytes",
                self.max_response_bytes as u128,
                MAX_RESPONSE_BYTES as u128,
            ),
            (
                "max_concurrency",
                self.max_concurrency as u128,
                MAX_CONCURRENCY as u128,
            ),
            (
                "request_body_timeout_ms",
                u128::from(self.request_body_timeout_ms),
                u128::from(MAX_REQUEST_BODY_TIMEOUT_MS),
            ),
        ];
        for (field, value, max) in checks {
            // Zero is refused as well as oversized: a zero ceiling is not "no
            // limit" but "cannot run", and a manifest that says it by accident
            // should say so at load rather than at the first request.
            if value == 0 || value > max {
                return Err(ManifestError::LimitOutOfRange { field, value, max });
            }
        }
        let footprint = self
            .request_footprint_bytes()
            .saturating_mul(self.max_concurrency as u128);
        if footprint > MAX_FOOTPRINT_BYTES {
            return Err(ManifestError::LimitOutOfRange {
                field: "the per-request host footprint × max_concurrency",
                value: footprint,
                max: MAX_FOOTPRINT_BYTES,
            });
        }
        Ok(())
    }
}

// ── Errors ───────────────────────────────────────────────────────────────

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestError {
    /// The TOML did not parse, or carried a key this build does not know.
    Toml(String),
    /// The manifest could not be rendered back to TOML.
    Serialize(String),
    /// A capability name this build does not understand.
    UnknownCapability(String),
    /// The manifest declares a wire version this build does not speak.
    UnsupportedWireVersion {
        /// The version the manifest declared.
        found: u32,
        /// The version this build speaks.
        supported: u32,
    },
    /// The plugin name is empty, over-long, or carries characters that have no
    /// business in a log line or a path.
    InvalidName(String),
    /// The declared version string is empty or over-long.
    InvalidVersion(String),
    /// The route prefix is not a plain, absolute, single-or-multi-segment path.
    InvalidPrefix {
        /// The offending prefix.
        prefix: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// The manifest grants no capability that would let the plugin serve.
    MissingCapability(SandboxCapability),
    /// The manifest declares no routes, so it could never serve anything.
    NoRoutes,
    /// A declared route uses a method the sandbox will not mount.
    InvalidMethod(String),
    /// A declared route's path is malformed.
    InvalidRoutePath {
        /// The offending path.
        path: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// A declared route does not live under the declared prefix.
    RouteOutsidePrefix {
        /// The offending route's method.
        method: String,
        /// The offending route's path.
        path: String,
        /// The prefix it was measured against.
        prefix: String,
    },
    /// Two declared routes are one route as far as the router is concerned.
    ConflictingRoutes {
        /// The route declared first.
        first: String,
        /// The route that collided with it.
        second: String,
    },
    /// The same `(method, path)` pair is declared twice.
    DuplicateRoute {
        /// The duplicated method.
        method: String,
        /// The duplicated path.
        path: String,
    },
    /// The module digest is not 64 lowercase hex characters.
    InvalidDigest(String),
    /// A declared limit is zero or above this build's ceiling.
    LimitOutOfRange {
        /// Which limit.
        field: &'static str,
        /// The declared value.
        value: u128,
        /// This build's ceiling.
        max: u128,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(detail) => write!(f, "malformed sandbox plugin manifest: {detail}"),
            Self::Serialize(detail) => {
                write!(f, "could not render the sandbox plugin manifest: {detail}")
            }
            Self::UnknownCapability(name) => write!(
                f,
                "unknown sandbox capability `{name}`; this build understands: {known}. \
                 A capability this host cannot enforce is refused rather than ignored",
                known = SandboxCapability::ALL
                    .iter()
                    .map(|cap| cap.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::UnsupportedWireVersion { found, supported } => write!(
                f,
                "sandbox plugin manifest declares wire_version {found}, but this build speaks \
                 version {supported}"
            ),
            Self::InvalidName(name) => write!(
                f,
                "invalid sandbox plugin name {name:?}; expected 1..={MAX_NAME_LEN} characters of \
                 `a-z A-Z 0-9 - _ .`"
            ),
            Self::InvalidVersion(version) => {
                write!(f, "invalid sandbox plugin version {version:?}")
            }
            Self::InvalidPrefix { prefix, reason } => {
                write!(f, "invalid sandbox plugin prefix {prefix:?}: {reason}")
            }
            Self::MissingCapability(cap) => write!(
                f,
                "the manifest grants no `{cap}` capability, so the plugin could never serve a \
                 request; add it to `capabilities` or do not install this plugin"
            ),
            Self::NoRoutes => write!(
                f,
                "the manifest declares no `[[routes]]`; a sandboxed plugin serves exactly the \
                 routes it declares, so an empty list can never serve anything"
            ),
            Self::InvalidMethod(method) => write!(
                f,
                "invalid sandbox plugin route method {method:?}; expected one of {allowed}",
                allowed = ALLOWED_METHODS.join(", ")
            ),
            Self::InvalidRoutePath { path, reason } => {
                write!(f, "invalid sandbox plugin route path {path:?}: {reason}")
            }
            Self::RouteOutsidePrefix {
                method,
                path,
                prefix,
            } => write!(
                f,
                "declared route `{method} {path}` is outside the declared prefix `{prefix}`; a \
                 sandboxed plugin may only mount under its own prefix"
            ),
            Self::ConflictingRoutes { first, second } => write!(
                f,
                "declared routes `{first}` and `{second}` are the same route to the router, so \
                 mounting both is impossible; give them distinct paths"
            ),
            Self::DuplicateRoute { method, path } => {
                write!(f, "declared route `{method} {path}` appears twice")
            }
            Self::InvalidDigest(digest) => write!(
                f,
                "invalid module digest {digest:?}; expected 64 lowercase hex characters"
            ),
            Self::LimitOutOfRange { field, value, max } => write!(
                f,
                "sandbox limit `{field}` = {value} is out of range; expected 1..={max}"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

// ── The manifest ─────────────────────────────────────────────────────────

/// A sandboxed plugin's capability manifest.
///
/// Construct one with [`SandboxManifest::parse`]; the constructor validates, so
/// a `SandboxManifest` value in hand is always one this build is willing to
/// enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxManifest {
    /// The plugin's name, used for route attribution (`plugin:<name>`), for
    /// duplicate-registration detection, and in every log line.
    pub name: String,
    /// The plugin's own version string, shown to the operator.
    pub version: String,
    /// The sandbox wire version the artifact speaks. Must equal
    /// [`WIRE_VERSION`].
    pub wire_version: u32,
    /// The single URL prefix under which this plugin mounts.
    pub prefix: String,
    /// The capabilities the plugin requires.
    pub capabilities: Vec<SandboxCapability>,
    /// Lowercase hex SHA-256 of the wasm module the manifest describes.
    pub sha256: String,
    /// The routes the plugin mounts. The host's router is built from exactly
    /// this list.
    #[serde(default)]
    pub routes: Vec<DeclaredRoute>,
    /// Per-request resource ceilings.
    #[serde(default)]
    pub limits: ResourceLimits,
}

impl SandboxManifest {
    /// Parse and validate a manifest from TOML.
    ///
    /// # Errors
    ///
    /// Returns a [`ManifestError`] for anything this build cannot fully
    /// understand or enforce — see the module documentation for why every such
    /// case is an error rather than a warning.
    pub fn parse(toml_src: &str) -> Result<Self, ManifestError> {
        let mut manifest: Self =
            toml::from_str(toml_src).map_err(|err| ManifestError::Toml(err.to_string()))?;
        for route in &mut manifest.routes {
            route.method = route.method.to_ascii_uppercase();
        }
        manifest.validate()?;
        Ok(manifest)
    }

    /// Render the manifest back to TOML.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Serialize`] if the value cannot be serialized.
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        toml::to_string_pretty(self).map_err(|err| ManifestError::Serialize(err.to_string()))
    }

    /// Whether this manifest grants `capability`.
    #[must_use]
    pub fn grants(&self, capability: SandboxCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The route metadata to hand to
    /// [`AppBuilder::declare_plugin_routes`](crate::app::AppBuilder::declare_plugin_routes).
    ///
    /// Routes are classified [`Public`](RouteClassification::Public) because
    /// that is the truth about them: the sandbox grants no session, auth or
    /// database capability, so a sandboxed route is unauthenticated *by
    /// construction* and could not be otherwise. Leaving them unclassified
    /// would fail `autumn routes audit` for a posture that is proven, not
    /// unproven.
    #[must_use]
    pub fn route_infos(&self) -> Vec<RouteInfo> {
        let mut infos = Vec::with_capacity(self.routes.len());
        for route in &self.routes {
            let info = |method: &str| RouteInfo {
                method: method.to_owned(),
                path: route.path.clone(),
                handler: format!("sandbox:{}", self.name),
                source: RouteSource::Plugin(self.name.clone()),
                middleware: vec!["sandboxed".to_owned()],
                classification: RouteClassification::Public,
                ..RouteInfo::default()
            };
            infos.push(info(&route.method));
            // HTTP defines HEAD as GET without the body, and axum's method
            // router dispatches a HEAD with no HEAD route to the GET one. That
            // is correct behaviour, but it means a manifest listing only GET
            // serves a method its own consent screen never named — so the
            // implication is reported rather than left implicit.
            if route.method == "GET" && !self.declares("HEAD", &route.path) {
                infos.push(info("HEAD"));
            }
        }
        infos
    }

    /// Whether the manifest declares this exact `(method, path)` pair.
    fn declares(&self, method: &str, path: &str) -> bool {
        self.routes
            .iter()
            .any(|route| route.method == method && route.path == path)
    }

    /// The operator-facing consent screen: what this plugin may do, what it
    /// may not, and which bytes were reviewed.
    #[must_use]
    pub fn consent_summary(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        // `write!` to a `String` is infallible; the results are dropped rather
        // than unwrapped so this stays panic-free by construction.
        let _ = writeln!(
            out,
            "Sandboxed plugin: {name} {version}",
            name = self.name,
            version = self.version
        );
        let _ = writeln!(out, "  module sha256: {}", self.sha256);
        let _ = writeln!(out, "  mounts prefix: {}", self.prefix);
        out.push_str("  routes it serves (and only these):\n");
        for route in &self.routes {
            let _ = writeln!(out, "    {} {}", route.method, route.path);
            if route.method == "GET" && !self.declares("HEAD", &route.path) {
                let _ = writeln!(
                    out,
                    "    HEAD {} (HTTP serves HEAD wherever it serves GET)",
                    route.path
                );
            }
        }
        out.push_str("  capabilities granted:\n");
        for capability in &self.capabilities {
            let _ = writeln!(
                out,
                "    {name} — {describe}",
                name = capability.as_str(),
                describe = capability.describe()
            );
        }
        out.push_str(
            "  denied, with no way to ask for it in this version:\n    \
             filesystem access, outbound network access, environment variables,\n    \
             database access, and any host authority not listed above\n",
        );
        out.push_str("  resource ceilings per request:\n");
        let _ = writeln!(
            out,
            "    cpu {fuel} fuel units, memory {memory} bytes, request body {body} bytes\n    \
             (read within {body_ms} ms), response {response} bytes, at most {concurrency} \
             concurrent requests",
            fuel = self.limits.fuel,
            memory = self.limits.memory_bytes,
            body = self.limits.max_request_body_bytes,
            response = self.limits.max_response_bytes,
            body_ms = self.limits.request_body_timeout_ms,
            concurrency = self.limits.max_concurrency,
        );
        out
    }

    pub(crate) fn validate(&self) -> Result<(), ManifestError> {
        if self.wire_version != WIRE_VERSION {
            return Err(ManifestError::UnsupportedWireVersion {
                found: self.wire_version,
                supported: WIRE_VERSION,
            });
        }
        validate_name(&self.name)?;
        // `version` is rendered verbatim on the consent screen an operator reads
        // before agreeing to run the artifact. A free-form field there can
        // rewrite the lines above it with terminal escapes — hide a route, hide
        // a capability, forge a verdict — so it gets the same treatment as the
        // name: printable ASCII, no spaces, bounded.
        let version_ok = !self.version.is_empty()
            && self.version.len() <= MAX_NAME_LEN
            && self.version.chars().all(|ch| ch.is_ascii_graphic());
        if !version_ok {
            return Err(ManifestError::InvalidVersion(self.version.clone()));
        }
        validate_prefix(&self.prefix)?;
        if !self.grants(SandboxCapability::HttpRequest) {
            return Err(ManifestError::MissingCapability(
                SandboxCapability::HttpRequest,
            ));
        }
        validate_digest(&self.sha256)?;
        if self.routes.is_empty() {
            return Err(ManifestError::NoRoutes);
        }
        let mut seen: Vec<(&str, &str)> = Vec::with_capacity(self.routes.len());
        // The same engine the mount will use, so "these two are one route" is
        // decided here rather than discovered by a panic at boot. Two routes
        // that differ only by method share one template legitimately, so a path
        // already inserted is skipped instead of self-conflicting.
        let mut shapes: matchit::Router<()> = matchit::Router::new();
        let mut inserted: Vec<&str> = Vec::with_capacity(self.routes.len());
        for route in &self.routes {
            if !ALLOWED_METHODS.contains(&route.method.as_str()) {
                return Err(ManifestError::InvalidMethod(route.method.clone()));
            }
            validate_route_path(&route.path)?;
            if !path_is_under_prefix(&route.path, &self.prefix) {
                return Err(ManifestError::RouteOutsidePrefix {
                    method: route.method.clone(),
                    path: route.path.clone(),
                    prefix: self.prefix.clone(),
                });
            }
            let key = (route.method.as_str(), route.path.as_str());
            if seen.contains(&key) {
                return Err(ManifestError::DuplicateRoute {
                    method: route.method.clone(),
                    path: route.path.clone(),
                });
            }
            seen.push(key);

            if !inserted.contains(&route.path.as_str()) {
                if let Err(matchit::InsertError::Conflict { with }) =
                    shapes.insert(route.path.as_str(), ())
                {
                    return Err(ManifestError::ConflictingRoutes {
                        first: with,
                        second: route.path.clone(),
                    });
                }
                inserted.push(route.path.as_str());
            }
        }
        self.limits.validate()
    }
}

/// Whether `path` is the prefix itself or a path beneath it.
///
/// The `/` check is what stops `/helloworld` from passing as "under `/hello`" —
/// a string-prefix test would mount a plugin over a sibling route's namespace.
fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn validate_name(name: &str) -> Result<(), ManifestError> {
    let legal = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        && name != "."
        && name != "..";
    if legal {
        Ok(())
    } else {
        Err(ManifestError::InvalidName(name.to_owned()))
    }
}

fn validate_digest(digest: &str) -> Result<(), ManifestError> {
    let legal = digest.len() == 64
        && digest
            .chars()
            .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch));
    if legal {
        Ok(())
    } else {
        Err(ManifestError::InvalidDigest(digest.to_owned()))
    }
}

/// A prefix must be a plain, absolute path with at least one real segment and
/// no routing syntax: it is a containment boundary, and a boundary that can
/// match dynamically is not one.
fn validate_prefix(prefix: &str) -> Result<(), ManifestError> {
    let refuse = |reason: &'static str| {
        Err(ManifestError::InvalidPrefix {
            prefix: prefix.to_owned(),
            reason,
        })
    };
    if !prefix.starts_with('/') {
        return refuse("a prefix must start with `/`");
    }
    if prefix == "/" {
        return refuse(
            "a plugin may not mount at the application root; give it a prefix of its own",
        );
    }
    if prefix.ends_with('/') {
        return refuse("a prefix must not end with `/`");
    }
    for segment in prefix.split('/').skip(1) {
        if segment.is_empty() {
            return refuse("a prefix must not contain an empty path segment");
        }
        if segment == "." || segment == ".." {
            return refuse("a prefix must not contain `.` or `..` segments");
        }
        if !segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~'))
        {
            return refuse(
                "a prefix must be a literal path: no wildcards, captures, query or fragment",
            );
        }
    }
    Ok(())
}

/// A declared route path may carry axum captures (`{id}`, `{*rest}`) — they are
/// matched by the host's router, never by the guest — but must otherwise be a
/// well-formed absolute path.
///
/// The capture syntax is checked by **inserting the path into a throwaway
/// `matchit` router**, which is the same engine axum 0.8 routes through, rather
/// than by a hand-written imitation of its rules. That matters more here than
/// anywhere else in this module: `axum::Router::route` *panics* on a template it
/// cannot insert, so a path this function waves through is a manifest that takes
/// the whole application down at boot. A plugin that can do that has defeated
/// the sandbox before it runs a single instruction.
fn validate_route_path(path: &str) -> Result<(), ManifestError> {
    let refuse = |reason: &'static str| {
        Err(ManifestError::InvalidRoutePath {
            path: path.to_owned(),
            reason,
        })
    };
    if !path.starts_with('/') {
        return refuse("a route path must start with `/`");
    }
    if path.len() > 1 && path.ends_with('/') {
        return refuse("a route path must not end with `/`");
    }
    if path.contains('?') || path.contains('#') {
        return refuse("a route path must not carry a query string or fragment");
    }
    for segment in path.split('/').skip(1) {
        if segment.is_empty() {
            return refuse("a route path must not contain an empty path segment");
        }
        if segment == "." || segment == ".." {
            return refuse("a route path must not contain `.` or `..` segments");
        }
        if segment.chars().any(char::is_whitespace) {
            return refuse("a route path must not contain whitespace");
        }
        // Not just whitespace: an ESC in a route path is printed verbatim on the
        // consent screen, where it can rewrite what the operator reads.
        if segment.chars().any(char::is_control) {
            return refuse("a route path must not contain control characters");
        }
        // axum 0.8 spells captures `{name}` / `{*rest}` and *panics* on a
        // segment starting with the 0.7 spelling, before matchit ever sees it.
        // Naming the fix beats reporting matchit's message for a path it never
        // received.
        if segment.starts_with(':') {
            return refuse("axum 0.8 spells a capture `{name}`, not `:name`");
        }
        if segment.starts_with('*') {
            return refuse("axum 0.8 spells a catch-all `{*name}`, not `*name`");
        }
    }
    let mut probe: matchit::Router<()> = matchit::Router::new();
    if let Err(err) = probe.insert(path, ()) {
        return Err(ManifestError::InvalidRoutePath {
            path: path.to_owned(),
            reason: match err {
                matchit::InsertError::InvalidParam => {
                    "a capture must be spelled `{name}` with a non-empty name"
                }
                matchit::InsertError::InvalidCatchAll => {
                    "a catch-all `{*name}` is only allowed as the last segment"
                }
                matchit::InsertError::InvalidParamSegment => {
                    "a path segment may hold one whole capture and nothing else"
                }
                _ => "the router cannot mount this path",
            },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_toml() -> String {
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"

[limits]
fuel = 200000000
memory_bytes = 33554432
max_request_body_bytes = 1048576
max_response_bytes = 4194304
max_concurrency = 8
"#,
            digest = "a".repeat(64)
        )
    }

    #[test]
    fn parses_a_valid_manifest() {
        let manifest = SandboxManifest::parse(&valid_toml()).expect("valid manifest");
        assert_eq!(manifest.name, "autumn-plugin-hello");
        assert_eq!(manifest.prefix, "/hello");
        assert_eq!(manifest.capabilities, vec![SandboxCapability::HttpRequest]);
        assert_eq!(manifest.routes.len(), 1);
        assert_eq!(manifest.limits.fuel, 200_000_000);
        assert!(manifest.grants(SandboxCapability::HttpRequest));
    }

    #[test]
    fn limits_default_when_the_section_is_absent() {
        let src = valid_toml();
        let trimmed = src.split("[limits]").next().expect("prefix").to_owned();
        let manifest = SandboxManifest::parse(&trimmed).expect("valid manifest");
        assert_eq!(manifest.limits, ResourceLimits::default());
    }

    #[test]
    fn an_unknown_capability_is_a_hard_error_naming_it() {
        let src = valid_toml().replace(r#"["http-request"]"#, r#"["http-request", "database"]"#);
        let err = SandboxManifest::parse(&src).expect_err("unknown capability must fail");
        let text = err.to_string();
        assert!(text.contains("database"), "{text}");
        assert!(text.contains("http-request"), "{text}");
    }

    #[test]
    fn an_unknown_manifest_key_is_a_hard_error() {
        let src = format!("{}\nallow_everything = true\n", valid_toml());
        let err = SandboxManifest::parse(&src).expect_err("unknown key must fail");
        assert!(err.to_string().contains("allow_everything"), "{err}");
    }

    #[test]
    fn a_manifest_without_the_http_request_capability_is_refused() {
        let src = valid_toml().replace(r#"["http-request"]"#, "[]");
        let err = SandboxManifest::parse(&src).expect_err("no capability must fail");
        assert!(matches!(err, ManifestError::MissingCapability(_)), "{err}");
    }

    #[test]
    fn a_future_wire_version_is_refused() {
        let src = valid_toml().replace("wire_version = 1", "wire_version = 2");
        let err = SandboxManifest::parse(&src).expect_err("wire version must fail");
        assert!(
            matches!(err, ManifestError::UnsupportedWireVersion { found: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_route_outside_the_declared_prefix_is_refused() {
        let src = valid_toml().replace(r#"path = "/hello/greet""#, r#"path = "/admin/users""#);
        let err = SandboxManifest::parse(&src).expect_err("off-prefix route must fail");
        assert!(
            matches!(err, ManifestError::RouteOutsidePrefix { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_route_that_only_shares_a_prefix_string_is_refused() {
        let src = valid_toml().replace(r#"path = "/hello/greet""#, r#"path = "/helloworld""#);
        let err = SandboxManifest::parse(&src).expect_err("string-prefix route must fail");
        assert!(
            matches!(err, ManifestError::RouteOutsidePrefix { .. }),
            "{err}"
        );
    }

    #[test]
    fn the_prefix_itself_is_a_legal_route_path() {
        let src = valid_toml().replace(r#"path = "/hello/greet""#, r#"path = "/hello""#);
        assert!(SandboxManifest::parse(&src).is_ok());
    }

    #[test]
    fn a_root_prefix_is_refused() {
        let src = valid_toml()
            .replace(r#"prefix = "/hello""#, r#"prefix = "/""#)
            .replace(r#"path = "/hello/greet""#, r#"path = "/greet""#);
        let err = SandboxManifest::parse(&src).expect_err("root prefix must fail");
        assert!(matches!(err, ManifestError::InvalidPrefix { .. }), "{err}");
    }

    #[test]
    fn a_prefix_with_a_wildcard_or_traversal_is_refused() {
        for bad in [
            "/he*llo",
            "/{tenant}",
            "/hello/",
            "/hello//x",
            "/../hello",
            "hello",
        ] {
            let src = valid_toml()
                .replace(r#"prefix = "/hello""#, &format!(r#"prefix = "{bad}""#))
                .replace(r#"path = "/hello/greet""#, &format!(r#"path = "{bad}""#));
            assert!(
                matches!(
                    SandboxManifest::parse(&src),
                    Err(ManifestError::InvalidPrefix { .. })
                ),
                "prefix {bad} must be refused"
            );
        }
    }

    #[test]
    fn a_route_path_the_router_would_refuse_is_refused_here() {
        // Each of these makes `axum::Router::route` panic. A manifest that
        // validates and then takes the app down at boot is the worst failure
        // this lane could have, so the validator has to speak the router's
        // language rather than a plausible imitation of it.
        for bad in [
            "/hello/:id",       // axum 0.7 capture syntax
            "/hello/*rest",     // axum 0.7 wildcard syntax
            "/hello/{id",       // unterminated capture
            "/hello/{}",        // unnamed capture
            "/hello/{*rest}/x", // catch-all that is not last
            "/hello/a{b}c",     // two things in one segment
        ] {
            let src =
                valid_toml().replace(r#"path = "/hello/greet""#, &format!(r#"path = "{bad}""#));
            assert!(
                matches!(
                    SandboxManifest::parse(&src),
                    Err(ManifestError::InvalidRoutePath { .. })
                ),
                "route path {bad} must be refused"
            );
        }
    }

    #[test]
    fn a_capture_route_is_accepted() {
        let src = valid_toml().replace(r#"path = "/hello/greet""#, r#"path = "/hello/{name}""#);
        assert!(SandboxManifest::parse(&src).is_ok());
        let src = valid_toml().replace(r#"path = "/hello/greet""#, r#"path = "/hello/{*rest}""#);
        assert!(SandboxManifest::parse(&src).is_ok());
    }

    #[test]
    fn two_routes_that_collide_in_the_router_are_refused() {
        // Distinct strings, one route as far as the router is concerned.
        for (first, second) in [("/hello/{a}", "/hello/{b}"), ("/hello/{*a}", "/hello/{b}")] {
            let src = format!(
                "{base}\n[[routes]]\nmethod = \"POST\"\npath = \"{second}\"\n",
                base = valid_toml()
                    .replace(r#"path = "/hello/greet""#, &format!(r#"path = "{first}""#)),
            );
            assert!(
                matches!(
                    SandboxManifest::parse(&src),
                    Err(ManifestError::ConflictingRoutes { .. })
                ),
                "{first} and {second} must be refused as a conflict"
            );
        }
    }

    #[test]
    fn the_same_path_under_two_methods_is_not_a_conflict() {
        let src = format!(
            "{base}\n[[routes]]\nmethod = \"POST\"\npath = \"/hello/{{name}}\"\n",
            base = valid_toml().replace(r#"path = "/hello/greet""#, r#"path = "/hello/{name}""#),
        );
        assert!(SandboxManifest::parse(&src).is_ok());
    }

    #[test]
    fn a_version_that_could_forge_a_consent_screen_is_refused() {
        // `version` is the one free-form field on the screen an operator reads
        // before agreeing to run the artifact.
        for bad in [
            "",
            "0.1.0 \u{1b}[2K",
            "0.1.0\u{7f}",
            "has space",
            &"9".repeat(200),
        ] {
            let src =
                valid_toml().replace(r#"version = "0.1.0""#, &format!(r#"version = "{bad}""#));
            assert!(
                SandboxManifest::parse(&src).is_err(),
                "version {bad:?} must be refused"
            );
        }
        assert!(SandboxManifest::parse(&valid_toml()).is_ok());
    }

    #[test]
    fn a_route_path_carrying_a_control_character_is_refused() {
        // A TOML `\u001B` escape decodes to a real ESC byte in the path — one
        // that would rewrite the consent screen an operator reads it from.
        let src = valid_toml().replace(
            r#"path = "/hello/greet""#,
            r#"path = "/hello/\u001B[2Kgreet""#,
        );
        assert!(
            matches!(
                SandboxManifest::parse(&src),
                Err(ManifestError::InvalidRoutePath { .. })
            ),
            "an escape sequence in a route path must be refused"
        );
    }

    #[test]
    fn a_manifest_with_no_routes_is_refused() {
        let src = valid_toml();
        let trimmed = src.split("[[routes]]").next().expect("prefix").to_owned();
        let err = SandboxManifest::parse(&trimmed).expect_err("no routes must fail");
        assert!(matches!(err, ManifestError::NoRoutes), "{err}");
    }

    #[test]
    fn an_unknown_http_method_is_refused() {
        let src = valid_toml().replace(r#"method = "GET""#, r#"method = "CONNECT""#);
        let err = SandboxManifest::parse(&src).expect_err("bad method must fail");
        assert!(matches!(err, ManifestError::InvalidMethod(_)), "{err}");
    }

    #[test]
    fn methods_are_normalised_to_upper_case() {
        let src = valid_toml().replace(r#"method = "GET""#, r#"method = "get""#);
        let manifest = SandboxManifest::parse(&src).expect("lowercase method is accepted");
        assert_eq!(manifest.routes[0].method, "GET");
    }

    #[test]
    fn duplicate_declared_routes_are_refused() {
        let src = format!(
            "{}\n[[routes]]\nmethod = \"GET\"\npath = \"/hello/greet\"\n",
            valid_toml()
        );
        let err = SandboxManifest::parse(&src).expect_err("duplicate route must fail");
        assert!(matches!(err, ManifestError::DuplicateRoute { .. }), "{err}");
    }

    #[test]
    fn a_malformed_digest_is_refused() {
        for bad in ["", "abc", &"A".repeat(64), &"z".repeat(64)] {
            let src = valid_toml().replace(&"a".repeat(64), bad);
            assert!(
                matches!(
                    SandboxManifest::parse(&src),
                    Err(ManifestError::InvalidDigest(_))
                ),
                "digest {bad} must be refused"
            );
        }
    }

    #[test]
    fn a_zero_or_oversized_limit_is_refused() {
        for (field, value) in [
            ("fuel", "0"),
            ("memory_bytes", "0"),
            ("max_concurrency", "0"),
            ("max_response_bytes", "0"),
            ("fuel", "999999999999999"),
            ("memory_bytes", "9999999999"),
        ] {
            let src = valid_toml()
                .lines()
                .map(|line| {
                    if line.starts_with(&format!("{field} = ")) {
                        format!("{field} = {value}")
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                matches!(
                    SandboxManifest::parse(&src),
                    Err(ManifestError::LimitOutOfRange { .. })
                ),
                "{field} = {value} must be refused"
            );
        }
    }

    #[test]
    fn the_footprint_counts_the_host_buffers_a_request_holds_too() {
        // Linear memory is not the only thing a concurrent request pins: the
        // buffered request body, the pending stdout frame and the decoded
        // response all live in host memory outside the guest limiter. A
        // manifest with tiny `memory_bytes` and 64 MiB body/response ceilings
        // would otherwise pass the product check and still allocate hundreds of
        // gigabytes.
        let src = valid_toml()
            .replace("memory_bytes = 33554432", "memory_bytes = 65536")
            .replace(
                "max_request_body_bytes = 1048576",
                "max_request_body_bytes = 67108864",
            )
            .replace(
                "max_response_bytes = 4194304",
                "max_response_bytes = 67108864",
            )
            .replace("max_concurrency = 8", "max_concurrency = 1024");
        let err = SandboxManifest::parse(&src).expect_err("must be refused");
        assert!(
            matches!(err, ManifestError::LimitOutOfRange { field, .. } if field.contains("footprint")),
            "{err}"
        );
    }

    #[test]
    fn the_footprint_counts_every_buffer_a_request_holds_at_once() {
        // The request body is buffered, cloned into the frame, and
        // base64-expanded into the NDJSON line that becomes the guest's stdin —
        // three live copies of an expanding thing, not one — and the instance's
        // tables are per-instance host storage too.
        let limits = ResourceLimits {
            memory_bytes: 1_000_000,
            max_request_body_bytes: 100_000,
            max_response_bytes: 10_000,
            ..ResourceLimits::default()
        };
        let tables = u128::from(crate::plugin_sandbox::host::MAX_TABLE_ELEMENTS) * 16;
        let metadata = crate::plugin_sandbox::host::MAX_REQUEST_METADATA_BYTES as u128 * 4;
        let globals = crate::plugin_sandbox::host::MAX_GLOBALS as u128 * 16;
        assert_eq!(
            limits.request_footprint_bytes(),
            1_000_000 + 4 * 100_000 + 5 * 10_000 + tables + metadata + globals + 4096
        );
    }

    #[test]
    fn the_footprint_counts_the_peak_while_a_response_is_being_decoded() {
        // Parsing the guest's answer is where the response side actually
        // peaks: the raw NDJSON line is still live (up to 2x the ceiling), the
        // base64 field may be copied out of it, and the decoded body is
        // allocated while both are held. A term that counted only "the line
        // plus the decoded response" described a moment that never happens.
        let limits = ResourceLimits {
            memory_bytes: 0,
            max_request_body_bytes: 0,
            max_response_bytes: 1_000_000,
            ..ResourceLimits::default()
        };
        let tables = u128::from(crate::plugin_sandbox::host::MAX_TABLE_ELEMENTS) * 16;
        let metadata = crate::plugin_sandbox::host::MAX_REQUEST_METADATA_BYTES as u128 * 4;
        let globals = crate::plugin_sandbox::host::MAX_GLOBALS as u128 * 16;
        assert_eq!(
            limits.request_footprint_bytes(),
            5 * 1_000_000 + tables + metadata + globals + 4096,
            "the response term must cover the line, the base64 copy and the decode at once"
        );
    }

    #[test]
    fn the_footprint_counts_the_metadata_a_request_may_carry() {
        // The ceiling that bounds request metadata is the host's rather than
        // this manifest's, but it is per-request storage all the same, cloned
        // into the frame and serialised around. Left out, this product
        // understated a near-maximum-concurrency plugin by hundreds of
        // megabytes — and this product is exactly what the validator checks and
        // what a reviewer reads.
        let bare = ResourceLimits {
            memory_bytes: 0,
            max_request_body_bytes: 0,
            max_response_bytes: 0,
            ..ResourceLimits::default()
        };
        let metadata = crate::plugin_sandbox::host::MAX_REQUEST_METADATA_BYTES as u128 * 4;
        assert!(
            bare.request_footprint_bytes() >= metadata,
            "the metadata a request may carry is not in the footprint"
        );
    }

    #[test]
    fn the_default_limits_are_within_the_footprint_ceiling() {
        let manifest = SandboxManifest::parse(&valid_toml()).expect("valid");
        assert_eq!(manifest.limits, ResourceLimits::default());
    }

    #[test]
    fn a_name_that_could_forge_a_log_line_is_refused() {
        for bad in ["", "a b", "../etc", "plugin:name", &"x".repeat(200)] {
            let src = valid_toml().replace("autumn-plugin-hello", bad);
            assert!(
                matches!(
                    SandboxManifest::parse(&src),
                    Err(ManifestError::InvalidName(_))
                ),
                "name {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_name_carrying_a_newline_is_refused() {
        // TOML itself refuses a raw newline inside a basic string, so this one
        // never reaches the name check — but it must still be a refusal, and
        // the test exists so a future manifest format that *does* accept it
        // cannot silently let a log-forging name through.
        let src = valid_toml().replace("autumn-plugin-hello", "plugin\nname");
        assert!(SandboxManifest::parse(&src).is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let manifest = SandboxManifest::parse(&valid_toml()).expect("valid");
        let rendered = manifest.to_toml().expect("serializes");
        let reparsed = SandboxManifest::parse(&rendered).expect("re-parses");
        assert_eq!(manifest, reparsed);
    }

    #[test]
    fn the_consent_summary_names_the_grant_the_prefix_and_the_digest() {
        let manifest = SandboxManifest::parse(&valid_toml()).expect("valid");
        let summary = manifest.consent_summary();
        assert!(summary.contains("autumn-plugin-hello"), "{summary}");
        assert!(summary.contains("/hello"), "{summary}");
        assert!(summary.contains("http-request"), "{summary}");
        assert!(summary.contains(&"a".repeat(64)), "{summary}");
        assert!(summary.contains("GET /hello/greet"), "{summary}");
        // Everything the sandbox denies is named too, so the reader sees the
        // shape of the "no" and not just the "yes".
        assert!(summary.contains("filesystem"), "{summary}");
        assert!(summary.contains("network"), "{summary}");
        assert!(summary.contains("environment"), "{summary}");
        assert!(summary.contains("database"), "{summary}");
    }

    #[test]
    fn a_declared_get_reports_the_head_it_also_serves() {
        // HTTP says HEAD is GET without the body, and axum's method router
        // dispatches a HEAD with no HEAD route to the GET one. A manifest that
        // listed only GET would therefore serve a method its own consent screen
        // never named.
        let manifest = SandboxManifest::parse(&valid_toml()).expect("valid");
        let infos = manifest.route_infos();
        assert_eq!(infos.len(), 2, "{infos:?}");
        assert!(infos.iter().any(|route| route.method == "HEAD"));
        assert!(manifest.consent_summary().contains("HEAD /hello/greet"));
    }

    #[test]
    fn route_infos_carry_plugin_attribution_under_the_prefix() {
        let manifest = SandboxManifest::parse(&valid_toml()).expect("valid");
        let infos = manifest.route_infos();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].method, "GET");
        assert_eq!(infos[0].path, "/hello/greet");
        assert_eq!(
            infos[0].source,
            crate::route_listing::RouteSource::Plugin("autumn-plugin-hello".to_owned())
        );
    }
}
