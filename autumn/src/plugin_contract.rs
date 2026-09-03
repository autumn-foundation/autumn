//! The versioned plugin API stability contract (issue #1601).
//!
//! Autumn's plugin system is a compile-time Rust surface: a plugin depends on
//! `autumn-web`, implements [`Plugin`](crate::plugin::Plugin), and is mounted
//! with [`AppBuilder::plugin`](crate::app::AppBuilder::plugin). That makes most
//! breakage a compiler error rather than a production incident — but only if
//! plugin authors know *which* APIs they may build on, and get told when the
//! framework they are mounted into is not the one they were written for.
//!
//! This module is the machine-readable half of that contract. It carries three
//! things:
//!
//! 1. [`PLUGIN_SURFACES`] — every plugin-facing API, declared [`Stable`] or
//!    [`Experimental`]. `docs/plugins.md` renders the same list, and
//!    `scripts/check-plugin-surface.sh` fails when the two disagree.
//! 2. [`PluginContract`] — what a plugin declares about *itself*: the
//!    `autumn-web` range it supports and any experimental surface it knowingly
//!    leans on. A plugin returns one from
//!    [`Plugin::contract`](crate::plugin::Plugin::contract).
//! 3. [`evaluate`] — the compatibility verdict, and the diagnostic an
//!    incompatible pairing produces.
//!
//! # The `SemVer` policy, by tier
//!
//! | Tier | Promise | What a break costs |
//! |------|---------|--------------------|
//! | [`SurfaceTier::Stable`] | Covered by `STABILITY.md`. Below `1.0` a break needs a minor bump, a migration guide, and a filled *Plugin authors* section in that guide; from `1.0` on it needs a major. | A build failure with an upgrade path. |
//! | [`SurfaceTier::Experimental`] | May change in **any** release, including a patch. Declare it with [`PluginContract::uses_experimental`] so `autumn plugin-check` can report it. | A build failure you opted into. |
//!
//! # Declaring a contract
//!
//! ```rust
//! use autumn_web::app::AppBuilder;
//! use autumn_web::plugin::Plugin;
//! use autumn_web::plugin_contract::PluginContract;
//!
//! struct MyPlugin;
//!
//! impl Plugin for MyPlugin {
//!     fn contract(&self) -> Option<PluginContract> {
//!         Some(
//!             PluginContract::new(env!("CARGO_PKG_NAME"))
//!                 .plugin_version(env!("CARGO_PKG_VERSION"))
//!                 .autumn_web("0.7"),
//!         )
//!     }
//!
//!     fn build(self, app: AppBuilder) -> AppBuilder {
//!         app
//!     }
//! }
//! ```
//!
//! [`Stable`]: SurfaceTier::Stable
//! [`Experimental`]: SurfaceTier::Experimental

use std::fmt;

use semver::{BuildMetadata, Prerelease, Version, VersionReq};
use serde::{Deserialize, Serialize};

/// The `autumn-web` version this build presents to plugins.
///
/// This is the framework's own crate version — plugins are compile-time
/// dependencies, so the version a plugin is mounted into is always the one it
/// was linked against.
pub const AUTUMN_WEB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Machine-readable stderr marker carrying the declared plugin contracts of a
/// built application.
///
/// Emitted by the `AUTUMN_DUMP_ROUTES` dump only when
/// `AUTUMN_DUMP_PLUGIN_CONTRACT=1` is *also* set — which `autumn plugin-check`
/// sets and the plain `autumn routes` listing does not. A single compact JSON
/// array of [`PluginContract`] follows the marker on the same line. Kept off
/// stdout so the routes-only JSON parse path stays byte-compatible, exactly as
/// for [`SECURITY_CONFIG_MARKER`](crate::route_listing::SECURITY_CONFIG_MARKER).
pub const PLUGIN_CONTRACT_MARKER: &str = "[autumn:plugin-contract] ";

// ── The surface registry ───────────────────────────────────────────────────

