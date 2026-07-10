//! Compile-time build and git provenance for `/actuator/info`.
//!
//! The consuming app's `build.rs` captures git + build-time facts and bakes
//! them into the binary as `cargo:rustc-env` variables. The
//! [`#[autumn_web::main]`](crate::main) entry-point macro reads those
//! (via `option_env!`) **in the app crate's compile context** — together with
//! the app's own `CARGO_PKG_NAME` / `CARGO_PKG_VERSION` — and hands them to
//! [`__set_build_context`] at startup. `/actuator/info` then renders them.
//!
//! Reading in the app crate's context is what fixes the long-standing
//! `"unknown"` regression: `CARGO_PKG_VERSION` only exists at compile time, so
//! the previous runtime `std::env::var` lookup always failed in a released
//! binary. Capturing at the app's compile time bakes the real value in.
//!
//! Graceful degradation: when the build is not in a git checkout (tarball / CI
//! cache) the git fields are `None` (serialized as JSON `null`) while the build
//! timestamp and version stay present. Hand-rolled apps that skip the macro get
//! `"unknown"` name/version — the documented one-liner is to adopt the
//! generated `build.rs` stanza plus `#[autumn_web::main]`.
//!
//! See issue #1242.

use std::sync::OnceLock;

use serde::Serialize;

/// Compile-time provenance captured by `#[autumn_web::main]`.
#[derive(Debug, Clone)]
struct BuildContext {
    app_name: &'static str,
    app_version: &'static str,
    git_sha: Option<&'static str>,
    git_sha_short: Option<&'static str>,
    git_branch: Option<&'static str>,
    git_dirty: Option<&'static str>,
    build_timestamp: Option<&'static str>,
}

static BUILD_CONTEXT: OnceLock<BuildContext> = OnceLock::new();

/// Record the app's compile-time provenance.
///
/// Called once, early, by the generated `main()` (`#[autumn_web::main]`). All
/// arguments are `'static` because they originate from `env!` / `option_env!`
/// evaluated in the app crate. Idempotent: only the first call wins.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn __set_build_context(
    app_name: &'static str,
    app_version: &'static str,
    git_sha: Option<&'static str>,
    git_sha_short: Option<&'static str>,
    git_branch: Option<&'static str>,
    git_dirty: Option<&'static str>,
    build_timestamp: Option<&'static str>,
) {
    let _ = BUILD_CONTEXT.set(BuildContext {
        app_name,
        app_version,
        git_sha,
        git_sha_short,
        git_branch,
        git_dirty,
        build_timestamp,
    });
}

/// Git facts for the build, folded into `/actuator/info`'s `build` object.
///
/// Every field is optional so a non-git build degrades to JSON `null` rather
/// than failing. Deliberately narrow — commit / branch / dirty only, never
/// remote URLs or environment (issue #1242, no secret leakage).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitProvenance {
    /// Full commit SHA (`git rev-parse HEAD`), or `None` outside a checkout.
    pub commit: Option<String>,
    /// Abbreviated commit SHA, or `None` outside a checkout.
    pub commit_short: Option<String>,
    /// Branch name (`git rev-parse --abbrev-ref HEAD`), or `None`.
    pub branch: Option<String>,
    /// `true` when the working tree had uncommitted changes at build time.
    pub dirty: Option<bool>,
}

/// Build + git provenance rendered under `/actuator/info`'s `build` key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BuildProvenance {
    /// The consuming app's compile-time `CARGO_PKG_VERSION`.
    pub version: String,
    /// ISO-8601 UTC build timestamp, or `None` when not captured.
    pub timestamp: Option<String>,
    /// Git provenance (all-`None` outside a checkout).
    pub git: GitProvenance,
}

fn context() -> Option<&'static BuildContext> {
    BUILD_CONTEXT.get()
}

/// The consuming app's name, or `"unknown"` when the macro did not run.
#[must_use]
pub fn app_name() -> String {
    context().map_or_else(|| "unknown".to_owned(), |ctx| ctx.app_name.to_owned())
}

/// The consuming app's compile-time version, or `"unknown"` when the macro did
/// not run.
///
/// Correct in a `--release` binary with the cargo env unset at runtime, because
/// it is baked in at the app's compile time (issue #1242).
#[must_use]
pub fn app_version() -> String {
    context().map_or_else(|| "unknown".to_owned(), |ctx| ctx.app_version.to_owned())
}

