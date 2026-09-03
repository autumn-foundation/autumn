//! Reading a security posture manifest back in, and reducing it to the
//! security-relevant projection everything else in this module compares.
//!
//! The emitting side (`crate::routes_audit`) owns `Serialize` types whose
//! `provenance` fields are `&'static str`; this is the deliberately separate
//! `Deserialize` side. Keeping the two apart buys forward tolerance: an unknown
//! field added by a later schema revision is ignored here rather than failing
//! the gate, while a *newer* `schema_version` than this CLI understands is
//! refused outright (a silently mis-read manifest is worse than no gate).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::routes_audit::MANIFEST_SCHEMA_VERSION;

/// Oldest manifest schema this differ can read.
///
/// v3 is the first revision that carries all four dimensions; nothing older
/// ever shipped a `posture diff`, so there is no compatibility debt to pay.
pub const MIN_SCHEMA_VERSION: u32 = 3;

/// Newest manifest schema this differ can read: whatever this CLI itself emits.
///
/// A manifest from a newer CLI is refused rather than read tolerantly, because
/// tolerance is only safe for *added* fields. When
/// [`MANIFEST_SCHEMA_VERSION`](crate::routes_audit::MANIFEST_SCHEMA_VERSION) is
/// next bumped, re-read [`PostureManifest::projection`] below: if the bump changes the
/// meaning of an existing field rather than adding new ones, the diff rules move with
/// it.
pub const MAX_SCHEMA_VERSION: u32 = 3;

// Deliberately a literal, not `MANIFEST_SCHEMA_VERSION`. Tracking the emitter
// would auto-widen what this differ accepts on the very bump whose doc comment
// says to re-read the rules — the compile error below is the point.
const _: () = assert!(
    MAX_SCHEMA_VERSION <= MANIFEST_SCHEMA_VERSION,
    "the differ claims to read a manifest schema the emitter does not produce"
);