/// Stability tier of a plugin-facing API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SurfaceTier {
    /// Covered by [`STABILITY.md`]'s `SemVer` promise. A break needs a version
    /// bump *and* a *Plugin authors* section in the release's migration guide.
    ///
    /// [`STABILITY.md`]: https://github.com/autumn-foundation/autumn/blob/trunk-dev/STABILITY.md
    Stable,
    /// May change in any release, including a patch. Plugins that use it should
    /// say so via [`PluginContract::uses_experimental`].
    Experimental,
}

impl fmt::Display for SurfaceTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable => f.write_str("stable"),
            Self::Experimental => f.write_str("experimental"),
        }
    }
}

/// One plugin-facing API, with the tier it is declared at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginSurface {
    /// Canonical name, as a plugin author would write it (`AppBuilder::nest`).
    pub name: &'static str,
    /// Which promise covers it.
    pub tier: SurfaceTier,
    /// One line an author can act on: what it is for, and — for experimental
    /// entries — what may change.
    pub note: &'static str,
}

/// Every plugin-facing API Autumn declares, sorted by [`name`](PluginSurface::name).
///
/// This is the contract `docs/plugins.md` renders and
/// `scripts/check-plugin-surface.sh` gates: an entry here that the reference
/// plugin (`autumn-plugin-reference`) does not exercise fails CI, and so does a
/// docs table that has drifted from this list.
///
/// Sorted so the table and the diff stay readable; `plugin_contract`'s test
/// suite enforces both the ordering and the absence of duplicates.
pub const PLUGIN_SURFACES: &[PluginSurface] = &[
    PluginSurface {
        name: "AppBuilder::config_section",
        tier: SurfaceTier::Stable,
        note: "Declare a plugin-owned top-level config root so `server.strict_config` treats it as known-and-opaque.",
    },
    PluginSurface {
        name: "AppBuilder::declare_plugin_routes",
        tier: SurfaceTier::Stable,
        note: "Make routes mounted through an opaque `nest`/`merge` router visible to `autumn routes` and the conformance harness.",
    },
    PluginSurface {
        name: "AppBuilder::error_pages",
        tier: SurfaceTier::Stable,
        note: "Replace the rendered error pages — a tier-1 subsystem seam (requires the `maud` feature). The plugin crate needs its own `maud` dependency: `html!` expands to absolute `::maud::` paths, which no re-export can satisfy.",
    },
    PluginSurface {
        name: "AppBuilder::merge",
        tier: SurfaceTier::Stable,
        note: "Merge a raw axum router at the root. Pair it with `declare_plugin_routes` so the routes stay attributable.",
    },
    PluginSurface {
        name: "AppBuilder::nest",
        tier: SurfaceTier::Stable,
        note: "Mount a raw axum router under a prefix. Pair it with `declare_plugin_routes` so the routes stay attributable.",
    },
    PluginSurface {
        name: "AppBuilder::on_shutdown",
        tier: SurfaceTier::Stable,
        note: "Register an async shutdown hook that runs during graceful drain.",
    },
    PluginSurface {
        name: "AppBuilder::on_startup",
        tier: SurfaceTier::Stable,
        note: "Register an async startup hook that runs once before the server binds.",
    },
    PluginSurface {
        name: "AppBuilder::plugin",
        tier: SurfaceTier::Stable,
        note: "Mount a plugin. Also the seam a cooperative plugin uses to mount a plugin of its own.",
    },
    PluginSurface {
        name: "AppBuilder::plugin_contracts",
        tier: SurfaceTier::Stable,
        note: "Read the contracts declared by the plugins mounted on a builder — what the route dump and `autumn plugin-check` are built on.",
    },
    PluginSurface {
        name: "AppBuilder::plugin_migrations",
        tier: SurfaceTier::Stable,
        note: "Contribute embedded database migrations tagged with the plugin's own name. Needs `reexports::diesel_migrations` in scope, because `embed_migrations!` expands to unqualified paths (requires the `db` feature).",
    },
    PluginSurface {
        name: "AppBuilder::plugins",
        tier: SurfaceTier::Stable,
        note: "Mount a tuple of up to eight plugins in declaration order.",
    },
    PluginSurface {
        name: "AppBuilder::routes",
        tier: SurfaceTier::Stable,
        note: "Register typed routes from `routes![]`. Plugin routes registered this way are attributed automatically.",
    },
    PluginSurface {
        name: "AppBuilder::with_config_loader",
        tier: SurfaceTier::Stable,
        note: "Replace the tier-1 configuration loader (e.g. a secrets-manager backend).",
    },
    PluginSurface {
        name: "AppBuilder::with_edge_kv",
        tier: SurfaceTier::Experimental,
        note: "Edge-capsule KV binding (requires the `edge` feature). The whole edge lane (issue #1790) may change in any release; the capsule wire protocol carries its own version field.",
    },
    PluginSurface {
        name: "AppBuilder::with_extension",
        tier: SurfaceTier::Stable,
        note: "Publish a typed value into application state for handlers to extract.",
    },
    PluginSurface {
        name: "AppBuilder::with_pool_provider",
        tier: SurfaceTier::Stable,
        note: "Replace the tier-1 database pool provider (requires the `db` feature).",
    },
    PluginSurface {
        name: "AppBuilder::with_session_store",
        tier: SurfaceTier::Stable,
        note: "Replace the tier-1 session store.",
    },
    PluginSurface {
        name: "AppBuilder::with_telemetry_provider",
        tier: SurfaceTier::Stable,
        note: "Replace the tier-1 telemetry provider.",
    },
    PluginSurface {
        name: "Plugin::build",
        tier: SurfaceTier::Stable,
        note: "The one required method: apply the plugin's wiring to the builder. Runs exactly once per app.",
    },
    PluginSurface {
        name: "Plugin::contract",
        tier: SurfaceTier::Stable,
        note: "Declare the `autumn-web` range this plugin supports and any experimental surface it uses.",
    },
    PluginSurface {
        name: "Plugin::name",
        tier: SurfaceTier::Stable,
        note: "Stable identifier used for duplicate-registration detection and route attribution.",
    },
    PluginSurface {
        name: "autumn_edge::host",
        tier: SurfaceTier::Experimental,
        note: "Reference edge-capsule host API. Experimental alongside the rest of the edge lane (issue #1790).",
    },
    PluginSurface {
        name: "db::Pool",
        tier: SurfaceTier::Stable,
        note: "The pool type `DatabasePoolProvider::create_pool` returns, re-exported so a plugin need not depend on `diesel-async` itself (requires the `db` feature).",
    },
    PluginSurface {
        name: "plugin_conformance",
        tier: SurfaceTier::Stable,
        note: "The library-level conformance harness plugin authors run in their own test suite.",
    },
    PluginSurface {
        name: "plugin_contract",
        tier: SurfaceTier::Stable,
        note: "This module: `PluginContract`, `PLUGIN_SURFACES`, `SurfaceTier`, `evaluate`, `lockstep_range`, and the `PLUGIN_CONTRACT_MARKER` dump protocol.",
    },
    PluginSurface {
        name: "route_listing::RouteInfo",
        tier: SurfaceTier::Stable,
        note: "The route manifest type the conformance harness and `autumn routes --format json` share.",
    },
];

