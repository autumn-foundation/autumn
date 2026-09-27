//! The curated plugin index (issue #1625).
//!
//! The index is a TOML file in this repository (`autumn-cli/plugin-index/`).
//! A maintainer reviews each listing in a pull request. The CLI embeds the
//! file, so `autumn plugin list` works offline and shows only listings that
//! were reviewed for this release.
//!
//! Each listing records the trust facts a consumer needs before install: the
//! supported `autumn-web` range, the last `autumn plugin-check` result, the
//! trust class, and the #1601 stability tier.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::install::Compat;

/// The index schema this CLI reads.
pub const SCHEMA: u32 = 1;

/// The index that ships with this CLI.
pub const BUNDLED: &str = include_str!("../../plugin-index/index.toml");

/// Env var that points the CLI at a different index file.
pub const OVERRIDE_ENV: &str = "AUTUMN_PLUGIN_INDEX";

/// The label for a native plugin.
pub const FULL_TRUST_LABEL: &str = "full trust: native code";

/// The whole index file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginIndex {
    /// Schema version. Must be [`SCHEMA`].
    pub schema: u32,
    /// The listings, in `plugin list` order.
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<Listing>,
}

/// Where a listed plugin comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListingOrigin {
    /// A crate in this workspace, released in lockstep with `autumn-web`.
    FirstParty,
    /// A crate a third party publishes on crates.io.
    Community,
}

/// The #1601 plugin API tier a listing builds on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// Only stable plugin surface.
    Stable,
    /// At least one experimental surface. It can break in any release.
    Experimental,
}

/// The trust class of a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// A `Plugin`-trait crate. It gets the whole `AppBuilder`.
    Native,
    /// A #1609 sandboxed plugin. The capability manifest limits it.
    Sandboxed,
}

/// The state of a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Verified against the current release.
    Listed,
    /// Failed re-verification. Shown with a flag; `add` refuses on the
    /// failed release.
    Incompatible,
    /// Failed re-verification on two different releases. Not shown.
    Delisted,
}

/// The result `autumn plugin-check` gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckOutcome {
    /// All checks passed.
    Pass,
    /// One or more checks failed.
    Fail,
    /// `plugin-check` does not apply (the crate is not a `Plugin`). First-party
    /// only; the install gate verifies it instead.
    Exempt,
}

/// The last conformance run for a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conformance {
    /// The result.
    pub result: CheckOutcome,
    /// The `autumn-web` release the check ran against.
    pub autumn_web: String,
    /// The date of the run, `YYYY-MM-DD`.
    pub checked: String,
    /// Why the result is [`CheckOutcome::Exempt`]. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// The manifest's `[grants]` lists, as `autumn plugin inspect` prints them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grants {
    /// Hostnames `http-outbound` may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Logical tables `db` owns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<String>,
    /// Job types `jobs` may enqueue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub job_types: Vec<String>,
    /// Render slots `render` may fill.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<String>,
}

impl Grants {
    /// Whether no list holds anything.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.tables.is_empty()
            && self.job_types.is_empty()
            && self.slots.is_empty()
    }
}

/// One plugin in the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    /// crates.io name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// First-party or community.
    pub origin: ListingOrigin,
    /// Source repository, `https://`.
    pub repository: String,
    /// The plugin version that was verified.
    pub version: String,
    /// The supported `autumn-web` range (a Cargo version requirement).
    pub autumn_web: String,
    /// The #1601 tier.
    pub tier: Tier,
    /// The experimental surfaces the plugin declares. Empty for
    /// [`Tier::Stable`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub experimental_surfaces: Vec<String>,
    /// The trust class.
    pub trust: Trust,
    /// The manifest capabilities of a sandboxed plugin. Empty for native.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// The reviewed `.autumn-plugin` artifact digest, as
    /// `autumn plugin inspect` prints it. Sandboxed only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_sha256: String,
    /// What each sandboxed capability is scoped to. Sandboxed only.
    #[serde(default, skip_serializing_if = "Grants::is_empty")]
    pub grants: Grants,
    /// The per-request quotas the manifest declares, as `inspect` prints
    /// them. Sandboxed only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quotas: BTreeMap<String, u32>,
    /// The per-request resource limits the manifest declares (fuel, memory,
    /// body sizes, timeouts, concurrency). Sandboxed only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, u64>,
    /// The listing state.
    pub status: Status,
    /// Why the listing is not [`Status::Listed`]. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The route prefix `plugin-check` verifies. Empty for a plugin with no
    /// routes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    /// `PATH:DESCRIPTION` pairs for `plugin-check --sensitive-route`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive_routes: Vec<String>,
    /// The last conformance run.
    pub conformance: Conformance,
}

/// Why an index could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// The file could not be read.
    Io(String),
    /// The TOML did not parse into the schema.
    Toml(String),
    /// The schema version is not [`SCHEMA`].
    Schema(u32),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "cannot read the plugin index: {msg}"),
            Self::Toml(msg) => write!(f, "the plugin index is not valid: {msg}"),
            Self::Schema(found) => write!(
                f,
                "the plugin index uses schema {found}; this CLI reads schema {SCHEMA}"
            ),
        }
    }
}

impl std::error::Error for IndexError {}

/// One problem `check` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// The listing name, or `index` for a file-level problem.
    pub plugin: String,
    /// What is wrong, and what to do.
    pub message: String,
}

/// Parse an index file.
///
/// # Errors
///
/// [`IndexError::Toml`] when the text does not match the schema, and
/// [`IndexError::Schema`] when the schema version is not [`SCHEMA`].
pub fn parse(src: &str) -> Result<PluginIndex, IndexError> {
    // Read the schema first, so a newer file gives a clear message rather
    // than a field error.
    #[derive(Deserialize)]
    struct Header {
        schema: u32,
    }
    let header: Header = toml::from_str(src).map_err(|e| IndexError::Toml(e.to_string()))?;
    if header.schema != SCHEMA {
        return Err(IndexError::Schema(header.schema));
    }
    toml::from_str(src).map_err(|e| IndexError::Toml(e.to_string()))
}

