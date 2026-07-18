//! Curated, version-locked built-in starters embedded in the CLI binary.
//!
//! Built-in starters apply with no network fetch and no provenance prompt — the
//! core set is vetted and shipped with `autumn-cli` (issue #993). Each starter's
//! template tree is embedded via [`include_dir!`]; the tree's root contains an
//! `autumn-starter.toml` manifest plus the template files.

use include_dir::{Dir, include_dir};

/// The flagship multi-tenant `SaaS` starter.
///
/// The rendered form of this tree (project name `saas`) is committed at
/// `examples/saas/`, where it participates in the existing examples drift gate
/// so it cannot rot silently. The `starter_matches_example` test pins the two
/// together.
pub static SAAS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/src/starters/saas");

/// A built-in starter: the `--starter` name, its one-line description, and the
/// embedded template tree.
#[derive(Debug)]
pub struct Builtin {
    /// The name users pass to `--starter`.
    pub name: &'static str,
    /// One-line description shown by `--list-starters`.
    pub description: &'static str,
    /// The embedded template tree (manifest + files).
    pub dir: &'static Dir<'static>,
}

/// The full set of curated built-in starters.
pub static BUILTINS: &[Builtin] = &[Builtin {
    name: "saas",
    description: "Multi-tenant SaaS: session auth + row-level tenancy + tenant-scoped dashboard",
    dir: &SAAS,
}];

/// Look up a built-in starter by its `--starter` name.
#[must_use]
pub fn find(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saas_is_registered() {
        let b = find("saas").expect("saas built-in should be registered");
        assert_eq!(b.name, "saas");
        assert!(!b.description.is_empty());
    }

    #[test]
    fn unknown_builtin_is_none() {
        assert!(find("does-not-exist").is_none());
    }

    #[test]
    fn saas_tree_contains_manifest() {
        assert!(
            SAAS.get_file(super::super::manifest::MANIFEST_FILE)
                .is_some(),
            "embedded saas starter must ship an autumn-starter.toml manifest"
        );
    }
}