/// Look a surface up by its canonical name.
#[must_use]
pub fn surface(name: &str) -> Option<&'static PluginSurface> {
    PLUGIN_SURFACES.iter().find(|s| s.name == name)
}

/// The names of every [`SurfaceTier::Stable`] surface, in registry order.
pub fn stable_surface_names() -> impl Iterator<Item = &'static str> {
    PLUGIN_SURFACES
        .iter()
        .filter(|s| s.tier == SurfaceTier::Stable)
        .map(|s| s.name)
}

/// The names of every [`SurfaceTier::Experimental`] surface, in registry order.
pub fn experimental_surface_names() -> impl Iterator<Item = &'static str> {
    PLUGIN_SURFACES
        .iter()
        .filter(|s| s.tier == SurfaceTier::Experimental)
        .map(|s| s.name)
}

// ── The contract a plugin declares ─────────────────────────────────────────

/// What a plugin declares about its own compatibility.
///
/// Built with the fluent constructors and returned from
/// [`Plugin::contract`](crate::plugin::Plugin::contract). Every field is
/// optional: a plugin that declares nothing keeps working exactly as before
/// ([`ContractVerdict::Undeclared`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PluginContract {
    /// The plugin's crate name, e.g. `"autumn-admin-plugin"`.
    pub plugin: String,
    /// The plugin's own version, when it declares one. Carried purely so the
    /// mismatch diagnostic can name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_version: Option<String>,
    /// A Cargo-style version requirement on `autumn-web` (`"0.7"`,
    /// `">=0.6, <0.9"`). `None` means the plugin declares no range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autumn_web: Option<String>,
    /// Names of [`SurfaceTier::Experimental`] surfaces this plugin knowingly
    /// depends on. Reported by `autumn plugin-check`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub experimental_surfaces: Vec<String>,
    /// The [`Plugin::name`](crate::plugin::Plugin::name) the plugin was
    /// registered under, filled in by
    /// [`AppBuilder::plugin`](crate::app::AppBuilder::plugin).
    ///
    /// **Not set by the plugin author.** It exists because
    /// [`plugin`](Self::plugin) is the plugin's *crate* name while route
    /// attribution keys on `Plugin::name()`, and the default `name()` is
    /// [`std::any::type_name`] — so a plugin that declares
    /// `env!("CARGO_PKG_NAME")` without overriding `name()` would otherwise be
    /// unfindable by the one identifier `autumn plugin-check --plugin-name`
    /// takes. Carrying both lets the CLI match on either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_as: Option<String>,
}