/// Largest index file the CLI reads.
pub const MAX_INDEX_BYTES: u64 = 1024 * 1024;

/// Escape characters that can drive or disguise terminal output.
#[must_use]
pub fn sanitize(text: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_unsafe_char(c) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Control, bidi, zero-width and line-separator characters.
#[must_use]
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// The crates.io identity of a name: case and `-`/`_` do not count.
#[must_use]
pub fn canonical(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

/// Where a loaded index came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The copy embedded in this CLI.
    Bundled,
    /// A file named by [`OVERRIDE_ENV`].
    Override(PathBuf),
}

/// A parsed index and its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The index.
    pub index: PluginIndex,
    /// Where it came from.
    pub source: Source,
}

/// Load the index from `path`, or the bundled copy when `path` is `None`.
///
/// # Errors
///
/// Any [`IndexError`]. An override that fails is an error, never a silent
/// fall back to the bundled copy.
pub fn load(path: Option<&Path>) -> Result<Loaded, IndexError> {
    let Some(path) = path else {
        return Ok(Loaded {
            index: parse(BUNDLED)?,
            source: Source::Bundled,
        });
    };
    let src = read_bounded(path)?;
    Ok(Loaded {
        index: parse(&src)?,
        source: Source::Override(path.to_path_buf()),
    })
}