/// Why a manifest could not be turned into a [`PostureManifest`].
#[derive(Debug)]
pub enum ManifestError {
    /// The file could not be read.
    Io {
        path: String,
        source: std::io::Error,
    },
    /// The bytes are not the JSON this expects.
    Malformed { path: String, message: String },
    /// The document announces a schema this CLI does not understand.
    UnsupportedSchema { path: String, found: u32 },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {path}: {source}"),
            Self::Malformed { path, message } => {
                write!(f, "{path} is not a security posture manifest: {message}")
            }
            Self::UnsupportedSchema { path, found } => write!(
                f,
                "{path} declares manifest schema v{found}, but this CLI understands \
                 v{MIN_SCHEMA_VERSION}..=v{MAX_SCHEMA_VERSION} — upgrade `autumn` \
                 (or regenerate the manifest with the CLI this project pins)"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// A security posture manifest, as read back from disk.
#[derive(Debug, Clone, Deserialize)]
pub struct PostureManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub dimensions: Dimensions,
}

/// The four manifest dimensions. Every one defaults to empty so a manifest that
/// predates a dimension — or omits one — reads as "nothing declared here"
/// instead of failing the gate.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Dimensions {
    #[serde(default)]
    pub routes: RoutesDimension,
    #[serde(default)]
    pub csrf: CsrfDimension,
    #[serde(default)]
    pub security_headers: HeadersDimension,
    #[serde(default)]
    pub authorization_policies: AuthorizationPoliciesDimension,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutesDimension {
    #[serde(default)]
    pub entries: Vec<RouteEntry>,
}

/// One route's proven auth posture.
///
/// `name`, `location`, `module` and `source` are deliberately **absent**: a
/// handler rename or a moved line is not a posture change, and including them
/// would make every refactor a finding.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteEntry {
    pub path: String,
    pub method: String,
    #[serde(default)]
    pub classification: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub policy: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CsrfDimension {
    #[serde(default)]
    pub entries: Vec<CsrfEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CsrfEntry {
    pub path: String,
    pub method: String,
    #[serde(default)]
    pub csrf_enforced: bool,
    /// Whether an exemption prefix — rather than CSRF being off entirely — is
    /// why this route is unprotected. Only used to say *which* of the two
    /// happened in the finding's detail.
    #[serde(default)]
    pub exempt: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HeadersDimension {
    #[serde(default)]
    pub entries: Vec<HeaderEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeaderEntry {
    pub header: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub emitted: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthorizationPoliciesDimension {
    #[serde(default)]
    pub entries: Vec<AuthorizationPolicyEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationPolicyEntry {
    pub path: String,
    pub method: String,
    pub action: String,
    pub resource: String,
}

/// The stable key a route, CSRF entry or authorization binding is compared on.
pub type RouteKey = (String, String);

impl RouteEntry {
    /// `(path, method)` — the identity of a mounted route. Handler name and
    /// source location are not part of it.
    #[must_use]
    pub fn key(&self) -> RouteKey {
        (self.path.clone(), self.method.clone())
    }

    /// Roles as a set. `#[secured("a", "b")]` admits *either* role, so ordering
    /// carries no meaning and re-ordering must never read as a change.
    #[must_use]
    pub fn role_set(&self) -> BTreeSet<String> {
        self.roles.iter().cloned().collect()
    }

    /// Scopes as a set. `__check_secured_scopes` requires *all* of them.
    #[must_use]
    pub fn scope_set(&self) -> BTreeSet<String> {
        self.scopes.iter().cloned().collect()
    }

    /// A one-line human rendering of this route's posture, e.g.
    /// `gated (roles: admin)` or `public`.
    #[must_use]
    pub fn posture_label(&self) -> String {
        let mut label = if self.classification.is_empty() {
            "unclassified".to_owned()
        } else {
            self.classification.clone()
        };
        let mut parts = Vec::new();
        if !self.roles.is_empty() {
            let mut roles: Vec<&str> = self.roles.iter().map(String::as_str).collect();
            roles.sort_unstable();
            parts.push(format!("roles: {}", roles.join(", ")));
        }
        if !self.scopes.is_empty() {
            let mut scopes: Vec<&str> = self.scopes.iter().map(String::as_str).collect();
            scopes.sort_unstable();
            parts.push(format!("scopes: {}", scopes.join(", ")));
        }
        if self.policy {
            parts.push("policy".to_owned());
        }
        if !parts.is_empty() {
            let _ = write!(label, " ({})", parts.join("; "));
        }
        label
    }
}

/// Whether a classification means "anyone can reach this route".
///
/// The vocabulary is the one `routes audit` assigns (see
/// [`AuditRoute::is_unclassified`](crate::routes_audit::AuditRoute::is_unclassified)):
/// `gated`, `public`, `framework`, `unclassified`. Anything else — including a
/// value a newer emitter might add — reads as open, which is the safe
/// direction: it over-reports rather than missing a widening.
#[must_use]
pub fn is_open(classification: &str) -> bool {
    // An empty or unknown classification is treated as open on purpose: it is
    // what `routes audit` calls `unclassified`, and the safe reading of "we
    // could not prove a guard" is "there is no guard".
    !matches!(classification, "gated" | "framework")
}

impl PostureManifest {
    /// Parse a manifest, refusing a schema this CLI cannot read.
    ///
    /// `path` is only used to make the diagnostics point somewhere.
    pub fn parse(json: &str, path: &str) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_str(json).map_err(|e| ManifestError::Malformed {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        if manifest.schema_version < MIN_SCHEMA_VERSION
            || manifest.schema_version > MAX_SCHEMA_VERSION
        {
            return Err(ManifestError::UnsupportedSchema {
                path: path.to_owned(),
                found: manifest.schema_version,
            });
        }
        Ok(manifest)
    }

    /// Read and parse a manifest from disk.
    pub fn read(path: &str) -> Result<Self, ManifestError> {
        let json = std::fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&json, path)
    }

    /// The canonical, security-relevant projection of this manifest: one sorted
    /// line per fact, with cosmetic fields (handler name, source location)
    /// dropped and set-valued fields sorted.
    ///
    /// This is what [`Self::posture_digest`] hashes, so two manifests that
    /// differ only cosmetically hash identically.
    #[must_use]
    pub fn projection(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for r in &self.dimensions.routes.entries {
            let roles: Vec<String> = r.role_set().into_iter().collect();
            let scopes: Vec<String> = r.scope_set().into_iter().collect();
            lines.push(format!(
                "route\t{}\t{}\t{}\troles={}\tscopes={}\tpolicy={}",
                escape_field(&r.path),
                escape_field(&r.method),
                escape_field(&r.classification),
                escape_list(&roles),
                escape_list(&scopes),
                r.policy
            ));
        }
        for c in &self.dimensions.csrf.entries {
            lines.push(format!(
                "csrf\t{}\t{}\tenforced={}",
                escape_field(&c.path),
                escape_field(&c.method),
                c.csrf_enforced
            ));
        }
        for h in &self.dimensions.security_headers.entries {
            lines.push(format!(
                "header\t{}\temitted={}\tvalue={}",
                escape_field(&h.header),
                h.emitted,
                escape_field(&h.value)
            ));
        }
        for a in &self.dimensions.authorization_policies.entries {
            lines.push(format!(
                "authz\t{}\t{}\t{}\t{}",
                escape_field(&a.path),
                escape_field(&a.method),
                escape_field(&a.action),
                escape_field(&a.resource)
            ));
        }
        lines.sort();
        lines.dedup();
        lines.join("\n")
    }

    /// SHA-256 of [`Self::projection`], lower-case hex.
    ///
    /// This is the number a release records and `posture verify` re-derives: it
    /// answers "is the posture in this file the posture that was acknowledged",
    /// not "are these bytes unmodified" (the attestation answers that).
    #[must_use]
    pub fn posture_digest(&self) -> String {
        hex_digest(self.projection().as_bytes())
    }
}

/// Escape a field for a delimiter-joined canonical form.
///
/// Route paths, role names, scope names and header values are all
/// app-controlled strings, and both digests in this module join fields with
/// tabs and lines with newlines. Unescaped, one route whose path contains a tab
/// and a newline hashes exactly like a *set* of ordinary routes — so an
/// acknowledgment for the crafted one silently covers the ordinary ones. This
/// is the function that makes the encoding unambiguous.
#[must_use]
pub fn escape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // The list separator too, so `escape_list` below can join with a
            // bare comma and stay unambiguous.
            ',' => out.push_str("\\,"),
            other => out.push(other),
        }
    }
    out
}

/// Escape each element and join with a comma.
///
/// Role and scope names are unrestricted string literals — `#[secured("a,b")]`
/// compiles — so joining first and escaping afterwards would make `["a,b"]` and
/// `["a", "b"]` identical. Under OR semantics those are different postures:
/// the second admits two roles. Every element is escaped on its own, so the
/// only bare comma in the result is a separator.
#[must_use]
pub fn escape_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| escape_field(item))
        .collect::<Vec<_>>()
        .join(",")
}

/// Lower-case hex SHA-256 of `bytes`.
#[must_use]
pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(routes_json: &str) -> String {
        format!(
            r#"{{
              "schema_version": 3,
              "dimensions": {{
                "routes": {{ "provenance": "provable", "source": "macro", "entries": [{routes_json}] }},
                "csrf": {{ "provenance": "declared", "source": "config", "exempt_paths": [], "entries": [] }},
                "security_headers": {{ "provenance": "declared", "source": "config", "entries": [] }},
                "authorization_policies": {{ "provenance": "provable", "source": "macro", "runtime_caveat": "x", "entries": [] }}
              }},
              "excluded": []
            }}"#
        )
    }

    #[test]
    fn parses_a_v3_manifest_and_ignores_unknown_fields() {
        let json = manifest(
            r#"{"path":"/admin","method":"GET","name":"admin","classification":"gated",
                "roles":["admin"],"scopes":[],"policy":false,"source":"user",
                "location":"src/admin.rs:1","provenance":"provable","future_field":42}"#,
        );
        let m = PostureManifest::parse(&json, "m.json").expect("v3 manifest parses");
        assert_eq!(m.schema_version, 3);
        assert_eq!(m.dimensions.routes.entries.len(), 1);
        assert_eq!(m.dimensions.routes.entries[0].roles, vec!["admin"]);
    }

    #[test]
    fn refuses_a_newer_schema_than_this_cli_understands() {
        let json = manifest("").replace("\"schema_version\": 3", "\"schema_version\": 99");
        let err = PostureManifest::parse(&json, "m.json").expect_err("v99 must be refused");
        assert!(matches!(
            err,
            ManifestError::UnsupportedSchema { found: 99, .. }
        ));
        assert!(err.to_string().contains("upgrade"), "{err}");
    }

    #[test]
    fn refuses_an_older_schema_than_this_cli_understands() {
        let json = manifest("").replace("\"schema_version\": 3", "\"schema_version\": 1");
        assert!(matches!(
            PostureManifest::parse(&json, "m.json"),
            Err(ManifestError::UnsupportedSchema { found: 1, .. })
        ));
    }

    #[test]
    fn malformed_json_names_the_file() {
        let err = PostureManifest::parse("{ not json", "base.json").expect_err("must fail");
        assert!(err.to_string().contains("base.json"), "{err}");
    }

    #[test]
    fn a_manifest_missing_dimensions_reads_as_empty() {
        let m = PostureManifest::parse(r#"{"schema_version":3}"#, "m.json").expect("parses");
        assert!(m.dimensions.routes.entries.is_empty());
        assert!(m.dimensions.csrf.entries.is_empty());
    }

    #[test]
    fn projection_drops_cosmetic_fields() {
        let a = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","name":"old_name","classification":"gated",
                    "roles":["admin"],"scopes":[],"policy":false,"location":"src/a.rs:10"}"#,
            ),
            "a",
        )
        .unwrap();
        let b = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","name":"new_name","classification":"gated",
                    "roles":["admin"],"scopes":[],"policy":false,"location":"src/other.rs:400"}"#,
            ),
            "b",
        )
        .unwrap();
        assert_eq!(a.posture_digest(), b.posture_digest());
    }

    #[test]
    fn projection_is_order_insensitive_for_roles_and_entries() {
        let a = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"gated","roles":["admin","editor"],"scopes":["read","write"],"policy":false},
                   {"path":"/b","method":"GET","classification":"public","roles":[],"scopes":[],"policy":false}"#,
            ),
            "a",
        )
        .unwrap();
        let b = PostureManifest::parse(
            &manifest(
                r#"{"path":"/b","method":"GET","classification":"public","roles":[],"scopes":[],"policy":false},
                   {"path":"/a","method":"GET","classification":"gated","roles":["editor","admin"],"scopes":["write","read"],"policy":false}"#,
            ),
            "b",
        )
        .unwrap();
        assert_eq!(a.posture_digest(), b.posture_digest());
    }

    /// `#[secured("a,b")]` is legal — role and scope names are unrestricted
    /// string literals — so joining a list with commas and escaping only the
    /// joined string makes `["a,b"]` and `["a", "b"]` hash identically. Under OR
    /// semantics those are different postures: the second admits two roles.
    #[test]
    fn a_comma_inside_a_role_name_does_not_forge_a_second_role() {
        let one_odd_role = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"gated","roles":["a,b"],"scopes":[],"policy":false}"#,
            ),
            "a",
        )
        .unwrap();
        let two_roles = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"gated","roles":["a","b"],"scopes":[],"policy":false}"#,
            ),
            "b",
        )
        .unwrap();
        assert_ne!(one_odd_role.posture_digest(), two_roles.posture_digest());
    }

    #[test]
    fn a_comma_inside_a_scope_name_does_not_forge_a_second_scope() {
        let one = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"gated","roles":[],"scopes":["x,y"],"policy":false}"#,
            ),
            "a",
        )
        .unwrap();
        let two = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"gated","roles":[],"scopes":["x","y"],"policy":false}"#,
            ),
            "b",
        )
        .unwrap();
        assert_ne!(one.posture_digest(), two.posture_digest());
    }

    #[test]
    fn escaping_is_reversible_enough_to_keep_fields_distinct() {
        // The pairs that would collide under a weaker scheme.
        assert_ne!(escape_field("a\tb"), escape_field("a\\tb"));
        assert_ne!(escape_field("a,b"), escape_list(&["a".into(), "b".into()]));
        assert_ne!(
            escape_list(&["a,b".into()]),
            escape_list(&["a".into(), "b".into()])
        );
    }

    #[test]
    fn a_real_posture_change_moves_the_digest() {
        let gated = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"gated","roles":["admin"],"scopes":[],"policy":false}"#,
            ),
            "a",
        )
        .unwrap();
        let public = PostureManifest::parse(
            &manifest(
                r#"{"path":"/a","method":"GET","classification":"public","roles":[],"scopes":[],"policy":false}"#,
            ),
            "b",
        )
        .unwrap();
        assert_ne!(gated.posture_digest(), public.posture_digest());
    }

    #[test]
    fn open_classifications_are_the_ones_without_a_proven_guard() {
        assert!(is_open("public"));
        assert!(is_open("unclassified"));
        assert!(is_open(""));
        assert!(!is_open("gated"));
        assert!(!is_open("framework"));
    }

    #[test]
    fn posture_label_reads_like_the_attribute_it_came_from() {
        let entry: RouteEntry = serde_json::from_str(
            r#"{"path":"/a","method":"GET","classification":"gated","roles":["editor","admin"],"scopes":["posts:write"],"policy":true}"#,
        )
        .unwrap();
        assert_eq!(
            entry.posture_label(),
            "gated (roles: admin, editor; scopes: posts:write; policy)"
        );
    }
}