impl PluginContract {
    /// Start a contract for the named plugin crate.
    ///
    /// Pass `env!("CARGO_PKG_NAME")` so the name can never drift from the
    /// crate it describes.
    #[must_use]
    pub fn new(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            ..Self::default()
        }
    }

    /// Record the plugin's own version, so a mismatch diagnostic can name it.
    ///
    /// Pass `env!("CARGO_PKG_VERSION")`.
    #[must_use]
    pub fn plugin_version(mut self, version: impl Into<String>) -> Self {
        self.plugin_version = Some(version.into());
        self
    }

    /// Declare the `autumn-web` range this plugin supports.
    ///
    /// The string is a Cargo version requirement: `"0.7"` (the whole `0.7`
    /// series), `">=0.6, <0.9"`, `"=0.7.1"`. First-party plugins ship in
    /// lockstep with the framework and declare their own minor series.
    #[must_use]
    pub fn autumn_web(mut self, requirement: impl Into<String>) -> Self {
        self.autumn_web = Some(requirement.into());
        self
    }

    /// Declare that this plugin knowingly depends on an experimental surface.
    ///
    /// The name must match a [`SurfaceTier::Experimental`] entry in
    /// [`PLUGIN_SURFACES`]; `autumn plugin-check` fails on a name it cannot
    /// resolve, because a declaration nobody can look up is worse than none.
    #[must_use]
    pub fn uses_experimental(mut self, surface: impl Into<String>) -> Self {
        self.experimental_surfaces.push(surface.into());
        self
    }

    /// The plugin as it should appear in a diagnostic: `name version` when a
    /// version was declared, bare `name` otherwise.
    #[must_use]
    pub fn label(&self) -> String {
        self.plugin_version
            .as_ref()
            .map_or_else(|| self.plugin.clone(), |v| format!("{} {v}", self.plugin))
    }
}