/// Read a regular file of at most [`MAX_INDEX_BYTES`].
fn read_bounded(path: &Path) -> Result<String, IndexError> {
    use std::io::Read as _;

    let io = |e: &dyn std::fmt::Display| IndexError::Io(format!("{}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(|e| io(&e))?;
    let meta = file.metadata().map_err(|e| io(&e))?;
    if !meta.is_file() {
        return Err(io(&"not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io(&e))?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err(io(&format!("larger than {MAX_INDEX_BYTES} bytes")));
    }
    String::from_utf8(bytes).map_err(|_| io(&"not UTF-8"))
}

/// Load the index named by [`OVERRIDE_ENV`], or the bundled copy.
///
/// # Errors
///
/// Any [`IndexError`] from [`load`].
pub fn load_from_env() -> Result<Loaded, IndexError> {
    let path = std::env::var_os(OVERRIDE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    load(path.as_deref())
}

impl PluginIndex {
    /// The listing for `name`, in any state.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Listing> {
        let name = canonical(name);
        self.plugins
            .iter()
            .find(|listing| canonical(&listing.name) == name)
    }

    /// The listings `plugin list` shows: all but [`Status::Delisted`].
    pub fn visible(&self) -> impl Iterator<Item = &Listing> {
        self.plugins
            .iter()
            .filter(|listing| listing.status != Status::Delisted)
    }
}

impl Listing {
    /// The trust line shown at discovery and before install.
    #[must_use]
    pub fn trust_label(&self) -> String {
        match self.trust {
            Trust::Native => FULL_TRUST_LABEL.to_owned(),
            Trust::Sandboxed => {
                let mut label = format!(
                    "sandboxed: capability manifest grants {}",
                    self.capabilities.join(", ")
                );
                let scopes: Vec<String> = [
                    ("hosts", &self.grants.hosts),
                    ("tables", &self.grants.tables),
                    ("job types", &self.grants.job_types),
                    ("render slots", &self.grants.slots),
                ]
                .into_iter()
                .filter(|(_, list)| !list.is_empty())
                .map(|(what, list)| format!("{what} {}", list.join(", ")))
                .collect();
                if !scopes.is_empty() {
                    label.push_str("; scoped to ");
                    label.push_str(&scopes.join("; "));
                }
                // Defaults go unsaid; a changed ceiling is authority.
                let defaults = autumn_web::plugin_sandbox::CapabilityQuotas::default();
                let changed: Vec<String> = defaults
                    .fields()
                    .into_iter()
                    .filter_map(|(key, default)| {
                        self.quotas
                            .get(key)
                            .filter(|v| **v != default)
                            .map(|v| format!("{key}={v}"))
                    })
                    .collect();
                if !changed.is_empty() {
                    label.push_str("; quotas ");
                    label.push_str(&changed.join(", "));
                }
                let defaults = autumn_web::plugin_sandbox::ResourceLimits::default();
                let changed: Vec<String> = defaults
                    .fields()
                    .into_iter()
                    .filter_map(|(key, default)| {
                        self.limits
                            .get(key)
                            .filter(|v| u128::from(**v) != default)
                            .map(|v| format!("{key}={v}"))
                    })
                    .collect();
                if !changed.is_empty() {
                    label.push_str("; limits ");
                    label.push_str(&changed.join(", "));
                }
                label
            }
        }
    }

    /// Whether this listing works with an app on `app` (the app's
    /// `autumn-web` requirement, e.g. `0.7.0`).
    #[must_use]
    pub fn compat(&self, app: &str) -> Compat {
        let Some(app) = concrete(app) else {
            return Compat::Unknown;
        };
        // A flag wins over the declared range: the range is a claim, the
        // failed run is evidence.
        if self.flag_applies(&app.to_string()) {
            return Compat::Incompatible;
        }
        match semver::VersionReq::parse(&self.autumn_web) {
            Ok(req) if req.matches(&app) => Compat::Compatible,
            Ok(_) => Compat::Incompatible,
            Err(_) => Compat::Unknown,
        }
    }

    /// Whether a failed re-verification applies to an app on `app`: the
    /// listing is flagged, and `app` is on the failed series or later. A
    /// flagged sandboxed listing applies to every app.
    #[must_use]
    pub fn flag_applies(&self, app: &str) -> bool {
        // A sandboxed artifact is not tied to an `autumn-web` series.
        if self.status == Status::Incompatible && self.trust == Trust::Sandboxed {
            return true;
        }
        self.status == Status::Incompatible
            && concrete(app)
                .zip(concrete(&self.conformance.autumn_web))
                .is_some_and(|(app, failed)| {
                    app >= semver::Version::new(failed.major, failed.minor, 0)
                })
    }

    /// Whether the last conformance run covers the `autumn-web` series of
    /// `app`.
    #[must_use]
    pub fn verified_for(&self, app: &str) -> bool {
        match (concrete(app), concrete(&self.conformance.autumn_web)) {
            (Some(app), Some(checked)) => same_series(&app, &checked),
            _ => false,
        }
    }
}

/// Check each listing against the admission rules.
#[must_use]
pub fn validate(index: &PluginIndex) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for listing in &index.plugins {
        let mut add = |message: String| {
            findings.push(Finding {
                plugin: listing.name.clone(),
                message,
            });
        };
        if !seen.insert(canonical(&listing.name)) {
            add("listed more than once".to_owned());
        }
        for message in admission_problems(listing) {
            add(message);
        }
    }
    findings
}

/// Every admission rule one listing breaks.
fn admission_problems(listing: &Listing) -> Vec<String> {
    let mut out = Vec::new();
    check_text(listing, &mut out);
    check_identity(listing, &mut out);
    check_versions(listing, &mut out);
    check_conformance(listing, &mut out);
    check_tier(listing, &mut out);
    check_trust(listing, &mut out);
    out
}

/// Text fields are printed to a terminal, so control characters are refused.
fn check_text(listing: &Listing, out: &mut Vec<String>) {
    let fields = [
        ("name", listing.name.as_str()),
        ("description", listing.description.as_str()),
        ("repository", listing.repository.as_str()),
        ("note", listing.note.as_str()),
        ("prefix", listing.prefix.as_str()),
        ("conformance.reason", listing.conformance.reason.as_str()),
    ];
    let lists = listing
        .sensitive_routes
        .iter()
        .map(|route| ("sensitive_routes", route.as_str()))
        .chain(
            listing
                .experimental_surfaces
                .iter()
                .map(|s| ("experimental_surfaces", s.as_str())),
        )
        .chain(
            listing
                .capabilities
                .iter()
                .map(|c| ("capabilities", c.as_str())),
        )
        .chain(
            [
                &listing.grants.hosts,
                &listing.grants.tables,
                &listing.grants.job_types,
                &listing.grants.slots,
            ]
            .into_iter()
            .flatten()
            .map(|g| ("grants", g.as_str())),
        );
    for (field, value) in fields.into_iter().chain(lists) {
        if value.chars().any(is_unsafe_char) {
            out.push(format!("`{field}` holds a control character"));
        }
    }
    if listing.description.trim().is_empty() {
        out.push("`description` is empty".to_owned());
    }
    if !listing.repository.starts_with("https://") {
        out.push("`repository` must be an https:// URL".to_owned());
    }
}

fn check_identity(listing: &Listing, out: &mut Vec<String>) {
    match listing.origin {
        ListingOrigin::FirstParty if super::catalog::lookup(&listing.name).is_none() => {
            out.push("origin is first-party, but the CLI catalog has no such crate".to_owned());
        }
        ListingOrigin::Community if !super::catalog::is_community_name(&listing.name) => {
            out.push(format!(
                "a community name must be `{}<name>`",
                super::catalog::COMMUNITY_PREFIX
            ));
        }
        ListingOrigin::FirstParty | ListingOrigin::Community => {}
    }
}

fn check_versions(listing: &Listing, out: &mut Vec<String>) {
    if semver::Version::parse(&listing.version).is_err() {
        out.push(format!(
            "`version` {:?} is not a semver version",
            listing.version
        ));
    }
    match semver::VersionReq::parse(&listing.autumn_web) {
        Err(_) => out.push(format!(
            "`autumn_web` {:?} is not a Cargo version requirement; declare the supported range",
            listing.autumn_web
        )),
        // A range with no upper bound claims releases nobody has checked.
        Ok(req) if req.matches(&semver::Version::new(u64::from(u32::MAX), 0, 0)) => {
            out.push(format!(
                "`autumn_web` {:?} has no upper bound; name the series you verified",
                listing.autumn_web
            ));
        }
        Ok(_) => {}
    }
    if semver::Version::parse(&listing.conformance.autumn_web).is_err() {
        out.push(format!(
            "`conformance.autumn_web` {:?} is not a semver version",
            listing.conformance.autumn_web
        ));
    }
    if chrono::NaiveDate::parse_from_str(&listing.conformance.checked, "%Y-%m-%d").is_err() {
        out.push(format!(
            "`conformance.checked` {:?} is not a YYYY-MM-DD date",
            listing.conformance.checked
        ));
    }
}

fn check_conformance(listing: &Listing, out: &mut Vec<String>) {
    let result = listing.conformance.result;
    match (listing.status, result) {
        (Status::Listed, CheckOutcome::Fail) => {
            out.push("status is listed, but the last `autumn plugin-check` run failed".to_owned());
        }
        (Status::Incompatible, CheckOutcome::Pass | CheckOutcome::Exempt) => {
            out.push(
                "status is incompatible, but the last conformance run did not fail".to_owned(),
            );
        }
        // Only `record` delists, after a second failure. A delisted entry is
        // hidden and never re-verified, so a passing one would vanish unseen.
        (Status::Delisted, CheckOutcome::Pass | CheckOutcome::Exempt) => {
            out.push("status is delisted, but the last conformance run did not fail".to_owned());
        }
        _ => {}
    }
    if listing.status != Status::Listed && listing.note.trim().is_empty() {
        out.push("a flagged or delisted listing needs a `note` that says why".to_owned());
    }
    if result == CheckOutcome::Exempt {
        if listing.origin != ListingOrigin::FirstParty {
            out.push("only a first-party listing can be exempt from plugin-check".to_owned());
        }
        if listing.conformance.reason.trim().is_empty() {
            out.push("an exempt result needs a `conformance.reason`".to_owned());
        }
    }
}

fn check_tier(listing: &Listing, out: &mut Vec<String>) {
    use autumn_web::plugin_contract::{SurfaceTier, surface};

    match (listing.tier, listing.experimental_surfaces.is_empty()) {
        (Tier::Experimental, true) => {
            out.push("tier is experimental, but `experimental_surfaces` is empty".to_owned());
        }
        (Tier::Stable, false) => out.push(
            "`experimental_surfaces` is not empty, so the tier must be experimental".to_owned(),
        ),
        _ => {}
    }
    for name in &listing.experimental_surfaces {
        match surface(name) {
            Some(found) if found.tier == SurfaceTier::Experimental => {}
            Some(_) => out.push(format!("`{name}` is a stable surface, not experimental")),
            None => out.push(format!("`{name}` is not a known plugin surface")),
        }
    }
}

fn check_trust(listing: &Listing, out: &mut Vec<String>) {
    use autumn_web::plugin_sandbox::SandboxCapability;

    match listing.trust {
        Trust::Native if !listing.capabilities.is_empty() => out.push(
            "a native plugin has full trust; remove `capabilities` or set trust to sandboxed"
                .to_owned(),
        ),
        Trust::Sandboxed if listing.capabilities.is_empty() => {
            out.push("a sandboxed listing must copy `capabilities` from its manifest".to_owned());
        }
        Trust::Native | Trust::Sandboxed => {}
    }
    let digest = &listing.artifact_sha256;
    match listing.trust {
        Trust::Sandboxed
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            out.push(
                "a sandboxed listing must record `artifact_sha256` from `autumn plugin inspect`"
                    .to_owned(),
            );
        }
        Trust::Native if !digest.is_empty() => {
            out.push("`artifact_sha256` is for a sandboxed listing only".to_owned());
        }
        Trust::Native if !listing.grants.is_empty() => {
            out.push("`grants` is for a sandboxed listing only".to_owned());
        }
        Trust::Native if !listing.quotas.is_empty() => {
            out.push("`quotas` is for a sandboxed listing only".to_owned());
        }
        Trust::Native if !listing.limits.is_empty() => {
            out.push("`limits` is for a sandboxed listing only".to_owned());
        }
        Trust::Native | Trust::Sandboxed => {}
    }
    let known = autumn_web::plugin_sandbox::CapabilityQuotas::default().fields();
    for key in listing.quotas.keys() {
        if !known.iter().any(|(k, _)| k == key) {
            out.push(format!("`{key}` is not a sandbox quota"));
        }
    }
    let sandboxed = listing.trust == Trust::Sandboxed;
    // A sandboxed listing records the whole authority `inspect` reported:
    // a missing ceiling would publish as unknown, not as approved.
    let missing: Vec<&str> = known
        .iter()
        .filter(|(k, _)| sandboxed && !listing.quotas.contains_key(*k))
        .map(|(k, _)| *k)
        .collect();
    if !missing.is_empty() {
        out.push(format!("`quotas` is missing {}", missing.join(", ")));
    }
    let known = autumn_web::plugin_sandbox::ResourceLimits::default().fields();
    for key in listing.limits.keys() {
        if !known.iter().any(|(k, _)| k == key) {
            out.push(format!("`{key}` is not a sandbox resource limit"));
        }
    }
    let missing: Vec<&str> = known
        .iter()
        .filter(|(k, _)| sandboxed && !listing.limits.contains_key(*k))
        .map(|(k, _)| *k)
        .collect();
    if !missing.is_empty() {
        out.push(format!("`limits` is missing {}", missing.join(", ")));
    }
    for name in &listing.capabilities {
        if !SandboxCapability::ALL.iter().any(|c| c.as_str() == name) {
            out.push(format!("`{name}` is not a sandbox capability"));
        }
    }
}

/// Check that each live listing was verified against `against`, the current
/// `autumn-web` release.
#[must_use]
pub fn staleness(index: &PluginIndex, against: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for listing in &index.plugins {
        let mut add = |message: String| {
            findings.push(Finding {
                plugin: listing.name.clone(),
                message,
            });
        };
        let checked = &listing.conformance.autumn_web;
        match listing.status {
            Status::Delisted => continue,
            Status::Incompatible => {
                if checked != against {
                    add(format!(
                        "flagged since autumn-web {checked}; re-verify against {against} to \
                         relist it, or delist it"
                    ));
                }
                continue;
            }
            Status::Listed => {}
        }
        if checked != against {
            add(format!(
                "last verified against autumn-web {checked}; re-verify against {against}"
            ));
        }
        // An RC of a series is that series, as `plugin_contract::evaluate`
        // treats it: match with the prerelease stripped.
        let excluded = semver::VersionReq::parse(&listing.autumn_web)
            .ok()
            .zip(semver::Version::parse(against).ok())
            .is_some_and(|(req, version)| {
                !req.matches(&semver::Version::new(
                    version.major,
                    version.minor,
                    version.patch,
                ))
            });
        if excluded {
            add(format!(
                "its range `{}` excludes autumn-web {against}; flag it incompatible or widen \
                 the range",
                listing.autumn_web
            ));
        }
        if listing.origin == ListingOrigin::FirstParty && listing.version != against {
            add(format!(
                "first-party version {} is not the release {against}",
                listing.version
            ));
        }
    }
    findings
}

/// Read a version or a simple requirement (`0.7.0`, `^0.7`, `=0.7.2`) as a
/// concrete version. `None` for a range such as `>=0.6`.
fn concrete(version: &str) -> Option<semver::Version> {
    super::install::parse_version(version)
        .map(|(major, minor, patch)| semver::Version::new(major, minor, patch))
}

/// Whether two versions share a compatibility series: `MAJOR.MINOR` below
/// 1.0, `MAJOR` from 1.0 on.
const fn same_series(a: &semver::Version, b: &semver::Version) -> bool {
    if a.major == 0 || b.major == 0 {
        a.major == b.major && a.minor == b.minor
    } else {
        a.major == b.major
    }
}

/// [`validate`] and [`staleness`] together: the gate CI runs.
#[must_use]
pub fn check(index: &PluginIndex, against: &str) -> Vec<Finding> {
    let mut findings = validate(index);
    findings.extend(staleness(index, against));
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = env!("CARGO_PKG_VERSION");

    fn native(name: &str, origin: ListingOrigin) -> Listing {
        Listing {
            name: name.to_owned(),
            description: "A plugin".to_owned(),
            origin,
            repository: "https://example.com/repo".to_owned(),
            version: "0.7.0".to_owned(),
            autumn_web: "0.7".to_owned(),
            tier: Tier::Stable,
            experimental_surfaces: vec![],
            trust: Trust::Native,
            capabilities: vec![],
            artifact_sha256: String::new(),
            grants: Grants::default(),
            quotas: BTreeMap::new(),
            limits: BTreeMap::new(),
            status: Status::Listed,
            note: String::new(),
            prefix: String::new(),
            sensitive_routes: vec![],
            conformance: Conformance {
                result: CheckOutcome::Pass,
                autumn_web: "0.7.0".to_owned(),
                checked: "2026-09-27".to_owned(),
                reason: String::new(),
            },
        }
    }

    fn community() -> Listing {
        native("autumn-plugin-audit", ListingOrigin::Community)
    }

    fn index_of(plugins: Vec<Listing>) -> PluginIndex {
        PluginIndex {
            schema: SCHEMA,
            plugins,
        }
    }

    fn messages(findings: &[Finding]) -> String {
        findings
            .iter()
            .map(|f| format!("{}: {}", f.plugin, f.message))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── The bundled index ───────────────────────────────────────────────

    /// AC 1: the bundled index parses and passes its own gate against this
    /// release. A version bump fails here until each listing is re-verified.
    #[test]
    fn the_bundled_index_passes_the_gate_for_this_release() {
        let index = parse(BUNDLED).expect("bundled index parses");
        let findings = check(&index, RELEASE);
        assert!(findings.is_empty(), "{}", messages(&findings));
    }

    /// AC 1: every first-party plugin the CLI can install has a listing.
    #[test]
    fn the_bundled_index_lists_every_first_party_plugin() {
        let index = parse(BUNDLED).expect("bundled index parses");
        for entry in super::super::catalog::FIRST_PARTY {
            let listing = index
                .get(entry.crate_name)
                .unwrap_or_else(|| panic!("{} has no listing", entry.crate_name));
            assert_eq!(listing.origin, ListingOrigin::FirstParty);
            assert_eq!(listing.status, Status::Listed, "{}", entry.crate_name);
            assert_eq!(
                listing.description, entry.summary,
                "{} description drifted from the catalog",
                entry.crate_name
            );
        }
    }

    /// AC 1 names these three by name.
    #[test]
    fn the_bundled_index_seeds_the_three_named_plugins() {
        let index = parse(BUNDLED).expect("bundled index parses");
        for name in [
            "autumn-admin-plugin",
            "autumn-cache-redis",
            "autumn-storage-s3",
        ] {
            assert!(index.get(name).is_some(), "{name}");
        }
    }

    /// AC 1: every listing carries a trust class; every native one says so.
    #[test]
    fn every_bundled_listing_has_a_trust_label() {
        let index = parse(BUNDLED).expect("bundled index parses");
        for listing in &index.plugins {
            let label = listing.trust_label();
            assert!(!label.is_empty(), "{}", listing.name);
            if listing.trust == Trust::Native {
                assert_eq!(label, FULL_TRUST_LABEL);
            }
        }
    }

    // ── Parse ───────────────────────────────────────────────────────────

    #[test]
    fn parse_rejects_an_unknown_schema() {
        let err = parse("schema = 2\n").unwrap_err();
        assert_eq!(err, IndexError::Schema(2));
    }

    #[test]
    fn parse_rejects_an_unknown_field_value() {
        let src = BUNDLED.replacen("trust = \"native\"", "trust = \"root\"", 1);
        assert!(matches!(parse(&src), Err(IndexError::Toml(_))));
    }

    /// A mistyped key (`experimental_surface`) must not drop a trust fact
    /// without a word.
    #[test]
    fn parse_rejects_an_unknown_key() {
        let src = BUNDLED.replacen(
            "tier = \"stable\"",
            "tier = \"stable\"\nexperimental_surface = [\"x\"]",
            1,
        );
        assert!(matches!(parse(&src), Err(IndexError::Toml(_))));
        let src = BUNDLED.replacen(
            "result = \"pass\"",
            "result = \"pass\"\nverdict = \"ok\"",
            1,
        );
        assert!(matches!(parse(&src), Err(IndexError::Toml(_))));
    }

    #[test]
    fn parse_accepts_an_empty_index() {
        let index = parse("schema = 1\n").expect("empty index");
        assert!(index.plugins.is_empty());
    }

    // ── Load ────────────────────────────────────────────────────────────

    #[test]
    fn load_without_a_path_uses_the_bundled_copy() {
        let loaded = load(None).expect("bundled");
        assert_eq!(loaded.source, Source::Bundled);
        assert!(!loaded.index.plugins.is_empty());
    }

    #[test]
    fn load_reads_an_override_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.toml");
        std::fs::write(&path, "schema = 1\n").expect("write");
        let loaded = load(Some(&path)).expect("override");
        assert_eq!(loaded.source, Source::Override(path));
        assert!(loaded.index.plugins.is_empty());
    }

    /// A broken override is an error, not a silent fall back.
    #[test]
    fn load_fails_on_a_missing_override() {
        let err = load(Some(Path::new("/nonexistent/autumn-index.toml"))).unwrap_err();
        assert!(matches!(err, IndexError::Io(_)), "{err:?}");
    }

    // ── Lookup ──────────────────────────────────────────────────────────

    #[test]
    fn visible_hides_delisted_listings() {
        let mut gone = community();
        gone.name = "autumn-plugin-gone".to_owned();
        gone.status = Status::Delisted;
        let index = index_of(vec![community(), gone]);
        let names: Vec<&str> = index.visible().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["autumn-plugin-audit"]);
        assert!(index.get("autumn-plugin-gone").is_some());
    }

    // ── Trust label ─────────────────────────────────────────────────────

    #[test]
    fn a_sandboxed_label_names_its_capabilities() {
        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["http-request".to_owned(), "kv".to_owned()];
        assert_eq!(
            listing.trust_label(),
            "sandboxed: capability manifest grants http-request, kv"
        );
    }

    // ── Compat ──────────────────────────────────────────────────────────

    #[test]
    fn compat_follows_the_declared_range() {
        let listing = community();
        assert_eq!(listing.compat("0.7.0"), Compat::Compatible);
        assert_eq!(listing.compat("^0.7.2"), Compat::Compatible);
        assert_eq!(listing.compat("0.6.0"), Compat::Incompatible);
        assert_eq!(listing.compat("0.8.0"), Compat::Incompatible);
        assert_eq!(listing.compat(">=0.6"), Compat::Unknown);
    }

    /// AC 4: a flagged listing is refused on the release it failed on and
    /// on later ones, even when its declared range still matches.
    #[test]
    fn a_flagged_listing_is_incompatible_from_the_failed_release_on() {
        let mut listing = community();
        listing.autumn_web = ">=0.6".to_owned();
        listing.status = Status::Incompatible;
        listing.conformance.result = CheckOutcome::Fail;
        listing.conformance.autumn_web = "0.7.0".to_owned();
        assert_eq!(listing.compat("0.7.1"), Compat::Incompatible);
        assert_eq!(listing.compat("0.8.0"), Compat::Incompatible);
        assert_ne!(listing.compat("0.6.0"), Compat::Incompatible);
    }

    #[test]
    fn verified_for_compares_the_series() {
        let listing = community();
        assert!(listing.verified_for("0.7.3"));
        assert!(!listing.verified_for("0.8.0"));
        assert!(!listing.verified_for("not a version"));
    }

    // ── Validate: admission rules ───────────────────────────────────────

    #[test]
    fn a_well_formed_index_has_no_findings() {
        let index = index_of(vec![community()]);
        assert!(
            validate(&index).is_empty(),
            "{}",
            messages(&validate(&index))
        );
    }

    /// AC 3: listing is granted only with a passing conformance run.
    #[test]
    fn a_listed_plugin_must_pass_conformance() {
        let mut listing = community();
        listing.conformance.result = CheckOutcome::Fail;
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("plugin-check"), "{text}");
    }

    /// AC 3: listing is granted only with a declared, parseable range.
    #[test]
    fn a_listing_must_declare_a_parseable_range() {
        let mut listing = community();
        listing.autumn_web = "zero point seven".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("autumn_web"), "{text}");
    }

    #[test]
    fn only_first_party_listings_can_be_exempt() {
        let mut listing = community();
        listing.conformance.result = CheckOutcome::Exempt;
        listing.conformance.reason = "trust me".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("exempt"), "{text}");
    }

    #[test]
    fn an_exemption_needs_a_reason() {
        let mut listing = native("autumn-storage-s3", ListingOrigin::FirstParty);
        listing.conformance.result = CheckOutcome::Exempt;
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("reason"), "{text}");
    }

    #[test]
    fn a_community_name_must_follow_the_convention() {
        let mut listing = community();
        listing.name = "serde".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("autumn-plugin-"), "{text}");
    }

    #[test]
    fn a_first_party_listing_must_name_a_catalog_crate() {
        let listing = native("autumn-plugin-audit", ListingOrigin::FirstParty);
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("first-party"), "{text}");
    }

    #[test]
    fn duplicate_listings_are_refused() {
        let text = messages(&validate(&index_of(vec![community(), community()])));
        assert!(text.contains("more than once"), "{text}");
    }

    /// AC 5: the tier must match the declared surfaces, and each surface
    /// must be a real experimental one.
    #[test]
    fn the_tier_must_match_the_declared_surfaces() {
        let mut listing = community();
        listing.tier = Tier::Experimental;
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("experimental_surfaces"), "{text}");

        let mut listing = community();
        listing.experimental_surfaces = vec!["no-such-surface".to_owned()];
        listing.tier = Tier::Experimental;
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("no-such-surface"), "{text}");

        let mut listing = community();
        listing.experimental_surfaces = vec![first_experimental_surface()];
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("tier"), "{text}");
    }

    #[test]
    fn an_experimental_listing_with_real_surfaces_is_valid() {
        let mut listing = community();
        listing.tier = Tier::Experimental;
        listing.experimental_surfaces = vec![first_experimental_surface()];
        assert!(validate(&index_of(vec![listing])).is_empty());
    }

    fn first_experimental_surface() -> String {
        autumn_web::plugin_contract::experimental_surface_names()
            .next()
            .expect("at least one experimental surface")
            .to_owned()
    }

    /// AC 1 / AC 6: a sandboxed listing must carry its capabilities, and
    /// only known ones.
    #[test]
    fn a_sandboxed_listing_must_name_known_capabilities() {
        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        let text = messages(&validate(&index_of(vec![listing.clone()])));
        assert!(text.contains("capabilities"), "{text}");

        listing.capabilities = vec!["root-shell".to_owned()];
        let text = messages(&validate(&index_of(vec![listing.clone()])));
        assert!(text.contains("root-shell"), "{text}");

        listing.capabilities = vec!["http-request".to_owned()];
        listing.artifact_sha256 = "ab".repeat(32);
        let listing = with_full_sandbox_maps(listing);
        assert!(validate(&index_of(vec![listing])).is_empty());
    }

    /// Grants scope a sandboxed capability; a native plugin has none.
    #[test]
    fn grants_are_for_sandboxed_listings_only() {
        let mut native = community();
        native.grants.hosts = vec!["api.example.com".to_owned()];
        let text = messages(&validate(&index_of(vec![native])));
        assert!(text.contains("grants"), "{text}");
    }

    #[test]
    fn a_sandboxed_label_names_its_scoped_grants() {
        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["http-outbound".to_owned()];
        listing.grants.hosts = vec!["api.example.com".to_owned()];
        assert!(
            listing.trust_label().contains("hosts api.example.com"),
            "{}",
            listing.trust_label()
        );
    }

    /// A sandboxed listing with every quota and limit at its default.
    fn with_full_sandbox_maps(mut listing: Listing) -> Listing {
        listing.quotas = autumn_web::plugin_sandbox::CapabilityQuotas::default()
            .fields()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        listing.limits = autumn_web::plugin_sandbox::ResourceLimits::default()
            .fields()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), u64::try_from(v).unwrap()))
            .collect();
        listing
    }

    /// A sandboxed listing publishes its whole authority: a partial map
    /// is refused.
    #[test]
    fn a_sandboxed_listing_needs_complete_quotas_and_limits() {
        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["kv".to_owned()];
        listing.artifact_sha256 = "ab".repeat(32);
        let text = messages(&validate(&index_of(vec![listing.clone()])));
        assert!(text.contains("`quotas` is missing"), "{text}");
        assert!(text.contains("`limits` is missing"), "{text}");
        let mut full = with_full_sandbox_maps(listing);
        assert!(validate(&index_of(vec![full.clone()])).is_empty());
        full.limits.remove("fuel");
        let text = messages(&validate(&index_of(vec![full])));
        assert!(text.contains("fuel"), "{text}");
    }

    /// Quotas: sandboxed only, and only names the sandbox enforces.
    #[test]
    fn quotas_are_validated() {
        let mut native = community();
        native.quotas.insert("kv_reads".to_owned(), 10);
        let text = messages(&validate(&index_of(vec![native])));
        assert!(text.contains("quotas"), "{text}");

        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["kv".to_owned()];
        listing.artifact_sha256 = "ab".repeat(32);
        listing.quotas.insert("warp_drives".to_owned(), 1);
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("warp_drives"), "{text}");
    }

    /// Limits: sandboxed only, known names only, and the label names the
    /// ones that differ from the default.
    #[test]
    fn limits_are_validated_and_labelled() {
        let mut native = community();
        native.limits.insert("fuel".to_owned(), 1);
        let text = messages(&validate(&index_of(vec![native])));
        assert!(text.contains("limits"), "{text}");

        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["kv".to_owned()];
        listing.artifact_sha256 = "ab".repeat(32);
        listing.limits.insert("warp".to_owned(), 1);
        let text = messages(&validate(&index_of(vec![listing.clone()])));
        assert!(text.contains("warp"), "{text}");

        listing.limits.clear();
        listing.limits.insert("fuel".to_owned(), 7);
        assert!(
            listing.trust_label().contains("fuel=7"),
            "{}",
            listing.trust_label()
        );
    }

    /// The label names quotas that differ from the sandbox default.
    #[test]
    fn a_sandboxed_label_names_its_non_default_quotas() {
        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["kv".to_owned()];
        listing.quotas = autumn_web::plugin_sandbox::CapabilityQuotas::default()
            .fields()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert!(
            !listing.trust_label().contains("quotas"),
            "{}",
            listing.trust_label()
        );
        listing.quotas.insert("kv_reads".to_owned(), 99_999);
        assert!(
            listing.trust_label().contains("kv_reads=99999"),
            "{}",
            listing.trust_label()
        );
    }

    /// AC 6: the manifest shown is bound to reviewed bytes.
    #[test]
    fn a_sandboxed_listing_must_record_its_artifact_digest() {
        let mut listing = community();
        listing.trust = Trust::Sandboxed;
        listing.capabilities = vec!["http-request".to_owned()];
        let text = messages(&validate(&index_of(vec![listing.clone()])));
        assert!(text.contains("artifact_sha256"), "{text}");
        listing.artifact_sha256 = "not hex".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("artifact_sha256"), "{text}");

        let mut native = community();
        native.artifact_sha256 = "ab".repeat(32);
        let text = messages(&validate(&index_of(vec![native])));
        assert!(text.contains("artifact_sha256"), "{text}");
    }

    #[test]
    fn a_native_listing_cannot_claim_capabilities() {
        let mut listing = community();
        listing.capabilities = vec!["kv".to_owned()];
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("native"), "{text}");
    }

    #[test]
    fn a_flagged_listing_needs_a_failed_run_and_a_note() {
        let mut listing = community();
        listing.status = Status::Incompatible;
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("fail"), "{text}");
        assert!(text.contains("note"), "{text}");
    }

    /// Index text is printed to a terminal. Control characters could drive
    /// it, so the gate refuses them.
    #[test]
    fn control_characters_are_refused() {
        let mut listing = community();
        listing.description = "safe\u{1b}[2Jevil".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("control"), "{text}");
    }

    /// Bidi, zero-width and line-separator characters can reorder or hide
    /// text in a terminal. `char::is_control` does not catch them.
    #[test]
    fn bidi_and_invisible_characters_are_refused() {
        for bad in ["\u{202E}", "\u{2066}", "\u{200B}", "\u{2028}", "\u{FEFF}"] {
            let mut listing = community();
            listing.note = format!("safe{bad}evil");
            let text = messages(&validate(&index_of(vec![listing])));
            assert!(text.contains("control"), "{bad:?}: {text}");
        }
    }

    /// Finding text is printed. It must not carry an escape sequence even
    /// when the listing does.
    #[test]
    fn sanitize_escapes_unsafe_characters() {
        assert_eq!(sanitize("a\u{1b}[2Jb\u{202E}c"), "a\\u{1b}[2Jb\\u{202e}c");
        assert_eq!(sanitize("plain text"), "plain text");
    }

    /// crates.io treats `-`/`_` and case as the same name.
    #[test]
    fn lookup_ignores_case_and_separator_variants() {
        let index = index_of(vec![community()]);
        assert!(index.get("autumn_plugin_AUDIT").is_some());
        let mut twin = community();
        twin.name = "autumn-plugin_audit".to_owned();
        let text = messages(&validate(&index_of(vec![community(), twin])));
        assert!(text.contains("more than once"), "{text}");
    }

    /// A range with no upper bound calls every future release compatible.
    #[test]
    fn a_range_without_an_upper_bound_is_refused() {
        for open in ["*", ">=0.6"] {
            let mut listing = community();
            listing.autumn_web = open.to_owned();
            let text = messages(&validate(&index_of(vec![listing])));
            assert!(text.contains("upper bound"), "{open}: {text}");
        }
    }

    #[test]
    fn load_refuses_an_override_that_is_not_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load(Some(dir.path())).unwrap_err();
        assert!(matches!(err, IndexError::Io(_)), "{err:?}");
    }

    #[test]
    fn load_refuses_an_oversized_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.toml");
        let mut text = String::from("schema = 1\n");
        text.push_str(&"#".repeat(usize::try_from(MAX_INDEX_BYTES).unwrap() + 1));
        std::fs::write(&path, text).expect("write");
        let err = load(Some(&path)).unwrap_err();
        assert!(matches!(err, IndexError::Io(_)), "{err:?}");
    }

    #[test]
    fn the_repository_must_be_https() {
        let mut listing = community();
        listing.repository = "http://example.com".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("https"), "{text}");
    }

    #[test]
    fn the_check_date_must_be_a_date() {
        let mut listing = community();
        listing.conformance.checked = "last tuesday".to_owned();
        let text = messages(&validate(&index_of(vec![listing])));
        assert!(text.contains("checked"), "{text}");
    }

    // ── Staleness: re-verification (AC 4) ───────────────────────────────

    #[test]
    fn a_listing_verified_on_this_release_is_fresh() {
        let index = index_of(vec![community()]);
        assert!(staleness(&index, "0.7.0").is_empty());
    }

    #[test]
    fn a_listing_verified_on_an_older_release_is_stale() {
        let index = index_of(vec![community()]);
        let text = messages(&staleness(&index, "0.7.1"));
        assert!(text.contains("re-verify"), "{text}");
    }

    /// An RC of a series is that series: `0.8` covers `0.8.0-rc.1`.
    #[test]
    fn a_prerelease_release_is_covered_by_its_series() {
        let mut listing = community();
        listing.autumn_web = "0.8".to_owned();
        listing.conformance.autumn_web = "0.8.0-rc.1".to_owned();
        let text = messages(&staleness(&index_of(vec![listing]), "0.8.0-rc.1"));
        assert!(text.is_empty(), "{text}");
    }

    /// A sandboxed artifact is not tied to a series: its flag applies to
    /// every app, so discovery agrees with the install gate.
    #[test]
    fn a_sandboxed_flag_applies_to_every_app() {
        let mut listing = flagged();
        listing.trust = Trust::Sandboxed;
        listing.conformance.autumn_web = "0.9.0".to_owned();
        listing.autumn_web = ">=0.1, <1".to_owned();
        assert!(listing.flag_applies("0.2.0"));
        assert_eq!(listing.compat("0.2.0"), Compat::Incompatible);
    }

    /// The flag applies from the failed series on, not to older apps.
    #[test]
    fn a_flag_applies_from_the_failed_series() {
        let mut listing = flagged();
        listing.conformance.autumn_web = "0.8.0".to_owned();
        assert!(listing.flag_applies("0.8.2"));
        assert!(listing.flag_applies("0.9.0"));
        assert!(!listing.flag_applies("0.7.0"));
        assert!(!community().flag_applies("0.8.0"));
    }

    #[test]
    fn a_range_that_excludes_the_release_must_be_flagged() {
        let mut listing = community();
        listing.conformance.autumn_web = "0.8.0".to_owned();
        let text = messages(&staleness(&index_of(vec![listing]), "0.8.0"));
        assert!(text.contains("incompatible"), "{text}");
    }

    #[test]
    fn a_first_party_listing_must_carry_the_release_version() {
        let mut listing = native("autumn-admin-plugin", ListingOrigin::FirstParty);
        listing.version = "0.6.0".to_owned();
        let text = messages(&staleness(&index_of(vec![listing]), "0.7.0"));
        assert!(text.contains("0.6.0"), "{text}");
    }

    fn flagged() -> Listing {
        let mut listing = community();
        listing.status = Status::Incompatible;
        listing.conformance.result = CheckOutcome::Fail;
        listing.note = "failed on 0.7.0".to_owned();
        listing
    }

    /// A flag is fresh on the release it was set on.
    #[test]
    fn a_flag_on_this_release_is_fresh() {
        assert!(staleness(&index_of(vec![flagged()]), "0.7.0").is_empty());
    }

    /// AC 4: a flag does not stay forever. On the next release the listing
    /// is re-verified (relisted) or delisted.
    #[test]
    fn a_flag_from_an_older_release_must_be_resolved() {
        let text = messages(&staleness(&index_of(vec![flagged()]), "0.8.0"));
        assert!(text.contains("delist"), "{text}");
    }

    #[test]
    fn a_delisted_listing_must_have_failed() {
        let mut gone = community();
        gone.status = Status::Delisted;
        gone.note = "removed".to_owned();
        let text = messages(&validate(&index_of(vec![gone.clone()])));
        assert!(
            text.contains("delisted, but the last conformance run did not fail"),
            "{text}"
        );
        gone.conformance.result = CheckOutcome::Fail;
        assert!(validate(&index_of(vec![gone])).is_empty());
    }

    #[test]
    fn delisted_listings_are_never_stale() {
        let mut gone = flagged();
        gone.status = Status::Delisted;
        assert!(staleness(&index_of(vec![gone]), "0.9.0").is_empty());
    }
}