/// Build + git provenance for `/actuator/info`.
#[must_use]
pub fn build_provenance() -> BuildProvenance {
    let ctx = context();
    let non_empty = |value: Option<&'static str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "unknown")
            .map(str::to_owned)
    };
    let dirty = ctx.and_then(|ctx| ctx.git_dirty).and_then(|value| {
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        }
    });
    BuildProvenance {
        version: app_version(),
        timestamp: non_empty(ctx.and_then(|ctx| ctx.build_timestamp)),
        git: GitProvenance {
            commit: non_empty(ctx.and_then(|ctx| ctx.git_sha)),
            commit_short: non_empty(ctx.and_then(|ctx| ctx.git_sha_short)),
            branch: non_empty(ctx.and_then(|ctx| ctx.git_branch)),
            dirty,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests share one process-global `OnceLock`; keep them read-only
    // about global state except the single canonical set below. To avoid
    // cross-test ordering issues we exercise the pure mapping via a helper that
    // does not touch the global.
    fn map(ctx: &BuildContext) -> BuildProvenance {
        // Mirror of `build_provenance` for a supplied context (no global).
        let non_empty = |value: Option<&'static str>| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "unknown")
                .map(str::to_owned)
        };
        let dirty =
            ctx.git_dirty
                .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => Some(true),
                    "false" | "0" | "no" => Some(false),
                    _ => None,
                });
        BuildProvenance {
            version: ctx.app_version.to_owned(),
            timestamp: non_empty(ctx.build_timestamp),
            git: GitProvenance {
                commit: non_empty(ctx.git_sha),
                commit_short: non_empty(ctx.git_sha_short),
                branch: non_empty(ctx.git_branch),
                dirty,
            },
        }
    }

    fn ctx(
        git_sha: Option<&'static str>,
        git_dirty: Option<&'static str>,
        timestamp: Option<&'static str>,
    ) -> BuildContext {
        BuildContext {
            app_name: "my_app",
            app_version: "1.2.3",
            git_sha,
            git_sha_short: git_sha.map(|s| &s[..s.len().min(7)]),
            git_branch: Some("trunk-dev"),
            git_dirty,
            build_timestamp: timestamp,
        }
    }

    #[test]
    fn full_provenance_is_rendered() {
        let p = map(&ctx(
            Some("0123456789abcdef"),
            Some("false"),
            Some("2026-07-09T12:00:00Z"),
        ));
        assert_eq!(p.version, "1.2.3");
        assert_eq!(p.timestamp.as_deref(), Some("2026-07-09T12:00:00Z"));
        assert_eq!(p.git.commit.as_deref(), Some("0123456789abcdef"));
        assert_eq!(p.git.commit_short.as_deref(), Some("0123456"));
        assert_eq!(p.git.branch.as_deref(), Some("trunk-dev"));
        assert_eq!(p.git.dirty, Some(false));
    }

    #[test]
    fn non_git_build_degrades_git_fields_to_null_but_keeps_version_and_time() {
        // No git SHA (tarball / CI cache), but timestamp + version still present.
        let p = map(&ctx(None, None, Some("2026-07-09T12:00:00Z")));
        assert_eq!(p.version, "1.2.3");
        assert_eq!(p.timestamp.as_deref(), Some("2026-07-09T12:00:00Z"));
        assert_eq!(p.git.commit, None);
        assert_eq!(p.git.commit_short, None);
        assert_eq!(p.git.dirty, None);
        // Serializes git fields as JSON null.
        let json = serde_json::to_value(&p).unwrap();
        assert!(json["git"]["commit"].is_null());
        assert!(json["git"]["dirty"].is_null());
        assert!(!json["timestamp"].is_null());
    }

    #[test]
    fn dirty_flag_parses_true_and_ignores_garbage() {
        assert_eq!(
            map(&ctx(Some("abc"), Some("true"), None)).git.dirty,
            Some(true)
        );
        assert_eq!(
            map(&ctx(Some("abc"), Some("1"), None)).git.dirty,
            Some(true)
        );
        assert_eq!(
            map(&ctx(Some("abc"), Some("nonsense"), None)).git.dirty,
            None
        );
    }

    #[test]
    fn literal_unknown_is_treated_as_absent() {
        let p = map(&ctx(Some("unknown"), None, Some("unknown")));
        assert_eq!(p.git.commit, None);
        assert_eq!(p.timestamp, None);
    }

    #[test]
    fn serialized_shape_only_exposes_safe_fields() {
        let p = map(&ctx(
            Some("abc123"),
            Some("false"),
            Some("2026-07-09T12:00:00Z"),
        ));
        let json = serde_json::to_value(&p).unwrap();
        let obj = json.as_object().unwrap();
        // No remote URLs, no env dump — only the documented keys.
        let mut keys: Vec<_> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["git", "timestamp", "version"]);
        let git = json["git"].as_object().unwrap();
        let mut git_keys: Vec<_> = git.keys().map(String::as_str).collect();
        git_keys.sort_unstable();
        assert_eq!(git_keys, vec!["branch", "commit", "commit_short", "dirty"]);
    }
}