/// The `autumn-web` range a crate released **in lockstep** with the framework
/// declares support for.
///
/// First-party plugins ship at the framework's own version, so their supported
/// range is their own version's compatibility key: the `MAJOR.MINOR` series
/// below `1.0` (where `STABILITY.md` treats every minor bump as breaking), and
/// the major from `1.0` on.
///
/// Pass `env!("CARGO_PKG_VERSION")` rather than a literal so a version bump
/// cannot leave a stale range behind:
///
/// ```rust
/// use autumn_web::plugin_contract::{PluginContract, lockstep_range};
///
/// let contract = PluginContract::new(env!("CARGO_PKG_NAME"))
///     .autumn_web(lockstep_range(env!("CARGO_PKG_VERSION")));
/// ```
///
/// # This is for lockstep crates only
///
/// A crate that versions **independently** of `autumn-web` must not use this:
/// a third-party plugin at its own `1.2.0` would derive `"1"`, which excludes
/// every `0.x` framework and makes every consuming app fail at registration.
/// Write the range you actually support as a literal instead.
///
/// A version this cannot split into `MAJOR.MINOR` is returned trimmed but
/// otherwise unchanged, so the malformed value reaches [`evaluate`] as written
/// rather than being silently reshaped into a range that means something else.
/// Note that some such strings — `"1"`, or the empty string — *are* valid
/// Cargo requirements (`^1` and `*`), so a caller passing something other than
/// `env!("CARGO_PKG_VERSION")` should validate it.
#[must_use]
pub fn lockstep_range(version: &str) -> String {
    let version = version.trim();
    let mut parts = version.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return version.to_owned();
    };
    if major.is_empty() || minor.is_empty() || !minor.chars().all(|c| c.is_ascii_digit()) {
        return version.to_owned();
    }
    if major == "0" {
        format!("0.{minor}")
    } else if major.chars().all(|c| c.is_ascii_digit()) {
        major.to_owned()
    } else {
        version.to_owned()
    }
}

// ── Evaluation ─────────────────────────────────────────────────────────────

/// The outcome of checking a [`PluginContract`] against a framework version.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContractVerdict {
    /// The plugin declares no `autumn-web` range. Nothing to enforce.
    Undeclared,
    /// The declared range admits this framework version.
    Compatible,
    /// The declared range excludes this framework version.
    Incompatible(PluginCompatibilityError),
    /// The requirement or the framework version could not be parsed, so no
    /// verdict is possible. Warned about at runtime and **failed** by
    /// `autumn plugin-check`: the author's gate is the place to catch a typo,
    /// not somebody else's application boot.
    #[non_exhaustive]
    Unparseable {
        /// The requirement string as declared.
        requirement: String,
        /// Why it could not be evaluated.
        reason: String,
    },
}

/// An incompatible plugin/framework pairing, rendered as the diagnostic a user
/// sees.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginCompatibilityError {
    /// The plugin as it appears in the message (`name version`).
    pub plugin: String,
    /// The plugin's crate name on its own, for `cargo update -p`.
    pub plugin_crate: String,
    /// The `autumn-web` range the plugin declared.
    pub declared: String,
    /// The `autumn-web` version actually in the build.
    pub found: String,
}

impl fmt::Display for PluginCompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "plugin `{plugin}` supports autumn-web {declared}, but this application builds against autumn-web {found}.\n  \
             \u{2192} upgrade the plugin to a release built for autumn-web {found} \
             (`cargo update -p {krate}`), or\n  \
             \u{2192} pin the framework the plugin supports: autumn-web = \"{declared}\"",
            plugin = self.plugin,
            declared = self.declared,
            found = self.found,
            krate = self.plugin_crate,
        )
    }
}

impl std::error::Error for PluginCompatibilityError {}

