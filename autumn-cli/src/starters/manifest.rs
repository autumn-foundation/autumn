//! The `autumn-starter.toml` manifest format.
//!
//! A starter manifest is the single, documented contract shared by built-in and
//! community starters (issue #993). It declares the starter's identity, any
//! files that must be copied verbatim (binary assets, pre-rendered credentials)
//! rather than run through template substitution, and post-scaffold notes shown
//! to the user once the project is on disk.

use serde::Deserialize;

/// Filename of the manifest at the root of every starter.
pub const MANIFEST_FILE: &str = "autumn-starter.toml";

/// A parsed `autumn-starter.toml`. Everything lives under a single `[starter]`
/// table.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// The `[starter]` block.
    pub starter: StarterMeta,
}

/// The `[starter]` block — the whole documented manifest schema.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct StarterMeta {
    /// Short machine name (e.g. `saas`). For built-ins this is the `--starter`
    /// value users type; for community starters it is informational.
    pub name: String,
    /// One-line human description shown by `autumn new --list-starters`.
    pub description: String,
    /// Paths (relative to the starter root, forward slashes) that must be
    /// emitted byte-for-byte without template substitution — e.g. vendored
    /// JS/binary assets. Substituting these would corrupt them.
    #[serde(default)]
    pub verbatim: Vec<String>,
    /// Optional notes printed after a successful scaffold. The standard `{{…}}`
    /// template tokens (e.g. `{{project_name}}`) are substituted before display.
    #[serde(default)]
    pub post_scaffold_notes: Option<String>,
}

impl Manifest {
    /// Parse a manifest from TOML source.
    ///
    /// # Errors
    /// Returns the TOML parse error message if the document is malformed or
    /// missing required fields.
    pub fn parse(toml_src: &str) -> Result<Self, String> {
        toml::from_str(toml_src).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let src = r#"
            [starter]
            name = "saas"
            description = "Multi-tenant SaaS starter"
            verbatim = ["static/js/htmx.min.js"]
            post_scaffold_notes = "cd {{project_name}} && autumn dev"
        "#;
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.starter.name, "saas");
        assert_eq!(m.starter.description, "Multi-tenant SaaS starter");
        assert_eq!(m.starter.verbatim, vec!["static/js/htmx.min.js".to_owned()]);
        assert_eq!(
            m.starter.post_scaffold_notes.as_deref(),
            Some("cd {{project_name}} && autumn dev")
        );
    }

    #[test]
    fn parses_minimal_manifest_with_defaults() {
        let src = r#"
            [starter]
            name = "minimal"
            description = "A tiny starter"
        "#;
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.starter.name, "minimal");
        assert!(m.starter.verbatim.is_empty());
        assert!(m.starter.post_scaffold_notes.is_none());
    }

    #[test]
    fn rejects_manifest_missing_required_fields() {
        let src = r#"
            [starter]
            name = "broken"
        "#;
        assert!(Manifest::parse(src).is_err());
    }
}
