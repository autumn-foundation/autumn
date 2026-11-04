//! Docs gates, ported from the `scripts/check-docs-*.sh` family onto one
//! shared model of the markdown corpus.
//!
//! - [`corpus`]: which files a gate reads, and how they are decoded.
//! - [`markdown`]: live prose, heading anchors and link destinations.
//! - [`links`]: the markdown link gate.
//! - [`aliases`]: every capability page carries the word readers search by.
//! - [`guide_index`]: every guide page listed in the index that owns it.
//!
//! A ported gate replaces its script only once it gives the same result on
//! the tree, with the script's self-test carried over as `#[test]`s. See
//! `docs/adr/0033-xtask-for-repo-tooling.md`.

pub mod aliases;
pub mod corpus;
pub mod guide_index;
pub mod links;
pub mod markdown;
mod re;

use std::io::Write;
use std::path::Path;

use anyhow::Result;

/// A docs gate implemented here rather than as a script.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// The name `cargo xtask check <name>` and `cargo xtask docs <name>` use,
    /// and the script it replaced was `scripts/check-<name>.sh`.
    pub name: &'static str,
    /// One line on what it checks.
    pub about: &'static str,
    run: fn(&Path, &mut dyn Write) -> Result<bool>,
}

impl Gate {
    /// Run the gate over `root`, printing its report. Returns whether it
    /// passed.
    ///
    /// # Errors
    ///
    /// Returns an error when the gate cannot run at all.
    pub fn run(&self, root: &Path, out: &mut dyn Write) -> Result<bool> {
        (self.run)(root, out)
    }
}

/// Every ported docs gate.
pub const GATES: &[Gate] = &[
    Gate {
        name: "docs-links",
        about: "Markdown link gate: every relative link and heading anchor in the tracked markdown corpus must resolve.",
        run: |root, mut out| links::run(root, &mut out),
    },
    Gate {
        name: "docs-guide-index",
        about: "Guide-index gate: every page in docs/guide/ is listed once in the index that owns it, and README.md links the index.",
        run: |root, mut out| guide_index::run(root, &mut out),
    },
    Gate {
        name: "docs-aliases",
        about: "Reader-vocabulary gate: the page that documents a capability carries the word a reader searches for it by.",
        run: |root, mut out| aliases::run(root, &mut out),
    },
];

/// The gate called `name`, accepting it with or without `check-`.
#[must_use]
pub fn gate(name: &str) -> Option<&'static Gate> {
    let name = name.trim_start_matches("check-");
    GATES.iter().find(|g| g.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_are_found_by_either_spelling() {
        assert_eq!(gate("docs-links").map(|g| g.name), Some("docs-links"));
        assert_eq!(gate("check-docs-links").map(|g| g.name), Some("docs-links"));
        assert!(gate("docs-nope").is_none());
    }

    /// A ported gate replaces its script; both existing would leave two
    /// definitions of one check to drift apart.
    #[test]
    fn a_ported_gate_has_no_script_left_behind() {
        for g in GATES {
            let script = crate::repo_root().join(format!("scripts/check-{}.sh", g.name));
            assert!(
                !script.exists(),
                "{} is ported to xtask; delete {}",
                g.name,
                script.display()
            );
        }
    }
}