/// Check a plugin's declared `autumn-web` range against a framework version.
///
/// # Prerelease handling
///
/// `SemVer` excludes prereleases from a requirement that does not mention one,
/// so a strict reading would call `autumn-web 0.7.0-rc.1` incompatible with a
/// plugin declaring `"0.7"`. That is the opposite of what an author means: an
/// RC of the series *is* the series. The framework version's prerelease and
/// build metadata are therefore stripped before matching — **unless the
/// requirement itself names a prerelease**, in which case the author is
/// deliberately pinning one (`"=0.8.0-rc.1"`) and stripping it would make the
/// pin fail against the exact build it names.
#[must_use]
pub fn evaluate(contract: &PluginContract, autumn_web_version: &str) -> ContractVerdict {
    let Some(requirement) = contract.autumn_web.as_deref() else {
        return ContractVerdict::Undeclared;
    };

    let parsed = match Version::parse(autumn_web_version) {
        Ok(v) => v,
        Err(e) => {
            return ContractVerdict::Unparseable {
                requirement: requirement.to_owned(),
                reason: format!(
                    "autumn-web version `{autumn_web_version}` is not a semver version: {e}"
                ),
            };
        }
    };

    let req = match VersionReq::parse(requirement) {
        Ok(r) => r,
        Err(e) => {
            return ContractVerdict::Unparseable {
                requirement: requirement.to_owned(),
                reason: format!("`{requirement}` is not a Cargo version requirement: {e}"),
            };
        }
    };

    let requirement_names_a_prerelease = req.comparators.iter().any(|c| !c.pre.is_empty());
    let version = if requirement_names_a_prerelease {
        parsed
    } else {
        Version {
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
            ..parsed
        }
    };

    if req.matches(&version) {
        ContractVerdict::Compatible
    } else {
        ContractVerdict::Incompatible(PluginCompatibilityError {
            plugin: contract.label(),
            plugin_crate: contract.plugin.clone(),
            declared: requirement.to_owned(),
            found: autumn_web_version.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_framework_version_constant_is_a_semver_version() {
        assert!(
            Version::parse(AUTUMN_WEB_VERSION).is_ok(),
            "AUTUMN_WEB_VERSION = {AUTUMN_WEB_VERSION}"
        );
    }

    #[test]
    fn undeclared_contracts_are_not_incompatible() {
        let c = PluginContract::new("p");
        assert_eq!(
            evaluate(&c, AUTUMN_WEB_VERSION),
            ContractVerdict::Undeclared
        );
    }

    #[test]
    fn a_caret_requirement_matches_its_series() {
        let c = PluginContract::new("p").autumn_web("0.7");
        assert_eq!(evaluate(&c, "0.7.4"), ContractVerdict::Compatible);
        assert!(matches!(
            evaluate(&c, "0.8.0"),
            ContractVerdict::Incompatible(_)
        ));
    }

    #[test]
    fn an_exact_pin_matches_only_that_version() {
        let c = PluginContract::new("p").autumn_web("=0.7.1");
        assert_eq!(evaluate(&c, "0.7.1"), ContractVerdict::Compatible);
        assert!(matches!(
            evaluate(&c, "0.7.2"),
            ContractVerdict::Incompatible(_)
        ));
    }

    #[test]
    fn label_omits_a_version_that_was_never_declared() {
        assert_eq!(PluginContract::new("p").label(), "p");
        assert_eq!(
            PluginContract::new("p").plugin_version("1.0.0").label(),
            "p 1.0.0"
        );
    }

    #[test]
    fn lockstep_range_is_the_minor_series_below_one_zero() {
        assert_eq!(lockstep_range("0.7.0"), "0.7");
        assert_eq!(lockstep_range("0.7.3-rc.1"), "0.7");
        assert_eq!(lockstep_range("0.12.0"), "0.12");
    }

    #[test]
    fn lockstep_range_is_the_major_from_one_zero_on() {
        assert_eq!(lockstep_range("1.4.2"), "1");
        assert_eq!(lockstep_range("2.0.0"), "2");
    }

    #[test]
    fn lockstep_range_passes_through_what_it_cannot_split() {
        // Returned unchanged so the malformed value surfaces as `Unparseable`
        // rather than as a range that silently means something else.
        for odd in ["", "1", "not.a.version", "x.y.z", ".7.0"] {
            assert_eq!(lockstep_range(odd), odd, "input {odd:?}");
        }
    }

    #[test]
    fn a_lockstep_range_admits_its_own_version() {
        for version in ["0.7.0", "0.7.9", "1.4.2"] {
            let c = PluginContract::new("p").autumn_web(lockstep_range(version));
            assert_eq!(
                evaluate(&c, version),
                ContractVerdict::Compatible,
                "version {version}"
            );
        }
    }

    #[test]
    fn every_experimental_surface_note_says_what_may_change() {
        for name in experimental_surface_names() {
            let note = surface(name).expect("registry entry").note;
            assert!(
                note.contains("change") || note.contains("Experimental"),
                "`{name}`'s note has to tell an author what they are opting into: {note}"
            );
        }
    }
}
