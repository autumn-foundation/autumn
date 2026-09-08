//! `autumn doctor` — first-run environment diagnostics.
//!
//! Runs a set of checks against the local environment and project configuration,
//! reports each as ✅/⚠️/❌ with a one-line remediation hint, and exits with
//! code 0 (all clear) or 1 (any failure detected).

use autumn_web::config::{DeployConfig, ProcessRole};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

// ── Signing secret validation constants (mirrored from autumn-web) ────────────

/// Minimum byte length for a valid production signing secret.
const SIGNING_SECRET_MIN_LEN: usize = 32;

/// Known demo / template values that must never reach production.
const SIGNING_SECRET_DEMO_VALUES: &[&str] = &[
    "changeme",
    "change_me",
    "change-me",
    "secret",
    "supersecret",
    "super-secret",
    "super_secret",
    "your-secret-here",
    "your_secret_here",
    "insert-secret-here",
    "replace-this",
    "replace_me",
    "todo",
    "fixme",
    "example",
    "placeholder",
    "dev_only",
    "dev-only",
    "test_secret",
    "test-secret",
    "test",
    "password",
];

/// Known top-level keys in a valid `autumn.toml`.
#[cfg(test)]
const KNOWN_TOML_SECTIONS: &[&str] = &[
    "server",
    "database",
    "log",
    "telemetry",
    "health",
    "actuator",
    "cors",
    "session",
    "jobs",
    "auth",
    "security",
    "i18n",
    "storage",
    "backup",
    "mail",
    "profile",
    "compression",
];

/// Result status for a single diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// Result of a single diagnostic check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Short identifier for this check.
    pub name: &'static str,
    pub status: CheckStatus,
    /// Human-readable detail (what was found, or what went wrong).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// One-line remediation hint shown on warn/fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
}

/// Aggregate counts across all checks.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub passed: usize,
    pub warned: usize,
    pub failed: usize,
}

/// Options parsed from CLI flags.
#[derive(Clone, Copy)]
pub struct DoctorOptions {
    /// Emit machine-readable JSON instead of human text.
    pub json: bool,
    /// Treat warnings as failures (exit 1).
    pub strict: bool,
    /// Run active network probes (ACME preflight: port reachability, DNS
    /// pointing here). Off by default so `autumn doctor` stays offline and
    /// non-flaky; opt in with `--online` / `--preflight`.
    pub online: bool,
}

/// Extension point: implement this trait to add custom checks.
#[allow(dead_code)]
pub trait Check {
    fn run(&self) -> CheckResult;
}

/// A deprecated config key detected in the project's resolved configuration.
/// Used as input to [`check_deprecated_keys_impl`].
#[derive(Debug, Clone)]
pub struct DoctorDeprecation {
    pub path: String,
    pub replacement: Option<String>,
    pub since: String,
    pub remove_in: String,
}

/// One plugin's wiring state, as `autumn doctor` can see it from the project's
/// `Cargo.toml`, `src/main.rs`, and (best effort) its migration history.
/// Input to [`check_plugin_residue_impl`] (issue #1631).
// Four independent yes/no facts about one plugin, each read from a different
// place (the manifest, the source tree, the source tree again with a stricter
// probe, the catalog). Folding them into an enum would mean enumerating the
// combinations, which is exactly what `check_plugin_residue_impl` does.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct PluginWiring {
    /// The plugin's crate name.
    pub crate_name: String,
    /// Whether this is a community `autumn-plugin-*` crate. `plugin add` is
    /// dependency-only for those BY DESIGN — it never writes their mount — so
    /// "run `autumn plugin add` to finish the install" is wrong advice for
    /// them, and the finding has to say something else.
    pub community: bool,
    /// Whether `[dependencies]` declares it.
    pub dependency: bool,
    /// Whether anything in the app's sources looks like a mount of it —
    /// including a bare `<Name>Plugin::new(` with no crate path, which is
    /// enough to stop this check nagging about a plugin that IS wired.
    pub mount: bool,
    /// Whether a mount names the plugin's fully-qualified type path. Only this
    /// proves the app is reaching into *this* crate; a bare constructor could
    /// be the app's own same-named type, which is why the "mounted but not
    /// declared, so this does not compile" failure needs the stronger signal.
    pub mount_qualified: bool,
    /// Migration versions this plugin declares that the database still records
    /// as applied. Only meaningful when the plugin is otherwise gone; empty
    /// whenever the migration history could not be read.
    pub orphaned_migrations: Vec<String>,
}

/// Check for orphaned plugin residue: a half-install in either direction, or
/// migrations applied by a plugin that is no longer in the app (issue #1631).
///
/// Pure and injectable, like every other `_impl` check here, so the three
/// findings can be tested without a project on disk or a database.
pub fn check_plugin_residue_impl(wirings: &[PluginWiring]) -> CheckResult {
    let mut failures: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // Two independent findings per plugin, not one match: a wiring asymmetry
    // and an orphaned migration are different residue, and an arm ordering
    // that reports one can silently drop the other. `check_plugin_residue_impl`
    // is `pub` and injectable, so the invariant cannot live in its caller.
    for wiring in wirings {
        let name = &wiring.crate_name;
        match (wiring.dependency, wiring.mount) {
            // A mount with no dependency does not compile. That is not a
            // matter of taste for `--strict` to decide — but it is only sound
            // on the QUALIFIED signal: a bare `SearchPlugin::new(` may be the
            // app's own type, and asserting a compile failure on a substring
            // collision would be a hard `Fail` this check has not earned.
            (false, true) if wiring.mount_qualified => failures.push(format!(
                "{name} is mounted in the builder chain but not declared in [dependencies] — this app does not compile; run `autumn plugin add {name}`, or delete the mount"
            )),
            (true, false) => warnings.push(format!(
                "{name} is declared in [dependencies] but never mounted — {}, or run `autumn plugin remove {name}` to take the dependency back",
                if wiring.community {
                    // `plugin add` is dependency-only for a community crate BY
                    // DESIGN — it never writes their mount — so "finish the
                    // install" is advice that would go nowhere.
                    "`autumn plugin add` writes no mount for a community crate, so add the `.plugin(...)` call from its README".to_owned()
                } else {
                    format!("run `autumn plugin add {name}` to finish the install")
                }
            )),
            // Unqualified evidence only, both wires agreeing, or neither
            // present: nothing to say about the wiring.
            (false, true | false) | (true, true) => {}
        }
        // Residue in the database is only residue once the plugin is gone from
        // the code; while it is installed, applied migrations are just a
        // working install.
        if !wiring.dependency && !wiring.mount && !wiring.orphaned_migrations.is_empty() {
            warnings.push(format!(
                "{name} is not installed, but migrations it declares are still recorded as applied: {} — the tables they created are still in the database; `autumn plugin remove {name} --drop-data` reverts them",
                wiring.orphaned_migrations.join(", ")
            ));
        }
    }

    let status = if failures.is_empty() {
        if warnings.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Warn
        }
    } else {
        CheckStatus::Fail
    };
    if status == CheckStatus::Pass {
        return CheckResult {
            name: "plugin_residue",
            status,
            detail: Some("no orphaned plugin wiring or migrations".into()),
            hint: None,
        };
    }
    // Failures first, then warnings: a non-compiling app is the finding to act
    // on, and every other finding is still listed alongside it.
    let detail = failures
        .into_iter()
        .chain(warnings)
        .collect::<Vec<_>>()
        .join("\n");
    CheckResult {
        name: "plugin_residue",
        status,
        detail: Some(detail),
        hint: Some(
            "`autumn plugin list` shows what is installable; `autumn plugin remove <name>` unwires a plugin cleanly",
        ),
    }
}

/// How many findings the dependency check names before it counts the rest.
const DEPENDENCY_DETAIL_LIMIT: usize = 10;

/// How many waived ids the single-line clean state names.
const DEPENDENCY_WAIVER_NAME_LIMIT: usize = 3;

/// Grade the app's dependency policy evaluation (issue #1633).
///
/// Pure and injectable, like every other `_impl` check here. The verdict is
/// the CI gate's verdict: a denied finding fails, a warned finding warns, and a
/// waived finding is neither.
///
/// The states where the policy could not be evaluated split. A missing auditor
/// or an unfetched database is the stock state of a machine that has not opted
/// in, so those PASS and say so in their detail — `exit_code` promotes any
/// warning to exit 1, and warning there would make `autumn doctor --strict`
/// red for everyone (the rule the `platform_support` check follows). A missing
/// policy file or an audit that produced no verdict is a repository or run
/// problem, and warns.
pub fn check_dependencies_impl(eval: &crate::deps::Evaluation) -> CheckResult {
    use crate::deps::{Evaluation, STALE_AFTER_DAYS};

    let (status, detail, hint) = match eval {
        Evaluation::AuditorMissing { checks } => (
            CheckStatus::Pass,
            format!(
                "not evaluated — cargo-deny is not installed (`cargo install --locked cargo-deny`); {}",
                dependency_checks_phrase(checks)
            ),
            None,
        ),
        Evaluation::DatabaseMissing { checks } => (
            CheckStatus::Pass,
            format!(
                "not evaluated — no local advisory database (`cargo deny fetch db`); {}",
                dependency_checks_phrase(checks)
            ),
            None,
        ),
        Evaluation::NoPolicy => (
            CheckStatus::Warn,
            format!(
                "no {} — the app has no dependency policy, and its CI gate needs one",
                crate::deps::POLICY_FILE
            ),
            Some(
                "copy the `deny.toml` that `autumn new` generates; docs/guide/supply-chain.md explains it",
            ),
        ),
        Evaluation::Unavailable { reason, .. } => (
            CheckStatus::Warn,
            format!("the dependency audit produced no verdict: {reason}"),
            Some(
                "`deny.toml` is the policy both `autumn doctor` and the CI gate read; `cargo deny check` shows the auditor's own error",
            ),
        ),
        Evaluation::Audited {
            findings,
            checks,
            db_age_days,
            auditor,
        } => {
            return grade_dependency_findings(
                findings,
                checks,
                *db_age_days,
                auditor,
                STALE_AFTER_DAYS,
            );
        }
    };
    CheckResult {
        name: "dependencies",
        status,
        detail: Some(detail),
        hint,
    }
}

/// The checks a policy activates.
///
/// Spelled the same way in every state, evaluated or not: this is the fragment
/// a reader — and the scaffold's parity test — compares against the check list
/// the generated CI workflow derives.
fn dependency_checks_phrase(checks: &[String]) -> String {
    if checks.is_empty() {
        return "checks: none".to_owned();
    }
    format!("checks: {}", checks.join(", "))
}

/// Grade a completed audit.
fn grade_dependency_findings(
    findings: &[crate::deps::Finding],
    checks: &[String],
    db_age_days: Option<u64>,
    auditor: &str,
    stale_after_days: u64,
) -> CheckResult {
    let stale = db_age_days.is_some_and(|age| age > stale_after_days);
    let live: Vec<&crate::deps::Finding> =
        findings.iter().filter(|finding| !finding.waived).collect();
    let blocking = live.iter().filter(|finding| finding.blocking).count();

    let status = if blocking > 0 {
        CheckStatus::Fail
    } else if !live.is_empty() || stale {
        CheckStatus::Warn
    } else {
        CheckStatus::Pass
    };

    // Context every reader needs to map this verdict onto the CI gate: the
    // auditor, the checks the policy activates, and how old the data is.
    let mut context = format!("{auditor}; {}", dependency_checks_phrase(checks));
    if let Some(age) = db_age_days {
        let plural = if age == 1 { "" } else { "s" };
        let _ = write!(context, "; advisory data {age} day{plural} old");
        if stale {
            context.push_str(" (stale)");
        }
    }
    let stale_hint = "`cargo deny fetch db` refreshes the advisory database; the verdict above is only as fresh as that data";
    let finding_hint = "`deny.toml` holds the policy and the waivers; docs/guide/supply-chain.md explains how to fix or waive a finding";

    // Nothing live: one line, whatever the waivers hold. An app scaffolded by
    // `autumn new` always carries a waiver, so listing waivers here would make
    // the single-line clean state unreachable for every generated app.
    if live.is_empty() {
        let waived: Vec<&str> = findings.iter().map(|finding| finding.id.as_str()).collect();
        let summary = if waived.is_empty() {
            "no advisories or policy violations".to_owned()
        } else {
            format!(
                "no live findings; {} waived ({})",
                waived.len(),
                crate::deps::name_some(&waived, DEPENDENCY_WAIVER_NAME_LIMIT)
            )
        };
        return CheckResult {
            name: "dependencies",
            status,
            detail: Some(crate::deps::one_line(&format!("{summary} — {context}"))),
            hint: stale.then_some(stale_hint),
        };
    }

    // Blocking findings first, then warnings, then waivers: the reader acts on
    // what fails CI, and still sees everything else.
    let mut ordered: Vec<&crate::deps::Finding> = live.clone();
    ordered.sort_by_key(|finding| !finding.blocking);
    ordered.extend(findings.iter().filter(|finding| finding.waived));

    let waived = findings.len() - live.len();
    let plural = if findings.len() == 1 { "" } else { "s" };
    let mut counts = format!("{} finding{plural}", findings.len());
    if blocking > 0 {
        let _ = write!(counts, ", {blocking} blocking");
    }
    if waived > 0 {
        let _ = write!(counts, ", {waived} waived");
    }

    // The summary shares the check's own line; every finding below it is
    // indented to the same column as doctor's `hint:`.
    let mut lines = vec![format!("{counts} — {context}")];
    lines.extend(
        ordered
            .iter()
            .take(DEPENDENCY_DETAIL_LIMIT)
            .map(|finding| format!("   {}", format_dependency_finding(finding))),
    );
    if ordered.len() > DEPENDENCY_DETAIL_LIMIT {
        lines.push(format!(
            "   …and {} more",
            ordered.len() - DEPENDENCY_DETAIL_LIMIT
        ));
    }

    CheckResult {
        name: "dependencies",
        status,
        detail: Some(lines.join("\n")),
        // Stale data explains a verdict the reader may not otherwise trust, so
        // it outranks the fix-or-waive pointer.
        hint: Some(if stale { stale_hint } else { finding_hint }),
    }
}

/// One finding, as exactly one line of doctor detail.
///
/// Collapsed here as well as in the parser: the cap above counts findings, so
/// a finding that renders as four lines would defeat it.
fn format_dependency_finding(finding: &crate::deps::Finding) -> String {
    // A policy violation's id is its code; naming both reads "duplicate low
    // duplicate aes 0.8.4".
    let identity = if finding.id == finding.code {
        finding.code.clone()
    } else {
        format!("{} {}", finding.id, finding.code)
    };
    let mut line = crate::deps::one_line(&format!(
        "{identity} ({}) {} — {}",
        finding.severity.label(),
        finding.package,
        finding.title
    ));
    if finding.waived {
        line.push_str(" (waived)");
    }
    line
}

/// Every migration version the `diesel migration list` output records as
/// applied, oldest first.
///
/// The sibling of [`parse_latest_applied_migration_version`], which only needs
/// the newest: an orphaned plugin migration can sit anywhere in the history,
/// so the residue check needs the whole set.
fn parse_applied_migration_versions(output: &str) -> Vec<String> {
    parse_applied_migration_tokens(output)
        .map(|token| {
            // `20260720000000_media_rooms` and a bare `20260720000000` both
            // reduce to the version key `__diesel_schema_migrations` uses.
            token
                .split_once('_')
                .map_or(token, |(head, _)| head)
                .to_owned()
        })
        .collect()
}

/// Every `[X] <token>` entry in a `diesel migration list` listing, verbatim.
///
/// Shared by [`parse_applied_migration_versions`] and
/// [`parse_latest_applied_migration_version`] so the two cannot disagree about
/// what "applied" looks like in that output.
fn parse_applied_migration_tokens(output: &str) -> impl Iterator<Item = &str> {
    output.lines().filter_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("[X]")
            .or_else(|| trimmed.strip_prefix("[x]"))?
            .split_whitespace()
            .next()
    })
}

/// Every applied migration version in the database at `database_url`.
///
/// Best effort, exactly like [`latest_applied_migration_version`]: no `diesel`
/// binary, no database, or a failed invocation all read as "nothing known",
/// which makes the orphaned-migration finding impossible rather than wrong.
fn applied_migration_versions(database_url: &str) -> Vec<String> {
    diesel_migration_list(database_url)
        .as_deref()
        .map_or_else(Vec::new, parse_applied_migration_versions)
}

/// The wiring state of every first-party plugin (and every community
/// `autumn-plugin-*` dependency) in the project rooted at `root`.
///
/// Everything is read from disk; `applied` is whatever migration history the
/// caller could obtain, and may be empty.
fn resolve_plugin_wirings(
    root: &std::path::Path,
    applied: impl FnOnce() -> Vec<String>,
) -> Vec<PluginWiring> {
    let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return Vec::new();
    };
    // The whole `src` tree, not just `main.rs`. An app whose builder lives in
    // `src/app.rs` — the shape `plugin add`'s manual fallback produces, since it
    // prints the mount for the user to paste wherever their chain is — is correctly
    // wired and must not be warned at, or failed under `--strict`. Also every target
    // the manifest gives an explicit path to (`[[bin]] path = "cmd/server.rs"`),
    // because a builder chain there is just as real. Same scan `plugin remove` uses
    // to decide whether a dependency is still needed.
    let main_src = read_app_sources(root);
    let masked = crate::rust_source::mask_non_code(&main_src);

    let mut wirings: Vec<PluginWiring> = crate::plugin::catalog::FIRST_PARTY
        .iter()
        .map(|entry| PluginWiring {
            crate_name: entry.crate_name.to_owned(),
            community: false,
            dependency: crate::plugin::install::dependency_present(&manifest, entry.crate_name),
            mount: crate::plugin::install::mount_present(&main_src, entry),
            mount_qualified: crate::plugin::install::mount_call_span(&masked, entry, |argument| {
                argument.contains(entry.mount_arg)
            })
            .is_some(),
            orphaned_migrations: Vec::new(),
        })
        .collect();

    // Reading the migration history costs a `diesel` subprocess and a database
    // round trip, so it happens ONLY when the answer could change something:
    // some plugin is absent from the code *and* declares migrations that could
    // still be applied. On the overwhelmingly common project — no departed
    // plugin, or none that owns schema — `autumn doctor` pays nothing for this
    // check beyond the file reads it already did.
    let candidates: Vec<(usize, Vec<String>)> = wirings
        .iter()
        .enumerate()
        .filter(|(_, wiring)| !wiring.dependency && !wiring.mount)
        .filter_map(|(index, wiring)| {
            let entry = crate::plugin::catalog::lookup(&wiring.crate_name)?;
            if entry.migrations.is_empty() {
                return None;
            }
            Some((
                index,
                entry
                    .migrations
                    .iter()
                    .map(|version| {
                        version
                            .split_once('_')
                            .map_or(*version, |(head, _)| head)
                            .to_owned()
                    })
                    .collect(),
            ))
        })
        .collect();
    if !candidates.is_empty() {
        let applied = applied();
        for (index, declared) in candidates {
            wirings[index].orphaned_migrations = declared
                .into_iter()
                .filter(|version| applied.contains(version))
                .collect();
        }
    }

    // Community crates: the dependency names them, and the convention names the
    // struct their mount must build, so the dependency-without-mount half of
    // the check works for them too. The reverse (a mount with no dependency)
    // is not detectable — there is nothing to enumerate from.
    for crate_name in community_dependencies(&manifest) {
        let Some(struct_name) = crate::plugin::catalog::community_struct_name(&crate_name) else {
            continue;
        };
        let mount = masked.contains(&struct_name);
        wirings.push(PluginWiring {
            mount,
            // A community crate has no catalog entry to give a qualified type
            // path, so there is nothing stronger to check — and with
            // `dependency: true` always set here, the `Fail` arm is
            // unreachable for them anyway.
            mount_qualified: mount,
            crate_name,
            community: true,
            dependency: true,
            orphaned_migrations: Vec::new(),
        });
    }
    wirings
}

/// Every Rust source file a Cargo target in `root` is built from,
/// concatenated: the `src/` tree plus every explicitly-pathed target.
///
/// Cheap and good enough for a presence probe: the caller only asks whether a
/// mount appears anywhere in the app's own sources, and concatenation cannot
/// invent one that is not in some file. Files are read in a stable order and
/// each is newline-terminated, so a probe can never straddle two of them.
fn read_app_sources(root: &std::path::Path) -> String {
    let mut out = String::new();
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    let conventional = crate::plugin::install::rs_files_under(&root.join("src"));
    let explicit = crate::plugin::install::explicit_target_sources(root);
    for path in conventional.into_iter().chain(explicit) {
        if seen.contains(&path) {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.push_str(&content);
            out.push('\n');
        }
        seen.push(path);
    }
    out
}

/// Every `[dependencies]` entry following the community `autumn-plugin-<name>`
/// convention.
fn community_dependencies(manifest: &str) -> Vec<String> {
    let Ok(table) = toml::from_str::<toml::Table>(manifest) else {
        return Vec::new();
    };
    table
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .map(|deps| {
            deps.keys()
                .filter(|name| crate::plugin::catalog::is_community_name(name))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Check for deprecated config key usage (pure, injectable for tests).
///
/// Emits one `⚠️ deprecated_keys` check with one line of `detail` per offending
/// key, consistent with how `check_toml_content` aggregates unknown-key errors.
pub fn check_deprecated_keys_impl(found: &[DoctorDeprecation]) -> CheckResult {
    if found.is_empty() {
        return CheckResult {
            name: "deprecated_keys",
            status: CheckStatus::Pass,
            detail: Some("no deprecated configuration keys in use".into()),
            hint: None,
        };
    }

    let lines: Vec<String> = found
        .iter()
        .map(|d| {
            let repl = d
                .replacement
                .as_deref()
                .unwrap_or("remove it (no replacement)");
            format!(
                "{} is deprecated since {} (use {}; removed in {})",
                d.path, d.since, repl, d.remove_in
            )
        })
        .collect();

    CheckResult {
        name: "deprecated_keys",
        status: CheckStatus::Warn,
        detail: Some(lines.join("\n")),
        hint: Some(
            "Migrate to the replacement keys before the removal version; \
             deprecated keys are still honored during the deprecation window",
        ),
    }
}

/// Check signing-secret readiness (pure, injectable for tests).
///
/// - **Dev/test** (`is_production = false`): warns when no secret is configured
///   (an ephemeral per-process key is in use) and passes when a secret is set.
/// - **Production** (`is_production = true`): fails when the secret is missing,
///   below the minimum entropy floor, or matches a known demo/template value.
pub fn check_signing_secret_impl(secret: Option<&str>, is_production: bool) -> CheckResult {
    match secret {
        None if is_production => CheckResult {
            name: "signing_secret",
            status: CheckStatus::Fail,
            detail: Some("no signing secret configured in production".into()),
            hint: Some(
                "Set AUTUMN_SECURITY__SIGNING_SECRET (generate with `openssl rand -hex 32`)",
            ),
        },
        None => CheckResult {
            name: "signing_secret",
            status: CheckStatus::Warn,
            detail: Some(
                "using an ephemeral per-process signing secret (dev/test only; \
                 sessions and signed URLs will not survive restarts or be shared across replicas)"
                    .into(),
            ),
            hint: Some("Set AUTUMN_SECURITY__SIGNING_SECRET before deploying to production"),
        },
        Some(s) if is_production => {
            // Demo-value check first: "changeme" is more informative than "too short".
            let lower = s.to_ascii_lowercase();
            if SIGNING_SECRET_DEMO_VALUES.iter().any(|&d| lower == d) {
                return CheckResult {
                    name: "signing_secret",
                    status: CheckStatus::Fail,
                    detail: Some("signing secret matches a known demo/template value".into()),
                    hint: Some("Generate a new secret: `openssl rand -hex 32`"),
                };
            }
            let byte_len = s.len();
            if byte_len < SIGNING_SECRET_MIN_LEN {
                return CheckResult {
                    name: "signing_secret",
                    status: CheckStatus::Fail,
                    detail: Some(format!(
                        "signing secret too short: {byte_len} bytes \
                         (minimum {SIGNING_SECRET_MIN_LEN})"
                    )),
                    hint: Some("Generate a longer secret: `openssl rand -hex 32`"),
                };
            }
            CheckResult {
                name: "signing_secret",
                status: CheckStatus::Pass,
                detail: Some("signing secret is present and meets entropy requirements".into()),
                hint: None,
            }
        }
        Some(_) => CheckResult {
            name: "signing_secret",
            status: CheckStatus::Pass,
            detail: Some("signing secret is configured".into()),
            hint: None,
        },
    }
}

/// Check that the rate-limit key strategy is consistent with mounted auth middleware.
///
/// When `key_strategy = "authenticated_principal"`, the rate limiter reads a
/// `RateLimitPrincipal` extension that must be set by the auth layer. If no
/// auth extractor is mounted, unauthenticated requests still fall back to IP
/// keying, but authenticated routes will never use the principal bucket.
///
/// `auth_extractor_mounted` should be `true` when the project has auth configured
/// (e.g. `[auth]` section present and enabled in `autumn.toml`).
///
/// In strict mode (`autumn doctor --strict`), a warning is surfaced as an error.
pub fn check_rate_limit_key_strategy_impl(
    key_strategy: &str,
    auth_extractor_mounted: bool,
) -> CheckResult {
    if !key_strategy.is_empty()
        && key_strategy != "ip"
        && key_strategy != "api_token"
        && key_strategy != "authenticated_principal"
    {
        return CheckResult {
            name: "rate_limit_key_strategy",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "rate_limit.key_strategy = {key_strategy:?} is not a valid strategy"
            )),
            hint: Some("Expected \"ip\", \"api_token\", or \"authenticated_principal\""),
        };
    }

    if key_strategy == "authenticated_principal" && !auth_extractor_mounted {
        return CheckResult {
            name: "rate_limit_key_strategy",
            status: CheckStatus::Warn,
            detail: Some(
                "rate_limit.key_strategy = \"authenticated_principal\" is configured \
                 but no auth extractor is mounted — unauthenticated requests will \
                 always fall back to IP keying, so the per-principal budget \
                 is never applied to authenticated callers"
                    .into(),
            ),
            hint: Some(
                "Mount an auth layer (e.g. `[auth]` in autumn.toml) or change \
                 key_strategy to \"ip\" or \"api_token\"",
            ),
        };
    }

    CheckResult {
        name: "rate_limit_key_strategy",
        status: CheckStatus::Pass,
        detail: Some(format!(
            "rate_limit.key_strategy = {:?} is compatible with the current auth configuration",
            if key_strategy.is_empty() {
                "ip"
            } else {
                key_strategy
            }
        )),
        hint: None,
    }
}

/// Input data for the proxy-conflict check, resolved from `autumn.toml` and env vars.
#[derive(Default)]
pub struct ProxyConflictData {
    pub new_ranges: Vec<String>,
    pub new_trust_fwd: bool,
    pub new_hops: Option<u32>,
    pub old_ranges: Vec<String>,
    pub old_trust_fwd: bool,
}

/// Warn when both `[security.trusted_proxies]` and the deprecated
/// `[security.rate_limit]` proxy fields are configured with conflicting values.
pub fn check_proxy_conflict_impl(data: &ProxyConflictData) -> CheckResult {
    let new_set = data.new_trust_fwd || !data.new_ranges.is_empty();
    let old_set = data.old_trust_fwd || !data.old_ranges.is_empty();

    if new_set && old_set {
        let new_set: std::collections::HashSet<&str> =
            data.new_ranges.iter().map(String::as_str).collect();
        let old_set: std::collections::HashSet<&str> =
            data.old_ranges.iter().map(String::as_str).collect();
        let conflicting = new_set != old_set
            || data.new_trust_fwd != data.old_trust_fwd
            || data.new_hops.is_some();

        if conflicting {
            return CheckResult {
                name: "proxy_config_conflict",
                status: CheckStatus::Warn,
                detail: Some(
                    "[security.trusted_proxies] and [security.rate_limit] trusted proxy \
                     fields are both set with conflicting values."
                        .into(),
                ),
                hint: Some(
                    "Remove the deprecated rate_limit.trusted_proxies / \
                     rate_limit.trust_forwarded_headers fields and keep only \
                     [security.trusted_proxies]",
                ),
            };
        }
    }

    CheckResult {
        name: "proxy_config_conflict",
        status: CheckStatus::Pass,
        detail: None,
        hint: None,
    }
}

pub fn check_trusted_hosts_impl(hosts: &[String], is_production: bool) -> CheckResult {
    let normalized: Vec<String> = hosts
        .iter()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .collect();
    if normalized.is_empty() {
        return if is_production {
            CheckResult {
                name: "trusted_hosts",
                status: CheckStatus::Fail,
                detail: Some("trusted hosts list is empty in production".into()),
                hint: Some(
                    "Set [security.trusted_hosts] hosts = [\"example.com\"] or \
                     AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS",
                ),
            }
        } else {
            CheckResult {
                name: "trusted_hosts",
                status: CheckStatus::Warn,
                detail: Some("trusted hosts list is empty".into()),
                hint: Some(
                    "Set [security.trusted_hosts] hosts to prevent Host-header rebinding attacks",
                ),
            }
        };
    }

    if normalized.iter().any(|h| h == "*") {
        return CheckResult {
            name: "trusted_hosts",
            status: CheckStatus::Warn,
            detail: Some("trusted hosts wildcard '*' disables host-header allow-listing".into()),
            hint: Some("Prefer explicit hosts in production to fail closed"),
        };
    }

    CheckResult {
        name: "trusted_hosts",
        status: CheckStatus::Pass,
        detail: Some(format!(
            "trusted hosts configured ({} entries)",
            normalized.len()
        )),
        hint: None,
    }
}

/// The resolved state of `[server.tls]` for the direct-HTTPS doctor check
/// (issue #1603). Constructed offline (from `autumn.toml` + the referenced
/// files, no network, no server boot) so [`check_tls_impl`] can grade it purely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsDoctorData {
    /// No `[server.tls]` section — the app serves plain HTTP.
    NotConfigured,
    /// Configured, the cert + key loaded and matched, and the leaf certificate
    /// expires in `days_until_expiry` days (negative once already expired).
    ///
    /// Only constructed when the CLI is built with the `tls` feature (the
    /// offline inspection lives behind it); `check_tls_impl` still grades it in
    /// every build (and the unit tests exercise it), hence the conditional
    /// `dead_code` allow for the feature-less build.
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    Healthy {
        /// Whether the leaf certificate is not yet valid — its `notBefore` lies
        /// in the future. Such a certificate fails every handshake until its
        /// window opens, so the runtime rejects it at startup; doctor grades it a
        /// failure, symmetric to `expired`. Checked before `expired` (a cert
        /// cannot be both), so the pass/fail decision keys off this flag first.
        not_yet_valid: bool,
        /// Whether the leaf certificate has already expired, decided from the
        /// raw `notAfter` timestamp rather than the truncated `days_until_expiry`
        /// bucket. A certificate expired less than 24h ago buckets to `0` days
        /// but must still grade as a failure, matching the runtime which rejects
        /// it at startup — so the pass/fail decision keys off this flag.
        expired: bool,
        /// Whole days until the leaf certificate's `notAfter` (negative if past).
        /// Used only for the human-readable message, never the grade.
        days_until_expiry: i64,
    },
    /// Configured, but the cert/key could not be loaded or validated (missing
    /// file, unparseable PEM, key/cert mismatch, …). `detail` is the reason.
    Invalid {
        /// Human-readable failure reason.
        detail: String,
    },
    /// Configured, but this CLI was built without the `tls` feature, so it
    /// cannot inspect the certificate. Only constructed in the feature-less
    /// build; graded in every build (and unit-tested), hence the conditional
    /// `dead_code` allow when the `tls` feature is on.
    #[cfg_attr(feature = "tls", allow(dead_code))]
    FeatureDisabled,
}

/// Grade the resolved `[server.tls]` state (pure, injectable for tests).
///
/// - Not configured → **Pass** (serving plain HTTP is a valid choice).
/// - Built without the `tls` feature → **Warn** (cannot diagnose; do not
///   silently omit the check).
/// - Cert/key invalid or missing → **Fail**.
/// - Leaf certificate not yet valid (`notBefore` in the future) → **Fail**.
/// - Leaf certificate already expired → **Fail**.
/// - Leaf certificate expiring within 30 days → **Warn**.
/// - Otherwise → **Pass**.
#[must_use]
pub fn check_tls_impl(data: &TlsDoctorData) -> CheckResult {
    match data {
        TlsDoctorData::NotConfigured => CheckResult {
            name: "tls",
            status: CheckStatus::Pass,
            detail: Some("no [server.tls] configured; serving plain HTTP".into()),
            hint: None,
        },
        TlsDoctorData::FeatureDisabled => CheckResult {
            name: "tls",
            status: CheckStatus::Warn,
            detail: Some(
                "[server.tls] is configured but this autumn CLI was built without the `tls` \
                 feature, so the certificate could not be inspected"
                    .into(),
            ),
            hint: Some("Rebuild the autumn CLI with the `tls` feature to enable TLS diagnostics"),
        },
        TlsDoctorData::Invalid { detail } => CheckResult {
            name: "tls",
            status: CheckStatus::Fail,
            detail: Some(detail.clone()),
            hint: Some(
                "Fix [server.tls] cert_path/key_path: ensure both files exist, are valid PEM, \
                 and the key matches the certificate",
            ),
        },
        TlsDoctorData::Healthy {
            not_yet_valid: true,
            ..
        } => CheckResult {
            name: "tls",
            status: CheckStatus::Fail,
            detail: Some(
                "the [server.tls] leaf certificate is not yet valid (its notBefore is in the \
                 future)"
                    .into(),
            ),
            hint: Some(
                "Wait for the certificate's validity window to open or install a currently-valid \
                 certificate; a not-yet-valid certificate fails every TLS handshake",
            ),
        },
        TlsDoctorData::Healthy {
            expired: true,
            days_until_expiry,
            ..
        } => CheckResult {
            name: "tls",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "the [server.tls] leaf certificate expired {} day(s) ago",
                days_until_expiry.abs()
            )),
            hint: Some("Renew the certificate; an expired certificate fails every TLS handshake"),
        },
        TlsDoctorData::Healthy {
            expired: false,
            days_until_expiry,
            ..
        } if *days_until_expiry <= 30 => CheckResult {
            name: "tls",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "the [server.tls] leaf certificate expires in {days_until_expiry} day(s)"
            )),
            hint: Some("Renew the certificate soon to avoid downtime"),
        },
        TlsDoctorData::Healthy {
            days_until_expiry, ..
        } => CheckResult {
            name: "tls",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "[server.tls] certificate valid ({days_until_expiry} day(s) until expiry)"
            )),
            hint: None,
        },
    }
}

// ── ACME preflight checks (issue #1608) ──────────────────────────────────────
//
// These active checks only run under `autumn doctor --online`, so the default
// offline `doctor` stays fast and non-flaky. Each grader is a pure function of
// injected inputs (mirroring `check_tls_impl`) with a thin, bounded I/O wrapper.

/// Reachability of a TCP port, as observed by a bounded connect attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortReachability {
    /// The connection succeeded.
    Open,
    /// The connection was actively refused (nothing listening).
    Refused,
    /// The attempt timed out (filtered/unroutable).
    TimedOut,
    /// Another error prevented the attempt (DNS failure, unroutable, …).
    Error,
}

/// Grade whether the ACME challenge port (80) and HTTPS port (443) are
/// reachable on the configured domain (pure; injectable for tests).
///
/// HTTP-01 validation requires the CA to reach `:80`, so a refused/timed-out
/// `:80` is a **Fail**. `:443` merely being down yet is a **Warn** (the listener
/// may not be up during preflight).
///
/// Under DNS-01 the CA never connects to `:80` — domain control is proved by a
/// TXT record — so an unreachable `:80` costs only the HTTP→HTTPS redirect for
/// visitors who type `http://`. Grading it a **Fail** would make `doctor
/// --online --strict` reject a perfectly correct wildcard deployment on a host
/// that deliberately exposes only `:443`.
#[must_use]
pub fn check_acme_ports_for_challenge(
    domain: &str,
    port_80: PortReachability,
    port_443: PortReachability,
    dns01: bool,
) -> CheckResult {
    match port_80 {
        PortReachability::Refused | PortReachability::TimedOut | PortReachability::Error => {
            return if dns01 {
                CheckResult {
                    name: "acme_ports",
                    status: CheckStatus::Warn,
                    detail: Some(format!(
                        "port 80 on {domain} is not reachable ({port_80:?}); DNS-01 issuance does \
                         not need it, but visitors who type http:// will not be redirected to \
                         HTTPS"
                    )),
                    hint: Some(
                        "Optional under DNS-01: open inbound TCP/80 only if you want the \
                         HTTP→HTTPS redirect",
                    ),
                }
            } else {
                CheckResult {
                    name: "acme_ports",
                    status: CheckStatus::Fail,
                    detail: Some(format!(
                        "port 80 on {domain} is not reachable ({port_80:?}); the ACME CA validates \
                         HTTP-01 over port 80"
                    )),
                    hint: Some(
                        "Open inbound TCP/80 to this host (or forward it) so Let's Encrypt can \
                         reach the HTTP-01 challenge",
                    ),
                }
            };
        }
        PortReachability::Open => {}
    }
    match port_443 {
        PortReachability::Open => CheckResult {
            name: "acme_ports",
            status: CheckStatus::Pass,
            detail: Some(format!("ports 80 and 443 on {domain} are reachable")),
            hint: None,
        },
        other => CheckResult {
            name: "acme_ports",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "port 80 on {domain} is reachable but port 443 is not yet ({other:?})"
            )),
            hint: Some("Ensure the HTTPS listener is up and inbound TCP/443 is open"),
        },
    }
}

/// The DNS resolution outcome for a domain, relative to this host's public IPs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsPointsHere {
    /// Every resolved address is one of this host's local public IPs.
    Matches,
    /// Some resolved addresses point here, but at least one does NOT. HTTP-01's
    /// challenge-token map is per-process, so if the CA validates against a
    /// resolved address that is not this host it 404s and issuance fails — a
    /// partial match is therefore not a clean pass. Graded a Warn (not a hard
    /// Fail) because `local_ips()` is best-effort and usually discovers only the
    /// single outbound source IP, so a legitimately multi-homed / multi-A host
    /// can land here; the unmatched addresses are named so the operator can act.
    PartialMatch {
        /// The resolved addresses that are NOT known local public IPs.
        unmatched: Vec<String>,
    },
    /// The domain resolves, but to none of this host's IPs.
    ResolvesElsewhere {
        /// The resolved addresses (for the message).
        resolved: Vec<String>,
    },
    /// The domain could not be resolved.
    Unresolved,
    /// This host's own public IPs could not be determined.
    LocalIpsUnknown,
}

/// Grade whether an ACME domain's DNS points at this host (pure; injectable).
///
/// Under HTTP-01 a clear mismatch is a **Fail** (the CA will hit the wrong
/// host); an indeterminate result (can't resolve, or can't tell where "here"
/// is) is a **Warn** rather than a hard failure, since split-horizon DNS and NAT
/// make "points here" unknowable from inside the host.
///
/// `dns01` softens all of that (issue #1620). "Does this domain resolve to THIS
/// host" is an HTTP-01 question: the CA
/// fetches the challenge token over `:80` from whatever the name resolves to, so
/// a mismatch means issuance hits the wrong server. Under DNS-01 the CA never
/// connects to this host at all — it reads a TXT record — so pointing the domain
/// at a load balancer, a CDN edge, or any other front end is not merely allowed,
/// it is the normal shape of the deployment this feature exists for. Grading it
/// a **Fail** would make `doctor --online --strict` reject a correct wildcard
/// deployment.
///
/// The check still runs, because where the name points is worth *reporting* —
/// it is just not a failure. Under DNS-01 every inconclusive-or-elsewhere
/// outcome is graded **Pass** with the addresses named.
#[must_use]
pub fn check_acme_dns_for_challenge(
    domain: &str,
    outcome: &DnsPointsHere,
    dns01: bool,
) -> CheckResult {
    if dns01 {
        return match outcome {
            DnsPointsHere::Unresolved => CheckResult {
                name: "acme_dns",
                status: CheckStatus::Warn,
                detail: Some(format!(
                    "{domain} did not resolve to any address; DNS-01 issuance does not need it \
                     to, but visitors will not reach the app until it does"
                )),
                hint: Some(
                    "Publish A/AAAA records for the domain (and a wildcard record for tenant \
                     subdomains) so traffic reaches this deployment",
                ),
            },
            DnsPointsHere::Matches => CheckResult {
                name: "acme_dns",
                status: CheckStatus::Pass,
                detail: Some(format!("{domain} resolves to this host")),
                hint: None,
            },
            _ => CheckResult {
                name: "acme_dns",
                status: CheckStatus::Pass,
                detail: Some(format!(
                    "{domain} resolves somewhere other than this host, which DNS-01 does not \
                     care about: the CA reads a TXT record rather than connecting here"
                )),
                hint: None,
            },
        };
    }
    match outcome {
        DnsPointsHere::Matches => CheckResult {
            name: "acme_dns",
            status: CheckStatus::Pass,
            detail: Some(format!("{domain} resolves to this host")),
            hint: None,
        },
        DnsPointsHere::PartialMatch { unmatched } => CheckResult {
            name: "acme_dns",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "{domain} resolves to this host but also to {} which {} not one of this host's \
                 addresses; HTTP-01's challenge token is served only by this process, so the ACME \
                 CA validating against an unmatched address would 404",
                unmatched.join(", "),
                if unmatched.len() == 1 { "is" } else { "are" }
            )),
            hint: Some(
                "Point every A/AAAA record for the domain at this host (or remove the extra \
                 addresses); a stray record can send the CA's HTTP-01 probe to a different server",
            ),
        },
        DnsPointsHere::ResolvesElsewhere { resolved } => CheckResult {
            name: "acme_dns",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "{domain} resolves to {} which is not one of this host's addresses; HTTP-01 \
                 validation will reach a different server",
                resolved.join(", ")
            )),
            hint: Some(
                "Point the domain's A/AAAA records at this host, or run ACME on the host the \
                 domain resolves to",
            ),
        },
        DnsPointsHere::Unresolved => CheckResult {
            name: "acme_dns",
            status: CheckStatus::Warn,
            detail: Some(format!("{domain} did not resolve to any address")),
            hint: Some("Publish A/AAAA records for the domain before requesting a certificate"),
        },
        DnsPointsHere::LocalIpsUnknown => CheckResult {
            name: "acme_dns",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "could not determine this host's public IPs, so cannot confirm {domain} points \
                 here"
            )),
            hint: None,
        },
    }
}

// ── DNS-01 / wildcard preflight checks (issue #1620) ─────────────────────────
//
// DNS-01 introduces exactly three failure classes HTTP-01 does not have, and
// each one costs an operator a failed issuance to discover the hard way:
// a credential the app cannot read, a zone whose `_acme-challenge` name public
// DNS cannot answer for, and a certificate that does not actually cover the
// tenant subdomains `tenancy.base_domain` will serve. Each is graded by a pure
// function of injected inputs, exactly like the #1608 ACME graders above.

/// What the DNS provider credential lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsCredentialOutcome {
    /// No `[server.tls.acme.dns]` section — HTTP-01 issuance, nothing to grade.
    NotConfigured,
    /// The section is present but does not deserialize (an unknown provider, or
    /// an inline secret the runtime refuses). Carries the rendered error.
    Malformed(String),
    /// The credential carries everything the provider needs.
    Usable {
        /// The provider name, for the message.
        provider: String,
        /// The credentials-store key it was read from.
        key: String,
    },
    /// The credential is missing or incomplete. Carries the runtime's own
    /// message, which names the missing field and where to put it.
    Unusable(String),
}

/// Grade the DNS-01 provider credential (pure; injectable).
///
/// A **Fail**: without a usable credential the app cannot write a single
/// challenge record, so every issuance and every renewal fails — and, unlike a
/// wrong hostname, nothing about the running app reveals it until the
/// certificate is already expiring.
#[must_use]
pub fn check_acme_dns_credential_impl(outcome: &DnsCredentialOutcome) -> CheckResult {
    match outcome {
        DnsCredentialOutcome::NotConfigured => CheckResult {
            name: "acme_dns_credential",
            status: CheckStatus::Pass,
            detail: Some(
                "no [server.tls.acme.dns] section: issuance uses HTTP-01, which needs no DNS \
                 provider credential"
                    .to_owned(),
            ),
            hint: None,
        },
        DnsCredentialOutcome::Malformed(error) => CheckResult {
            name: "acme_dns_credential",
            status: CheckStatus::Fail,
            detail: Some(format!("[server.tls.acme.dns] does not load: {error}")),
            hint: Some(
                "Fix the section: `provider` must be one of cloudflare/route53/exec, and API \
                 tokens belong in the encrypted credentials store (`autumn credentials edit`) or \
                 an AUTUMN_ACME_DNS_* environment variable — never in autumn.toml",
            ),
        },
        DnsCredentialOutcome::Usable { provider, key } => CheckResult {
            name: "acme_dns_credential",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "the {provider} DNS-01 credential `{key}` carries the fields it needs"
            )),
            hint: None,
        },
        DnsCredentialOutcome::Unusable(message) => CheckResult {
            name: "acme_dns_credential",
            status: CheckStatus::Fail,
            detail: Some(message.clone()),
            hint: Some(
                "Without it no _acme-challenge record can be written, so every issuance and \
                 renewal fails. Run `autumn credentials edit` to add the credential",
            ),
        },
    }
}

/// Read the DNS provider credential the way the runtime does and grade it.
///
/// Reads the encrypted credentials store for `profile`, overlaid with the
/// documented `AUTUMN_ACME_DNS_*` environment variables — the same resolution
/// order `build_dns_challenge` uses at boot, so a Pass here means the app will
/// find the same credential.
#[cfg(feature = "tls")]
#[must_use]
pub fn resolve_acme_dns_credential(
    config: &AcmeDoctorConfig,
    profile: &str,
    base_dir: &std::path::Path,
) -> DnsCredentialOutcome {
    use autumn_web::acme::dns::{DnsCredential, process_env, validate_credential};

    if let Some(error) = &config.dns_error {
        return DnsCredentialOutcome::Malformed(error.clone());
    }
    let Some(dns) = config.dns.as_ref() else {
        return DnsCredentialOutcome::NotConfigured;
    };
    let store = autumn_web::credentials::load_credentials(profile, base_dir).unwrap_or_default();
    let credential = DnsCredential::resolve(dns, &store, &process_env);
    let key = dns.credential.trim().to_owned();
    match validate_credential(dns.provider, &key, &credential) {
        Ok(()) => DnsCredentialOutcome::Usable {
            provider: dns.provider.as_str().to_owned(),
            key,
        },
        Err(message) => DnsCredentialOutcome::Unusable(message),
    }
}

/// What public DNS said about a zone's `_acme-challenge` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeDnsVisibility {
    /// Public DNS answers authoritatively for the name (whether or not a record
    /// is currently published there).
    Answered {
        /// How many TXT values are currently visible — normally `0` between
        /// issuances.
        values: usize,
    },
    /// A record is still published from an earlier run.
    Stale {
        /// How many leftover TXT values are visible.
        values: usize,
    },
    /// The resolver could not answer for the name.
    Unanswerable(String),
}

/// Grade whether a DNS-01 challenge TXT record would be visible in public DNS
/// (pure; injectable).
///
/// A name public DNS cannot answer for is a **Fail**: the CA queries public DNS,
/// so a broken or missing delegation means every DNS-01 validation fails no
/// matter how correctly the record is written. Leftover records are a **Warn** —
/// harmless for issuance, but the fingerprint of an earlier run that died before
/// cleanup.
#[must_use]
pub fn check_acme_dns_propagation_impl(
    fqdn: &str,
    visibility: &ChallengeDnsVisibility,
) -> CheckResult {
    match visibility {
        ChallengeDnsVisibility::Answered { .. } => CheckResult {
            name: "acme_dns_propagation",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "public DNS answers for {fqdn}, so a published DNS-01 challenge record will be \
                 visible to the CA"
            )),
            hint: None,
        },
        ChallengeDnsVisibility::Stale { values } => CheckResult {
            name: "acme_dns_propagation",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "{values} leftover TXT record(s) are still published at {fqdn}; an earlier \
                 issuance did not clean up"
            )),
            hint: Some(
                "Harmless for issuance, but worth removing: they are the fingerprint of an order \
                 that failed or was interrupted before cleanup",
            ),
        },
        ChallengeDnsVisibility::Unanswerable(reason) => CheckResult {
            name: "acme_dns_propagation",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "public DNS could not answer for {fqdn}: {reason}. The ACME CA reads the DNS-01 \
                 challenge record from public DNS, so no wildcard certificate can be issued while \
                 this name is unanswerable"
            )),
            hint: Some(
                "Check that the zone's NS delegation is live and that its nameservers answer for \
                 the _acme-challenge name (a `dig TXT _acme-challenge.<domain>` from off-network \
                 should return NOERROR or NXDOMAIN, never SERVFAIL)",
            ),
        },
    }
}

/// Query the configured resolvers for a zone's `_acme-challenge` name.
///
/// Bounded and only run under `--online`, like the other active ACME probes. A
/// name that answers on ANY configured resolver is answerable; only a name no
/// resolver can answer for is graded a failure, so one flaky resolver does not
/// fail the check.
#[cfg(feature = "tls")]
#[must_use]
pub fn resolve_challenge_dns_visibility(
    fqdn: &str,
    resolvers: &[std::net::SocketAddr],
) -> ChallengeDnsVisibility {
    use autumn_web::acme::dns::resolver::lookup_txt_blocking;

    let mut last_error = "no resolvers were configured".to_owned();
    for resolver in resolvers {
        match lookup_txt_blocking(*resolver, fqdn, std::time::Duration::from_secs(3)) {
            Ok(answer) if answer.values.is_empty() => {
                return ChallengeDnsVisibility::Answered { values: 0 };
            }
            Ok(answer) => {
                return ChallengeDnsVisibility::Stale {
                    values: answer.values.len(),
                };
            }
            Err(e) => last_error = e,
        }
    }
    ChallengeDnsVisibility::Unanswerable(last_error)
}

/// Grade whether the configured certificate covers `tenancy.base_domain`
/// (pure; injectable).
///
/// A **Fail** when it does not: subdomain-per-tenant routing resolves a tenant
/// from the Host header, so every tenant subdomain would serve a certificate
/// name mismatch — the browser error that looks like the whole product is
/// broken. This is the check that catches `base_domain = "myapp.com"` with
/// `domains = ["myapp.com"]` and no wildcard.
#[must_use]
pub fn check_acme_tenancy_domain_impl(
    base_domain: Option<&str>,
    domains: &[String],
    covers_subdomains: bool,
) -> CheckResult {
    let Some(base) = base_domain.map(str::trim).filter(|b| !b.is_empty()) else {
        return CheckResult {
            name: "acme_tenancy_domain",
            status: CheckStatus::Pass,
            detail: Some(
                "no [tenancy] base_domain configured, so no tenant subdomains need certificate \
                 coverage"
                    .to_owned(),
            ),
            hint: None,
        };
    };
    if covers_subdomains {
        return CheckResult {
            name: "acme_tenancy_domain",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "the configured certificate covers every subdomain of the [tenancy] base_domain \
                 {base}"
            )),
            hint: None,
        };
    }
    CheckResult {
        name: "acme_tenancy_domain",
        status: CheckStatus::Fail,
        detail: Some(format!(
            "[tenancy] base_domain is {base}, but [server.tls.acme] domains ({}) do not cover its \
             subdomains: every tenant host would serve a certificate name mismatch",
            domains.join(", ")
        )),
        hint: Some(
            "Add the wildcard `*.<base_domain>` to [server.tls.acme] domains and configure \
             [server.tls.acme.dns] — a wildcard can only be issued over DNS-01",
        ),
    }
}

/// Whether `domains` covers subdomains of `base_domain`, using the runtime's own
/// RFC 6125 matcher against a probe host no explicit SAN could plausibly name.
///
/// A certificate "covers tenant subdomains" only if an ARBITRARY one matches, so
/// the probe is a random-looking label: an explicit `tenant1.myapp.com` SAN must
/// not read as coverage for tenant 2.
#[must_use]
pub fn acme_covers_tenant_subdomains(base_domain: &str, domains: &[String]) -> bool {
    let base = base_domain.trim().trim_end_matches('.');
    if base.is_empty() {
        return false;
    }
    let probe = format!("autumn-doctor-probe-tenant.{base}");
    domains
        .iter()
        .any(|san| autumn_web::config::san_covers_host(san, &probe))
}

/// Thin bounded I/O wrapper: probe a TCP port on `domain` with a 2s connect
/// timeout, resolving the domain first.
#[must_use]
pub fn probe_port(domain: &str, port: u16) -> PortReachability {
    use std::net::ToSocketAddrs as _;
    let addrs = match (domain, port).to_socket_addrs() {
        Ok(addrs) => addrs.collect::<Vec<_>>(),
        Err(_) => return PortReachability::Error,
    };
    let Some(addr) = addrs.into_iter().next() else {
        return PortReachability::Error;
    };
    match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)) {
        Ok(_) => PortReachability::Open,
        Err(e) => match e.kind() {
            std::io::ErrorKind::ConnectionRefused => PortReachability::Refused,
            std::io::ErrorKind::TimedOut => PortReachability::TimedOut,
            _ => PortReachability::Error,
        },
    }
}

/// Thin bounded I/O wrapper: resolve `domain` and compare its addresses to this
/// host's local IPs via the pure [`grade_dns_points_here`] grader.
#[must_use]
pub fn resolve_dns_points_here(domain: &str) -> DnsPointsHere {
    use std::net::ToSocketAddrs as _;
    let resolved: Vec<std::net::IpAddr> = match (domain, 0_u16).to_socket_addrs() {
        Ok(addrs) => addrs.map(|s| s.ip()).collect(),
        Err(_) => return DnsPointsHere::Unresolved,
    };
    grade_dns_points_here(&resolved, &local_ips())
}

/// Grade a domain's resolved addresses against this host's local IPs (pure;
/// injectable for tests).
///
/// Only *public* local IPs can prove "points here": in NAT/container deployments
/// the discovered local source IP is private (RFC1918), CGNAT (100.64/10), or
/// link/unique-local while the ACME domain resolves to a public LB/elastic IP, so
/// comparing a private local IP against a public DNS result would wrongly report
/// [`DnsPointsHere::ResolvesElsewhere`] and fail `doctor --online --strict` even
/// though HTTP-01 routes fine through NAT. When no public local IP is known the
/// result is [`DnsPointsHere::LocalIpsUnknown`] (inconclusive Warn), never a hard
/// mismatch.
#[must_use]
fn grade_dns_points_here(
    resolved: &[std::net::IpAddr],
    local_ips: &[std::net::IpAddr],
) -> DnsPointsHere {
    if resolved.is_empty() {
        return DnsPointsHere::Unresolved;
    }
    let local_public: Vec<std::net::IpAddr> = local_ips
        .iter()
        .copied()
        .filter(|ip| is_public_ip(*ip))
        .collect();
    if local_public.is_empty() {
        return DnsPointsHere::LocalIpsUnknown;
    }
    // Every resolved address the CA might pick must point here — a partial match
    // still lets the CA hit an address this process does not serve the HTTP-01
    // token on (a 404). Split the resolved set into addresses that ARE a known
    // local public IP and those that are not, and grade on the unmatched set
    // rather than passing as soon as one address matches.
    let unmatched: Vec<std::net::IpAddr> = resolved
        .iter()
        .copied()
        .filter(|ip| !local_public.contains(ip))
        .collect();
    if unmatched.is_empty() {
        DnsPointsHere::Matches
    } else if unmatched.len() == resolved.len() {
        DnsPointsHere::ResolvesElsewhere {
            resolved: resolved.iter().map(ToString::to_string).collect(),
        }
    } else {
        DnsPointsHere::PartialMatch {
            unmatched: unmatched.iter().map(ToString::to_string).collect(),
        }
    }
}

/// Whether `ip` is a globally-routable address a public ACME peer could see as
/// "this host". Filters out loopback, unspecified, RFC1918 private, CGNAT
/// (`100.64/10`), link-local (`169.254/16`, `fe80::/10`), and IPv6 unique-local
/// (`fc00::/7`) ranges — a source IP in any of those is a NAT/container-internal
/// address, not a public identity, so it cannot confirm or deny where the domain
/// points.
const fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || is_cgnat_v4(v4))
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || is_link_local_v6(v6)
                || is_unique_local_v6(v6))
        }
    }
}

/// CGNAT shared address space, `100.64.0.0/10` (RFC 6598): first octet `100`,
/// second octet `64..=127`.
const fn is_cgnat_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

/// IPv6 link-local unicast, `fe80::/10`.
const fn is_link_local_v6(v6: std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// IPv6 unique-local addresses, `fc00::/7`.
const fn is_unique_local_v6(v6: std::net::Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// This host's local IP addresses, best-effort.
///
/// Uses a UDP "connect" trick (no packets sent) to discover the outbound source
/// address, which is the address a public peer would see for a simple setup.
/// Returns empty when it cannot be determined (NAT, no route). The pure
/// [`grade_dns_points_here`] grader filters these to *public* addresses and
/// treats an empty public set as "unknown" rather than a failure.
fn local_ips() -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    for target in ["8.8.8.8:80", "[2001:4860:4860::8888]:80"] {
        if let Ok(sock) = std::net::UdpSocket::bind(if target.starts_with('[') {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        }) && sock.connect(target).is_ok()
            && let Ok(addr) = sock.local_addr()
        {
            let ip = addr.ip();
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// Check that an operator-alert destination is configured (issue #1610).
///
/// Operator alerts (dead-lettered jobs, Down health indicators, 5xx-rate
/// spikes, scheduled-task failures) are only delivered when an `[alerts]`
/// destination — an operator `email` and/or a `webhook_url` — is configured.
///
/// An `email` destination only counts when a *usable* `[mail] transport` can
/// actually send: the runtime (`alerts::install_from_config`) refuses to
/// register a `MailAlertChannel` when `mailer.is_disabled()` (the disabled
/// transport, which is the default outside dev), so an email paired with a
/// disabled transport delivers nothing. `mail_transport_usable` mirrors that —
/// it is `false` when the effective merged `[mail] transport` resolves to
/// `disabled`. The webhook path is independent of the mailer.
///
/// An `email` destination ALSO needs a mail `from` address for transports that
/// build a real RFC 5322 message: the runtime's alert mail
/// (`MailAlertChannel`) sets no per-message `from`, so it falls back to the
/// mailer default (`[mail] from`). Under `transport = "smtp"` with no `[mail]
/// from`, delivery fails in `lettre_message` with "mail from address is
/// required" — so an SMTP email with no `from` is not a working destination.
/// `transport_requires_from` mirrors which transports actually enforce this:
/// only `smtp` (the `log` and `file` transports render `from` only when present
/// and deliver without it; `disabled` is already not usable). `mail_from` is the
/// effective merged `[mail] from` (empty string when unset); for SMTP it must be
/// both non-empty AND a valid lettre mailbox — the runtime parses it at boot
/// (`MailerBuilder::build`) and refuses to start on an unparsable sender, so a
/// present-but-invalid value is not a working destination.
///
/// The `[alerts] enabled` master switch is honoured up front: when alerting is
/// disabled the runtime (`alerts::install_from_config`) returns BEFORE
/// installing any `Alerter`, so NO operator alerts are delivered regardless of a
/// configured destination. In production that earns a dedicated warning (a
/// deploy running blind), distinct from the "no destination" one; outside
/// production it stays lenient like the rest of this check.
///
/// - **Production** (`is_production = true`): warns when alerting is disabled, or
///   when no usable destination is configured, so a deploy does not run silently
///   blind to its own failures. An email whose mail transport is disabled — or
///   whose SMTP transport has no `[mail] from` — gets a dedicated, clearer
///   warning rather than the generic "no destination" one.
/// - **Dev/test**: passes quietly (alerts are optional locally).
///
/// The returned check name is `"alert_destination"`.
/// Dedicated production warning for an email destination on an SMTP transport
/// with no `[mail] from`: the runtime's alert mail carries no per-message
/// `from`, so `lettre_message` fails with "mail from address is required" and
/// nothing is delivered. Distinct from the disabled-transport and no-destination
/// warnings. Extracted so [`check_alert_destination_impl`] stays under the
/// line-count lint.
fn alert_email_from_missing_warning() -> CheckResult {
    CheckResult {
        name: "alert_destination",
        status: CheckStatus::Warn,
        detail: Some(
            "an operator alert email is configured but `[mail] from` is not set, so SMTP \
             delivery will fail; the runtime's alert mail carries no per-message from and \
             the mailer has no default from address to fall back to"
                .into(),
        ),
        hint: Some(
            "Set [mail] from (or AUTUMN_MAIL__FROM) to a sender address like \
             alerts@example.com, or configure a signed webhook ([alerts] webhook_url + \
             webhook_secret). See docs/guide/operator-alerts.md",
        ),
    }
}

/// Dedicated production warning for a present-but-syntactically-invalid `[mail]
/// from` on an SMTP transport: the runtime parses `[mail] from` at boot
/// (`MailerBuilder::build` runs `parse_mailbox`), so an unparsable value like
/// `not-a-mailbox` aborts startup outright — and even before that the alert mail
/// (which carries no per-message `from` and falls back to this default) would
/// fail EVERY send with an invalid-address error. Distinct from the
/// missing-`from`, disabled-transport, invalid-email, and no-destination
/// warnings, and it names the offending value. Validity is checked with the SAME
/// lettre `Mailbox` parser the runtime uses (`is_valid_alert_mailbox_doctor`).
/// Extracted so [`check_alert_destination_impl`] stays under the line-count lint.
fn alert_email_from_invalid_warning(from: &str) -> CheckResult {
    CheckResult {
        name: "alert_destination",
        status: CheckStatus::Warn,
        detail: Some(format!(
            "`[mail] from` (\"{from}\") is not a valid email address, so SMTP alert \
             delivery will fail; the runtime parses it at boot and refuses to start on \
             an unparsable sender, and the alert mail has no other `from` to fall back to"
        )),
        hint: Some(
            "Set [mail] from (or AUTUMN_MAIL__FROM) to a valid sender address like \
             alerts@example.com, or configure a signed webhook ([alerts] webhook_url + \
             webhook_secret). See docs/guide/operator-alerts.md",
        ),
    }
}

/// Dedicated production warning for a present-but-syntactically-invalid
/// `[alerts] email`: lettre parses the recipient only at send time (in
/// `lettre_message`, not at `Mail::builder().to(...)`), so an unparsable
/// address like `not-an-address` passes every earlier gate yet fails EVERY
/// alert delivery at runtime with `MailError::InvalidAddress`. Distinct from
/// the disabled-transport, missing-`from`, and no-destination warnings.
fn alert_email_invalid_warning(email: &str) -> CheckResult {
    CheckResult {
        name: "alert_destination",
        status: CheckStatus::Warn,
        detail: Some(format!(
            "`[alerts] email` (\"{email}\") is not a valid email address, so alert delivery \
             will fail; the runtime parses the recipient only when sending and rejects it \
             with an invalid-address error, delivering nothing"
        )),
        hint: Some(
            "Set [alerts] email (or AUTUMN_ALERTS__EMAIL) to a valid address like \
             alerts@example.com, or configure a signed webhook ([alerts] webhook_url + \
             webhook_secret). See docs/guide/operator-alerts.md",
        ),
    }
}

/// Dedicated production warning for a present-but-non-absolute `[alerts]
/// webhook_url`: the runtime's HTTP client (`http_client::Client::build_request`)
/// dispatches a URL verbatim only when it starts with `http://` or `https://`,
/// otherwise treating it as a path to join onto a base-url alias the alert
/// webhook never has — so a relative value like `hooks.example/x` fails EVERY
/// signed POST. Distinct from the missing-secret, invalid-email, and
/// no-destination warnings, and it names the offending value.
fn alert_webhook_url_invalid_warning(url: &str) -> CheckResult {
    CheckResult {
        name: "alert_destination",
        status: CheckStatus::Warn,
        detail: Some(format!(
            "`[alerts] webhook_url` (\"{url}\") is not an absolute http(s) URL, so alert \
             delivery will fail; the runtime's HTTP client only sends a URL starting with \
             http:// or https://, and a relative value has no base URL to resolve against, so \
             every signed webhook POST fails to send"
        )),
        hint: Some(
            "Set [alerts] webhook_url (or AUTUMN_ALERTS__WEBHOOK_URL) to an absolute URL like \
             https://alerts.example.com/hooks/autumn, or configure an email destination \
             ([alerts] email). See docs/guide/operator-alerts.md",
        ),
    }
}

/// Resolve the production result when no `[alerts]` email/webhook destination is
/// configured and no earlier value-validation warning fired.
///
/// When `custom_channel` is true the operator has DECLARED, via `[alerts]
/// custom_channel = true` (or `AUTUMN_ALERTS__CUSTOM_CHANNEL`), that they register
/// an alert channel in code with `AppBuilder::with_alert_channel`. The runtime
/// installs code-registered channels regardless, so the destination is treated as
/// present and the check passes — the sanctioned way to satisfy `autumn doctor
/// --strict` for a code-only alert setup. Otherwise there is genuinely nowhere to
/// deliver, so it warns and points at the flag. This runs AFTER every
/// value-validation warning, so `custom_channel` only suppresses the pure "no
/// destination configured" fall-through, never a broken *configured* destination.
/// Extracted so [`check_alert_destination_impl`] stays under the line-count lint.
fn alert_no_destination_in_production(custom_channel: bool) -> CheckResult {
    if custom_channel {
        return CheckResult {
            name: "alert_destination",
            status: CheckStatus::Pass,
            detail: Some(
                "no [alerts] email/webhook destination configured, but [alerts] \
                 custom_channel = true declares a code-registered alert channel \
                 (AppBuilder::with_alert_channel), which the runtime installs regardless"
                    .into(),
            ),
            hint: None,
        };
    }
    CheckResult {
        name: "alert_destination",
        status: CheckStatus::Warn,
        detail: Some(
            "no operator alert destination found in [alerts] config (email or webhook) in \
             production; if your app registers an alert channel in code via \
             AppBuilder::with_alert_channel, set [alerts] custom_channel = true (or \
             AUTUMN_ALERTS__CUSTOM_CHANNEL=true) to declare that and silence this warning \
             (doctor is a config-only checker and cannot see code-registered channels); \
             otherwise failure conditions (dead-lettered jobs, Down health indicators, 5xx \
             spikes, scheduled-task failures) will not be delivered anywhere"
                .into(),
        ),
        hint: Some(
            "Set [alerts] email and/or webhook_url (or AUTUMN_ALERTS__EMAIL / \
             AUTUMN_ALERTS__WEBHOOK_URL); or if you register a channel in code, set [alerts] \
             custom_channel = true. See docs/guide/operator-alerts.md",
        ),
    }
}

/// Dedicated production warning for the `[alerts] enabled = false` master switch:
/// the runtime (`alerts::install_from_config`) returns BEFORE installing any
/// `Alerter`, so NO operator alerts are delivered regardless of a configured
/// destination. Mentions the (otherwise valid) destination when one is present so
/// the operator understands the deploy is blind despite it. Extracted so
/// [`check_alert_destination_impl`] stays under the line-count lint.
fn alert_disabled_in_production_warning(has_destination: bool) -> CheckResult {
    let detail = if has_destination {
        "alerting is disabled ([alerts] enabled = false) in production, so no operator \
         alerts will be delivered — even though a destination is configured"
    } else {
        "alerting is disabled ([alerts] enabled = false) in production, so no operator \
         alerts will be delivered"
    };
    CheckResult {
        name: "alert_destination",
        status: CheckStatus::Warn,
        detail: Some(detail.into()),
        hint: Some(
            "Set [alerts] enabled = true (or AUTUMN_ALERTS__ENABLED=true) to deliver \
             operator alerts, or remove the setting (it defaults to enabled). See \
             docs/guide/operator-alerts.md",
        ),
    }
}

// The independent config booleans — alerting enabled, webhook usable as a signed
// destination, mail transport usable, production, transport requires a `from`, a
// native transport configured — plus the resolved `[alerts] email`, `[alerts]
// webhook_url`, and `[mail] from` strings each gate a distinct branch; grouping them
// into enums would obscure the destination-resolution logic. The raw `webhook_url`
// only names a present-but-non-absolute value in its own warning, since
// `webhook_configured` already folds in absoluteness and the signing secret;
// `mail_from` likewise names a present-but-invalid sender. A native transport — a
// PagerDuty routing key, or a Slack or Discord webhook URL — counts as a destination
// exactly as the runtime's `AlertConfig::has_destination` does, and
// `check_alert_transports_impl` validates its delivery-worthiness separately.
#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
pub fn check_alert_destination_impl(
    alerts_enabled: bool,
    email: &str,
    webhook_configured: bool,
    webhook_url: &str,
    mail_transport_usable: bool,
    is_production: bool,
    mail_from: &str,
    transport_requires_from: bool,
    custom_channel: bool,
    native_transport_configured: bool,
) -> CheckResult {
    // Presence and syntactic validity of the resolved `[alerts] email`. lettre parses
    // the recipient only at send time (`lettre_message`), not when the alert `Mail` is
    // built, so a present-but-unparsable address such as `not-an-address` passes every
    // earlier gate and then fails every delivery with `MailError::InvalidAddress`. An
    // invalid address therefore is not a usable destination. Validity is checked with
    // the same lettre parser the runtime uses: `is_valid_alert_mailbox_doctor` runs
    // `value.parse::<lettre::message::Mailbox>()`, exact parity with
    // `autumn/src/alerts.rs`. Deliberately not `is_valid_mailto_address_doctor`, which
    // strips a leading `mailto:` — correct for `List-Unsubscribe`, wrong here. This
    // accepts everything lettre accepts, including RFC 5322 display-name forms like
    // `Ops <ops@example.com>`, and rejects what lettre rejects: the runtime hands
    // `[alerts] email` verbatim to `Mail::builder().to(...)`, so a `mailto:` URI would
    // fail every delivery and doctor must reject it too.
    let email_trimmed = email.trim();
    let email_configured = !email_trimmed.is_empty();
    let email_valid = email_configured && is_valid_alert_mailbox_doctor(email_trimmed);

    // Presence and syntactic validity of the resolved `[mail] from`. For
    // transports that build a real RFC 5322 message (SMTP), a `from` must be both
    // present AND a valid lettre mailbox: the runtime parses it at boot
    // (`MailerBuilder::build` runs `parse_mailbox`), so an unparsable value like
    // `not-a-mailbox` aborts startup and the alert mail — which sets no
    // per-message `from` — has no valid default to fall back to. Checked with the
    // SAME lettre parser the recipient uses (`is_valid_alert_mailbox_doctor`),
    // exact parity with `autumn/src/mail.rs`'s `parse_mailbox`.
    let mail_from_trimmed = mail_from.trim();
    let mail_from_present = !mail_from_trimmed.is_empty();
    let mail_from_valid = mail_from_present && is_valid_alert_mailbox_doctor(mail_from_trimmed);

    // Whether a configured email would actually deliver. It needs a valid
    // address, a usable (non-disabled) transport AND — for transports that build
    // a real RFC 5322 message (SMTP) — a present, VALID `[mail] from`, because the
    // runtime's alert mail carries no per-message `from` and falls back to the
    // mailer default. `log`/`file` deliver without a `from`, so they do not need one.
    let email_from_ok = mail_from_valid || !transport_requires_from;
    let email_destination = email_valid && mail_transport_usable && email_from_ok;

    // Master off switch: when `[alerts] enabled = false`, the runtime installs no
    // alerter at all, so a configured destination is inert. Handle this BEFORE
    // the destination gate — doctor must not pass on the strength of a
    // configured-but-inert destination.
    if !alerts_enabled {
        if is_production {
            // Name the disabled switch explicitly. Mention the (otherwise valid)
            // destination when one is configured so the operator understands the
            // deploy is blind despite it; keep the message accurate otherwise.
            return alert_disabled_in_production_warning(
                email_destination || webhook_configured || native_transport_configured,
            );
        }
        return CheckResult {
            name: "alert_destination",
            status: CheckStatus::Pass,
            detail: Some(
                "operator alerting is disabled ([alerts] enabled = false) (optional outside \
                 production)"
                    .into(),
            ),
            hint: None,
        };
    }
    // `email_destination` above already folds in the usable-transport and `from`
    // requirements, mirroring the runtime, which skips `MailAlertChannel` when
    // `mailer.is_disabled()` and whose SMTP send fails without a `from`. A native
    // transport (PagerDuty, Slack, Discord) counts as a destination exactly as the
    // runtime's `AlertConfig::has_destination` does — the runtime registers the
    // channel for a native-only config, so doctor must not warn "no destination".
    // `check_alert_transports_impl` validates the transports' own delivery-worthiness
    // (routing-key shape, absolute https URL) separately.
    if email_destination || webhook_configured || native_transport_configured {
        return CheckResult {
            name: "alert_destination",
            status: CheckStatus::Pass,
            detail: Some("operator alert destination configured".into()),
            hint: None,
        };
    }
    if is_production {
        // An email is set but is not a syntactically valid address: the runtime
        // parses the recipient only at send time, so every alert fails in
        // `lettre_message` with an invalid-address error and nothing is
        // delivered. Surface a dedicated warning naming the offending value,
        // distinct from the missing-`from`, disabled-transport, and
        // no-destination cases. Checked first because a malformed address is
        // unusable regardless of transport or `from`.
        if email_configured && !email_valid {
            return alert_email_invalid_warning(email_trimmed);
        }
        // An email is set on a usable transport that needs a `from` (SMTP), but
        // no `[mail] from` is configured: the runtime's alert mail has no
        // per-message `from`, so `lettre_message` fails with "mail from address
        // is required" and nothing is delivered. Surface a dedicated warning
        // distinct from both the disabled-transport and no-destination cases.
        if email_configured
            && mail_transport_usable
            && transport_requires_from
            && !mail_from_present
        {
            return alert_email_from_missing_warning();
        }
        // An email is set on a usable transport that needs a `from` (SMTP) and a
        // `[mail] from` IS set but is not a syntactically valid mailbox: the
        // runtime parses it at boot (`MailerBuilder::build`) and refuses to start,
        // so it never even reaches a send. Surface a dedicated warning naming the
        // offending value, distinct from the missing-`from` case (which reads as
        // "you set nothing" and would mislead here).
        if email_configured
            && mail_transport_usable
            && transport_requires_from
            && mail_from_present
            && !mail_from_valid
        {
            return alert_email_from_invalid_warning(mail_from_trimmed);
        }
        // An email is set but the mail transport is disabled: the runtime
        // installs no alert channel and delivers nothing. Surface a dedicated
        // warning that points at the real cause instead of the generic
        // "no destination" message (which would read as "you set nothing").
        if email_configured && !mail_transport_usable {
            return CheckResult {
                name: "alert_destination",
                status: CheckStatus::Warn,
                detail: Some(
                    "an operator alert email is configured but [mail] transport is disabled, so \
                     no email alerts will be delivered; the runtime installs no email alert \
                     channel when the mailer is disabled"
                        .into(),
                ),
                hint: Some(
                    "Set [mail] transport to a real backend (smtp/log/file) or configure a signed \
                     webhook ([alerts] webhook_url + webhook_secret). See \
                     docs/guide/operator-alerts.md",
                ),
            };
        }
        // A webhook URL is set but is not an absolute http(s) URL: the runtime's
        // HTTP client dispatches a URL verbatim only when it starts with
        // `http://`/`https://`, and an alert webhook has no base-url alias to join
        // a relative value onto — so every signed POST fails to send. Surface a
        // dedicated warning naming the offending value, distinct from the
        // missing-secret ("no destination") and email cases. `webhook_configured`
        // is already false here (it requires an absolute URL), so this only fires
        // when there is no other working destination.
        let webhook_url_trimmed = webhook_url.trim();
        if !webhook_url_trimmed.is_empty() && !is_absolute_http_url_doctor(webhook_url_trimmed) {
            return alert_webhook_url_invalid_warning(webhook_url_trimmed);
        }
        // No config destination resolved. Either the operator has DECLARED a
        // code-registered channel via `[alerts] custom_channel = true` (pass), or
        // there is genuinely nowhere to deliver (warn). Extracted so this function
        // stays under the line-count lint.
        alert_no_destination_in_production(custom_channel)
    } else {
        CheckResult {
            name: "alert_destination",
            status: CheckStatus::Pass,
            detail: Some(
                "no operator alert destination configured (optional outside production)".into(),
            ),
            hint: None,
        }
    }
}

/// Whether `url` is a non-empty ABSOLUTE `https` URL with a host — the form the
/// Slack and Discord webhook endpoints require. Slack and Discord only expose
/// `https` webhook endpoints, so a plaintext `http://` (or relative) value will
/// not deliver. Stricter than [`is_absolute_http_url_doctor`], which also accepts
/// `http://`.
fn is_absolute_https_url_doctor(url: &str) -> bool {
    url.starts_with("https://")
        && ::url::Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(|h| !h.is_empty()))
            .unwrap_or(false)
}

/// Check that configured native alert transports (`PagerDuty` / Slack / Discord,
/// issue #1630) carry the config values they need to deliver.
///
/// Mirrors the runtime `alerts::native_transport_channels`, which registers a
/// transport only when its required config is usable and otherwise skips it with
/// a warning:
/// - **`PagerDuty`** needs a non-blank `pagerduty_routing_key`; a
///   `pagerduty_routing_key` containing embedded whitespace is malformed (a
///   copy-paste error), and a set-but-non-absolute `pagerduty_url` override is
///   never dispatched by the SSRF-hardened HTTP client. A `pagerduty_url` set
///   with no routing key configures nothing.
/// - **Slack / Discord** need an absolute `https` webhook URL: the HTTP client
///   cannot dispatch a relative value, and both providers only expose `https`
///   endpoints, so a plaintext `http://` URL will not deliver.
///
/// Production warns on any of the above; dev/test passes quietly (these
/// transports are optional locally). Returns the first problem found so the
/// operator gets one actionable message. The returned check name is
/// `"alert_transports"`.
#[must_use]
pub fn check_alert_transports_impl(
    pagerduty_routing_key: &str,
    pagerduty_url: &str,
    slack_webhook_url: &str,
    discord_webhook_url: &str,
    is_production: bool,
) -> CheckResult {
    const NAME: &str = "alert_transports";
    let pd_key = pagerduty_routing_key.trim();
    let pd_url = pagerduty_url.trim();
    let slack = slack_webhook_url.trim();
    let discord = discord_webhook_url.trim();

    let pass = |detail: &str| CheckResult {
        name: NAME,
        status: CheckStatus::Pass,
        detail: Some(detail.to_owned()),
        hint: None,
    };
    let warn = |detail: String, hint: &'static str| CheckResult {
        name: NAME,
        status: CheckStatus::Warn,
        detail: Some(detail),
        hint: Some(hint),
    };

    let configured =
        !pd_key.is_empty() || !pd_url.is_empty() || !slack.is_empty() || !discord.is_empty();
    if !is_production {
        return pass(if configured {
            "native alert transports configured (validated in production mode)"
        } else {
            "no native alert transports configured (optional outside production)"
        });
    }

    // PagerDuty: routing key must be present and well-formed; the URL override,
    // when set, must be absolute; a URL with no routing key delivers nothing.
    if !pd_key.is_empty() && pd_key.contains(char::is_whitespace) {
        return warn(
            format!(
                "`[alerts] pagerduty_routing_key` (\"{pd_key}\") contains whitespace, so it is a \
                 malformed Events API v2 routing key and PagerDuty will reject the event"
            ),
            "Set [alerts] pagerduty_routing_key (or AUTUMN_ALERTS__PAGERDUTY_ROUTING_KEY) to your \
             integration's routing key with no surrounding or embedded whitespace. See \
             docs/guide/operator-alerts.md",
        );
    }
    if pd_key.is_empty() && !pd_url.is_empty() {
        return warn(
            "`[alerts] pagerduty_url` is set but no `pagerduty_routing_key` is configured, so no \
             PagerDuty events will be delivered"
                .to_owned(),
            "Set [alerts] pagerduty_routing_key (or AUTUMN_ALERTS__PAGERDUTY_ROUTING_KEY), or \
             remove pagerduty_url. See docs/guide/operator-alerts.md",
        );
    }
    if !pd_key.is_empty() && !pd_url.is_empty() && !is_absolute_http_url_doctor(pd_url) {
        return warn(
            format!(
                "`[alerts] pagerduty_url` (\"{pd_url}\") is not an absolute http(s) URL, so the \
                 SSRF-hardened HTTP client cannot dispatch it and no PagerDuty events will be \
                 delivered"
            ),
            "Set [alerts] pagerduty_url (or AUTUMN_ALERTS__PAGERDUTY_URL) to an absolute URL like \
             https://events.pagerduty.com/v2/enqueue, or remove it to use the default. See \
             docs/guide/operator-alerts.md",
        );
    }
    if !slack.is_empty() && !is_absolute_https_url_doctor(slack) {
        return warn(
            format!(
                "`[alerts] slack_webhook_url` (\"{slack}\") is not an absolute https URL, so no \
                 Slack alerts will be delivered; Slack only exposes https incoming-webhook \
                 endpoints and the HTTP client cannot dispatch a relative value"
            ),
            "Set [alerts] slack_webhook_url (or AUTUMN_ALERTS__SLACK_WEBHOOK_URL) to your Slack \
             incoming-webhook URL (https://hooks.slack.com/services/…). See \
             docs/guide/operator-alerts.md",
        );
    }
    if !discord.is_empty() && !is_absolute_https_url_doctor(discord) {
        return warn(
            format!(
                "`[alerts] discord_webhook_url` (\"{discord}\") is not an absolute https URL, so no \
                 Discord alerts will be delivered; use the Slack-compatible endpoint \
                 (https://discord.com/api/webhooks/…/slack)"
            ),
            "Set [alerts] discord_webhook_url (or AUTUMN_ALERTS__DISCORD_WEBHOOK_URL) to your \
             Discord webhook's Slack-compatible URL (append /slack). See \
             docs/guide/operator-alerts.md",
        );
    }

    pass(if configured {
        "configured native alert transports (PagerDuty/Slack/Discord) look deliverable"
    } else {
        "no native alert transports configured"
    })
}

/// Check that a configured `[alerts] error_rate_threshold` is a usable 5xx rate
/// (issue #1610).
///
/// The runtime compares the threshold against a rate computed as
/// `err_delta / req_delta` (`autumn/src/alerts.rs`'s `evaluate_error_rate`), i.e.
/// a **fraction in `[0, 1]`**, so the only meaningful thresholds are in `(0, 1]`.
/// A non-finite value (`nan`/`inf`) or one `> 1` makes `rate >= threshold` false
/// for every possible rate — the 5xx alert would NEVER fire; a value `<= 0` makes
/// it fire on a 0%-error window. The runtime (`AlerterSettings::from_config`)
/// sanitizes such a value by falling back to the default, but doctor still flags
/// it so the operator fixes the config rather than silently running on the default.
///
/// `raw` is the RAW resolved value (empty when unset — the default `0.05` is used,
/// which is valid). In **production** an invalid value warns with a dedicated
/// message naming it; outside production it passes quietly (alerts are optional
/// locally), matching the leniency of the other alert checks. Never blocks beyond
/// a warning.
///
/// The returned check name is `"alert_error_rate_threshold"`.
pub fn check_alert_error_rate_threshold_impl(raw: &str, is_production: bool) -> CheckResult {
    const NAME: &str = "alert_error_rate_threshold";
    let trimmed = raw.trim();
    // Unset (or cleared) -> the runtime uses the valid default (0.05). Nothing to
    // flag.
    if trimmed.is_empty() {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some("no [alerts] error_rate_threshold set (defaults to 0.05)".into()),
            hint: None,
        };
    }
    // Valid iff it parses to a finite fraction in (0, 1]. `str::parse::<f64>`
    // accepts `nan`/`inf`/`-inf` (which `is_finite()` then rejects), matching the
    // runtime's `f64` config value; an entirely unparsable value is invalid too.
    let valid = trimmed
        .parse::<f64>()
        .is_ok_and(|v| v.is_finite() && v > 0.0 && v <= 1.0);
    if valid {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some(format!(
                "[alerts] error_rate_threshold ({trimmed}) is a valid 5xx rate in (0, 1]"
            )),
            hint: None,
        };
    }
    if is_production {
        CheckResult {
            name: NAME,
            status: CheckStatus::Warn,
            detail: Some(format!(
                "[alerts] error_rate_threshold (\"{trimmed}\") is not a valid rate in (0, 1]; \
                 the 5xx alert will not work as intended (the runtime falls back to the default \
                 0.05)"
            )),
            hint: Some(
                "Set [alerts] error_rate_threshold (or AUTUMN_ALERTS__ERROR_RATE_THRESHOLD) to a \
                 fraction in (0, 1], e.g. 0.05 for 5%. See docs/guide/operator-alerts.md",
            ),
        }
    } else {
        CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some(format!(
                "[alerts] error_rate_threshold (\"{trimmed}\") is not a valid rate in (0, 1] \
                 (optional outside production; the runtime falls back to the default 0.05)"
            )),
            hint: None,
        }
    }
}

/// Check a single `[auth.oauth2.<provider>]` entry for common misconfigurations.
///
/// - In production (`is_production = true`): fails when `client_secret` is empty.
/// - Outside production: warns when `client_secret` is empty.
///
/// The returned check name is `"oauth2_provider"`.
pub fn check_oauth2_provider_impl(
    provider_name: &str,
    client_id: &str,
    client_secret: &str,
    is_production: bool,
) -> CheckResult {
    // Use a static string for the name field to satisfy lifetime constraints.
    // The name is formatted in the `detail` field instead.
    let detail_prefix = format!("[auth.oauth2.{provider_name}]");

    // Check client_id first
    if client_id.trim().is_empty() {
        return if is_production {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "{detail_prefix}: client_id is empty — OAuth2 login will fail in production"
                )),
                hint: Some(
                    "Set client_id via AUTUMN_AUTH__OAUTH2__<PROVIDER>__CLIENT_ID \
                     or in autumn.toml",
                ),
            }
        } else {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Warn,
                detail: Some(format!(
                    "{detail_prefix}: client_id is empty (OK for dev if using env vars)"
                )),
                hint: Some("Set client_id before deploying to production"),
            }
        };
    }

    // Check client_secret next
    if client_secret.trim().is_empty() {
        return if is_production {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "{detail_prefix}: client_secret is empty — OAuth2 login will fail in production"
                )),
                hint: Some(
                    "Set client_secret via AUTUMN_AUTH__OAUTH2__<PROVIDER>__CLIENT_SECRET \
                     or autumn credentials edit",
                ),
            }
        } else {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Warn,
                detail: Some(format!(
                    "{detail_prefix}: client_secret is empty (OK for dev if using env vars)"
                )),
                hint: Some("Set client_secret before deploying to production"),
            }
        };
    }

    CheckResult {
        name: "oauth2_provider",
        status: CheckStatus::Pass,
        detail: Some(format!("{detail_prefix}: provider is correctly configured")),
        hint: None,
    }
}

/// Check that List-Unsubscribe is wired when any `#[mailer]` declares
/// `list_unsubscribe` (pure, injectable for tests).
///
/// - No `list_unsubscribe` usage, or the transport is disabled (no mail is
///   emitted) → pass.
/// - Usage with a base URL or mailto configured → pass.
/// - Usage with neither configured → **fail** in production (Gmail/Yahoo will
///   reject the bulk mail) and **warn** outside production so
///   `autumn doctor --strict` still surfaces it before deploy.
pub fn check_mail_unsubscribe_config_impl(
    has_list_unsubscribe_usage: bool,
    base_url: Option<&str>,
    mailto: Option<&str>,
    transport_disabled: bool,
    is_production: bool,
    token_ttl_days: i64,
) -> CheckResult {
    let base_url = base_url.map(str::trim).filter(|s| !s.is_empty());
    let mailto = mailto.map(str::trim).filter(|s| !s.is_empty());
    let any_set = base_url.is_some() || mailto.is_some();

    // The runtime `MailConfig::validate` runs the following checks at boot
    // regardless of whether any #[mailer] declares list_unsubscribe or whether
    // the transport is disabled. Mirror them *before* the usage/transport early
    // returns so `autumn doctor --strict` never greenlights a deploy that the
    // runtime would reject at startup.

    // TTL: a non-positive value is rejected in every profile (it would make every
    // unsubscribe token immediately expired).
    if token_ttl_days <= 0 {
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "mail.unsubscribe_token_ttl_days must be a positive number of days (got {token_ttl_days})"
            )),
            hint: Some(
                "Set mail.unsubscribe_token_ttl_days to a positive value (default 30); a non-positive value would make every unsubscribe token immediately expired",
            ),
        };
    }

    // Destination format: a non-empty but malformed base_url / mailto is rejected
    // in prod (mailbox providers require HTTPS one-click / a real mailbox).
    if is_production {
        if let Some(url) = base_url
            && !is_valid_https_base_url_doctor(url)
        {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "mail.unsubscribe_base_url is set to an invalid value ({url:?}): \
                     the runtime requires an absolute https:// URL with a real host"
                )),
                hint: Some(
                    "Use an absolute https:// URL with a host and no query/fragment, e.g. https://app.example.com",
                ),
            };
        }
        if let Some(addr) = mailto
            && !is_valid_mailto_address_doctor(addr)
        {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "mail.unsubscribe_mailto is set to an invalid value ({addr:?}): \
                     the runtime requires a bare mailbox address or mailto: URI"
                )),
                hint: Some(
                    "Use a mailbox like unsubscribe@example.com (or mailto:unsubscribe@example.com)",
                ),
            };
        }
    }

    if !has_list_unsubscribe_usage {
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Pass,
            detail: Some("no #[mailer] declares list_unsubscribe".into()),
            hint: None,
        };
    }
    if transport_disabled {
        // Mirrors the runtime guard: a disabled transport emits no list mail, so
        // unsubscribe destinations aren't required to boot.
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Pass,
            detail: Some("mail.transport is disabled; no list mail is emitted".into()),
            hint: None,
        };
    }

    if any_set {
        // Reached only when a #[mailer] uses list_unsubscribe and the transport is
        // active (the no-usage / disabled-transport cases returned above).
        return mail_unsubscribe_configured_result(base_url.is_some(), is_production);
    }
    let detail = Some(
        "a #[mailer] declares list_unsubscribe but neither mail.unsubscribe_base_url \
         nor mail.unsubscribe_mailto is configured"
            .into(),
    );
    let hint = Some(
        "Set mail.unsubscribe_base_url (e.g. https://app.example.com) or mail.unsubscribe_mailto so RFC 8058 List-Unsubscribe headers can be emitted",
    );
    if is_production {
        CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Fail,
            detail,
            hint,
        }
    } else {
        CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Warn,
            detail,
            hint,
        }
    }
}

/// Result for a configured unsubscribe destination (a valid `base_url` and/or
/// `mailto` is set, a list mailer is in use, and the transport is active).
///
/// A `base_url` drives the framework's one-click HTTP endpoint, which must
/// persist opt-outs. The runtime fails closed in production when a base URL is
/// set but no suppression backend (a database pool, or a `SuppressionStore`
/// registered via `AppBuilder::with_suppression_store`) is available. doctor
/// can't see a programmatically registered store, so it warns rather than
/// greenlight a config the app may reject at startup — under `--strict` this
/// surfaces as an error. A mailto-only destination needs no store, so it passes.
fn mail_unsubscribe_configured_result(base_url_set: bool, is_production: bool) -> CheckResult {
    if is_production && base_url_set {
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Warn,
            detail: Some(
                "mail.unsubscribe_base_url is set, so the one-click endpoint must persist \
                 opt-outs; the runtime fails closed in production unless a suppression backend \
                 (a database pool, or a SuppressionStore registered via \
                 AppBuilder::with_suppression_store) is available — which doctor cannot verify \
                 statically"
                    .into(),
            ),
            hint: Some(
                "Configure a database pool (db feature) or register a SuppressionStore before deploying so one-click unsubscribes are recorded",
            ),
        };
    }
    CheckResult {
        name: "mail_unsubscribe",
        status: CheckStatus::Pass,
        detail: Some("list_unsubscribe is wired (unsubscribe URL/mailto configured)".into()),
        hint: None,
    }
}

/// Mirror of `autumn_web`'s `is_valid_https_base_url` (mail module, feature-gated
/// so not reachable from the CLI build). Kept byte-for-byte in sync so
/// `autumn doctor --strict` rejects exactly what the runtime rejects at boot.
fn is_valid_https_base_url_doctor(url: &str) -> bool {
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'))
    {
        return false;
    }
    match url.strip_prefix("https://") {
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') => {}
        _ => return false,
    }
    let Ok(parsed) = ::url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str().is_some_and(|h| !h.is_empty())
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

/// Whether `url` is an absolute http(s) URL that the runtime's HTTP client would
/// actually dispatch — the exact absoluteness rule an `[alerts] webhook_url`
/// must satisfy to deliver.
///
/// `autumn_web`'s `http_client::Client::build_request` sends a URL verbatim ONLY
/// when it starts with `http://` or `https://`; any other string is treated as a
/// path to be joined onto a base-url alias, and an alert webhook (posted via
/// `named(&url).post(&url)`) has no such alias, so a relative value like
/// `hooks.example/x` fails EVERY signed send. Mirror that rule byte-for-byte:
/// accept BOTH schemes (the runtime does not require TLS, so neither do we) and
/// additionally require the URL to parse with a non-empty host — a value such as
/// `https://` or `http:///path` carries the prefix yet reqwest cannot send it.
///
/// Deliberately MORE permissive than [`is_valid_https_base_url_doctor`]
/// (https-only; forbids query/fragment/userinfo): a webhook URL is a full
/// endpoint the runtime posts as-is, so rejecting `http://` or a query string
/// here would warn about configs that deliver fine — a false positive.
fn is_absolute_http_url_doctor(url: &str) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    ::url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(|h| !h.is_empty()))
        .unwrap_or(false)
}

/// Mirror of `autumn_web`'s `is_valid_mailto_address` (see above).
fn is_valid_mailto_address_doctor(value: &str) -> bool {
    // Reject control characters and RFC 2369 delimiters (`<`/`>`/`,`) anywhere in
    // the value; mirrors autumn_web's is_valid_mailto_address.
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ','))
    {
        return false;
    }
    let address = value
        .trim()
        .strip_prefix("mailto:")
        .unwrap_or_else(|| value.trim());
    let address = address.split('?').next().unwrap_or("");
    match address.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !address.contains(char::is_whitespace)
                && !address.contains([':', '/'])
        }
        None => false,
    }
}

/// Whether `value` parses as the SAME lettre `Mailbox` (`lettre::message::Mailbox`)
/// the alert mail send path requires of every recipient — EXACT parity with the
/// runtime's `is_valid_alert_mailbox` (`autumn/src/alerts.rs`), which validates
/// `[alerts] email` with `email.parse::<lettre::message::Mailbox>()`.
///
/// The runtime hands `[alerts] email` verbatim to `Mail::builder().to(...)`, and
/// lettre parses the recipient only at SEND time (`parse_mailbox`/`lettre_message`),
/// not when the alert `Mail` is built. Reusing the exact same parser here means
/// doctor accepts precisely what the runtime accepts — bare mailboxes
/// (`ops@example.com`), `+`-addressing (`a+b@sub.example.co`) AND RFC 5322
/// display-name forms (`Ops <ops@example.com>`) — instead of a hand-rolled
/// approximation that was STRICTER than lettre and rejected valid display-name
/// mailboxes, producing a false `autumn doctor --strict` failure on a config the
/// runtime registers and delivers to fine.
///
/// A `mailto:` URI (e.g. `mailto:oncall@example.com`) is still rejected: lettre's
/// parser treats the whole string as one mailbox and the `:` is not valid in a
/// dot-atom local part, so it fails to parse — matching the runtime, where such a
/// value fails EVERY delivery with `MailError::InvalidAddress`.
///
/// lettre is reachable from the CLI via `autumn_web`'s `mail`-gated re-export
/// (`autumn_web::reexports::lettre`); the default (Postgres) CLI enables that
/// feature via the `postgres` bundle, so no direct `lettre` dependency is
/// required.
#[cfg(feature = "postgres")]
fn is_valid_alert_mailbox_doctor(value: &str) -> bool {
    value
        .parse::<autumn_web::reexports::lettre::message::Mailbox>()
        .is_ok()
}

/// `SQLite` backend-flip build fallback: the `mail` feature (and its `lettre`
/// re-export) is not compiled under `--features sqlite`, so this syntactic check
/// stands in for the lettre `Mailbox` parser. The alert-email doctor check is a
/// Postgres-runtime concern; the fallback keeps the sqlite CLI compiling without
/// pulling the pg-only `mail` path while agreeing with lettre for the cases
/// doctor validates: it accepts a bare `local@domain` mailbox and the RFC 5322
/// display-name form `Name <local@domain>` (validating the bracketed address),
/// and rejects scheme-prefixed / whitespaced / `@`-malformed / empty values. Full
/// lettre parity applies to the default (Postgres) build.
#[cfg(not(feature = "postgres"))]
fn is_valid_alert_mailbox_doctor(value: &str) -> bool {
    let value = value.trim();
    // Accept `Name <local@domain>` by validating the bracketed address; the
    // display-name text before `<` is free-form and not checked.
    let addr = match (value.rfind('<'), value.rfind('>')) {
        (Some(lt), Some(gt)) if gt > lt => value[lt + 1..gt].trim(),
        _ => value,
    };
    if addr.is_empty() || addr.contains(':') || addr.contains(char::is_whitespace) {
        return false;
    }
    let mut parts = addr.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty() && !domain.is_empty() && domain.contains('.')
        }
        _ => false,
    }
}

// ─── Platform support tier (issue #1616) ─────────────────────────────────────

/// Report this platform's support tier, and on Windows the prerequisites and
/// WSL2-only journeys a developer would otherwise discover by hitting them.
///
/// Takes the OS family as a string so the Windows branch is exercised by tests
/// on every host — the branch that matters here is the one this CI never runs.
/// Both branches read [`crate::platform::POLICY`], so doctor cannot describe a
/// policy different from the one the commands enforce.
#[must_use]
pub fn check_platform_support_impl(os: &str) -> CheckResult {
    use crate::platform::{SupportTier, WINDOWS_PREREQUISITES, commands_in_tier};

    if os != "windows" {
        return CheckResult {
            name: "platform_support",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "{os}: every autumn journey runs natively on this platform"
            )),
            hint: None,
        };
    }

    let tier_one = commands_in_tier(SupportTier::Native).join(", ");
    let tier_two = commands_in_tier(SupportTier::Wsl2).join(", ");
    let prerequisites = WINDOWS_PREREQUISITES
        .iter()
        .map(|p| format!("{} needs {}", p.subject, p.requirement))
        .collect::<Vec<_>>()
        .join("; ");

    CheckResult {
        name: "platform_support",
        // Pass, not Warn. Windows is a supported development platform and the
        // Tier 2 journeys are documented with a working answer (WSL2), so
        // nothing here is a defect. It matters concretely: `exit_code` treats
        // any warning as a failure under `--strict`, so warning unconditionally
        // would make `autumn doctor --strict` — itself a Tier 1 command — exit 1
        // forever on every Windows machine.
        status: CheckStatus::Pass,
        detail: Some(format!(
            "windows: Tier 1 (native) — {tier_one}. Tier 2 (WSL2), run these from a \
             WSL2 shell — {tier_two}. Prerequisites: {prerequisites}. Policy: \
             docs/guide/platform-support.md"
        )),
        // `format_check_line` prints a hint only on warn/fail, so the pointer to
        // the policy lives in the detail above rather than being silently
        // dropped here.
        hint: None,
    }
}

#[cfg(test)]
mod platform_support_tests {
    use super::{CheckStatus, check_platform_support_impl};
    use crate::platform::{SupportTier, WINDOWS_PREREQUISITES};

    #[test]
    fn windows_reports_tier_status_and_flags_prerequisites() {
        let detail = check_platform_support_impl("windows")
            .detail
            .expect("a detail line");
        // AC: doctor "reports the platform's tier status".
        assert!(detail.contains("Tier 1"), "{detail}");
        assert!(detail.contains("Tier 2"), "{detail}");
        // AC: "flags known Windows-specific prerequisites (e.g. the
        // vcpkg/OpenSSL requirement for `generate auth --passkeys`)".
        assert!(detail.contains("vcpkg"), "{detail}");
        assert!(detail.contains("--passkeys"), "{detail}");
        for prerequisite in WINDOWS_PREREQUISITES {
            assert!(
                detail.contains(prerequisite.subject),
                "prerequisite `{}` is not reported: {detail}",
                prerequisite.subject
            );
        }
    }

    #[test]
    fn windows_names_every_tier_two_command_so_nothing_is_a_surprise() {
        let detail = check_platform_support_impl("windows")
            .detail
            .expect("a detail line");
        for command in crate::platform::commands_in_tier(SupportTier::Wsl2) {
            assert!(
                detail.contains(command),
                "Tier 2 command `{command}` missing from doctor output: {detail}"
            );
        }
    }

    #[test]
    fn unix_platforms_pass_with_every_journey_native() {
        for os in ["linux", "macos"] {
            let result = check_platform_support_impl(os);
            assert_eq!(result.status, CheckStatus::Pass, "{os} should pass");
            let detail = result.detail.unwrap_or_default();
            assert!(
                detail.contains("natively"),
                "{os} detail should say every journey is native: {detail}"
            );
            assert!(
                !detail.contains("WSL2"),
                "{os} must not mention WSL2: {detail}"
            );
        }
    }

    #[test]
    fn the_check_is_named_so_json_consumers_can_key_on_it() {
        assert_eq!(
            check_platform_support_impl("windows").name,
            "platform_support"
        );
        assert_eq!(
            check_platform_support_impl("linux").name,
            "platform_support"
        );
    }

    #[test]
    fn windows_does_not_fail_doctor_strict() {
        // Windows is a supported development platform, so `autumn doctor
        // --strict` must not reject a machine for being a Windows one — and
        // `doctor` is itself a Tier 1 command in this very policy.
        //
        // A `Warn` is not enough: `exit_code` treats ANY warning as a failure
        // under `--strict`, so an always-warning check makes `--strict` exit 1
        // forever on Windows. The tier is information, not a defect.
        let result = check_platform_support_impl("windows");
        assert_eq!(result.status, CheckStatus::Pass);
        let summary = super::compute_summary(std::slice::from_ref(&result));
        assert_eq!(
            super::exit_code(&summary, true),
            0,
            "platform_support must not fail `doctor --strict` on Windows"
        );
    }

    #[test]
    fn the_windows_detail_carries_the_policy_pointer_itself() {
        // `format_check_line` prints a check's `hint` only on warn/fail, so a
        // passing check's pointer to the guide has to live in the detail or it
        // is never shown at all.
        let detail = check_platform_support_impl("windows")
            .detail
            .expect("a detail line");
        assert!(detail.contains("platform-support.md"), "{detail}");
    }
}

// ─── Pure helper functions (fully unit-testable) ──────────────────────────────

pub const fn glyph(status: &CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    }
}

pub fn compute_summary(results: &[CheckResult]) -> Summary {
    let mut passed = 0;
    let mut warned = 0;
    let mut failed = 0;
    for r in results {
        match r.status {
            CheckStatus::Pass => passed += 1,
            CheckStatus::Warn => warned += 1,
            CheckStatus::Fail => failed += 1,
        }
    }
    Summary {
        passed,
        warned,
        failed,
    }
}

pub const fn exit_code(summary: &Summary, strict: bool) -> i32 {
    if summary.failed > 0 || (strict && summary.warned > 0) {
        1
    } else {
        0
    }
}

pub fn format_check_line(result: &CheckResult) -> String {
    let g = glyph(&result.status);
    let mut line = format!("{g} {}", result.name);
    if let Some(ref detail) = result.detail {
        let _ = write!(line, " — {detail}");
    }
    if let Some(hint) = result.hint
        && result.status != CheckStatus::Pass
    {
        let _ = write!(line, "\n   hint: {hint}");
    }
    line
}

pub fn format_summary_line(summary: &Summary, code: i32) -> String {
    let verdict = if code == 0 {
        "all clear"
    } else {
        "problems found"
    };
    let w_label = if summary.warned == 1 {
        "warning"
    } else {
        "warnings"
    };
    format!(
        "{} passed, {} {}, {} failed — {verdict}",
        summary.passed, summary.warned, w_label, summary.failed
    )
}

pub fn to_json_output(results: &[CheckResult], summary: &Summary) -> String {
    #[derive(Serialize)]
    struct Output<'a> {
        checks: &'a [CheckResult],
        summary: &'a Summary,
    }
    serde_json::to_string_pretty(&Output {
        checks: results,
        summary,
    })
    .unwrap_or_else(|_| "{}".to_string())
}

// ─── Check implementations ────────────────────────────────────────────────────

/// Recursively validates TOML content against the derived schema.
/// Returns a list of errors: (`dotted_path`, `option_suggestion`)
pub fn validate_toml_content(
    content: &str,
    schema: &HashMap<String, HashSet<String>>,
) -> Vec<(String, Option<String>)> {
    autumn_web::config::AutumnConfig::validate_toml(content, schema)
}

fn find_all_profile_files() -> Vec<(String, std::path::PathBuf)> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(".") {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                let is_profile = path.is_file()
                    && filename.starts_with("autumn-")
                    && std::path::Path::new(filename)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
                if is_profile {
                    let profile =
                        filename["autumn-".len()..filename.len() - ".toml".len()].to_string();
                    files.push((profile, path));
                }
            }
        }
    }
    files
}

/// Check that `autumn.toml` content parses cleanly (pure, injectable for tests).
pub fn check_toml_content(content: &str) -> CheckResult {
    if let Err(e) = toml::from_str::<toml::Table>(content) {
        CheckResult {
            name: "autumn_toml",
            status: CheckStatus::Fail,
            detail: Some(e.to_string()),
            hint: Some("Fix the syntax error in autumn.toml"),
        }
    } else {
        let schema = autumn_web::config::AutumnConfig::get_schema_keys();
        let mut errors = Vec::new();

        // 1. Validate base autumn.toml
        let base_errors = validate_toml_content(content, &schema);
        for (path, sug) in base_errors {
            errors.push(format!(
                "autumn.toml: unknown key \"{path}\"{}",
                sug.map(|s| format!(" — did you mean \"{s}\"?"))
                    .unwrap_or_default()
            ));
        }

        // 2. Validate any autumn-*.toml profile files
        let profile_files = find_all_profile_files();
        for (_, path) in profile_files {
            if let Ok(file_content) = std::fs::read_to_string(&path) {
                let profile_errors = validate_toml_content(&file_content, &schema);
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("profile config");
                for (path, sug) in profile_errors {
                    errors.push(format!(
                        "{filename}: unknown key \"{path}\"{}",
                        sug.map(|s| format!(" — did you mean \"{s}\"?"))
                            .unwrap_or_default()
                    ));
                }
            }
        }

        if errors.is_empty() {
            CheckResult {
                name: "autumn_toml",
                status: CheckStatus::Pass,
                detail: Some("autumn.toml and profile configurations are valid".into()),
                hint: None,
            }
        } else {
            CheckResult {
                name: "autumn_toml",
                status: CheckStatus::Warn,
                detail: Some(errors.join(", ")),
                hint: Some("Remove or rename unrecognised keys in configuration files"),
            }
        }
    }
}

/// Compare CLI version against the project's `autumn-web` version (pure, injectable for tests).
///
/// For semver < 1.0 (`0.MINOR.PATCH`), a minor-version mismatch is treated as
/// a breaking incompatibility (Fail); a patch-only mismatch is a warning.
pub fn check_version_compat(cli_version: &str, web_version: &str) -> CheckResult {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        // Strip leading `=`, `^`, `~` requirement operators if present.
        let v = v.trim().trim_start_matches(['=', '^', '~', ' ']);
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() < 2 {
            return None;
        }
        let major: u64 = parts[0].parse().ok()?;
        let minor: u64 = parts[1].parse().ok()?;
        let patch: u64 = if parts.len() >= 3 {
            parts[2]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0)
        } else {
            0
        };
        Some((major, minor, patch))
    };

    let Some(cli) = parse(cli_version) else {
        return CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse CLI version: {cli_version}")),
            hint: Some("Reinstall autumn-cli"),
        };
    };
    let Some(web) = parse(web_version) else {
        return CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse autumn-web version: {web_version}")),
            hint: Some("Check autumn-web version in Cargo.toml"),
        };
    };

    if cli.0 != web.0 || cli.1 != web.1 {
        CheckResult {
            name: "version_compat",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "autumn-cli {cli_version} is incompatible with autumn-web {web_version}"
            )),
            hint: Some(
                "Run `cargo install --path autumn-cli` to match your project's autumn-web version",
            ),
        }
    } else if cli.2 != web.2 {
        CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "autumn-cli {cli_version} vs autumn-web {web_version} (patch skew)"
            )),
            hint: Some("Consider updating either the CLI or your project's autumn-web dependency"),
        }
    } else {
        CheckResult {
            name: "version_compat",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "autumn-cli {cli_version} matches autumn-web {web_version}"
            )),
            hint: None,
        }
    }
}

/// Check whether a port is bindable using an injectable binding function.
pub fn check_port_bindable_impl(port: u16, try_bind: impl Fn(u16) -> bool) -> CheckResult {
    if try_bind(port) {
        CheckResult {
            name: "port_bindable",
            status: CheckStatus::Pass,
            detail: Some(format!("port {port} is available")),
            hint: None,
        }
    } else {
        CheckResult {
            name: "port_bindable",
            status: CheckStatus::Fail,
            detail: Some(format!("port {port} is already in use")),
            hint: Some("Kill the process using that port, or change server.port in autumn.toml"),
        }
    }
}

/// Compare version strings for the Rust toolchain check (pure, injectable for tests).
pub fn check_rust_toolchain_impl(current_output: &str, required: &str) -> CheckResult {
    let parse_ver = |s: &str| -> Option<(u64, u64, u64)> {
        let s = s.trim();
        let s = s
            .strip_prefix("rustc ")
            .map_or(s, |rest| rest.split_whitespace().next().unwrap_or(rest));
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() < 3 {
            return None;
        }
        let major: u64 = parts[0].parse().ok()?;
        let minor: u64 = parts[1].parse().ok()?;
        let patch: u64 = parts[2]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        Some((major, minor, patch))
    };

    let Some(cur) = parse_ver(current_output) else {
        return CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse rustc version: {current_output}")),
            hint: Some("Run `rustup update` to ensure a known Rust version"),
        };
    };
    let Some(req) = parse_ver(required) else {
        return CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse MSRV: {required}")),
            hint: None,
        };
    };

    if cur >= req {
        CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "rustc {}.{}.{} ≥ MSRV {}.{}.{}",
                cur.0, cur.1, cur.2, req.0, req.1, req.2
            )),
            hint: None,
        }
    } else {
        CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "rustc {}.{}.{} < MSRV {}.{}.{}",
                cur.0, cur.1, cur.2, req.0, req.1, req.2
            )),
            hint: Some("Run `rustup update stable` to upgrade your Rust toolchain"),
        }
    }
}

// ─── IO-dependent checks ──────────────────────────────────────────────────────

fn check_rust_toolchain(msrv: &str) -> CheckResult {
    match std::process::Command::new("rustc")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).into_owned();
            check_rust_toolchain_impl(ver.trim(), msrv)
        }
        _ => CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Fail,
            detail: Some("`rustc --version` failed".into()),
            hint: Some("Install Rust via https://rustup.rs/"),
        },
    }
}

fn check_port_bindable(port: u16) -> CheckResult {
    check_port_bindable_impl(port, |p| {
        std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
    })
}

/// Check whether the Tailwind binary is present and executable without launching it.
pub fn check_tailwind_binary_at(path: &std::path::Path) -> CheckResult {
    check_tailwind_binary_at_with_executable_probe(path, tailwind_file_is_executable)
}

fn check_tailwind_binary_at_with_executable_probe(
    path: &std::path::Path,
    executable_probe: impl Fn(&std::path::Path, &std::fs::Metadata) -> bool,
) -> CheckResult {
    let symlink_metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("{} not found", path.display())),
                hint: Some("Run `autumn setup` to download the Tailwind CSS binary"),
            };
        }
        Err(e) => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("cannot inspect {}: {e}", path.display())),
                hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
            };
        }
    };

    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(_) if symlink_metadata.file_type().is_symlink() => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("{} is a broken symlink", path.display())),
                hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
            };
        }
        Err(e) => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("cannot inspect {}: {e}", path.display())),
                hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
            };
        }
    };

    if !metadata.is_file() {
        let kind = if metadata.is_dir() {
            "directory"
        } else {
            "non-file filesystem entry"
        };
        return CheckResult {
            name: "tailwind_binary",
            status: CheckStatus::Fail,
            detail: Some(format!("{} is a {kind}, not a file", path.display())),
            hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
        };
    }

    if !executable_probe(path, &metadata) {
        return CheckResult {
            name: "tailwind_binary",
            status: CheckStatus::Fail,
            detail: Some(format!("{} exists but is not executable", path.display())),
            hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
        };
    }

    CheckResult {
        name: "tailwind_binary",
        status: CheckStatus::Pass,
        detail: Some(format!("{} is present and executable", path.display())),
        hint: None,
    }
}

#[cfg(unix)]
fn tailwind_file_is_executable(path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

#[cfg(windows)]
fn tailwind_file_is_executable(path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    use std::io::{Read as _, Seek as _};

    if !path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return false;
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut dos_header = [0_u8; 64];
    if file.read_exact(&mut dos_header).is_err() || &dos_header[0..2] != b"MZ" {
        return false;
    }

    let pe_offset = u32::from_le_bytes([
        dos_header[0x3c],
        dos_header[0x3d],
        dos_header[0x3e],
        dos_header[0x3f],
    ]);
    if file
        .seek(std::io::SeekFrom::Start(u64::from(pe_offset)))
        .is_err()
    {
        return false;
    }

    let mut pe_signature = [0_u8; 4];
    file.read_exact(&mut pe_signature).is_ok() && pe_signature == *b"PE\0\0"
}

#[cfg(not(any(unix, windows)))]
fn tailwind_file_is_executable(_path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    true
}

fn check_tailwind_binary() -> CheckResult {
    let path = if cfg!(windows) {
        std::path::PathBuf::from("target/autumn/tailwindcss.exe")
    } else {
        std::path::PathBuf::from("target/autumn/tailwindcss")
    };

    check_tailwind_binary_at(&path)
}

fn check_stale_artifacts() -> CheckResult {
    let cargo_lock = std::path::Path::new("Cargo.lock");
    let dist = std::path::Path::new("dist");
    let target = std::path::Path::new("target");

    let lock_mtime = cargo_lock.metadata().and_then(|m| m.modified()).ok();

    let dir_older_than_lock = |dir: &std::path::Path| -> bool {
        let Some(lock_t) = lock_mtime else {
            return false;
        };
        dir.metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|dir_t| dir_t < lock_t)
    };

    let dist_stale = dist.exists() && dir_older_than_lock(dist);
    let target_stale = target.exists() && dir_older_than_lock(target);

    if dist_stale || target_stale {
        let which: Vec<&str> = [
            dist_stale.then_some("dist/"),
            target_stale.then_some("target/"),
        ]
        .into_iter()
        .flatten()
        .collect();
        CheckResult {
            name: "stale_artifacts",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "{} may be stale relative to Cargo.lock",
                which.join(", ")
            )),
            hint: Some("Run `cargo build` or `autumn build` to refresh artifacts"),
        }
    } else {
        CheckResult {
            name: "stale_artifacts",
            status: CheckStatus::Pass,
            detail: Some("artifacts look fresh".into()),
            hint: None,
        }
    }
}

/// Result of probing for the `PostgreSQL` client tools, factored out so the
/// pass/warn logic is unit-testable without touching PATH.
fn check_pg_client_tools_impl(missing: &[&str]) -> CheckResult {
    if missing.is_empty() {
        CheckResult {
            name: "pg_client_tools",
            status: CheckStatus::Pass,
            detail: Some("pg_dump and pg_restore found".into()),
            hint: None,
        }
    } else {
        CheckResult {
            name: "pg_client_tools",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "{} not found (checked AUTUMN_PG_BIN_DIR, the managed-Postgres bundle, and PATH); `autumn db backup` / `autumn db restore` need it",
                missing.join(", ")
            )),
            hint: Some(
                "Install the PostgreSQL client tools (e.g. `postgresql-client`) or set AUTUMN_PG_BIN_DIR to their bin directory",
            ),
        }
    }
}

/// Warn when the `PostgreSQL` client tools behind `autumn db backup` /
/// `autumn db restore` aren't on PATH. Mirrors the diesel-CLI nudge in
/// [`check_pending_migrations`]: a soft warning (managed-Postgres apps bundle
/// these and not every project runs backups), never a hard failure.
fn check_pg_client_tools() -> CheckResult {
    // Reuse `db backup`/`restore`'s own locator so doctor honours the exact same
    // precedence (`AUTUMN_PG_BIN_DIR`, then the managed-Postgres bundle, then
    // PATH) and never drifts from what those commands actually resolve.
    check_pg_client_tools_with(&crate::db::backup::PgTools::locate())
}

/// Compute the pg-client-tools check result for a given (already-configured)
/// locator. Split from [`check_pg_client_tools`] so the discovery/resolution
/// path is unit-testable against an explicit `bin` directory.
fn check_pg_client_tools_with(tools: &crate::db::backup::PgTools) -> CheckResult {
    let missing: Vec<&str> = ["pg_dump", "pg_restore"]
        .into_iter()
        .filter(|tool| tools.require(tool).is_err())
        .collect();
    check_pg_client_tools_impl(&missing)
}

/// `SQLite`-target variant of the pg-client-tools check (`SQLite` foundation,
/// issue #1614). `pg_dump`/`pg_restore` are Postgres-only, so their absence is
/// not a problem for a `SQLite` app — warning about them would be misleading.
/// `SQLite` backup/restore is not yet wired (tracked in #1909), so this is an
/// honest informational Pass rather than a claim that backups work today.
fn check_pg_client_tools_sqlite() -> CheckResult {
    CheckResult {
        name: "pg_client_tools",
        status: CheckStatus::Pass,
        detail: Some(
            "SQLite app: PostgreSQL client tools (pg_dump/pg_restore) are not required".into(),
        ),
        hint: Some(
            "SQLite backup/restore is tracked in https://github.com/autumn-foundation/autumn/issues/1909",
        ),
    }
}

/// Read the `rust-version` MSRV from the nearest workspace/package `Cargo.toml`.
fn read_msrv() -> Option<String> {
    let content = std::fs::read_to_string("Cargo.toml").ok()?;
    let table: toml::Table = toml::from_str(&content).ok()?;

    // Workspace: [workspace.package] rust-version
    if let Some(ver) = table
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("rust-version"))
        .and_then(|v| v.as_str())
    {
        return Some(ver.to_owned());
    }

    // Plain package: [package] rust-version
    table
        .get("package")
        .and_then(|p| p.get("rust-version"))
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned)
}

/// Read the `autumn-web` version requirement from the project's `Cargo.toml`.
fn read_autumn_web_version() -> Option<String> {
    read_autumn_web_version_at(std::path::Path::new("."))
}

/// How a manifest declares `autumn-web`, if it does.
///
/// `autumn upgrade` needs the middle case told apart from the absent one: a
/// `{ path = "../autumn" }` or `{ git = "..." }` dependency *is* a declaration,
/// it just carries no version to compare. Treating it as "not declared" lets a
/// sibling manifest pick the floor for a crate whose version is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutumnWebDependency {
    /// The manifest does not mention `autumn-web`.
    Absent,
    /// Declared with a version requirement.
    Version(String),
    /// Declared, but with no version to read — a path or git entry.
    WithoutVersion,
    /// The manifest exists but could not be read or parsed.
    ///
    /// Distinct from [`Self::Absent`] on purpose: a crate whose manifest cannot
    /// be read may well be the oldest in the workspace, and reading it as
    /// "declares nothing" lets a newer sibling decide the floor while this
    /// crate's sources are scanned and rewritten anyway.
    Unreadable,
    /// `{ workspace = true }`: the version lives in the enclosing workspace's
    /// `[workspace.dependencies]`, which may be in an *ancestor* directory when
    /// the command is pointed at a member rather than the workspace root.
    ///
    /// Carries the *key* the member used, because that is the only handle on
    /// the entry: under Cargo's renamed form the member writes `autumn =
    /// { workspace = true }` and nothing but the workspace entry it points at
    /// says the package is `autumn-web`. Resolve it with
    /// [`workspace_dependency_for`].
    Inherited(String),
}

/// Read the `autumn-web` version requirement from the `Cargo.toml` at `root`.
///
/// The first readable version across every dependency table. Callers that need
/// *all* of them — `autumn upgrade`, which takes the oldest floor — use
/// [`autumn_web_declarations_at`] instead.
pub fn read_autumn_web_version_at(root: &std::path::Path) -> Option<String> {
    autumn_web_declarations_at(root)
        .into_iter()
        .find_map(|declaration| match declaration {
            AutumnWebDependency::Version(version) => Some(version),
            AutumnWebDependency::Absent
            | AutumnWebDependency::Unreadable
            | AutumnWebDependency::WithoutVersion
            | AutumnWebDependency::Inherited(_) => None,
        })
}

/// Every `autumn-web` declaration in the `Cargo.toml` at `root`.
///
/// All of them, not the first: a manifest can declare the dependency under
/// `[dependencies]`, `[workspace.dependencies]`, and any number of
/// `[target.'cfg(…)'.dependencies]` tables, and Cargo honours each. Returning
/// the first match let a newer target-specific requirement hide an older one
/// while the older target's `#[cfg]` code was still scanned and rewritten.
///
/// Both spellings are recognised: the literal `autumn-web` key, and Cargo's
/// renamed form `autumn_web = { package = "autumn-web", version = "…" }`.
///
/// An empty result means the manifest does not mention `autumn-web` at all.
pub fn autumn_web_declarations_at(root: &std::path::Path) -> Vec<AutumnWebDependency> {
    /// Every dependency-table kind Cargo reads. `dev-` and `build-` count: a
    /// crate depending on autumn-web only for its tests or its `build.rs` still
    /// has those sources scanned and rewritten, so it gets a vote on the
    /// version.
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

    /// Whether this dependency entry is `autumn-web`, under either spelling.
    fn is_autumn_web(key: &str, entry: &toml::Value) -> bool {
        key == "autumn-web"
            || entry
                .get("package")
                .and_then(toml::Value::as_str)
                .is_some_and(|package| package == "autumn-web")
    }

    /// Whether this entry defers to the enclosing workspace.
    fn is_inherited(entry: &toml::Value) -> bool {
        entry
            .get("workspace")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
    }

    fn classify(key: &str, entry: &toml::Value) -> AutumnWebDependency {
        match entry {
            toml::Value::String(version) => AutumnWebDependency::Version(version.clone()),
            toml::Value::Table(table) => table
                .get("version")
                .and_then(toml::Value::as_str)
                .map_or_else(
                    || {
                        if table.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                            // Resolved against the enclosing workspace, which
                            // is not necessarily inside the scanned tree.
                            AutumnWebDependency::Inherited(key.to_owned())
                        } else {
                            // A path or git entry: declared, no version to read.
                            AutumnWebDependency::WithoutVersion
                        }
                    },
                    |version| AutumnWebDependency::Version(version.to_owned()),
                ),
            _ => AutumnWebDependency::WithoutVersion,
        }
    }

    let manifest = root.join("Cargo.toml");
    let Ok(content) = std::fs::read_to_string(&manifest) else {
        // No manifest at all is a directory that is not a crate. One that
        // exists and cannot be read is a crate whose version is unknown.
        return if manifest.exists() {
            vec![AutumnWebDependency::Unreadable]
        } else {
            Vec::new()
        };
    };
    let Ok(table) = toml::from_str::<toml::Table>(&content) else {
        return vec![AutumnWebDependency::Unreadable];
    };

    // Every dependency table Cargo reads: the package's own, the workspace's,
    // and one per target predicate — in each of the three kinds.
    let mut tables: Vec<&toml::Value> = Vec::new();
    for kind in KINDS {
        tables.extend(table.get(kind));
        tables.extend(
            table
                .get("workspace")
                .and_then(|workspace| workspace.get(kind)),
        );
    }
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            for kind in KINDS {
                tables.extend(target.get(kind));
            }
        }
    }

    tables
        .into_iter()
        .filter_map(toml::Value::as_table)
        .flat_map(|deps| {
            deps.iter()
                // Inherited entries come along under *any* key: a member
                // writing `autumn = { workspace = true }` names neither
                // `autumn-web` nor a `package`, so whether it is this
                // dependency can only be answered by the workspace entry it
                // points at. The caller resolves them and drops the ones that
                // turn out to be some other crate.
                .filter(|(key, entry)| is_autumn_web(key, entry) || is_inherited(entry))
                .map(|(key, entry)| classify(key, entry))
        })
        .filter(|declaration| *declaration != AutumnWebDependency::Absent)
        .collect()
}

/// The `[workspace.dependencies]` entry named `key` in the `Cargo.toml` at
/// `dir`, but only when that entry resolves to `autumn-web`.
///
/// Used to resolve `{ workspace = true }` when `autumn upgrade` is pointed at a
/// member directory: Cargo walks up to the workspace root, and so must this.
/// The lookup is by the member's key rather than by the crate name because a
/// renamed workspace entry — `autumn = { package = "autumn-web", … }` — is the
/// only place the real package is written down. `None` means the workspace does
/// not define `key`, or defines it as a different crate.
pub fn workspace_dependency_for(dir: &std::path::Path, key: &str) -> Option<AutumnWebDependency> {
    let content = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let table = toml::from_str::<toml::Table>(&content).ok()?;
    let entry = table
        .get("workspace")?
        .get("dependencies")?
        .as_table()?
        .iter()
        .find(|(candidate, entry)| {
            *candidate == key
                && (key == "autumn-web"
                    || entry
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .is_some_and(|package| package == "autumn-web"))
        })
        .map(|(_, entry)| entry)?;
    Some(match entry {
        toml::Value::String(version) => AutumnWebDependency::Version(version.clone()),
        toml::Value::Table(table) => table
            .get("version")
            .and_then(toml::Value::as_str)
            .map_or(AutumnWebDependency::WithoutVersion, |version| {
                AutumnWebDependency::Version(version.to_owned())
            }),
        _ => AutumnWebDependency::WithoutVersion,
    })
}

/// Try to TCP-connect to a host:port within a short timeout.
fn tcp_reachable(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    use std::time::Duration;
    let addrs: Vec<_> = match format!("{host}:{port}").to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return false,
    };
    addrs
        .iter()
        .any(|addr| std::net::TcpStream::connect_timeout(addr, Duration::from_secs(1)).is_ok())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DoctorReplicaFallback {
    #[default]
    FailReadiness,
    Primary,
}

impl DoctorReplicaFallback {
    fn from_config_value(value: Option<&str>) -> Self {
        match value.unwrap_or("fail_readiness") {
            "primary" | "fallback_to_primary" | "fallback-to-primary" => Self::Primary,
            _ => Self::FailReadiness,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DoctorDatabaseTopology {
    pub primary_url: Option<String>,
    pub replica_url: Option<String>,
    /// The legacy `database.url` role, resolved separately from `primary_url`.
    ///
    /// `primary_url` above collapses `database.primary_url` and the legacy
    /// `database.url` into a single effective write URL (primary wins). But the
    /// backend-consistency rule must still see the legacy `url` as its own role:
    /// a config with `primary_url = "sqlite://…"` and `url = "postgres://…"` is a
    /// mixed-backend topology the app refuses to boot (finding F26). Carrying the
    /// legacy URL separately lets doctor delegate to
    /// [`autumn_web::config::database_backend_consistency`] with every role.
    pub legacy_url: Option<String>,
    pub auto_migrate_in_production: bool,
    pub replica_fallback: DoctorReplicaFallback,
}

fn check_database_topology_contract(
    topology: &DoctorDatabaseTopology,
    is_production: bool,
) -> CheckResult {
    if topology.replica_url.is_some() && topology.primary_url.is_none() {
        return CheckResult {
            name: "database_topology",
            status: CheckStatus::Fail,
            detail: Some("replica role configured without a primary/write role".into()),
            hint: Some("Set database.primary_url or remove database.replica_url"),
        };
    }

    if is_production && topology.replica_url.is_some() && topology.auto_migrate_in_production {
        return CheckResult {
            name: "database_topology",
            status: CheckStatus::Fail,
            detail: Some(
                "unsafe migration ownership: production primary/replica topology cannot run migrations from every web replica"
                    .into(),
            ),
            hint: Some("Run `autumn migrate` as one primary-role job before starting web replicas"),
        };
    }

    if topology.primary_url.is_some() && topology.replica_url.is_some() {
        CheckResult {
            name: "database_topology",
            status: CheckStatus::Pass,
            detail: Some("primary and replica database roles configured".into()),
            hint: None,
        }
    } else if topology.primary_url.is_some() {
        CheckResult {
            name: "database_topology",
            status: CheckStatus::Pass,
            detail: Some("single primary database role configured".into()),
            hint: None,
        }
    } else {
        CheckResult {
            name: "database_topology",
            status: CheckStatus::Pass,
            detail: Some("database not configured".into()),
            hint: None,
        }
    }
}

/// Verify the selected process role is compatible with the jobs backend.
///
/// A split web/worker role runs the HTTP tier and the job/scheduler tier in
/// **separate processes**, so it needs a durable jobs backend the two processes
/// can share. Only the recognized durable backends (`postgres`/`redis`) qualify;
/// any other value — the in-process `local` queue, a typo like `postgresql`, or a
/// blank backend — falls through to the per-process local runtime, where a web
/// replica's enqueue never reaches a worker replica's queue.
/// [`autumn_web::config::split_role_requires_durable_backend`] flags that invalid
/// combo; the app itself rejects it at startup, and doctor surfaces it up front.
fn check_split_topology_on_local(role: ProcessRole, jobs_backend: &str) -> CheckResult {
    let backend = jobs_backend.trim();
    if autumn_web::config::split_role_requires_durable_backend(role, jobs_backend) {
        return CheckResult {
            name: "process_role_backend",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "role={} with jobs.backend=\"{backend}\": a split web/worker role needs a durable \
                 (postgres/redis) jobs backend; \"{backend}\" is not a recognized durable backend \
                 and falls through to the in-process `local` runtime, which cannot share a job \
                 queue across the separate web and worker processes",
                role.as_str(),
            )),
            hint: Some(
                "Set jobs.backend = \"postgres\" (or redis) for split web/worker roles, or run the combined role",
            ),
        };
    }
    let detail = if role == ProcessRole::Combined {
        format!("role={}", role.as_str())
    } else {
        format!("role={}, jobs.backend={backend}", role.as_str())
    };
    CheckResult {
        name: "process_role_backend",
        status: CheckStatus::Pass,
        detail: Some(detail),
        hint: None,
    }
}

/// Queue-pinning coverage report (issue #1623, AC6). When `jobs.pin` is set,
/// each process only claims jobs from the pinned queues. This check is
/// **INFORMATIONAL-ONLY**: it always returns [`CheckStatus::Pass`] and never
/// Warns or Fails, so `--strict` can never promote it. A config-only,
/// per-process `doctor` CLI structurally cannot soundly hard-fail on coverage —
/// it can't see sibling worker tiers (a valid multi-tier subset pin) and can't
/// see `#[job(queue = "…")]`-declared queues (which the runtime appends to the
/// effective schedule and drains), so any hard-fail it asserts false-positives
/// on valid deployments. The authoritative zero-coverage guard is instead the
/// runtime startup warning (`warn_pinned_uncovered_queues` in
/// `autumn/src/job.rs`), which has the job registry and the real effective
/// schedule. This check only reports, for operator awareness, what this process
/// claims and which queues it does not.
///
/// Gated on the resolved [`ProcessRole`]: a web-only role
/// ([`runs_workers`](ProcessRole::runs_workers) `== false`) runs zero job
/// workers, so queue-pinning coverage is irrelevant to it — it passes with a
/// not-applicable note and never evaluates the pin.
fn check_queue_coverage(
    role: ProcessRole,
    configured_queues: &[String],
    pin: &[String],
) -> CheckResult {
    // Web-only role: runs no job workers at all, so queue pinning cannot leave a
    // queue uncovered *in this process* — the check is not applicable. Do not
    // evaluate pin/empty-schedule; this must never fail `--strict`.
    if !role.runs_workers() {
        return CheckResult {
            name: "jobs_queue_coverage",
            status: CheckStatus::Pass,
            detail: Some(
                "web-only role runs no job workers; queue pinning not applicable".to_string(),
            ),
            hint: None,
        };
    }

    // Unpinned (empty/absent pin): the process drains every configured queue.
    if pin.is_empty() {
        return CheckResult {
            name: "jobs_queue_coverage",
            status: CheckStatus::Pass,
            detail: Some("no jobs.pin; drains all configured queues".to_string()),
            hint: None,
        };
    }

    let known: std::collections::HashSet<&str> =
        configured_queues.iter().map(String::as_str).collect();
    let pinned: std::collections::HashSet<&str> = pin.iter().map(String::as_str).collect();

    // Informational only. A config-only, per-process `doctor` run structurally cannot
    // hard-fail on queue coverage, so this check never returns Warn or Fail, and
    // `--strict` can never promote it:
    //   * It sees one process and cannot know what sibling worker tiers drain, so a
    //     subset pin may be a valid multi-tier deployment.
    //   * It reads only `[jobs.queues]` and cannot see `#[job(queue = "…")]` queues,
    //     which the runtime appends to the effective schedule and drains, so a pin to
    //     a queue absent from `[jobs.queues]` is not necessarily an empty schedule.
    // The authoritative zero-coverage guard is the runtime startup warning
    // (`warn_pinned_uncovered_queues` in autumn/src/job.rs), which has the job registry
    // and the real schedule. Here we report only, for operator awareness: configured
    // queues this pin does not claim, and pinned queues absent from `[jobs.queues]`.
    let unclaimed: Vec<&str> = configured_queues
        .iter()
        .map(String::as_str)
        .filter(|q| !pinned.contains(q))
        .collect();
    let unknown_pinned: Vec<&str> = pin
        .iter()
        .map(String::as_str)
        .filter(|q| !known.contains(q))
        .collect();

    let mut parts: Vec<String> = Vec::new();
    if unclaimed.is_empty() {
        parts.push(format!(
            "jobs.pin={pin:?} claims every configured queue {configured_queues:?}"
        ));
    } else {
        parts.push(format!(
            "jobs.pin={pin:?} does not claim configured queue(s): {} — ensure another \
             worker tier covers them (doctor sees only this process and cannot verify \
             topology-wide coverage)",
            unclaimed.join(", ")
        ));
    }
    if !unknown_pinned.is_empty() {
        parts.push(format!(
            "pinned queue(s) not in [jobs.queues]: {} — these may be #[job(queue = \"…\")]-\
             declared queues that the runtime drains (doctor is config-only and cannot verify)",
            unknown_pinned.join(", ")
        ));
    }
    parts.push(
        "informational only: the runtime startup warning is the authoritative queue-coverage check"
            .to_string(),
    );
    CheckResult {
        name: "jobs_queue_coverage",
        status: CheckStatus::Pass,
        detail: Some(parts.join("; ")),
        hint: None,
    }
}

/// A declared fleet topology (#1756): the `jobs.pin` of every worker tier in the
/// deployment. Each inner `Vec` is one tier's pin; an **empty** inner `Vec` is an
/// *unpinned* tier that drains every queue. Declared by the operator under
/// `[jobs.fleet] tiers = [["critical"], ["bulk", "default"]]`.
///
/// This is the input that makes a `--strict` hard-fail SOUND: with every tier's
/// pin known, doctor reasons about **topology-wide** (union) coverage instead of
/// a single process, so a valid multi-tier subset split is no longer mistaken for
/// a gap. Absent this declaration the coverage check stays informational-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FleetTopology {
    tiers: Vec<Vec<String>>,
    /// `[jobs.fleet] tiers` was present but not a list of lists, so no topology
    /// could be read from it. Reported as a failure rather than treated as
    /// "nothing declared" — see [`resolve_fleet_topology`].
    malformed: bool,
}

impl FleetTopology {
    /// A topology that could not be parsed. Carries no tiers, so it can never be
    /// mistaken for coverage.
    const fn malformed() -> Self {
        Self {
            tiers: Vec::new(),
            malformed: true,
        }
    }

    /// At least one declared tier runs job workers. A topology with no tiers
    /// declares nothing coverable, so it cannot prove a gap.
    const fn runs_any_worker(&self) -> bool {
        !self.tiers.is_empty()
    }

    /// `true` when any tier is unpinned (empty pin) — such a tier drains every
    /// queue, so union coverage is total.
    fn has_unpinned_tier(&self) -> bool {
        self.tiers.iter().any(Vec::is_empty)
    }

    /// The union of all pinned queue names across every tier.
    fn pinned_union(&self) -> BTreeSet<String> {
        self.tiers.iter().flatten().cloned().collect()
    }
}

/// Topology-aware queue-coverage check (#1756). Restores a **sound** `--strict`
/// hard-fail on top of the informational-only per-process report.
///
/// When the operator declares the fleet topology (`[jobs.fleet] tiers`) — and,
/// via a jobs manifest or inline list, the compiled `#[job(queue = "…")]` set —
/// doctor can PROVE a genuine zero-coverage gap: it computes the queues that must
/// be drained (configured `[jobs.queues]` ∪ job-declared) and the queues covered
/// by the union of all tier pins, then FAILS only if some needed queue is drained
/// by no tier anywhere. This has zero false positives on valid deployments:
///
/// * a valid multi-tier subset split (one tier `critical`, another
///   `bulk,default`) has full union coverage → Pass;
/// * a queue named in a pin but absent from `[jobs.queues]` (a job-declared
///   queue) is in the needed set only when the manifest surfaces it, and is
///   covered by the pinning tier either way → never a false fail.
///
/// **Regression guard:** when no fleet topology is declared (`fleet == None`)
/// this delegates verbatim to the informational-only [`check_queue_coverage`],
/// so absent the topology input the check behaves exactly as it does today.
fn check_queue_coverage_topology(
    role: ProcessRole,
    configured_queues: &[String],
    pin: &[String],
    declared_queues: &[String],
    fleet: Option<&FleetTopology>,
) -> CheckResult {
    // No topology declared → informational-only, exactly as today. The hard-fail
    // only activates once the operator supplies the topology that makes coverage
    // provable, so existing deployments never regress.
    if let Some(fleet) = fleet.filter(|f| f.malformed) {
        let _ = fleet;
        return CheckResult {
            name: "jobs_queue_coverage",
            status: CheckStatus::Fail,
            detail: Some(
                "[jobs.fleet] tiers is present but is not a list of lists, so no fleet \
                 topology could be read and topology-wide queue coverage cannot be checked"
                    .to_string(),
            ),
            hint: Some(
                "Write one list per worker tier, e.g. tiers = [[\"critical\"], [\"bulk\", \"default\"]] — a flat list like tiers = [\"critical\"] is a single tier's pin, not a topology",
            ),
        };
    }
    let Some(fleet) = fleet.filter(|f| f.runs_any_worker()) else {
        return check_queue_coverage(role, configured_queues, pin);
    };

    // Needed = configured `[jobs.queues]` ∪ job-declared queues. Declared queues
    // only ADD to the needed set, so a missing/partial manifest can only shrink
    // it — never manufacture a gap (keeps the check zero-false-positive).
    let needed: BTreeSet<String> = configured_queues
        .iter()
        .chain(declared_queues.iter())
        .cloned()
        .collect();

    // Covered = union of every tier's pin. An unpinned tier drains everything, so
    // union coverage is total.
    let gap: Vec<String> = if fleet.has_unpinned_tier() {
        Vec::new()
    } else {
        let covered = fleet.pinned_union();
        needed
            .iter()
            .filter(|q| !covered.contains(*q))
            .cloned()
            .collect()
    };

    let tier_count = fleet.tiers.len();
    if gap.is_empty() {
        CheckResult {
            name: "jobs_queue_coverage",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "declared fleet topology ({tier_count} tier(s)) drains every needed queue {:?} \
                 (configured + #[job(queue)]-declared); coverage is provable topology-wide",
                needed.iter().collect::<Vec<_>>()
            )),
            hint: None,
        }
    } else {
        CheckResult {
            name: "jobs_queue_coverage",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "declared fleet topology ({tier_count} tier(s)) leaves queue(s) {} drained by no \
                 worker tier; jobs enqueued to them will accumulate forever",
                gap.join(", ")
            )),
            hint: Some(
                "Add the uncovered queue(s) to a tier's jobs.pin in [jobs.fleet].tiers, or run an \
                 unpinned tier that drains everything",
            ),
        }
    }
}

fn check_replica_migration_versions(
    primary_latest: Option<&str>,
    replica_latest: Option<&str>,
    fallback: DoctorReplicaFallback,
) -> CheckResult {
    match (primary_latest, replica_latest) {
        (Some(primary), Some(replica)) if primary == replica => CheckResult {
            name: "replica_migrations",
            status: CheckStatus::Pass,
            detail: Some(format!("replica replayed latest migration {primary}")),
            hint: None,
        },
        (Some(primary), Some(replica)) => {
            let detail = format!(
                "replica migration version {replica} is behind primary migration version {primary}"
            );
            match fallback {
                DoctorReplicaFallback::FailReadiness => CheckResult {
                    name: "replica_migrations",
                    status: CheckStatus::Fail,
                    detail: Some(detail),
                    hint: Some(
                        "Wait for the replica to replay the latest migration before admitting traffic",
                    ),
                },
                DoctorReplicaFallback::Primary => CheckResult {
                    name: "replica_migrations",
                    status: CheckStatus::Warn,
                    detail: Some(format!("{detail}; reads may fall back to primary")),
                    hint: Some(
                        "Restore replica replay or set replica_fallback = \"fail_readiness\" for stricter rollout gates",
                    ),
                },
            }
        }
        _ => CheckResult {
            name: "replica_migrations",
            status: CheckStatus::Warn,
            detail: Some("could not determine primary and replica migration versions".into()),
            hint: Some("Ensure both roles expose __diesel_schema_migrations"),
        },
    }
}

/// Whether a `sqlite://` target is a valid backend for the doctor's resolved
/// database topology, by **delegating to the real boot-time rule**
/// [`autumn_web::config::database_backend_consistency`] rather than re-deriving
/// it here.
///
/// `SQLite` is single-writer/single-host: it has no read-replica role, cannot be
/// horizontally sharded, and cannot be mixed with a Postgres role. So a
/// `sqlite://` URL only boots when it is the lone primary in a single-role,
/// single-backend config. Rather than mirror that rule (which drifted three
/// times — F19 missed the replica, F23 missed shards, F26 missed the legacy
/// `database.url`), this asks the exact function `DatabaseConfig::validate` uses:
/// doctor now agrees with boot for every role/backend mismatch (legacy `url`,
/// `primary_url`, `replica_url`, read replicas, shards, and anything added
/// later).
///
/// The effective primary backend is `primary_url` if set, else the legacy `url`
/// (mirroring `DatabaseConfig::effective_primary_url`). Returns `true` only when
/// the effective primary is `SQLite` *and* the real validation accepts the
/// topology; a non-`SQLite` primary returns `false` because this helper only gates
/// the `SQLite` Pass path (Postgres roles keep their TCP reachability checks).
fn sqlite_primary_target_is_bootable(
    legacy_url: Option<&str>,
    primary_url: Option<&str>,
    replica_url: Option<&str>,
    has_shards: bool,
) -> bool {
    use autumn_web::config::{DatabaseBackend, database_backend_consistency};
    // Only gates the SQLite Pass: a non-SQLite effective primary is handled by
    // the ordinary host:port reachability path.
    if primary_url.or(legacy_url).and_then(DatabaseBackend::detect) != Some(DatabaseBackend::Sqlite)
    {
        return false;
    }
    // Bootable iff the real boot-time rule accepts every configured role.
    database_backend_consistency(legacy_url, primary_url, replica_url, has_shards).is_ok()
}

/// Finding F27: run the boot-time backend-consistency rule as an
/// **unconditional** doctor check, independent of which backend is the effective
/// primary.
///
/// `DatabaseConfig::validate` refuses to boot any config whose roles mix
/// backends: a `sqlite://` primary with a Postgres legacy `database.url` (F26), a
/// Postgres primary with a `sqlite://` legacy `database.url` (F27, the mirror
/// image), a `SQLite` read replica or shard, etc. The `SQLite`-only bootability
/// gate ([`sqlite_primary_target_is_bootable`]) only fires when the *effective
/// primary* is `SQLite`, so a Postgres-primary mismatch slipped through: with the
/// Postgres primary reachable, `autumn doctor --strict` could greenlight a config
/// the app rejects at boot because the legacy `SQLite` role was never validated.
///
/// This check asks the exact function boot uses on **every** resolved role, so
/// doctor Fails the same mismatched configs boot Fails, whatever the primary
/// backend. It is the authoritative signal: when it Fails, `run()` withholds the
/// per-role connectivity Pass so a reachable primary can't contradict it.
fn check_database_backend_consistency(
    legacy_url: Option<&str>,
    primary_url: Option<&str>,
    replica_url: Option<&str>,
    has_shards: bool,
) -> CheckResult {
    match autumn_web::config::database_backend_consistency(
        legacy_url,
        primary_url,
        replica_url,
        has_shards,
    ) {
        Ok(()) => CheckResult {
            name: "db_backend_consistency",
            status: CheckStatus::Pass,
            detail: Some("all configured database roles use the same backend".to_owned()),
            hint: None,
        },
        Err(msg) => CheckResult {
            name: "db_backend_consistency",
            status: CheckStatus::Fail,
            detail: Some(msg),
            hint: Some(
                "Align every database role (database.url, primary_url, replica_url, shards) on a \
                 single backend — this is exactly what the app validates at boot",
            ),
        },
    }
}

/// Finding F28: a malformed `[[database.shards]]` entry (a missing `primary_url`
/// or `name`, a non-table entry, a duplicate shard name) is REJECTED by
/// [`crate::migrate::try_resolve_shard_database_urls_from_sources`] — the exact
/// resolver `autumn migrate` and the runtime config loader use — so it is a
/// boot-blocking problem, not "no shards".
///
/// Before F28, `run()` collapsed that `Err` to `has_shards = false`, so a
/// malformed shard entry on a `sqlite://` primary was treated as an unsharded
/// lone-primary config: the backend-consistency check Passed and the `SQLite`
/// primary still received the no-host connectivity Pass, letting `autumn doctor
/// --strict` greenlight a topology the app cannot boot. This turns the
/// resolver's rejection into an authoritative doctor Fail (like the F27
/// `backend_consistent` gate, `run()` also withholds the per-role connectivity
/// Pass when this fires) so doctor agrees with boot.
fn check_shard_config_resolvable(resolver_error: &str) -> CheckResult {
    CheckResult {
        name: "db_shard_config",
        status: CheckStatus::Fail,
        detail: Some(format!(
            "[[database.shards]] is malformed — doctor cannot validate the database topology: \
             {resolver_error}"
        )),
        hint: Some(
            "Fix the [[database.shards]] entries so each has a string `name` and `primary_url` \
             with unique names — the app rejects the same config at boot",
        ),
    }
}

fn check_db_role_connectivity(
    role: &'static str,
    database_url: &str,
    reachable: impl Fn(&str, u16) -> bool,
    sqlite_target_bootable: bool,
) -> CheckResult {
    // A `sqlite://` target is a valid backend (#1614): it names a local file, not a
    // host:port service, so TCP reachability and the "switch to postgres://" hint do
    // not apply. The runtime pool that serves such a target is built under the
    // `sqlite` feature (#1905); doctor only confirms the target here, and must not
    // mislead a SQLite app into thinking its URL is malformed.
    //
    // But SQLite is single-writer and single-host: `DatabaseConfig::validate` rejects
    // it as a read replica, alongside a Postgres role, or in any mixed-backend
    // topology (F19). Pass a SQLite target only when it is the bootable lone primary;
    // otherwise Fail, mirroring `validate`, so `doctor --strict` cannot greenlight a
    // config that cannot boot.
    if autumn_web::config::DatabaseBackend::detect(database_url)
        == Some(autumn_web::config::DatabaseBackend::Sqlite)
    {
        if sqlite_target_bootable {
            return CheckResult {
                name: "db_connectivity",
                status: CheckStatus::Pass,
                detail: Some(format!(
                    "{role} database is a local SQLite target (no host:port to reach)"
                )),
                hint: None,
            };
        }
        return CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "{role} database is a SQLite target, but SQLite is single-writer/single-host: \
                 read replicas and shards require the postgres backend, and every database role \
                 must use the same backend"
            )),
            hint: Some(
                "Use a single SQLite database (no replica_url), or switch every database role to \
                 postgres://",
            ),
        };
    }
    match parse_db_host_port(database_url) {
        None => CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "cannot parse {role} database URL to extract host:port"
            )),
            hint: Some("Ensure database URLs in autumn.toml use postgres:// or postgresql://"),
        },
        Some((host, port)) if reachable(&host, port) => CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Pass,
            detail: Some(format!("{role} database reachable at {host}:{port}")),
            hint: None,
        },
        Some((host, port)) => CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "cannot connect to {role} database at {host}:{port}"
            )),
            hint: Some("Start Postgres and verify the configured database role URL"),
        },
    }
}

fn check_db_connectivity(database_url: &str, sqlite_target_bootable: bool) -> CheckResult {
    check_db_role_connectivity(
        "primary",
        database_url,
        tcp_reachable,
        sqlite_target_bootable,
    )
}

fn check_pending_migrations(database_url: &str) -> CheckResult {
    match std::process::Command::new("diesel")
        .args(["migration", "pending"])
        .env("DATABASE_URL", database_url)
        .output()
    {
        Err(_) => CheckResult {
            name: "pending_migrations",
            status: CheckStatus::Warn,
            detail: Some("diesel CLI not found; cannot check pending migrations".into()),
            hint: Some(
                "Install diesel_cli: `cargo install diesel_cli --no-default-features --features postgres`",
            ),
        },
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let pending = stdout.lines().filter(|l| !l.trim().is_empty()).count();
            if pending == 0 {
                CheckResult {
                    name: "pending_migrations",
                    status: CheckStatus::Pass,
                    detail: Some("no pending migrations".into()),
                    hint: None,
                }
            } else {
                CheckResult {
                    name: "pending_migrations",
                    status: CheckStatus::Warn,
                    detail: Some(format!("{pending} pending migration(s)")),
                    hint: Some("Run `autumn migrate` to apply pending migrations"),
                }
            }
        }
        Ok(_) => CheckResult {
            name: "pending_migrations",
            status: CheckStatus::Warn,
            detail: Some("diesel migration pending returned non-zero".into()),
            hint: Some("Run `autumn migrate` to apply pending migrations"),
        },
    }
}

/// `SQLite`-target variant of the pending-migration probe (`SQLite` foundation,
/// issue #1614). [`check_pending_migrations`] shells out to `diesel migration
/// pending` and, when the diesel CLI is absent, points users at
/// `cargo install diesel_cli ... --features postgres` — a Postgres-specific
/// hint/`--strict` failure that doesn't apply to a `SQLite` app. `SQLite`
/// migration checks are not yet wired (tracked in #1906), so this is an honest
/// informational Pass with a deferred note rather than the Postgres diesel probe
/// or a silent claim that migrations are up to date.
fn check_pending_migrations_sqlite() -> CheckResult {
    CheckResult {
        name: "pending_migrations",
        status: CheckStatus::Pass,
        detail: Some(
            "SQLite app: migration checks are not yet wired; skipping the Postgres diesel probe"
                .into(),
        ),
        hint: Some(
            "SQLite migration support is tracked in https://github.com/autumn-foundation/autumn/issues/1906",
        ),
    }
}

fn check_replica_migrations(
    primary_url: &str,
    replica_url: &str,
    fallback: DoctorReplicaFallback,
) -> CheckResult {
    let primary_latest = latest_applied_migration_version(primary_url);
    let replica_latest = latest_applied_migration_version(replica_url);
    check_replica_migration_versions(
        primary_latest.as_deref(),
        replica_latest.as_deref(),
        fallback,
    )
}

fn latest_applied_migration_version(database_url: &str) -> Option<String> {
    parse_latest_applied_migration_version(&diesel_migration_list(database_url)?)
}

/// The `diesel migration list` output for `database_url`, or `None` when the
/// binary is absent, the database is unreachable, or the command failed.
///
/// Best effort by design: every caller treats "nothing known" as "raise no
/// finding", which makes a missing `diesel` CLI impossible to mistake for a
/// clean database.
fn diesel_migration_list(database_url: &str) -> Option<String> {
    let output = std::process::Command::new("diesel")
        .args(["migration", "list"])
        .env("DATABASE_URL", database_url)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_latest_applied_migration_version(output: &str) -> Option<String> {
    parse_applied_migration_tokens(output)
        .max()
        .map(std::borrow::ToOwned::to_owned)
}

/// Parse (host, port) from a Postgres connection URL.
pub fn parse_db_host_port(url: &str) -> Option<(String, u16)> {
    // Expect: postgres://[user:pass@]host[:port]/db
    let without_scheme = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))?;

    // Drop everything after the first `/` (the database name).
    let authority = without_scheme.split('/').next()?;

    // Drop user:pass@ prefix if present.
    let host_port = authority
        .rfind('@')
        .map_or(authority, |at| &authority[at + 1..]);

    if let Some(rest) = host_port.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after_bracket = &rest[close + 1..];
        let port = if let Some(port) = after_bracket.strip_prefix(':') {
            port.parse().ok()?
        } else if after_bracket.is_empty() {
            5432
        } else {
            return None;
        };
        return Some((host.to_owned(), port));
    }

    match host_port.matches(':').count() {
        1 => {
            let (host, port) = host_port.split_once(':')?;
            Some((host.to_owned(), port.parse().ok()?))
        }
        _ => Some((host_port.to_owned(), 5432)),
    }
}

/// Resolve the configured HTTP port from `autumn.toml` (fallback: 3000).
fn resolve_server_port() -> u16 {
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| {
            t.get("server")
                .and_then(|s| s.get("port"))
                .and_then(toml::Value::as_integer)
                .and_then(|p| u16::try_from(p).ok())
        })
        .unwrap_or(3000)
}

fn read_autumn_toml_table() -> Option<toml::Table> {
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|contents| toml::from_str::<toml::Table>(&contents).ok())
}

fn deep_merge(target: &mut toml::Table, source: &toml::Table) {
    for (k, v) in source {
        if let (Some(source_tbl), Some(target_tbl)) = (
            v.as_table(),
            target.get_mut(k).and_then(|t| t.as_table_mut()),
        ) {
            deep_merge(target_tbl, source_tbl);
            continue;
        }
        target.insert(k.clone(), v.clone());
    }
}

fn get_merged_toml_table(profile: &str) -> toml::Table {
    get_merged_toml_table_profiles(&[profile])
}

/// Merges config from all given profile names in order (last wins).
///
/// Each profile name contributes `[profile.{name}]` from `autumn.toml` and
/// `autumn-{name}.toml`. The base `autumn.toml` top-level is applied once
/// before the per-profile layers.
fn get_merged_toml_table_profiles(profiles: &[&str]) -> toml::Table {
    let mut merged = toml::Table::new();

    let base_toml = std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok());

    // 1. Base autumn.toml top-level (applied once)
    if let Some(ref table) = base_toml {
        deep_merge(&mut merged, table);
    }

    for &profile in profiles {
        // 2. Base autumn.toml [profile.{name}]
        if let Some(prof) = base_toml
            .as_ref()
            .and_then(|t| t.get("profile"))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get(profile))
            .and_then(toml::Value::as_table)
        {
            deep_merge(&mut merged, prof);
        }

        // 3. autumn-{name}.toml
        if let Some(table) = std::fs::read_to_string(format!("autumn-{profile}.toml"))
            .ok()
            .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        {
            deep_merge(&mut merged, &table);
        }
    }

    merged
}

/// Inline `[profile.{name}]` lookup order, mirroring the runtime config loader's
/// `profile_lookup_names`: the canonical spelling is applied **last** so it wins
/// (`production` then `prod`; `development` then `dev`).
fn inline_profile_lookup_names(profile: &str) -> Vec<&str> {
    match profile {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Ordered `autumn-{name}.toml` override-file lookup, mirroring the runtime's
/// `profile_override_file_lookup_names`: only the **first existing** file is
/// loaded, preferring the spelling the operator actually selected.
fn override_file_lookup_names(profile: &str, selected_input: &str) -> Vec<String> {
    match profile {
        "prod" if selected_input.eq_ignore_ascii_case("production") => {
            vec!["production".to_owned(), "prod".to_owned()]
        }
        "prod" => vec!["prod".to_owned(), "production".to_owned()],
        "dev" if selected_input.eq_ignore_ascii_case("development") => {
            vec!["development".to_owned(), "dev".to_owned()]
        }
        "dev" => vec!["dev".to_owned(), "development".to_owned()],
        other => vec![other.to_owned()],
    }
}

/// Build the merged TOML the same way the runtime config loader layers profile
/// sources, so doctor evaluates exactly what the app will boot with. Mirrors
/// `autumn_web::config`: inline `[profile.{name}]` sections are applied in alias
/// order (canonical spelling wins), and exactly one `autumn-{name}.toml` file is
/// loaded — the first that exists, preferring the selected spelling.
///
/// Reads each config file from the process CWD. Use
/// [`get_merged_toml_table_runtime_with`] to honor `AUTUMN_MANIFEST_DIR` the way
/// the runtime loader does.
fn get_merged_toml_table_runtime(normalized_profile: &str, selected_input: &str) -> toml::Table {
    get_merged_toml_table_runtime_with(normalized_profile, selected_input, |f| {
        std::path::PathBuf::from(f)
    })
}

/// Like [`get_merged_toml_table_runtime`], but resolves each config filename
/// through `resolve_path`, so a caller can honor `AUTUMN_MANIFEST_DIR` (the
/// runtime prefers the manifest dir for `autumn.toml` / `autumn-{name}.toml`,
/// falling back to the CWD). Injectable so tests can point the lookup at a
/// temp dir without mutating the process environment.
fn get_merged_toml_table_runtime_with(
    normalized_profile: &str,
    selected_input: &str,
    resolve_path: impl Fn(&str) -> std::path::PathBuf,
) -> toml::Table {
    let mut merged = toml::Table::new();

    let base_toml = std::fs::read_to_string(resolve_path("autumn.toml"))
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok());

    // 1. Base autumn.toml top-level.
    if let Some(ref table) = base_toml {
        deep_merge(&mut merged, table);
    }

    // 2. Inline [profile.{name}] sections, canonical spelling applied last (wins).
    for profile_name in inline_profile_lookup_names(normalized_profile) {
        if let Some(prof) = base_toml
            .as_ref()
            .and_then(|t| t.get("profile"))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get(profile_name))
            .and_then(toml::Value::as_table)
        {
            deep_merge(&mut merged, prof);
        }
    }

    // 3. Exactly one autumn-{name}.toml override file (first existing).
    for profile_name in override_file_lookup_names(normalized_profile, selected_input) {
        if let Some(table) =
            std::fs::read_to_string(resolve_path(&format!("autumn-{profile_name}.toml")))
                .ok()
                .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        {
            deep_merge(&mut merged, &table);
            break;
        }
    }

    merged
}

/// Resolve a config filename the way the runtime loader does: prefer
/// `$AUTUMN_MANIFEST_DIR/<filename>` when that variable is set (through the
/// crate's [`autumn_web::config::OsEnv`], which surfaces the value baked in by
/// `#[autumn_web::main]` for installed apps) and the file exists there;
/// otherwise fall back to the process CWD. Mirrors `config`'s
/// `find_config_file_named`, so doctor reads the same `autumn.toml` the app
/// boots from when launched with `AUTUMN_MANIFEST_DIR`.
fn resolve_config_path_manifest_aware(filename: &str) -> std::path::PathBuf {
    use autumn_web::config::Env as _;
    if let Ok(manifest_dir) = autumn_web::config::OsEnv.var("AUTUMN_MANIFEST_DIR") {
        let candidate = std::path::PathBuf::from(manifest_dir).join(filename);
        if candidate.exists() {
            return candidate;
        }
    }
    std::path::PathBuf::from(filename)
}

pub struct DoctorOAuth2Provider {
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
}

fn resolve_oauth2_providers() -> Vec<DoctorOAuth2Provider> {
    let raw_profile = std::env::var("AUTUMN_ENV")
        .or_else(|_| std::env::var("AUTUMN_PROFILE"))
        .unwrap_or_else(|_| "dev".to_owned());
    let profile = match raw_profile.trim().to_lowercase().as_str() {
        "production" => "prod".to_owned(),
        "development" => "dev".to_owned(),
        other => other.to_owned(),
    };
    let merged_toml = get_merged_toml_table(&profile);
    resolve_oauth2_providers_from_sources(
        |key| std::env::var(key).ok().filter(|v| !v.is_empty()),
        Some(&merged_toml),
        &profile,
    )
}

fn resolve_oauth2_providers_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
    profile: &str,
) -> Vec<DoctorOAuth2Provider>
where
    F: Fn(&str) -> Option<String>,
{
    // Load credentials if available
    let credentials =
        autumn_web::credentials::load_credentials(profile, std::path::Path::new(".")).ok();

    table
        .and_then(|t| t.get("auth"))
        .and_then(toml::Value::as_table)
        .and_then(|auth| auth.get("oauth2"))
        .and_then(toml::Value::as_table)
        .map(|providers| {
            providers
                .iter()
                .map(|(name, val)| {
                    let mut client_id = val
                        .as_table()
                        .and_then(|t| t.get("client_id"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let mut client_secret = val
                        .as_table()
                        .and_then(|t| t.get("client_secret"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_owned();

                    let normalized_name = name
                        .chars()
                        .map(|c| if c.is_alphanumeric() { c } else { '_' })
                        .collect::<String>()
                        .to_lowercase();
                    let upper = normalized_name.to_uppercase();

                    // Get values from env vars
                    let env_id_key = format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID");
                    if let Some(id) = env_var(&env_id_key) {
                        client_id = id;
                    }
                    let env_secret_key = format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_SECRET");
                    if let Some(sec) = env_var(&env_secret_key) {
                        client_secret = sec;
                    }

                    // Get values from credentials
                    if let Some(ref creds) = credentials {
                        let id_key = format!("oauth2_{normalized_name}_client_id");
                        if client_id.is_empty() {
                            if let Some(id) = creds.get::<String>(&id_key) {
                                client_id = id;
                            } else if let Some(id) =
                                creds.get::<String>(&format!("oauth2_{name}_client_id"))
                            {
                                client_id = id;
                            }
                        }

                        let secret_key = format!("oauth2_{normalized_name}_client_secret");
                        if client_secret.is_empty() {
                            if let Some(sec) = creds.get::<String>(&secret_key) {
                                client_secret = sec;
                            } else if let Some(sec) =
                                creds.get::<String>(&format!("oauth2_{name}_client_secret"))
                            {
                                client_secret = sec;
                            }
                        }
                    }

                    DoctorOAuth2Provider {
                        name: name.clone(),
                        client_id,
                        client_secret,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a Rust source file declares the `#[mailer(list_unsubscribe = ...)]`
/// attribute (whitespace-insensitive), as opposed to merely calling the
/// `.list_unsubscribe(...)` builder method or mentioning it in a comment.
pub fn file_declares_list_unsubscribe(content: &str) -> bool {
    // Strip line and block comments so a commented-out attribute (`//` or
    // `/* ... */`) does not trip a false positive.
    let without_comments = strip_rust_comments(content);
    let collapsed: String = without_comments
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    collapsed.contains("mailer(list_unsubscribe")
}

/// Best-effort removal of `//` line and `/* ... */` block comments. Intended for
/// heuristic source scanning, not as a Rust lexer; it does not special-case
/// comment markers inside string or char literals.
fn strip_rust_comments(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            // Line comment: skip to (and keep) the newline.
            for n in chars.by_ref() {
                if n == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            // Block comment: skip until the closing `*/`.
            chars.next();
            let mut prev = '\0';
            for n in chars.by_ref() {
                if prev == '*' && n == '/' {
                    break;
                }
                prev = n;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Detect whether any Rust source in the project (including workspace member
/// crates) declares `#[mailer(list_unsubscribe = "...")]`.
///
/// Walks from the project root, skipping build/VCS/vendor directories, so a
/// mailer in e.g. `crates/marketing/src` is found. Matches the attribute form
/// specifically (whitespace-insensitive) so builder calls or comments do not
/// trip a false positive.
fn detect_list_unsubscribe_usage() -> bool {
    scan_dir_for_list_unsubscribe(std::path::Path::new("."))
}

/// Recursively scan `root` for a source file declaring
/// `#[mailer(list_unsubscribe = "...")]`.
///
/// Skips build/VCS/vendor directories so a `cargo vendor` copy of the framework
/// (whose own tests declare a real list mailer) can't trip a false positive for
/// an application binary that registers none. Also skips `tests/` and `examples/`
/// trees: the runtime inventory only registers mailers compiled into the
/// application binary, so an integration-test or example fixture must not make
/// `autumn doctor --strict` require unsubscribe config for mail that can't be sent.
fn scan_dir_for_list_unsubscribe(root: &std::path::Path) -> bool {
    /// Directories never worth scanning for source.
    const SKIP_DIRS: &[&str] = &[
        "target",
        ".git",
        "node_modules",
        "dist",
        ".github",
        "vendor",
        "tests",
        "examples",
    ];
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SKIP_DIRS.contains(&n));
            if !skip && scan_dir_for_list_unsubscribe(&path) {
                return true;
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && std::fs::read_to_string(&path)
                .is_ok_and(|content| file_declares_list_unsubscribe(&content))
        {
            return true;
        }
    }
    false
}

/// Resolve `[mail]` unsubscribe destinations, preferring env overrides over the
/// merged toml table (mirroring the runtime's `AUTUMN_MAIL__*` precedence).
fn resolve_mail_unsubscribe(table: Option<&toml::Table>) -> (Option<String>, Option<String>) {
    let mail = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table);
    // Match the runtime's precedence: a *present* env var wins even when blank
    // (it clears the TOML value); only an *absent* env var falls back to TOML.
    let read = |env_key: &str, toml_key: &str| {
        std::env::var(env_key).map_or_else(
            |_| {
                mail.and_then(|m| m.get(toml_key))
                    .and_then(toml::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::borrow::ToOwned::to_owned)
            },
            |value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            },
        )
    };
    (
        read("AUTUMN_MAIL__UNSUBSCRIBE_BASE_URL", "unsubscribe_base_url"),
        read("AUTUMN_MAIL__UNSUBSCRIBE_MAILTO", "unsubscribe_mailto"),
    )
}

/// Resolve the effective `[mail] from` sender address, preferring an
/// `AUTUMN_MAIL__FROM` env override over the merged `[mail].from` (mirroring the
/// runtime's `AUTUMN_MAIL__*` precedence — a *present* env var wins even when
/// blank, clearing the TOML value; only an *absent* env var falls back to TOML).
/// Returns the trimmed non-empty value, or `None` when no usable `from` is set.
fn resolve_mail_from(table: Option<&toml::Table>) -> Option<String> {
    let mail = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table);
    std::env::var("AUTUMN_MAIL__FROM").map_or_else(
        |_| {
            mail.and_then(|m| m.get("from"))
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::borrow::ToOwned::to_owned)
        },
        |value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        },
    )
}

/// Resolve the effective `unsubscribe_token_ttl_days`, mirroring the runtime
/// precedence: `[mail].unsubscribe_token_ttl_days` (or the default of 30) is the
/// base, and a present, *parseable* `AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS`
/// overrides it. A present-but-invalid env value (blank or non-integer) is
/// ignored exactly as `apply_mail_env_overrides_with_env` does — it warns and
/// leaves the TOML/default in place — so doctor must not treat it as `0` and
/// block a deploy the app would boot with the effective positive TTL.
fn resolve_unsubscribe_token_ttl_days(table: Option<&toml::Table>) -> i64 {
    resolve_unsubscribe_token_ttl_days_from_sources(
        std::env::var("AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS").ok(),
        table,
    )
}

/// True when `[mail].unsubscribe_token_ttl_days` is present in the merged TOML
/// but is not an integer (e.g. a quoted string or a float).
///
/// The runtime deserializes this field as `i64` from the TOML *before* any
/// `AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS` override is applied (the override
/// mutates the already-typed value), so a non-integer TOML value fails config
/// loading at boot regardless of the env var. doctor reads the merged config as
/// an untyped table, so it must check the type explicitly instead of silently
/// defaulting (which would greenlight a deploy the app can't start).
fn unsubscribe_ttl_toml_type_invalid(table: Option<&toml::Table>) -> bool {
    table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table)
        .and_then(|m| m.get("unsubscribe_token_ttl_days"))
        .is_some_and(|v| v.as_integer().is_none())
}

/// True when `[mail].unsubscribe_base_url` or `[mail].unsubscribe_mailto` is
/// present in the merged TOML but is not a string (e.g. an integer or array).
///
/// The runtime deserializes these as `Option<String>`, so a present non-string
/// value fails config loading before boot. `resolve_mail_unsubscribe` only reads
/// `as_str()` and would otherwise treat such a value as absent, letting doctor
/// report `Pass` for a config the app can't start with — so flag it explicitly,
/// like the TTL type guard.
fn unsubscribe_dest_toml_type_invalid(table: Option<&toml::Table>) -> bool {
    let mail = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table);
    ["unsubscribe_base_url", "unsubscribe_mailto"]
        .iter()
        .any(|key| {
            mail.and_then(|m| m.get(*key))
                .is_some_and(|v| v.as_str().is_none())
        })
}

fn resolve_unsubscribe_token_ttl_days_from_sources(
    env_value: Option<String>,
    table: Option<&toml::Table>,
) -> i64 {
    const DEFAULT_TTL_DAYS: i64 = 30;
    let toml_or_default = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table)
        .and_then(|m| m.get("unsubscribe_token_ttl_days"))
        .and_then(toml::Value::as_integer)
        .unwrap_or(DEFAULT_TTL_DAYS);
    // Match the runtime's `val.parse::<i64>()` (no trim): an unparseable override
    // is ignored, falling back to the TOML/default value.
    if let Some(raw) = env_value
        && let Ok(days) = raw.parse::<i64>()
    {
        return days;
    }
    toml_or_default
}

/// Mirror of `Transport::from_env_value` (mail module, feature-gated so not
/// reachable from the CLI build): trim + lowercase and accept only a known
/// transport, returning its canonical spelling, else `None`.
fn parse_mail_transport(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "log" => Some("log"),
        "file" => Some("file"),
        "smtp" => Some("smtp"),
        "disabled" => Some("disabled"),
        _ => None,
    }
}

/// Resolve the effective mail transport the way the runtime does: a *valid* env
/// override wins, then a valid `[mail].transport`, then the profile smart-default
/// (`dev` → `log`, otherwise `disabled`). Invalid/whitespace values are ignored
/// just as `Transport::from_env_value` ignores them, so doctor does not treat a
/// malformed override as an active transport.
fn resolve_effective_mail_transport(
    env_value: Option<String>,
    toml_transport: Option<&str>,
    normalized_profile: &str,
) -> String {
    if let Some(raw) = env_value
        && let Some(parsed) = parse_mail_transport(&raw)
    {
        return parsed.to_owned();
    }
    if let Some(parsed) = toml_transport.and_then(parse_mail_transport) {
        return parsed.to_owned();
    }
    if normalized_profile == "dev" {
        "log".to_owned()
    } else {
        "disabled".to_owned()
    }
}

/// Whether a mail transport actually requires a `from` address to deliver.
///
/// Only `smtp` builds a real RFC 5322 message: autumn-web's `lettre_message`
/// errors with "mail from address is required" when `from` is absent, and it is
/// reached solely from the SMTP transport. The `log` and `file` transports
/// render `from` only when present and deliver without it; `disabled` delivers
/// nothing at all (and is already treated as an unusable transport). Mirrors
/// autumn-web `mail.rs`.
fn mail_transport_requires_from(transport: &str) -> bool {
    transport == "smtp"
}

fn resolve_database_topology_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> DoctorDatabaseTopology
where
    F: Fn(&str) -> Option<String>,
{
    let database = table
        .and_then(|t| t.get("database"))
        .and_then(toml::Value::as_table);

    let primary_url = first_env(
        &env_var,
        &[
            "AUTUMN_DATABASE__PRIMARY_URL",
            "AUTUMN_DATABASE__URL",
            "DATABASE_URL",
        ],
    )
    .or_else(|| first_toml_string(database, &["primary_url", "url"]));

    // The legacy `database.url` role, resolved as its OWN role (not collapsed
    // into `primary_url` above) so the backend-consistency delegation can see a
    // `primary_url`/`url` mismatch (finding F26). Mirrors the runtime env
    // mapping where `AUTUMN_DATABASE__URL` populates `database.url`; the
    // doctor-only `DATABASE_URL` fallback feeds `primary_url` only.
    let legacy_url = first_env(&env_var, &["AUTUMN_DATABASE__URL"])
        .or_else(|| first_toml_string(database, &["url"]));

    let replica_url = first_env(&env_var, &["AUTUMN_DATABASE__REPLICA_URL"])
        .or_else(|| first_toml_string(database, &["replica_url"]));

    let auto_migrate_in_production =
        first_env(&env_var, &["AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION"])
            .as_deref()
            .and_then(parse_config_bool)
            .or_else(|| database?.get("auto_migrate_in_production")?.as_bool())
            .unwrap_or(false);

    let replica_fallback = DoctorReplicaFallback::from_config_value(
        first_env(&env_var, &["AUTUMN_DATABASE__REPLICA_FALLBACK"])
            .or_else(|| first_toml_string(database, &["replica_fallback"]))
            .as_deref(),
    );

    DoctorDatabaseTopology {
        primary_url,
        replica_url,
        legacy_url,
        auto_migrate_in_production,
        replica_fallback,
    }
}

/// Resolve the selected process role and jobs backend from the same sources the
/// runtime reads: the `AUTUMN_ROLE` / `AUTUMN_JOBS__BACKEND` env vars take
/// precedence over the top-level `role` / `[jobs] backend` keys in the config
/// table. Defaults to [`ProcessRole::Combined`] and the `"local"` backend.
fn resolve_process_role_and_backend(table: Option<&toml::Table>) -> (ProcessRole, String) {
    resolve_process_role_and_backend_from_sources(
        |key| std::env::var(key).ok().filter(|value| !value.is_empty()),
        table,
    )
}

fn resolve_process_role_and_backend_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> (ProcessRole, String)
where
    F: Fn(&str) -> Option<String>,
{
    // Mirror the runtime (`apply_role_env_overrides_with_env`): an invalid
    // `AUTUMN_ROLE` env value is ignored and we fall through to the configured
    // TOML `role`, rather than short-circuiting straight to `Combined`.
    let role = first_env(&env_var, &["AUTUMN_ROLE"])
        .as_deref()
        .and_then(ProcessRole::from_env_value)
        .or_else(|| {
            first_toml_string(table, &["role"])
                .as_deref()
                .and_then(ProcessRole::from_env_value)
        })
        .unwrap_or(ProcessRole::Combined);

    let jobs = table
        .and_then(|t| t.get("jobs"))
        .and_then(toml::Value::as_table);
    let backend = first_env(&env_var, &["AUTUMN_JOBS__BACKEND"])
        .or_else(|| first_toml_string(jobs, &["backend"]))
        .unwrap_or_else(|| "local".to_owned());

    (role, backend)
}

/// Resolve the configured queue names and the effective `jobs.pin` for the
/// zero-coverage guard (#1623). `jobs.queues` may be a TOML array (strict) or a
/// `name = weight|table` map (weighted); either way we only need the names.
/// `AUTUMN_JOBS__PIN` (comma-separated) overrides the TOML `jobs.pin` array,
/// mirroring the runtime env override.
fn resolve_queues_and_pin(table: Option<&toml::Table>) -> (Vec<String>, Vec<String>) {
    // Do NOT drop empty env values here: a *present but empty* `AUTUMN_JOBS__PIN`
    // is an explicit override that clears the pin (see the pin-resolution logic
    // below and #2290). `std::env::var(key).ok()` returns `Some("")` for a
    // present-but-empty var and `None` for an absent one, mirroring the
    // runtime's `if let Ok(val) = env.var(...)` presence check.
    resolve_queues_and_pin_from_sources(|key| std::env::var(key).ok(), table)
}

fn resolve_queues_and_pin_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> (Vec<String>, Vec<String>)
where
    F: Fn(&str) -> Option<String>,
{
    let jobs = table
        .and_then(|t| t.get("jobs"))
        .and_then(toml::Value::as_table);

    let mut queues =
        jobs.and_then(|j| j.get("queues"))
            .map_or_else(Vec::new, |value| match value {
                toml::Value::Array(items) => items
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                toml::Value::Table(map) => map.keys().cloned().collect(),
                _ => Vec::new(),
            });
    // When `[jobs.queues]` is omitted the runtime still creates the implicit
    // `default` queue (`JobQueuesConfig::default()` == a single `"default"`
    // queue). Mirror that here so the coverage check can flag `default` as
    // uncovered when a zero-config app is pinned to some other queue — instead
    // of seeing no configured queues and silently passing (#1623).
    if queues.is_empty() {
        queues.push("default".to_owned());
    }

    // A PRESENT `AUTUMN_JOBS__PIN` (even empty) is an explicit override, matching
    // the runtime's `apply_jobs_env_overrides_with_env`: it does
    // `if let Ok(val) = env.var("AUTUMN_JOBS__PIN") { self.jobs.pin = val.split(',')…filter(non-empty)… }`,
    // so an empty string clears the pin entirely (the process becomes unpinned
    // and drains every queue). Only when the env var is *absent* do we fall back
    // to the TOML `jobs.pin` (#2290).
    let toml_pin = || {
        jobs.and_then(|j| j.get("pin"))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let pin = env_var("AUTUMN_JOBS__PIN").map_or_else(toml_pin, |csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    });

    (queues, pin)
}

/// Resolve the declared fleet topology (#1756) from `[jobs.fleet] tiers`. Each
/// entry is one worker tier's pin (a TOML array of queue names); an empty array
/// is an unpinned tier that drains every queue. Returns `None` when the section
/// is absent or declares no tiers, keeping the coverage check informational-only.
///
/// Chosen as a config section (rather than a repeatable CLI flag) because doctor
/// already resolves `jobs.queues` / `jobs.pin` from the merged active-profile
/// table, so the topology lives beside them, is profile-aware, and is checked
/// into the repo as one source of truth for the fleet.
fn resolve_fleet_topology(table: Option<&toml::Table>) -> Option<FleetTopology> {
    // A malformed `[jobs.fleet]` must NOT be silently dropped at ANY level.
    // Every `?` here would otherwise return `None`, which the caller reads as
    // "no topology declared" and answers with the informational-only report —
    // quietly switching the AC6 hard-fail off, with nothing reporting it. The
    // typed `JobFleetConfig` rejects each of these shapes at app boot, so a
    // pre-deploy gate that passes them has the ordering exactly backwards.
    let jobs = table
        .and_then(|t| t.get("jobs"))
        .and_then(toml::Value::as_table)?;
    // Genuinely absent: nothing declared, nothing to check.
    let fleet = jobs.get("fleet")?;
    let Some(fleet) = fleet.as_table() else {
        return Some(FleetTopology::malformed());
    };
    let declared = fleet.get("tiers")?;
    // `tiers = "critical"` — a bare value rather than a list of lists.
    let Some(declared) = declared.as_array() else {
        return Some(FleetTopology::malformed());
    };
    // `tiers = ["critical", "bulk"]` — the flat shape `jobs.pin` uses, sitting a
    // few lines above it in the same file, so an easy mistake to make.
    let mut tiers: Vec<Vec<String>> = Vec::with_capacity(declared.len());
    for entry in declared {
        let Some(tier) = entry.as_array() else {
            return Some(FleetTopology::malformed());
        };
        // Queue names must be strings. Dropping a non-string with `filter_map`
        // is the worst variant of the same silent-pass bug: `tiers = [[1]]`
        // collapses to an EMPTY tier, which `has_unpinned_tier` reads as a tier
        // that drains everything — so coverage becomes total and the check
        // passes unconditionally on a document the app refuses to boot with.
        let mut names = Vec::with_capacity(tier.len());
        for name in tier {
            let Some(name) = name.as_str() else {
                return Some(FleetTopology::malformed());
            };
            names.push(name.to_owned());
        }
        tiers.push(names);
    }
    if tiers.is_empty() {
        return None;
    }
    Some(FleetTopology {
        tiers,
        malformed: false,
    })
}

/// Resolve the compiled `#[job(queue = "…")]`-declared queue set for the
/// coverage check (#1756), so doctor's view of "queues that must be drained"
/// matches what the runtime actually drains. Two sources, in precedence order:
///
/// 1. `[jobs.fleet] manifest = "path"` — a jobs manifest the app emits (reusing
///    `effective_drained_queues_from_jobs` on the runtime side), a TOML file with a
///    `queues = [...]` array. This is the ground-truth set the runtime drains.
/// 2. `[jobs.fleet] declared_queues = ["…"]` — an inline list the operator
///    maintains by hand (the MVP path when no manifest is emitted).
///
/// A manifest that reads and parses wins outright, **including** when its
/// `queues` array is empty — that is the app answering "no job-declared queues",
/// not failing to answer. Only a manifest that says nothing at all (absent path,
/// unreadable, unparseable, or no `queues` array) falls through to the inline
/// list.
///
/// Returns an empty `Vec` when neither is present; an unknown declared set only
/// shrinks the needed set, so it can never cause a false failure.
fn resolve_declared_queues(table: Option<&toml::Table>) -> Vec<String> {
    resolve_declared_queues_from_sources(|p| std::fs::read_to_string(p).ok(), table)
}

fn resolve_declared_queues_from_sources<F>(read_file: F, table: Option<&toml::Table>) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(fleet) = table
        .and_then(|t| t.get("jobs"))
        .and_then(toml::Value::as_table)
        .and_then(|j| j.get("fleet"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };

    // 1. A jobs manifest the app emits: TOML `queues = [...]`.
    //
    // A manifest that reads and parses is authoritative even when its `queues` array
    // is empty — that is the app stating it declares no `#[job(queue = "…")]` queues
    // beyond the configured set, a real answer rather than a missing one. Falling
    // through to `declared_queues` there would let a stale hand-maintained entry
    // manufacture a coverage failure against ground truth, and would contradict the
    // documented precedence that the manifest wins when both are set.
    //
    // The fall-through is reserved for a manifest that genuinely says nothing: an
    // absent path, an unreadable file, unparseable TOML, or no `queues` array.
    if let Some(path) = fleet.get("manifest").and_then(toml::Value::as_str)
        && let Some(contents) = read_file(path)
        && let Ok(manifest) = toml::from_str::<toml::Table>(&contents)
        && let Some(queues) = manifest.get("queues").and_then(toml::Value::as_array)
    {
        return queues
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect();
    }

    // 2. Inline declared-queues list (MVP).
    fleet
        .get("declared_queues")
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn first_env<F>(env_var: &F, keys: &[&str]) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter().find_map(|key| {
        env_var(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn first_toml_string(database: Option<&toml::Table>, keys: &[&str]) -> Option<String> {
    let database = database?;
    keys.iter().find_map(|key| {
        database
            .get(*key)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(std::borrow::ToOwned::to_owned)
    })
}

fn parse_config_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Resolves the active profile names the runtime would load.
///
/// Returns `(canonical, selected_input, profiles)`:
/// - `canonical` — normalized name (`"prod"` / `"dev"` / custom)
/// - `selected_input` — the raw env-var value (used by `override_file_lookup_names`
///   to pick the right `autumn-{name}.toml` spelling when both exist)
/// - `profiles` — `[alias, canonical]` list for inline `[profile.{name}]` merging
fn resolve_active_profiles() -> (String, String, Vec<String>) {
    let raw_profile = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("AUTUMN_PROFILE").ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    let raw_trimmed = raw_profile.trim().to_owned();
    // Mirror normalize_profile_name from config.rs: built-in aliases are
    // matched case-insensitively; custom profile names preserve the original
    // user-specified case (so AUTUMN_ENV=QA loads [profile.QA], not [profile.qa]).
    let canonical = if raw_trimmed.eq_ignore_ascii_case("production")
        || raw_trimmed.eq_ignore_ascii_case("prod")
    {
        "prod".to_owned()
    } else if raw_trimmed.eq_ignore_ascii_case("development")
        || raw_trimmed.eq_ignore_ascii_case("dev")
        || raw_trimmed.is_empty()
    {
        "dev".to_owned()
    } else {
        raw_trimmed.clone()
    };
    // Mirror profile_lookup_names in config.rs: the runtime always loads the
    // legacy long-form alias first (e.g. "production") then the canonical short
    // form ("prod"), regardless of which spelling was used in the env var.
    let alias = match canonical.as_str() {
        "prod" => Some("production".to_owned()),
        "dev" => Some("development".to_owned()),
        _ => None,
    };
    let profiles: Vec<String> = alias
        .into_iter()
        .chain(std::iter::once(canonical.clone()))
        .collect();
    (canonical, raw_trimmed, profiles)
}

fn resolve_proxy_conflict_data() -> ProxyConflictData {
    // Mirror the runtime: load only the first existing override file so a
    // stale autumn-production.toml doesn't shadow autumn-prod.toml at startup.
    let (canonical, selected, _profiles) = resolve_active_profiles();
    let table = get_merged_toml_table_runtime(&canonical, &selected);

    let parse_csv_env = |var: &str| -> Option<Vec<String>> {
        std::env::var(var).ok().map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::borrow::ToOwned::to_owned)
                .collect()
        })
    };
    let parse_bool_env = |var: &str| -> Option<bool> {
        std::env::var(var)
            .ok()
            .map(|v| matches!(v.trim(), "true" | "1"))
    };

    let security = table.get("security");
    let tp_section = security.and_then(|s| s.get("trusted_proxies"));
    let rl_section = security.and_then(|s| s.get("rate_limit"));

    let toml_csv = |section: Option<&toml::Value>, key: &str| -> Vec<String> {
        section
            .and_then(|s| s.get(key))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(std::borrow::ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let toml_bool = |section: Option<&toml::Value>, key: &str| -> bool {
        section
            .and_then(|s| s.get(key))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
    };

    ProxyConflictData {
        new_ranges: parse_csv_env("AUTUMN_SECURITY__TRUSTED_PROXIES__RANGES")
            .unwrap_or_else(|| toml_csv(tp_section, "ranges")),
        new_trust_fwd: parse_bool_env("AUTUMN_SECURITY__TRUSTED_PROXIES__TRUST_FORWARDED_HEADERS")
            .unwrap_or_else(|| toml_bool(tp_section, "trust_forwarded_headers")),
        new_hops: std::env::var("AUTUMN_SECURITY__TRUSTED_PROXIES__TRUSTED_HOPS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .or_else(|| {
                tp_section
                    .and_then(|s| s.get("trusted_hops"))
                    .and_then(toml::Value::as_integer)
                    .and_then(|v| u32::try_from(v).ok())
            }),
        old_ranges: parse_csv_env("AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES")
            .unwrap_or_else(|| toml_csv(rl_section, "trusted_proxies")),
        old_trust_fwd: parse_bool_env("AUTUMN_SECURITY__RATE_LIMIT__TRUST_FORWARDED_HEADERS")
            .unwrap_or_else(|| toml_bool(rl_section, "trust_forwarded_headers")),
    }
}

/// Resolve which deprecated config keys are currently set in this project.
///
/// Uses [`autumn_web::config::detect_deprecated_keys_for`], which seeds the same
/// profile-default base layer the runtime loader applies before merging the
/// file table — so doctor evaluates the *same* layered config the runtime would
/// load (a key set only in a profile default is still detected).
fn resolve_deprecations() -> Vec<DoctorDeprecation> {
    use autumn_web::config::{OsEnv, deprecated_config_keys, detect_deprecated_keys_for};

    let (canonical, selected, _profiles) = resolve_active_profiles();
    // Use the same single-file lookup path the runtime uses: only the first
    // existing autumn-{profile}.toml is loaded, never both alias spellings.
    let file_table = get_merged_toml_table_runtime(&canonical, &selected);

    detect_deprecated_keys_for(&canonical, &file_table, &OsEnv, deprecated_config_keys())
        .into_iter()
        .map(|f| DoctorDeprecation {
            path: f.path,
            replacement: f.replacement,
            since: f.since,
            remove_in: f.remove_in,
        })
        .collect()
}

/// Resolve the rate-limit key strategy from config/env.
///
/// Priority:
/// 1. `AUTUMN_SECURITY__RATE_LIMIT__KEY_STRATEGY` env var
/// 2. `[security.rate_limit] key_strategy` in `autumn.toml`
fn resolve_rate_limit_key_strategy() -> String {
    if let Ok(val) = std::env::var("AUTUMN_SECURITY__RATE_LIMIT__KEY_STRATEGY")
        && !val.is_empty()
    {
        return val;
    }
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| {
            t.get("security")
                .and_then(|s| s.get("rate_limit"))
                .and_then(|rl| rl.get("key_strategy"))
                .and_then(toml::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(std::borrow::ToOwned::to_owned)
        })
        .unwrap_or_default()
}

/// Detect whether an auth extractor is mounted (i.e. `[auth]` section exists
/// and is not explicitly disabled).
fn resolve_auth_extractor_mounted() -> bool {
    if let Ok(val) = std::env::var("AUTUMN_AUTH__ENABLED") {
        return !val.trim().eq_ignore_ascii_case("false");
    }
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .is_some_and(|t| {
            // [auth] section present and not explicitly disabled.
            t.get("auth").is_some_and(|auth| {
                auth.get("enabled")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true) // enabled by default when section exists
            })
        })
}

fn resolve_optional_signing_secret() -> Option<String> {
    if let Ok(val) = std::env::var("AUTUMN_SECURITY__SIGNING_SECRET")
        && !val.is_empty()
    {
        return Some(val);
    }
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| {
            t.get("security")
                .and_then(|s| s.get("signing_secret"))
                .and_then(|ss| ss.get("secret"))
                .and_then(toml::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(std::borrow::ToOwned::to_owned)
        })
}

/// Resolve the signing secret the deploy preflight should grade, from the SAME
/// merged active-profile runtime table used for the deploy config and DB URL
/// (base autumn.toml + `[profile.<env>]` + `autumn-<env>.toml`), env first. A
/// secret supplied only in an active profile
/// (`[profile.<env>].security.signing_secret` / `autumn-<env>.toml`) is invisible
/// to the raw top-level `autumn.toml` that [`resolve_optional_signing_secret`]
/// reads, so a raw-table lookup would report it MISSING even though
/// `AutumnConfig::load()` and `autumn deploy check` see it. Never returns/prints
/// the value except to the (secret-safe) grader. Env-var precedence matches the
/// runtime loader.
fn resolve_deploy_signing_secret(
    merged: &toml::Table,
    env: &dyn autumn_web::config::Env,
) -> Option<String> {
    if let Ok(val) = env.var("AUTUMN_SECURITY__SIGNING_SECRET")
        && !val.is_empty()
    {
        return Some(val);
    }
    merged
        .get("security")
        .and_then(|s| s.get("signing_secret"))
        .and_then(|ss| ss.get("secret"))
        .and_then(toml::Value::as_str)
        .filter(|v| !v.is_empty())
        .map(std::borrow::ToOwned::to_owned)
}

/// Resolve the `previous_secrets` rotation entries the deploy preflight should
/// grade, from the SAME merged active-profile runtime table used for the current
/// signing secret. `previous_secrets` has no env override in the runtime loader
/// (only `AUTUMN_SECURITY__SIGNING_SECRET` sets the current secret), so it is
/// read from the merged table alone — matching what `AutumnConfig::load()` and
/// `autumn deploy check` see. Never returns/prints the values except to the
/// (secret-safe) grader.
///
/// Returns `Err` on a MALFORMED value — a non-array `previous_secrets` or an
/// array with any non-string element — to mirror `AutumnConfig::load()`, which
/// deserializes this field as `Vec<String>` and hard-fails on the same shapes.
/// An absent key yields `Ok(vec![])`. Empty strings are NOT rejected here
/// (`load()` accepts them; production value-validation is downstream in the
/// grader).
fn resolve_deploy_previous_signing_secrets(merged: &toml::Table) -> Result<Vec<String>, String> {
    let Some(value) = merged
        .get("security")
        .and_then(|s| s.get("signing_secret"))
        .and_then(|ss| ss.get("previous_secrets"))
    else {
        return Ok(vec![]);
    };
    let Some(arr) = value.as_array() else {
        return Err("previous_secrets must be an array of strings".into());
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let Some(s) = elem.as_str() else {
            return Err(format!("previous_secrets[{i}] must be a string"));
        };
        out.push(s.to_owned());
    }
    Ok(out)
}

/// Whether any enabled runtime feature requires a configured Postgres pool at
/// startup, resolved from the merged active-profile runtime table (env first).
/// Mirrors the exact backend conditions the runtime enforces:
/// - `jobs.backend = "postgres"` → `job::start_postgres_runtime` requires a pool.
/// - `scheduler.backend = "postgres"` → `scheduler::coordinator_from_config`
///   requires a pool.
///
/// Cache, channels, and idempotency have only in-memory/Redis backends (no
/// Postgres variant), so they never require a DB pool.
fn resolve_deploy_db_backed_runtime(
    merged: &toml::Table,
    env: &dyn autumn_web::config::Env,
) -> bool {
    let env_var = |key: &str| env.var(key).ok().filter(|value| !value.is_empty());

    let jobs = merged.get("jobs").and_then(toml::Value::as_table);
    let jobs_postgres = first_env(&env_var, &["AUTUMN_JOBS__BACKEND"])
        .or_else(|| first_toml_string(jobs, &["backend"]))
        .as_deref()
        .map(str::trim)
        == Some("postgres");

    let scheduler = merged.get("scheduler").and_then(toml::Value::as_table);
    let scheduler_postgres = first_env(&env_var, &["AUTUMN_SCHEDULER__BACKEND"])
        .or_else(|| first_toml_string(scheduler, &["backend"]))
        .as_deref()
        .and_then(autumn_web::config::SchedulerBackend::from_env_value)
        .is_some_and(|backend| backend == autumn_web::config::SchedulerBackend::Postgres);

    jobs_postgres || scheduler_postgres
}

fn resolve_trusted_hosts() -> Vec<String> {
    if let Ok(val) = std::env::var("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS") {
        return val
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(std::borrow::ToOwned::to_owned)
            .collect();
    }
    let table = read_autumn_toml_table().unwrap_or_default();
    let profile = std::env::var("AUTUMN_ENV")
        .ok()
        .or_else(|| std::env::var("AUTUMN_PROFILE").ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let parse_hosts = |root: &toml::Table| {
        root.get("security")
            .and_then(|s| s.get("trusted_hosts"))
            .and_then(|th| th.get("hosts"))
            .and_then(toml::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(std::borrow::ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    if !profile.is_empty()
        && let Some(profile_table) = table
            .get("profile")
            .and_then(|v| v.get(&profile))
            .and_then(toml::Value::as_table)
    {
        let profile_hosts = parse_hosts(profile_table);
        if !profile_hosts.is_empty() {
            return profile_hosts;
        }
    }

    parse_hosts(&table)
}

/// Every env var the runtime recognizes as configuring `[server.tls]`: the
/// cert/key paths plus the tuning knobs. This is the ONLY set that counts as
/// "TLS configured via the environment", kept in lockstep with the keys
/// `apply_server_env_overrides_with_env` reads in `autumn-web`'s config loader
/// (`CERT_PATH`, `KEY_PATH`, `RELOAD_INTERVAL_SECS`, `HANDSHAKE_TIMEOUT_SECS`).
///
/// Detection is deliberately restricted to exactly these keys rather than a
/// broad `AUTUMN_SERVER__TLS__*` prefix scan: an unrelated or mistyped var such
/// as `AUTUMN_SERVER__TLS__ENABLED` does NOT make the runtime materialize
/// `[server.tls]`, so that process serves plain HTTP. Treating it as configured
/// would make `autumn doctor --strict` fail on a missing cert/key for exactly
/// the deployment the server itself accepts. The recognized tuning vars
/// (`RELOAD_INTERVAL_SECS`/`HANDSHAKE_TIMEOUT_SECS`) DO materialize the table, so
/// they must still count.
///
/// Doctor scans these through the profile-aware dotenv overlay (an
/// [`Env`](autumn_web::config::Env), which exposes only `var(key)` lookups, not
/// iteration) so a TLS deployment supplied through `.env`/`.env.<profile>` — or
/// through the real process env, which the overlay layers on top of — is
/// detected.
const TLS_ENV_VAR_NAMES: [&str; 4] = [
    "AUTUMN_SERVER__TLS__CERT_PATH",
    "AUTUMN_SERVER__TLS__KEY_PATH",
    "AUTUMN_SERVER__TLS__RELOAD_INTERVAL_SECS",
    "AUTUMN_SERVER__TLS__HANDSHAKE_TIMEOUT_SECS",
];

/// Whether any `[server.tls]` env var is present in `env` (including a
/// tuning-only var). Reads through the passed [`Env`](autumn_web::config::Env), so a var supplied only
/// via the `.env`/`.env.<profile>` overlay counts — the runtime materializes an
/// (otherwise-empty) `[server.tls]` table for it, so doctor must grade it rather
/// than emit a false Pass. Presence of any [`TLS_ENV_VAR_NAMES`] key marks
/// configured; an unrecognized `AUTUMN_SERVER__TLS__*` var does not.
fn any_tls_env_var_set_in(env: &dyn autumn_web::config::Env) -> bool {
    TLS_ENV_VAR_NAMES.iter().any(|name| env.var(name).is_ok())
}

/// Pure core of [`resolve_tls_paths`]: decide the resolved cert/key paths from
/// already-extracted sources, so it can be graded in tests without mutating the
/// process environment. `env_*` win over `toml_*` (matching the runtime).
///
/// TLS is "configured" — and therefore worth grading — when the `[server.tls]`
/// section is present OR any recognized [`TLS_ENV_VAR_NAMES`] var is set. A
/// configured deployment missing a cert/key yields `Some((empty, empty))` so the
/// caller reports an actionable "must set both" failure rather than silently
/// skipping.
fn resolve_tls_paths_from_sources(
    tls_section_present: bool,
    toml_cert: Option<String>,
    toml_key: Option<String>,
    env_cert: Option<String>,
    env_key: Option<String>,
    any_tls_env: bool,
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let cert = env_cert.or(toml_cert);
    let key = env_key.or(toml_key);

    if !(tls_section_present || any_tls_env) {
        return None;
    }

    Some((
        std::path::PathBuf::from(cert.unwrap_or_default()),
        std::path::PathBuf::from(key.unwrap_or_default()),
    ))
}

/// Resolve the configured `[server.tls]` cert/key paths, honoring env-var
/// overrides and the active profile's merged config, exactly as the runtime
/// loads them. Returns `None` when TLS is not configured at all.
///
/// The `AUTUMN_SERVER__TLS__*` vars are read through the SAME profile-aware
/// dotenv overlay the sibling checks use (`os_env_with_dotenv_for_profile`), so
/// a cert/key or tuning var supplied via `.env`/`.env.<profile>` — which the
/// runtime loads via `TomlEnvConfigLoader` and would boot a TLS listener from —
/// is visible to doctor instead of being reported as not-configured.
///
/// The merged TOML is resolved through [`resolve_config_path_manifest_aware`],
/// so a `[server.tls]` section present only in the `AUTUMN_MANIFEST_DIR` config
/// (where an installed app keeps `autumn.toml`) is graded, rather than being
/// reported as not-configured because doctor read the CWD instead.
fn resolve_tls_paths() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let (canonical, selected, _) = resolve_active_profiles();
    let merged = get_merged_toml_table_runtime_with(
        &canonical,
        &selected,
        resolve_config_path_manifest_aware,
    );
    let tls = merged
        .get("server")
        .and_then(toml::Value::as_table)
        .and_then(|s| s.get("tls"))
        .and_then(toml::Value::as_table);

    // Read TLS env vars through the profile-aware `.env` overlay — the real OS
    // env still wins, but `.env`/`.env.<profile>` values now fill keys the OS
    // env lacks. Fall back to the bare OS env only if the overlay can't be built
    // (a malformed `.env`), matching `resolve_offsite_backup_data`.
    let denv: Box<dyn autumn_web::config::Env> =
        match autumn_web::dotenv::os_env_with_dotenv_for_profile(&canonical) {
            Ok(e) => Box::new(e),
            Err(_) => Box::new(autumn_web::config::OsEnv),
        };

    // `.ok()` deliberately does NOT filter empty values: a set-but-empty env
    // var is a PRESENT higher-priority override. The runtime materializes it as
    // an empty `PathBuf` that OVERWRITES any TOML `cert_path`/`key_path`
    // (`apply_env_overrides_with_env` uses `env.var(...).ok()` with no
    // empty-filter, then `PathBuf::from(cert)`), so startup fails fast on the
    // missing file. Dropping the empty override here would let doctor fall back
    // to the TOML path and pass a config the server rejects — mirror the
    // runtime's presence rule so doctor resolves to the same empty path.
    let env_cert = denv.var("AUTUMN_SERVER__TLS__CERT_PATH").ok();
    let env_key = denv.var("AUTUMN_SERVER__TLS__KEY_PATH").ok();

    let toml_cert = tls
        .and_then(|t| t.get("cert_path"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let toml_key = tls
        .and_then(|t| t.get("key_path"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);

    // Consider TLS configured only when a RECOGNIZED `[server.tls]` env var is
    // present (through the overlay, which layers the real process env under the
    // `.env`/`.env.<profile>` values). Restricting to `TLS_ENV_VAR_NAMES` — the
    // exact set the runtime materializes the table from — avoids treating an
    // unrelated/mistyped var like `AUTUMN_SERVER__TLS__ENABLED` as configured and
    // failing `doctor --strict` on a config the server serves as plain HTTP.
    let any_tls_env = any_tls_env_var_set_in(denv.as_ref());

    resolve_tls_paths_from_sources(
        tls.is_some(),
        toml_cert,
        toml_key,
        env_cert,
        env_key,
        any_tls_env,
    )
}

/// Whether the static `cert_path` / `key_path` KEYS are PRESENT — set in the
/// merged runtime `[server.tls]` TOML or as an env override — regardless of
/// whether the value is empty.
///
/// This mirrors [`autumn_web::config::TlsConfig::validate`], which keys the
/// static-vs-ACME XOR on `cert_path.is_some()` / `key_path.is_some()`: an
/// explicitly-present-but-empty path is `Some("")` at runtime, hence PRESENT.
/// Doctor must gate the XOR on presence, not non-emptiness, or a
/// `[server.tls.acme]` config paired with `cert_path = ""` (or an empty env
/// override) collapses to "absent" and wrongly passes as ACME-only, when the
/// runtime rejects it as a mixed static+ACME config.
fn resolve_static_tls_presence() -> (bool, bool) {
    let (canonical, selected, _) = resolve_active_profiles();
    let merged = get_merged_toml_table_runtime(&canonical, &selected);
    let tls = merged
        .get("server")
        .and_then(toml::Value::as_table)
        .and_then(|s| s.get("tls"))
        .and_then(toml::Value::as_table);
    // Read the static cert/key env overrides through the same profile-aware dotenv
    // overlay `resolve_tls_paths()` uses, not the bare process env. The runtime
    // applies `AUTUMN_SERVER__TLS__CERT_PATH`/`KEY_PATH` from the `.env` and
    // `.env.<profile>` overlay (`TomlEnvConfigLoader`), so a static cert or key
    // supplied there alongside `[server.tls.acme]` is a mixed static+ACME config the
    // runtime rejects at boot. Reading only the process env would miss that override
    // and let the static-vs-ACME XOR pass `doctor --strict` on a config the server
    // will not start. Fall back to the bare OS env only if the overlay cannot be
    // built, from a malformed `.env`.
    let denv: Box<dyn autumn_web::config::Env> =
        match autumn_web::dotenv::os_env_with_dotenv_for_profile(&canonical) {
            Ok(e) => Box::new(e),
            Err(_) => Box::new(autumn_web::config::OsEnv),
        };
    let (cert_env, key_env) = static_tls_env_presence_in(denv.as_ref());
    static_tls_presence_from(tls, cert_env, key_env)
}

/// Whether the static `[server.tls]` cert/key env OVERRIDES are PRESENT in `env`
/// (injectable for tests). A set-but-empty value counts as present, mirroring the
/// runtime's `env.var(..).ok()` (an empty override is `Some("")`, a clearing
/// override). Fed the profile-aware dotenv overlay by [`resolve_static_tls_presence`]
/// so a cert/key supplied via `.env`/`.env.<profile>` is detected.
fn static_tls_env_presence_in(env: &dyn autumn_web::config::Env) -> (bool, bool) {
    (
        env.var("AUTUMN_SERVER__TLS__CERT_PATH").is_ok(),
        env.var("AUTUMN_SERVER__TLS__KEY_PATH").is_ok(),
    )
}

/// Pure core of [`resolve_static_tls_presence`] (injectable for tests): the
/// `cert_path` / `key_path` keys are present when set in the `[server.tls]` TOML
/// table OR supplied as an env override, independent of an empty value.
fn static_tls_presence_from(
    tls: Option<&toml::Table>,
    cert_env_present: bool,
    key_env_present: bool,
) -> (bool, bool) {
    let cert_present = tls.is_some_and(|t| t.contains_key("cert_path")) || cert_env_present;
    let key_present = tls.is_some_and(|t| t.contains_key("key_path")) || key_env_present;
    (cert_present, key_present)
}

/// Resolve `[server.tls]` into the graded [`TlsDoctorData`] for [`check_tls_impl`].
///
/// Offline only: reads `autumn.toml` and the referenced cert/key files, never
/// boots a server or touches the network. When the CLI is built without the
/// `tls` feature the certificate cannot be inspected, so a configured section
/// reports [`TlsDoctorData::FeatureDisabled`] rather than being silently dropped.
fn resolve_tls_doctor_data() -> TlsDoctorData {
    let Some((cert, key)) = resolve_tls_paths() else {
        return TlsDoctorData::NotConfigured;
    };

    if cert.as_os_str().is_empty() || key.as_os_str().is_empty() {
        return TlsDoctorData::Invalid {
            detail: "[server.tls] must set both cert_path and key_path".to_owned(),
        };
    }

    #[cfg(feature = "tls")]
    {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
        .unwrap_or(i64::MAX);
        match autumn_web::tls::inspect_leaf(&cert, &key, now) {
            Ok(inspection) => TlsDoctorData::Healthy {
                not_yet_valid: inspection.is_not_yet_valid(now),
                expired: inspection.is_expired(now),
                days_until_expiry: inspection.days_until_expiry(now),
            },
            Err(e) => TlsDoctorData::Invalid {
                detail: e.to_string(),
            },
        }
    }
    #[cfg(not(feature = "tls"))]
    {
        let _ = (cert, key);
        TlsDoctorData::FeatureDisabled
    }
}

/// Whether the static `[server.tls]` cert/key doctor check should run.
///
/// It runs whenever a genuine static cert/key pair is present. It is SKIPPED
/// only for an ACME-only deployment — `[server.tls.acme]` configured with no
/// static `cert_path`/`key_path` — because `resolve_tls_paths()` still sees the
/// enclosing `[server.tls]` table and would make `check_tls_impl` emit a
/// spurious "must set both `cert_path` and `key_path`" failure, even though the
/// runtime's `TlsConfig::validate()` accepts ACME-only. ACME deployments are
/// graded by the dedicated `acme_stored_cert`/`acme_ports`/`acme_dns` checks.
const fn should_run_static_tls_check(acme_configured: bool, static_cert_key_present: bool) -> bool {
    static_cert_key_present || !acme_configured
}

/// Grade the static-vs-ACME TLS mode invariants, mirroring
/// [`autumn_web::config::TlsConfig::validate`] (pure; injectable for tests).
///
/// The runtime fails fast at boot when `[server.tls.acme]` is combined with a
/// static `cert_path`/`key_path` (mutually exclusive), and when only ONE of
/// `cert_path`/`key_path` is set (half pair). Doctor only used the static paths
/// to decide whether to run the static-cert check, so it could bless configs the
/// app won't start: a config with BOTH a valid static cert/key AND acme, or a
/// single static path plus acme. This grader replicates the same rules and
/// returns a `Fail` [`CheckResult`] for the offending combination, or `None` when
/// the mode is valid (ACME-only, static-only with both paths, or neither — TLS is
/// optional). Messages mirror `TlsConfig::validate`'s so the operator sees the
/// same guidance doctor and the runtime would give.
#[must_use]
pub fn check_tls_mode_impl(
    has_cert: bool,
    has_key: bool,
    acme_configured: bool,
) -> Option<CheckResult> {
    let static_configured = has_cert || has_key;
    match (static_configured, acme_configured) {
        (true, true) => Some(CheckResult {
            name: "tls_mode",
            status: CheckStatus::Fail,
            detail: Some(
                "[server.tls] sets a static cert_path/key_path AND [server.tls.acme]; choose \
                 exactly one — remove the static cert to use ACME, or remove [server.tls.acme] \
                 to serve the static certificate"
                    .into(),
            ),
            hint: Some(
                "Static TLS cert/key and [server.tls.acme] are mutually exclusive; configure \
                 exactly one",
            ),
        }),
        (true, false) if !(has_cert && has_key) => Some(CheckResult {
            name: "tls_mode",
            status: CheckStatus::Fail,
            detail: Some(
                "[server.tls] cert_path and key_path must be set together; set both, or \
                 configure [server.tls.acme] instead"
                    .into(),
            ),
            hint: Some("Set both cert_path and key_path"),
        }),
        _ => None,
    }
}

/// The resolved `[server.tls.acme]` inputs the doctor needs (issue #1608).
#[derive(Debug, Clone)]
pub struct AcmeDoctorConfig {
    /// Configured domains (first is used for active probes).
    pub domains: Vec<String>,
    /// Contact email registered with the ACME account. Empty when unset; the
    /// runtime `AcmeConfig::validate()` rejects a missing/blank value at boot, so
    /// doctor mirrors that as a FAIL.
    pub contact_email: String,
    /// Port to serve the HTTP-01 challenge on. Defaults to 80 when unset. The
    /// runtime `AcmeConfig::validate()` rejects `0` (it binds an ephemeral OS port
    /// the HTTP-01 validator can never reach), so doctor mirrors that as a FAIL.
    pub http_challenge_port: u16,
    /// Days-before-expiry renewal window. Defaults to 30 when unset. The runtime
    /// `AcmeConfig::validate()` rejects a value `>= 90` (the effective max cert
    /// lifetime): such a window keeps a freshly-issued cert perpetually "due",
    /// re-ordering every tick and burning CA rate limits — doctor mirrors that as
    /// a FAIL.
    pub renew_before_days: u32,
    /// Directory holding the stored account + certificates.
    pub cache_dir: std::path::PathBuf,
    /// The per-directory subdirectory label the runtime store namespaces
    /// certificates under (`{cache_dir}/{directory_label}/`). Derived from the
    /// configured `acme.directory`, matching `FsAcmeStore`. Falls back to the
    /// `staging` label when `directory` fails to deserialize (see
    /// [`directory_error`](Self::directory_error)); that path FAILs, so the label
    /// is unused.
    pub directory_label: String,
    /// The rendered invalid `acme.directory` value when it fails to deserialize
    /// as [`autumn_web::config::AcmeDirectory`] the way the runtime does (a typo
    /// like `directory = "prod"` or a malformed custom table). The runtime fails
    /// to boot on such a value, so doctor surfaces it as an `acme_config` FAIL
    /// instead of silently defaulting to staging. `None` when the directory is
    /// valid or unset (unset uses the runtime default, staging).
    pub directory_error: Option<String>,
    /// The rendered invalid `acme.http_challenge_port` value when it is PRESENT
    /// but does not deserialize as a `u16` the way the runtime's typed
    /// `AcmeConfig` does (out of range like `70000`/`-1`, or a non-integer like a
    /// quoted string). The runtime rejects such a value before boot, so doctor
    /// surfaces it as an `acme_config` FAIL instead of silently falling back to
    /// `80` (which would hide a config the server won't start). `None` when the
    /// port is a valid `u16` or unset (unset uses the runtime default, `80`).
    pub port_error: Option<String>,
    /// The configured `ca_root_path`: the PEM root that signs the ACME
    /// **directory's own HTTPS certificate**, needed only for a private CA /
    /// Pebble directory. `None` when unset (the client uses the platform trust
    /// store, which is correct for Let's Encrypt staging and production).
    pub ca_root_path: Option<std::path::PathBuf>,
    /// The rendered invalid `acme.ca_root_path` value when the key is PRESENT
    /// but is not a TOML string (e.g. `ca_root_path = 123` or an array). The
    /// runtime's typed `Option<PathBuf>` deserialization FAILS on such a value
    /// and the server will not boot, so doctor surfaces it rather than silently
    /// treating it as unset — which would report the "no `ca_root_path`
    /// configured" Pass for a config the server rejects. Mirrors the
    /// [`port_error`](Self::port_error) / [`directory_error`](Self::directory_error)
    /// treatment. `None` when the value is a valid string or the key is absent.
    pub ca_root_error: Option<String>,
    /// The rendered offending `domains` entry when the `domains` array contains a
    /// NON-STRING element (e.g. `domains = ["app.example.com", 123]`). The
    /// runtime's typed `Vec<String>` deserialization FAILS on such an entry and
    /// the server won't boot, but doctor previously `filter_map`ped non-strings
    /// away — keeping the valid names and passing while the server rejects the
    /// config. Recorded here so the grader surfaces it as an `acme_config` FAIL.
    /// `None` when every entry is a string (or `domains` is absent).
    pub domains_error: Option<String>,
    /// The rendered invalid `acme.renew_before_days` value when the key is
    /// PRESENT but does not deserialize as a `u32` the way the runtime's typed
    /// `AcmeConfig` does (a quoted string like `"30"`, a float, a bool, a
    /// negative, or an out-of-`u32`-range integer). Doctor's old `as_integer()`
    /// chain treated a NON-INTEGER as ABSENT and silently defaulted to 30, so
    /// `doctor --strict` passed a file the runtime refuses to boot on (#1874);
    /// a negative / out-of-range integer did parse, but was clamped to
    /// `u32::MAX` and reported against the `>= 90` renewal-window rule rather
    /// than as the malformed value it is.
    /// Mirrors the [`port_error`](Self::port_error) treatment. `None` when the
    /// value is a valid `u32` or the key is absent (absent uses the runtime
    /// default, 30).
    pub renew_before_days_error: Option<String>,
    /// The `[server.tls.acme.dns]` section (issue #1620), when configured, as the
    /// runtime's typed `AcmeDnsConfig` sees it. `None` when the section is absent
    /// (HTTP-01 issuance) or when it does not deserialize — see
    /// [`dns_error`](Self::dns_error).
    pub dns: Option<autumn_web::config::AcmeDnsConfig>,
    /// The rendered deserialization error for a PRESENT but malformed
    /// `[server.tls.acme.dns]` section — an unknown provider name, or (most
    /// usefully) an inline `api_token`, which the runtime rejects outright
    /// because DNS credentials never belong in `autumn.toml`. Recorded so the
    /// grader FAILs rather than reporting "no DNS provider configured" for a
    /// config the server refuses to boot on. `None` when the section is valid or
    /// absent.
    pub dns_error: Option<String>,
}

/// Deserialize `[server.tls.acme] directory` exactly as the runtime does.
///
/// Returns the parsed [`autumn_web::config::AcmeDirectory`], or — on a
/// deserialize error the runtime would fail to boot on (a typo like
/// `directory = "prod"` or a malformed custom table) — the rendered invalid
/// value, so the caller can surface it as an `acme_config` FAIL instead of
/// silently defaulting to staging. An absent `directory` key uses the runtime
/// default (staging).
fn parse_acme_directory(acme: &toml::Table) -> Result<autumn_web::config::AcmeDirectory, String> {
    acme.get("directory").cloned().map_or_else(
        || Ok(autumn_web::config::AcmeDirectory::default()),
        |value| {
            // Render the value BEFORE `try_into` consumes it, for the FAIL message.
            let rendered = value.to_string();
            value
                .try_into::<autumn_web::config::AcmeDirectory>()
                .map_err(|_| rendered.trim().to_owned())
        },
    )
}

/// Deserialize one scalar `[server.tls.acme]` key exactly as the runtime's typed
/// `AcmeConfig` does, keeping the offending value on failure.
///
/// Returns `default` when the key is absent (matching the runtime's `#[serde(default
/// = ...)]`), the deserialized `T` when the value is one the runtime accepts, and
/// otherwise the RENDERED invalid value — so the caller can surface it as an
/// `acme_config` FAIL naming what the operator actually wrote, instead of
/// silently falling back to the default and blessing a config the server refuses
/// to boot on.
fn parse_acme_scalar<T: serde::de::DeserializeOwned>(
    acme: &toml::Table,
    key: &str,
    default: T,
) -> Result<T, String> {
    acme.get(key).cloned().map_or_else(
        || Ok(default),
        |value| {
            // Render the value BEFORE `try_into` consumes it, for the FAIL message.
            let rendered = value.to_string();
            value
                .try_into::<T>()
                .map_err(|_| rendered.trim().to_owned())
        },
    )
}

/// Deserialize `[server.tls.acme] http_challenge_port` exactly as the runtime's
/// typed `AcmeConfig` does.
///
/// Returns the parsed `u16`, or — on a value the runtime would fail to
/// deserialize (out of `u16` range like `70000`/`-1`, or a non-integer such as a
/// quoted string) — the rendered invalid value, so the caller can surface it as
/// an `acme_config` FAIL instead of silently falling back to the default `80`.
/// An absent `http_challenge_port` key uses the runtime default (`80`).
fn parse_acme_http_challenge_port(acme: &toml::Table) -> Result<u16, String> {
    parse_acme_scalar(acme, "http_challenge_port", 80)
}

/// Deserialize `[server.tls.acme] renew_before_days` exactly as the runtime's
/// typed `AcmeConfig` does.
///
/// Returns the parsed `u32`, or — on a value the runtime would fail to
/// deserialize (negative, out of `u32` range, or a non-integer such as a quoted
/// string, a float, or a bool) — the rendered invalid value, so the caller can
/// surface it as an `acme_config` FAIL instead of silently falling back to the
/// default `30`. An absent `renew_before_days` key uses the runtime default
/// (`30`).
fn parse_acme_renew_before_days(acme: &toml::Table) -> Result<u32, String> {
    parse_acme_scalar(acme, "renew_before_days", 30)
}

/// The `FsAcmeStore` subdirectory label for a parsed `acme.directory`.
///
/// Mirrors `autumn_web::acme::directory_label`, replicated here because that
/// helper lives behind the `acme` feature while this doctor path also compiles
/// in the feature-less build. Keep in sync with the store — see
/// `autumn_web::acme::store::FsAcmeStore`, which reads/writes certificates only
/// under `{cache_dir}/{this label}/`.
fn acme_directory_label(directory: &autumn_web::config::AcmeDirectory) -> String {
    match directory {
        autumn_web::config::AcmeDirectory::Staging => "staging".to_owned(),
        autumn_web::config::AcmeDirectory::Production => "production".to_owned(),
        // A custom URL is hashed to a short, filesystem-safe token, exactly as
        // `autumn_web::acme::directory_label` / `short_hash` do.
        autumn_web::config::AcmeDirectory::Custom { url } => {
            format!("custom-{}", acme_short_hash(url))
        }
    }
}

/// A short hex digest (first 8 bytes of SHA-256) of `input`.
///
/// Replicates `autumn_web::acme::short_hash` (feature-gated behind `acme`) so
/// the feature-less doctor build derives the same custom-directory label.
fn acme_short_hash(input: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Resolve `[server.tls.acme]` from the parsed `autumn.toml`, if configured.
fn resolve_acme_doctor_config(toml_table: Option<&toml::Table>) -> Option<AcmeDoctorConfig> {
    let acme = toml_table?
        .get("server")
        .and_then(toml::Value::as_table)?
        .get("tls")
        .and_then(toml::Value::as_table)?
        .get("acme")
        .and_then(toml::Value::as_table)?;

    // Collect the domains as the runtime's typed `Vec<String>` deserialization
    // does — and, unlike a lenient `filter_map`, RECORD any non-string entry
    // (e.g. `domains = ["app.example.com", 123]`) instead of silently dropping it.
    // The runtime fails to deserialize such an array and won't boot, so the grader
    // must FAIL rather than pass on the surviving string names.
    let mut domains = Vec::new();
    let mut domains_error = None;
    if let Some(array) = acme.get("domains").and_then(toml::Value::as_array) {
        for (index, entry) in array.iter().enumerate() {
            let Some(domain) = entry.as_str() else {
                domains_error = Some(format!(
                    "entry at index {index} ({}) is not a string",
                    entry.to_string().trim()
                ));
                break;
            };
            domains.push(domain.to_owned());
        }
    }

    let contact_email = acme
        .get("contact_email")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // Deserialize `http_challenge_port` the way the runtime's typed `AcmeConfig`
    // does: an absent key defaults to 80, an explicit valid `u16` is preserved
    // (including `0`, so the acme-config grader FAILs it exactly as
    // `AcmeConfig::validate()` does), and a value the runtime would reject
    // pre-boot (out of `u16` range, or a non-integer like a quoted string)
    // records the bad value so the grader FAILs rather than silently defaulting.
    let (http_challenge_port, port_error) = match parse_acme_http_challenge_port(acme) {
        Ok(port) => (port, None),
        Err(bad_value) => (80, Some(bad_value)),
    };

    // Deserialize `renew_before_days` the way the runtime's typed `AcmeConfig`
    // does: an absent key defaults to 30, a valid in-range integer is preserved
    // (including a >= 90 value, so the acme-config grader FAILs it exactly as
    // `AcmeConfig::validate()` does), and a value the runtime would reject
    // pre-boot (negative, out of `u32` range, or a non-integer such as a quoted
    // string) records the bad value so the grader FAILs rather than silently
    // defaulting to 30.
    let (renew_before_days, renew_before_days_error) = match parse_acme_renew_before_days(acme) {
        Ok(days) => (days, None),
        Err(bad_value) => (30, Some(bad_value)),
    };

    let cache_dir = acme
        .get("cache_dir")
        .and_then(toml::Value::as_str)
        .map_or_else(|| std::path::PathBuf::from("config/acme"), Into::into);

    // Deserialize `directory` the way the runtime does; on error, record the bad
    // value so the acme-config grader FAILs (the runtime won't boot on it) rather
    // than silently defaulting to staging. On success derive the store label.
    let (directory_label, directory_error) = match parse_acme_directory(acme) {
        Ok(directory) => (acme_directory_label(&directory), None),
        Err(bad_value) => ("staging".to_owned(), Some(bad_value)),
    };

    // Deserialize `ca_root_path` the way the runtime's typed `Option<PathBuf>`
    // does: absent is unset, a string is the path, and anything else is a value
    // the runtime rejects pre-boot — recorded so the grader FAILs instead of
    // reporting the "unset" Pass for a config the server won't start on.
    let (ca_root_path, ca_root_error) = match acme.get("ca_root_path") {
        None => (None, None),
        Some(toml::Value::String(path)) => (Some(std::path::PathBuf::from(path)), None),
        Some(other) => (None, Some(other.to_string())),
    };

    // Deserialize `[server.tls.acme.dns]` the way the runtime's typed
    // `AcmeDnsConfig` does. Its `deny_unknown_fields` is the point: an operator
    // who pastes `api_token = "..."` into `autumn.toml` gets a FAIL naming the
    // key, not a silently-ignored secret sitting in a plaintext file (#1620).
    let (dns, dns_error) =
        acme.get("dns").map_or((None, None), |value| match value
            .clone()
            .try_into::<autumn_web::config::AcmeDnsConfig>(
        ) {
            Ok(dns) => (Some(dns), None),
            Err(e) => (None, Some(e.to_string())),
        });

    Some(AcmeDoctorConfig {
        domains,
        contact_email,
        http_challenge_port,
        renew_before_days,
        cache_dir,
        directory_label,
        directory_error,
        port_error,
        ca_root_path,
        ca_root_error,
        domains_error,
        renew_before_days_error,
        dns,
        dns_error,
    })
}

/// The shared `acme_config` Fail shape: every ACME-config violation reports the
/// same check name and status, so each rule contributes only a detail + hint.
///
/// Returns the bare [`CheckResult`] rather than a `Some(..)`; the graders that
/// call it return `Option<CheckResult>` (`None` meaning "this rule is satisfied")
/// and wrap it themselves, so the "always Some" is theirs to state, not this
/// constructor's.
const fn acme_config_fail(detail: String, hint: &'static str) -> CheckResult {
    CheckResult {
        name: "acme_config",
        status: CheckStatus::Fail,
        detail: Some(detail),
        hint: Some(hint),
    }
}

/// Grade the `[server.tls.acme]` values that the runtime rejects while
/// DESERIALIZING `AcmeConfig` — before `validate()` ever runs.
///
/// Each field here holds the rendered value the operator actually wrote (see
/// [`AcmeDoctorConfig::port_error`] and siblings), recorded because the doctor's
/// own lenient parse would otherwise substitute a default and bless a config the
/// server refuses to boot on. Extracted from [`check_acme_config_impl`] so that
/// grader stays within the line budget, and so this "the runtime cannot even
/// parse this" tier reads as one unit.
fn check_acme_deserialize_errors(config: &AcmeDoctorConfig) -> Option<CheckResult> {
    let AcmeDoctorConfig {
        directory_error,
        port_error,
        domains_error,
        renew_before_days_error,
        ..
    } = config;

    if let Some(bad_value) = domains_error {
        return Some(acme_config_fail(
            format!(
                "[server.tls.acme] domains {bad_value}: every entry must be a string hostname. \
                 The runtime deserializes domains as a list of strings and fails to boot on a \
                 non-string entry"
            ),
            "List only string hostnames in [server.tls.acme] domains",
        ));
    }
    if let Some(bad_value) = directory_error {
        return Some(acme_config_fail(
            format!(
                "[server.tls.acme] directory value {bad_value} is not a valid ACME directory: use \
                 \"staging\", \"production\", or a custom directory URL. The runtime fails to boot \
                 on this value"
            ),
            "Set [server.tls.acme] directory to \"staging\", \"production\", or a custom directory \
             URL",
        ));
    }
    if let Some(bad_value) = port_error {
        return Some(acme_config_fail(
            format!(
                "[server.tls.acme] http_challenge_port value {bad_value} is not a valid port: it \
                 must be an integer in the range 0-65535 (the runtime fails to boot on an \
                 out-of-range or non-integer value)"
            ),
            "Set [server.tls.acme] http_challenge_port to a valid port number (80, or the port a \
             front-end forwards `:80` to)",
        ));
    }
    if let Some(bad_value) = renew_before_days_error {
        return Some(acme_config_fail(
            format!(
                "[server.tls.acme] renew_before_days value {bad_value} is not a valid renewal \
                 window: it must be a whole number of days in the range 0-4294967295 (the runtime \
                 fails to boot on a negative, out-of-range, or non-integer value such as a quoted \
                 string)"
            ),
            "Set [server.tls.acme] renew_before_days to a whole number of days below 90 (default \
             30), unquoted",
        ));
    }
    None
}

/// Grade the resolved ACME config against the runtime's boot-time invariants,
/// mirroring [`autumn_web::config::AcmeConfig::validate`] (pure; injectable for
/// tests).
///
/// The runtime `TlsConfig::validate()` / `AcmeConfig::validate()` REJECTS an ACME
/// config that (a) has a `directory` value that fails to deserialize as
/// [`autumn_web::config::AcmeDirectory`], (b) lists no `domains`, (c) has a blank
/// `contact_email`, (d) includes a wildcard domain with no
/// `[server.tls.acme.dns]` section (or a malformed one), or (e) has a
/// `[server.tls.acme.dns]` section its own `validate()` rejects — the server
/// exits at boot in every case. `validate()`'s fifth
/// rule, a blank `ca_root_path`, is graded by
/// [`check_acme_ca_root_impl`] instead: the whole `ca_root_path` story (blank,
/// non-string, unreadable, unusable, a bundle, or redundant against a public
/// directory) belongs in one check with one name, rather than splitting the
/// blank case away from its siblings. Doctor previously turned
/// a missing/empty `domains` into an empty list and reported `acme_stored_cert`
/// as Pass with no probes, and silently defaulted a malformed `directory` to
/// staging, so `doctor --strict` blessed a deployment that immediately exits.
/// This grader returns a `Fail` [`CheckResult`] for the first violated rule
/// (messages mirror the runtime's), or `None` when the ACME config is valid.
///
/// The recorded deserialize errors on `config` — [`domains_error`],
/// [`directory_error`], [`port_error`] and [`renew_before_days_error`] — are
/// checked FIRST, because the runtime DESERIALIZES `AcmeConfig` (failing on a
/// non-string `domains` entry, a bad `directory`, or an out-of-range /
/// non-integer `http_challenge_port` / `renew_before_days`) before it ever runs
/// `validate()`. Each one is a rendered invalid value the operator wrote, so the
/// FAIL can name it.
///
/// [`domains_error`]: AcmeDoctorConfig::domains_error
/// [`directory_error`]: AcmeDoctorConfig::directory_error
/// [`port_error`]: AcmeDoctorConfig::port_error
/// [`renew_before_days_error`]: AcmeDoctorConfig::renew_before_days_error
#[must_use]
pub fn check_acme_config_impl(config: &AcmeDoctorConfig) -> Option<CheckResult> {
    let AcmeDoctorConfig {
        domains,
        contact_email,
        http_challenge_port,
        renew_before_days,
        ..
    } = config;

    // The runtime deserializes `AcmeConfig` before it validates it, so a value it
    // cannot even parse is graded first.
    if let Some(deserialize_fail) = check_acme_deserialize_errors(config) {
        return Some(deserialize_fail);
    }

    if domains.is_empty() {
        return Some(acme_config_fail(
            "[server.tls.acme] domains must list at least one domain to request a certificate for"
                .to_owned(),
            "Add at least one domain to [server.tls.acme] domains",
        ));
    }
    if contact_email.trim().is_empty() {
        return Some(acme_config_fail(
            "[server.tls.acme] contact_email must be set (the ACME CA requires an account contact \
             for expiry notifications)"
                .to_owned(),
            "Set [server.tls.acme] contact_email",
        ));
    }
    if *http_challenge_port == 0 {
        return Some(acme_config_fail(
            "[server.tls.acme] http_challenge_port must not be 0: port 0 binds an ephemeral \
             OS-assigned port that the ACME HTTP-01 validator (which always connects on port 80) \
             can never reach, so every issuance fails. Use 80, or the port a front-end forwards \
             `:80` to"
                .to_owned(),
            "Set [server.tls.acme] http_challenge_port to 80 (or the port `:80` forwards to)",
        ));
    }
    if *renew_before_days >= 90 {
        return Some(acme_config_fail(
            format!(
                "[server.tls.acme] renew_before_days ({renew_before_days}) must be less than 90: \
                 it is compared against the issued certificate's remaining validity, and \
                 publicly-trusted CAs (e.g. Let's Encrypt) issue certificates that live at most \
                 ~90 days. A value >= the certificate lifetime keeps the cert perpetually inside \
                 its renew-before window, so the renewal loop would order a fresh certificate \
                 every hour and burn the CA's rate limits"
            ),
            "Set [server.tls.acme] renew_before_days below 90 (default 30)",
        ));
    }
    if let Some(fail) = check_acme_domain_entries(domains, config.dns.is_some()) {
        return Some(fail);
    }
    // Mirror `AcmeDnsConfig::validate()` too: a DNS section the runtime rejects
    // (an exec provider with no command, a zero propagation budget, an
    // unparseable resolver) exits at boot exactly like a bad `domains` entry.
    config.dns.as_ref().and_then(|dns| {
        dns.validate().err().map(|message| {
            acme_config_fail(
                message,
                "Fix the [server.tls.acme.dns] section; the server refuses to boot on it",
            )
        })
    })
}

/// Grade individual `[server.tls.acme] domains` entries (blank / wildcard /
/// padded), mirroring `AcmeConfig::validate`'s per-entry rules. Extracted from
/// [`check_acme_config_impl`] to keep that grader within the line budget.
///
/// `dns_configured` is whether `[server.tls.acme.dns]` is present: since #1620 a
/// wildcard is valid exactly when it is, because only DNS-01 can validate a
/// wildcard identifier.
fn check_acme_domain_entries(domains: &[String], dns_configured: bool) -> Option<CheckResult> {
    for (index, domain) in domains.iter().enumerate() {
        let trimmed = domain.trim();
        if trimmed.is_empty() {
            return Some(acme_config_fail(
                format!(
                    "[server.tls.acme] domains must not contain blank entries (entry at index \
                     {index} is empty or whitespace-only)"
                ),
                "Remove blank/whitespace-only entries from [server.tls.acme] domains",
            ));
        }
        if trimmed.contains('*') {
            if !dns_configured {
                return Some(acme_config_fail(
                    format!(
                        "[server.tls.acme] wildcard domain `{trimmed}` needs the DNS-01 \
                         challenge: an ACME CA will not validate a wildcard identifier over \
                         HTTP-01. Add a [server.tls.acme.dns] section naming your DNS provider \
                         (and the credentials-store key holding its API token), or list explicit \
                         hostnames instead"
                    ),
                    "Add a [server.tls.acme.dns] section, or list explicit hostnames",
                ));
            }
            let Some(base) = trimmed.strip_prefix("*.") else {
                return Some(acme_config_fail(
                    format!(
                        "[server.tls.acme] domain `{trimmed}` is not a usable wildcard: a \
                         wildcard SAN must be written as `*.` followed by the base domain (e.g. \
                         `*.myapp.com`) — a `*` anywhere else is not matched by any client"
                    ),
                    "Write the wildcard as `*.<base domain>`",
                ));
            };
            if base.is_empty() || base.contains('*') {
                return Some(acme_config_fail(
                    format!(
                        "[server.tls.acme] domain `{trimmed}` is not a usable wildcard: exactly \
                         one leading `*.` is allowed and the base domain after it must be \
                         non-empty (e.g. `*.myapp.com`)"
                    ),
                    "Write the wildcard as `*.<base domain>`, with exactly one leading `*.`",
                ));
            }
        }
        // The two rules above read `trimmed`, but the runtime stores and uses the
        // entry UNTRIMMED — as the certificate's SAN and as the ACME order's DNS
        // identifier — so `AcmeConfig::validate()` rejects a padded entry. Mirror
        // that here, or `doctor --strict` passes a file the server won't boot on.
        if domain != trimmed {
            return Some(acme_config_fail(
                format!(
                    "[server.tls.acme] domain `{domain}` (entry at index {index}) has leading or \
                     trailing whitespace: the entry is used verbatim as the certificate's SAN and \
                     as the ACME order's DNS identifier, so the padded value would be requested \
                     as-is. Write it as `{trimmed}`"
                ),
                "Remove the leading/trailing whitespace from the [server.tls.acme] domains entry",
            ));
        }
    }
    None
}

/// The set of domains the `--online` doctor actively probes (port 80/443 + DNS).
///
/// This is EVERY configured domain, not just the first: issuance orders an
/// authorization for each name in `config.domains`, so probing only the first
/// name can let doctor pass while issuance fails on an unprobed domain. Extracted
/// as a pure helper so the "probe every configured domain" contract is
/// unit-testable without real network I/O — `run()` enqueues one bounded port
/// task and one DNS task for each domain returned here.
fn acme_online_probe_domains(config: &AcmeDoctorConfig) -> Vec<String> {
    // A `*.myapp.com` entry has no address record of its own, so probing it
    // literally would resolve to nothing and report a permanent, meaningless
    // Warn on every wildcard deployment. Probe the base domain it covers
    // instead — that IS the host tenants' subdomains point at — and drop the
    // duplicate when the apex is also listed explicitly (#1620).
    let mut probed: Vec<String> = Vec::new();
    for domain in &config.domains {
        let target = domain.strip_prefix("*.").unwrap_or(domain).to_owned();
        if !target.is_empty() && !probed.contains(&target) {
            probed.push(target);
        }
    }
    probed
}

/// Inspect the stored ACME certificate for the CONFIGURED domains under the
/// configured directory namespace and grade its expiry, offline (reuses the
/// #1603 `inspect_leaf` path). `cache_dir` + `directory_label` are the same
/// pair `FsAcmeStore` is constructed from; `domains` is the active
/// `acme.domains`. Returns [`TlsDoctorData::NotConfigured`] when no cert for
/// the configured domains is stored yet (a first run before issuance is not a
/// failure).
fn resolve_acme_stored_cert_data(
    cache_dir: &std::path::Path,
    directory_label: &str,
    domains: &[String],
) -> TlsDoctorData {
    // Inspect EXACTLY the cert the runtime loads for these domains
    // (`CertId::from_domains(domains)`), not whichever file is newest.
    let Some((chain, key)) = configured_acme_cert_pair(cache_dir, directory_label, domains) else {
        return TlsDoctorData::NotConfigured;
    };

    #[cfg(feature = "tls")]
    {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
        .unwrap_or(i64::MAX);
        match autumn_web::tls::inspect_leaf(&chain, &key, now) {
            Ok(inspection) => TlsDoctorData::Healthy {
                not_yet_valid: inspection.is_not_yet_valid(now),
                expired: inspection.is_expired(now),
                days_until_expiry: inspection.days_until_expiry(now),
            },
            Err(e) => TlsDoctorData::Invalid {
                detail: e.to_string(),
            },
        }
    }
    #[cfg(not(feature = "tls"))]
    {
        let _ = (chain, key);
        TlsDoctorData::FeatureDisabled
    }
}

/// Locate the stored chain+key pair for the CONFIGURED domains under
/// `{cache_dir}/{directory_label}/` — the same directory namespace the
/// runtime's `FsAcmeStore` loads from. Inspecting the cert id derived from
/// the ACTIVE `acme.domains` (rather than whichever file is newest) matches
/// what `build_acme_tls_listener` loads: if `domains` changed and old cert
/// files remain, an expired-but-ignored old cert must not fail `--strict`,
/// and a healthy old cert for other domains must not be reported as the
/// stored cert for these domains. Returns `None` (→ "no cert yet for the
/// configured domains", a benign first-run state) when either half is
/// absent.
///
/// This crate's `tls` feature — on by default — pulls in `autumn-web/acme`
/// (see `tls = [...]` in `Cargo.toml`); this crate's OWN, separate `acme`
/// feature flag is not in `default` and is not what gates
/// `autumn_web::acme`'s availability, so `tls` is the cfg to key off here.
/// When it's enabled, this reuses `FsAcmeStore::find_cert_for_domains`
/// directly (issue #1864) rather than re-deriving the on-disk layout, so
/// doctor and the runtime store can never drift apart on where a certificate
/// lives. The feature-less fallback below re-derives the same layout by
/// hand, since it cannot name the feature-gated `FsAcmeStore` type at all.
#[cfg(feature = "tls")]
fn configured_acme_cert_pair(
    cache_dir: &std::path::Path,
    directory_label: &str,
    domains: &[String],
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    autumn_web::acme::store::FsAcmeStore::new(cache_dir, directory_label)
        .find_cert_for_domains(domains)
}

/// Feature-less fallback: replicates `FsAcmeStore::find_cert_for_domains`'s
/// on-disk layout and `CertId::from_domains`'s hashing by hand, because
/// `autumn_web::acme` is not compiled in without this crate's `tls` feature
/// (see the sibling `#[cfg(feature = "tls")]` implementation above). Keep in
/// EXACT sync with `store.rs` — a `#[cfg(feature = "tls")]` test
/// (`doctor_cert_id_matches_store`) asserts `acme_cert_id` stays byte-for-byte
/// equal to `CertId::from_domains`.
#[cfg(not(feature = "tls"))]
fn configured_acme_cert_pair(
    cache_dir: &std::path::Path,
    directory_label: &str,
    domains: &[String],
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let cert_dir = cache_dir.join(directory_label);
    let id = acme_cert_id(domains);
    let chain = cert_dir.join(format!("{id}.chain.pem"));
    let key = cert_dir.join(format!("{id}.key.pem"));
    if chain.exists() && key.exists() {
        Some((chain, key))
    } else {
        None
    }
}

/// The stable certificate id for a domain set, replicating
/// `autumn_web::acme::store::CertId::from_domains`. Only compiled for the
/// feature-less fallback above (or under test, where the drift-guard
/// comparison needs it too even when `tls` is enabled): sort + dedup the
/// domains, SHA-256 each with a trailing NUL separator, and hex-encode the
/// first 16 digest bytes.
#[cfg(any(not(feature = "tls"), test))]
fn acme_cert_id(domains: &[String]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut sorted: Vec<&str> = domains.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = Sha256::new();
    for domain in sorted {
        hasher.update(domain.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The largest `ca_root_path` file doctor will parse. A root PEM is a couple of
/// kilobytes; anything past this is not one, and reading it unbounded (a FIFO,
/// `/dev/zero`, a stray archive) would hang or balloon the CLI.
const MAX_CA_ROOT_BYTES: u64 = 1 << 20;

/// What `autumn doctor` observed about the configured ACME `ca_root_path`.
///
/// A dedicated type (rather than a bare `Result`) so the grader stays pure and
/// every outcome is unit-testable without touching the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeCaRootData {
    /// No `ca_root_path` configured — the ACME client uses the platform's trust
    /// store, which is what Let's Encrypt needs.
    NotConfigured,
    /// No `ca_root_path` configured, but the directory is a CUSTOM one whose
    /// root the platform trust store probably does not carry.
    MissingForPrivateDirectory,
    /// The key is present but is not a TOML string, so the runtime's
    /// `Option<PathBuf>` deserialization rejects it and the server will not boot.
    Malformed {
        /// The rendered offending value.
        value: String,
    },
    /// The key is present but blank, which `AcmeConfig::validate()` rejects at
    /// boot.
    Blank,
    /// The file parses and its first certificate is a usable trust anchor.
    Usable,
    /// Usable, but the file holds more than one certificate and only the
    /// **first** is installed as a trust anchor.
    ExtraCertificatesIgnored {
        /// The configured path, for the message.
        path: String,
        /// How many certificates the file holds.
        certificates: usize,
    },
    /// Configured alongside a built-in Let's Encrypt directory, whose API
    /// endpoint is publicly trusted — so this can only ever narrow trust and
    /// break issuance.
    UnneededForPublicDirectory {
        /// The configured path, for the message.
        path: String,
        /// The configured built-in directory (`staging` / `production`).
        directory: String,
    },
    /// The path does not exist, is not a regular file, is too large to be a
    /// root PEM, or could not be opened.
    Unreadable {
        /// The configured path, for the message.
        path: String,
        /// Why it could not be used.
        reason: String,
    },
    /// The file was read but yields no usable trust anchor.
    NotACertificate {
        /// The configured path, for the message.
        path: String,
        /// Why the content is unusable.
        reason: String,
    },
}

/// Grade the ACME `ca_root_path` (pure; injectable for tests).
///
/// The ACME client speaks HTTPS to the directory. When `ca_root_path` is set it
/// replaces the client's trust anchors wholesale, so an unreadable or non-PEM
/// file is not a warning: **every** order fails at the TLS handshake, before a
/// single authorization is created, and the app serves nothing but the
/// self-signed placeholder. That is a Fail, caught here rather than in
/// production.
/// Grade the `ca_root_path` outcomes that CANNOT work until the operator
/// changes something — a value the server refuses to boot on, or a file that
/// yields no usable trust anchor. Split out of
/// [`check_acme_ca_root_impl`] to keep both functions readable; the seam is
/// "can this configuration ever succeed as written?".
fn check_acme_ca_root_failure(data: &AcmeCaRootData) -> CheckResult {
    match data {
        AcmeCaRootData::Malformed { value } => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "[server.tls.acme] ca_root_path is {value}, not a string; the server will not \
                 start with this value"
            )),
            hint: Some("Set ca_root_path to a quoted path, e.g. \"config/ca-root.pem\""),
        },
        AcmeCaRootData::Blank => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Fail,
            detail: Some(
                "[server.tls.acme] ca_root_path is set but blank; the server refuses to start \
                 with it"
                    .into(),
            ),
            hint: Some(
                "Remove ca_root_path to use the platform trust store, or point it at the PEM \
                 root that signs your ACME directory's HTTPS certificate",
            ),
        },
        AcmeCaRootData::Unreadable { path, reason } => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "[server.tls.acme] ca_root_path {path} cannot be read ({reason}); the ACME \
                 client would trust no roots at all and every order would fail its TLS handshake"
            )),
            hint: Some(
                "Point ca_root_path at the PEM root that signs your ACME directory's HTTPS \
                 certificate, or remove it to use the platform trust store",
            ),
        },
        AcmeCaRootData::NotACertificate { path, reason } => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "[server.tls.acme] ca_root_path {path} yields no usable trust anchor ({reason})"
            )),
            hint: Some(
                "ca_root_path must be a PEM file whose first section is the ACME directory's \
                 root certificate (a `-----BEGIN CERTIFICATE-----` block)",
            ),
        },
        // The advisory outcomes are graded by the caller.
        AcmeCaRootData::NotConfigured
        | AcmeCaRootData::MissingForPrivateDirectory
        | AcmeCaRootData::Usable
        | AcmeCaRootData::ExtraCertificatesIgnored { .. }
        | AcmeCaRootData::UnneededForPublicDirectory { .. } => {
            unreachable!("advisory ca_root_path outcomes are graded by check_acme_ca_root_impl")
        }
    }
}

#[must_use]
pub fn check_acme_ca_root_impl(data: &AcmeCaRootData) -> CheckResult {
    match data {
        AcmeCaRootData::NotConfigured => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Pass,
            detail: Some(
                "no ca_root_path configured; the ACME client uses the platform trust store \
                 (correct for Let's Encrypt staging and production)"
                    .into(),
            ),
            hint: None,
        },
        AcmeCaRootData::MissingForPrivateDirectory => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Warn,
            detail: Some(
                "[server.tls.acme] sets a custom directory but no ca_root_path, so the ACME \
                 client falls back to the platform trust store; unless that directory's root is \
                 installed host-wide, the TLS handshake to it fails and every order dies before \
                 an authorization is created"
                    .into(),
            ),
            hint: Some(
                "Point ca_root_path at the PEM root that signs your ACME directory's HTTPS \
                 certificate, or install it in the host trust store",
            ),
        },
        AcmeCaRootData::Usable => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Pass,
            detail: Some("ca_root_path holds a usable trust anchor for the ACME directory".into()),
            hint: None,
        },
        AcmeCaRootData::ExtraCertificatesIgnored { path, certificates } => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "[server.tls.acme] ca_root_path {path} holds {certificates} certificates, but \
                 only the FIRST is installed as a trust anchor; if the root is not first, every \
                 ACME order will fail its TLS handshake"
            )),
            hint: Some(
                "Point ca_root_path at a file containing just the ACME directory's root \
                 certificate, not a leaf/intermediate bundle",
            ),
        },
        AcmeCaRootData::UnneededForPublicDirectory { path, directory } => CheckResult {
            name: "acme_ca_root",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "[server.tls.acme] ca_root_path {path} is set with directory = \"{directory}\"; \
                 Let's Encrypt's API endpoint is publicly trusted, and this replaces the \
                 platform trust store with just that root, so orders will fail unless it \
                 happens to sign Let's Encrypt's endpoint"
            )),
            hint: Some(
                "Remove ca_root_path for a Let's Encrypt directory; it is only for a private CA \
                 or a Pebble test server reached via a custom directory",
            ),
        },
        failure => check_acme_ca_root_failure(failure),
    }
}

/// Whether `directory_label` names a custom (non-Let's Encrypt) ACME directory.
///
/// `acme_directory_label` renders the built-ins as `staging` / `production` and
/// every custom URL as `custom-{hash}`, so anything else is custom by
/// construction. Kept as one predicate because two branches of
/// [`resolve_acme_ca_root_data`] depend on the same distinction and must not
/// drift apart.
fn is_custom_directory(directory_label: &str) -> bool {
    !matches!(directory_label, "staging" | "production")
}

/// Read and validate the configured `ca_root_path` into an [`AcmeCaRootData`].
///
/// Deliberately parses with the SAME code path
/// `instant_acme::Account::builder_with_root` uses — `CertificateDer::from_pem_file`
/// (which decodes only the **first** PEM section) followed by
/// `RootCertStore::add` — so doctor cannot pass a file the runtime then rejects,
/// nor bless a bundle whose root is not first.
fn resolve_acme_ca_root_data(
    path: Option<&std::path::Path>,
    ca_root_error: Option<&str>,
    directory_label: &str,
) -> AcmeCaRootData {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject as _;

    if let Some(value) = ca_root_error {
        return AcmeCaRootData::Malformed {
            value: value.to_owned(),
        };
    }
    let Some(path) = path else {
        // A custom directory is served under a root the platform store almost
        // never carries — the very handshake failure `ca_root_path` exists to
        // fix. Grading that Pass would let `doctor --strict` bless a config in
        // which every order dies before an authorization is created. Warn, not
        // Fail: installing the root host-wide is a legitimate alternative that
        // doctor cannot see from here.
        return if is_custom_directory(directory_label) {
            AcmeCaRootData::MissingForPrivateDirectory
        } else {
            AcmeCaRootData::NotConfigured
        };
    };
    // Mirror `AcmeConfig::validate()`, which rejects a blank path before boot —
    // otherwise this reports "cannot be read" with an empty path in the message.
    if path.to_str().is_none_or(|p| p.trim().is_empty()) {
        return AcmeCaRootData::Blank;
    }
    let rendered = path.display().to_string();

    // Bound the read before touching the contents: a FIFO or a device node would
    // otherwise stall the CLI, and every other doctor probe is bounded.
    match std::fs::metadata(path) {
        Err(e) => {
            return AcmeCaRootData::Unreadable {
                path: rendered,
                reason: e.to_string(),
            };
        }
        Ok(meta) if !meta.is_file() => {
            return AcmeCaRootData::Unreadable {
                path: rendered,
                reason: "not a regular file".to_owned(),
            };
        }
        Ok(meta) if meta.len() > MAX_CA_ROOT_BYTES => {
            return AcmeCaRootData::Unreadable {
                path: rendered,
                reason: format!("{} bytes is far larger than a root PEM", meta.len()),
            };
        }
        Ok(_) => {}
    }

    let first = match CertificateDer::from_pem_file(path) {
        Ok(der) => der,
        Err(e) => {
            return AcmeCaRootData::NotACertificate {
                path: rendered,
                reason: e.to_string(),
            };
        }
    };
    // `builder_with_root` installs it via `RootCertStore::add`, which parses the
    // DER; a PEM block holding junk gets past the decoder but not past this.
    let mut roots = rustls::RootCertStore::empty();
    if let Err(e) = roots.add(first) {
        return AcmeCaRootData::NotACertificate {
            path: rendered,
            reason: e.to_string(),
        };
    }

    // A private root pinned against a publicly-trusted Let's Encrypt endpoint can
    // only narrow trust and break issuance. Graded BEFORE the bundle case below,
    // and deliberately: for a Let's Encrypt directory the remedy is to remove
    // `ca_root_path` entirely, so reporting "trim the bundle down to its root"
    // would hand the operator a fix that preserves — and, once the file really
    // does contain only that unrelated root, guarantees — the failure.
    if !is_custom_directory(directory_label) {
        return AcmeCaRootData::UnneededForPublicDirectory {
            path: rendered,
            directory: directory_label.to_owned(),
        };
    }

    // Only the first section is ever installed, so warn when the operator has
    // handed us a bundle: the root is conventionally LAST in one.
    let certificates = CertificateDer::pem_file_iter(path).map_or(1, |iter| iter.flatten().count());
    if certificates > 1 {
        return AcmeCaRootData::ExtraCertificatesIgnored {
            path: rendered,
            certificates,
        };
    }

    AcmeCaRootData::Usable
}

/// Grade the stored ACME certificate's expiry (pure; injectable for tests).
///
/// Mirrors [`check_tls_impl`] but names the check `acme_stored_cert` and reads
/// "no stored certificate yet" (`NotConfigured`) as a benign Pass — the renewal
/// task will issue one on first boot.
#[must_use]
pub fn check_acme_stored_cert_impl(data: &TlsDoctorData) -> CheckResult {
    match data {
        TlsDoctorData::NotConfigured => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Pass,
            detail: Some("no ACME certificate stored yet; it will be issued on first boot".into()),
            hint: None,
        },
        TlsDoctorData::FeatureDisabled => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Warn,
            detail: Some(
                "[server.tls.acme] is configured but this autumn CLI was built without the `tls` \
                 feature, so the stored certificate could not be inspected"
                    .into(),
            ),
            hint: Some("Rebuild the autumn CLI with the `tls` feature to enable TLS diagnostics"),
        },
        TlsDoctorData::Invalid { detail } => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Fail,
            detail: Some(detail.clone()),
            hint: Some("Delete the corrupt cert from the ACME cache_dir so it is re-issued"),
        },
        TlsDoctorData::Healthy {
            not_yet_valid: true,
            ..
        } => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Fail,
            detail: Some(
                "the stored ACME certificate is not yet valid (its notBefore is in the future)"
                    .into(),
            ),
            hint: Some("Check the renewal task logs; renewal should have replaced it"),
        },
        TlsDoctorData::Healthy {
            expired: true,
            days_until_expiry,
            ..
        } => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "the stored ACME certificate expired {} day(s) ago",
                days_until_expiry.abs()
            )),
            hint: Some("Check the renewal task logs; renewal should have replaced it"),
        },
        TlsDoctorData::Healthy {
            expired: false,
            days_until_expiry,
            ..
        } if *days_until_expiry <= 30 => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "the stored ACME certificate expires in {days_until_expiry} day(s)"
            )),
            hint: Some("The renewal task should renew it automatically; watch for renewal errors"),
        },
        TlsDoctorData::Healthy {
            days_until_expiry, ..
        } => CheckResult {
            name: "acme_stored_cert",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "stored ACME certificate valid ({days_until_expiry} day(s) until expiry)"
            )),
            hint: None,
        },
    }
}

/// Resolve whether an operator-alert destination (email and/or webhook URL) is
/// configured, from the environment or the fully-merged effective `[alerts]` (base
/// `autumn.toml` layered with the active profile's `[profile.{name}]` section and
/// `autumn-{profile}.toml` override file, alias-normalized exactly as the runtime).
///
/// Returns `(email, webhook_configured, webhook_url)`, where `email` and
/// `webhook_url` are the RAW resolved `[alerts] email` / `[alerts] webhook_url`
/// values (empty when unset/cleared) so the caller can check both their presence
/// and their syntactic validity — an invalid address, or a non-absolute webhook
/// URL, delivers nothing at runtime. `webhook_configured` already folds in the
/// URL absoluteness and signing-secret requirements.
fn resolve_alert_destination() -> (String, bool, String) {
    // Evaluate the same fully-merged effective config the runtime boots with, not just
    // the raw base `autumn.toml`. The runtime loader normalizes profile aliases
    // (`production` → `prod`, `development` → `dev`) and layers the active profile's
    // inline `[profile.{name}]` section plus its `autumn-{profile}.toml` over the base.
    // Reading only the base `[alerts]` with a raw lower-cased profile string let doctor
    // greenlight a prod deploy whose `autumn-prod.toml`, or `[profile.prod.alerts]`,
    // cleared the destination — installing no alert channel at runtime. Build the
    // merged table through the runtime-mirroring loader and resolve against its
    // flattened top-level `[alerts]`; env vars still take highest precedence.
    //
    // Profile selection mirrors `resolve_profile_input`: a blank AUTUMN_ENV is ignored
    // before falling back to AUTUMN_PROFILE, so it cannot downgrade prod to dev.
    let selected_input = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    let normalized_profile = normalize_alert_profile(&selected_input);
    // The merge flattens the active profile's inline section and override file into
    // the top-level `[alerts]`, so resolve against an EMPTY profile: `_from_sources`
    // then reads only the effective top-level `[alerts]` (env vars still override).
    // Passing the profile here would re-read the STALE raw `[profile.{name}.alerts]`
    // section and ignore the higher-priority `autumn-{profile}.toml` layer.
    let merged = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    // `.ok()` deliberately does NOT filter empty values: a set-but-empty env
    // var is a PRESENT higher-priority layer that CLEARS the destination,
    // distinct from an unset var (which falls through to the TOML layers).
    let (_email_present, webhook) =
        resolve_alert_destination_from_sources(|key| std::env::var(key).ok(), Some(&merged), "");
    // Surface the RAW resolved email string (not just presence) so the
    // destination check can validate its syntax. Mirror the same env > base
    // precedence `_from_sources` applies to the flattened top-level `[alerts]`:
    // a PRESENT `AUTUMN_ALERTS__EMAIL` (even empty) wins and is terminal;
    // otherwise fall through to the merged base `[alerts] email`.
    let email = std::env::var("AUTUMN_ALERTS__EMAIL").unwrap_or_else(|_| {
        merged
            .get("alerts")
            .and_then(|a| a.get("email"))
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_owned()
    });
    // Surface the RAW resolved webhook URL (not just presence) under the same
    // env > merged-base precedence, so the destination check can name a
    // present-but-non-absolute value in its dedicated warning. `webhook` (above)
    // already gates on this URL being absolute AND the secret resolving.
    let webhook_url = std::env::var("AUTUMN_ALERTS__WEBHOOK_URL").unwrap_or_else(|_| {
        merged
            .get("alerts")
            .and_then(|a| a.get("webhook_url"))
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_owned()
    });
    (email, webhook, webhook_url)
}

/// Normalize a raw profile input to the canonical name the runtime config loader
/// uses, mirroring `autumn_web::config::normalize_profile_name`'s alias table
/// (`production`/`PROD` → `prod`, `development`/`DEV` → `dev`; custom names keep
/// their operator-specified spelling; blank stays blank). Kept in lock-step with
/// the runtime so doctor merges the same profile sources the app will boot with.
fn normalize_alert_profile(selected_input: &str) -> String {
    match selected_input.trim().to_ascii_lowercase().as_str() {
        "prod" | "production" => "prod".to_owned(),
        "dev" | "development" => "dev".to_owned(),
        "" => String::new(),
        _ => selected_input.trim().to_owned(),
    }
}

/// Resolve whether an email and/or webhook alert destination is configured,
/// honouring layer precedence exactly as the runtime config merge does.
///
/// The highest-priority layer that is PRESENT wins, and an explicitly-empty
/// higher layer CLEARS the value rather than falling back to a lower one — so a
/// base `[alerts] email = "…"` cleared by `[profile.prod.alerts] email = ""`
/// (or an empty `AUTUMN_ALERTS__EMAIL`) correctly resolves as "no destination".
/// Layers, highest first: env var, `[profile.{profile}.alerts]`, base
/// `[alerts]`. Only a layer that omits the key entirely falls through.
///
/// A webhook counts as configured only when `webhook_url` resolves to a
/// non-empty ABSOLUTE http(s) URL AND `webhook_secret` resolves non-empty under
/// this precedence — mirroring the runtime, which never registers an unsigned
/// webhook channel and whose HTTP client cannot dispatch a relative URL — so a
/// webhook URL with a missing/cleared secret, or a non-absolute URL, is not a
/// destination.
///
/// Split out from [`resolve_alert_destination`] so the precedence logic is unit
/// testable without mutating process-global env/cwd state.
fn resolve_alert_destination_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
    profile: &str,
) -> (bool, bool)
where
    F: Fn(&str) -> Option<String>,
{
    let profile_alerts = if profile.is_empty() {
        None
    } else {
        table
            .and_then(|t| t.get("profile"))
            .and_then(|v| v.get(profile))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("alerts"))
            .and_then(toml::Value::as_table)
    };
    let base_alerts = table
        .and_then(|t| t.get("alerts"))
        .and_then(toml::Value::as_table);

    // Resolve the RAW winning value for a key under the same layer precedence,
    // stopping at (not falling through) the first PRESENT layer — a present layer
    // wins even when its value is empty (an explicit clear). Returns an empty
    // string when no layer sets the key.
    let resolve_value = |env_key: &str, key: &str| -> String {
        // 1. Env var — highest priority. Present (even empty) is terminal.
        if let Some(val) = env_var(env_key) {
            return val;
        }
        // 2. Profile-specific `[profile.{profile}.alerts].{key}`.
        if let Some(v) = profile_alerts
            .and_then(|t| t.get(key))
            .and_then(toml::Value::as_str)
        {
            return v.to_owned();
        }
        // 3. Base `[alerts].{key}`.
        if let Some(v) = base_alerts
            .and_then(|t| t.get(key))
            .and_then(toml::Value::as_str)
        {
            return v.to_owned();
        }
        String::new()
    };
    let resolve =
        |env_key: &str, key: &str| -> bool { !resolve_value(env_key, key).trim().is_empty() };

    let email = resolve("AUTUMN_ALERTS__EMAIL", "email");
    // A webhook destination needs both a well-formed URL and a signing secret:
    //   * `alerts::install_from_config` refuses to register an unsigned webhook
    //     channel, so a URL with a missing or cleared secret installs no channel.
    //   * `http_client::Client::build_request` dispatches only a URL starting with
    //     `http://` or `https://`. A relative value — which an alert webhook can never
    //     resolve against a base-url alias — fails every signed POST, so a
    //     present-but-non-absolute URL is not a working destination either.
    // Count the webhook only when the resolved URL is a non-empty absolute http(s) URL
    // and the secret resolves non-empty, under the same per-field precedence, so
    // doctor agrees with the runtime: a URL without a secret, or a relative URL, is
    // not a destination, and in production with no other destination doctor warns.
    let webhook_url = resolve_value("AUTUMN_ALERTS__WEBHOOK_URL", "webhook_url");
    let webhook_secret = resolve("AUTUMN_ALERTS__WEBHOOK_SECRET", "webhook_secret");
    let webhook = is_absolute_http_url_doctor(webhook_url.trim()) && webhook_secret;
    (email, webhook)
}

/// Resolve the effective `[alerts] enabled` master switch, mirroring the runtime
/// config merge and the `AlertConfig::default().enabled` default of `true`.
///
/// The runtime's `alerts::install_from_config` returns before installing any
/// `Alerter` when `enabled` is false, so doctor must consult it: an
/// `enabled = false` prod config delivers NO operator alerts even with an
/// `email`/signed-`webhook` populated.
fn resolve_alert_enabled() -> bool {
    // Mirror `resolve_alert_destination`'s profile selection and merged-table
    // build so `enabled` resolves against the same effective config the runtime
    // boots with (profile inline section + `autumn-{profile}.toml` layered in),
    // env vars still highest priority.
    let selected_input = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    let normalized_profile = normalize_alert_profile(&selected_input);
    // The merge flattens the active profile into the top-level `[alerts]`, so
    // resolve against an EMPTY profile (mirroring `resolve_alert_destination`).
    let merged = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    resolve_alert_enabled_from_sources(|key| std::env::var(key).ok(), Some(&merged), "")
}

/// Resolve the effective `[alerts] enabled` flag under the same layer precedence
/// the runtime config merge applies (env `AUTUMN_ALERTS__ENABLED` >
/// `[profile.{profile}.alerts].enabled` > base `[alerts].enabled`), defaulting to
/// `true` when unset — matching `AlertConfig::default().enabled`.
///
/// Env parsing mirrors the runtime's `parse_env_bool`: `true`/`1` enable,
/// `false`/`0` disable, and any other value is ignored (falls through to the
/// TOML layers). Split out from [`resolve_alert_enabled`] so the precedence is
/// unit-testable without mutating process-global env/cwd state.
fn resolve_alert_enabled_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
    profile: &str,
) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    // 1. Env var — highest priority, mirroring runtime `parse_env_bool`. An
    //    invalid value (not true/false/1/0) is ignored and falls through.
    if let Some(val) = env_var("AUTUMN_ALERTS__ENABLED") {
        match val.as_str() {
            "true" | "1" => return true,
            "false" | "0" => return false,
            _ => {}
        }
    }
    // 2. Profile-specific `[profile.{profile}.alerts].enabled`.
    if !profile.is_empty()
        && let Some(v) = table
            .and_then(|t| t.get("profile"))
            .and_then(|v| v.get(profile))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("alerts"))
            .and_then(toml::Value::as_table)
            .and_then(|a| a.get("enabled"))
            .and_then(toml::Value::as_bool)
    {
        return v;
    }
    // 3. Base `[alerts].enabled`.
    if let Some(v) = table
        .and_then(|t| t.get("alerts"))
        .and_then(toml::Value::as_table)
        .and_then(|a| a.get("enabled"))
        .and_then(toml::Value::as_bool)
    {
        return v;
    }
    // 4. Unset everywhere → alerts enabled (mirrors `AlertConfig::default()`).
    true
}

/// Resolve the effective native-transport config strings (`pagerduty_routing_key`,
/// `pagerduty_url`, `slack_webhook_url`, `discord_webhook_url`) the runtime boots
/// with (issue #1630), honouring the same env `>` merged-base precedence as
/// [`resolve_alert_destination`]: a PRESENT `AUTUMN_ALERTS__*` env var (even
/// empty) wins and is terminal; otherwise the merged top-level `[alerts]` value
/// is used (empty when unset). Returned as raw strings so
/// [`check_alert_transports_impl`] can both validate and name a bad value.
fn resolve_alert_transports() -> (String, String, String, String) {
    let selected_input = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    let normalized_profile = normalize_alert_profile(&selected_input);
    let merged = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    let read = |env_key: &str, key: &str| -> String {
        std::env::var(env_key).unwrap_or_else(|_| {
            merged
                .get("alerts")
                .and_then(|a| a.get(key))
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_owned()
        })
    };
    (
        read(
            "AUTUMN_ALERTS__PAGERDUTY_ROUTING_KEY",
            "pagerduty_routing_key",
        ),
        read("AUTUMN_ALERTS__PAGERDUTY_URL", "pagerduty_url"),
        read("AUTUMN_ALERTS__SLACK_WEBHOOK_URL", "slack_webhook_url"),
        read("AUTUMN_ALERTS__DISCORD_WEBHOOK_URL", "discord_webhook_url"),
    )
}

/// Whether any native alert transport (`PagerDuty` routing key, Slack or Discord
/// webhook URL) is configured — the SAME predicate the runtime's
/// `AlertConfig::has_destination` uses (non-empty after trim).
///
/// Counting this as a destination keeps doctor's `alert_destination` gate in
/// lock-step with the runtime, which registers a native channel (and so
/// `has_destination()`/`is_active()` return true) for a native-only config;
/// without it a valid `pagerduty_routing_key`-only production config would fail
/// `autumn doctor --strict` with a spurious "no operator alert destination".
fn native_alert_transport_configured(
    pagerduty_routing_key: &str,
    slack_webhook_url: &str,
    discord_webhook_url: &str,
) -> bool {
    !pagerduty_routing_key.trim().is_empty()
        || !slack_webhook_url.trim().is_empty()
        || !discord_webhook_url.trim().is_empty()
}

/// Resolve the effective `[alerts] custom_channel` flag the same way the runtime
/// config merge and `AlertConfig::default().custom_channel` (default `false`)
/// do.
///
/// This is a doctor-only declaration: when true, the operator asserts they
/// register an alert channel in code via `AppBuilder::with_alert_channel`, so the
/// no-destination warning is suppressed. The runtime installs code-registered
/// channels regardless of this flag; it exists purely so `autumn doctor --strict`
/// can pass a validly-configured code-only alert setup.
fn resolve_alert_custom_channel() -> bool {
    // Mirror `resolve_alert_enabled`'s profile selection and merged-table build so
    // `custom_channel` resolves against the same effective config the runtime
    // boots with, env vars still highest priority.
    let selected_input = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    let normalized_profile = normalize_alert_profile(&selected_input);
    // The merge flattens the active profile into the top-level `[alerts]`, so
    // resolve against an EMPTY profile (mirroring `resolve_alert_enabled`).
    let merged = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    resolve_alert_custom_channel_from_sources(|key| std::env::var(key).ok(), Some(&merged), "")
}

/// Resolve the effective `[alerts] custom_channel` flag under the same layer
/// precedence the runtime config merge applies (env
/// `AUTUMN_ALERTS__CUSTOM_CHANNEL` > `[profile.{profile}.alerts].custom_channel` >
/// base `[alerts].custom_channel`), defaulting to `false` when unset — matching
/// `AlertConfig::default().custom_channel`.
///
/// Env parsing mirrors the runtime's `parse_env_bool`: `true`/`1` enable,
/// `false`/`0` disable, and any other value is ignored (falls through to the TOML
/// layers). Split out from [`resolve_alert_custom_channel`] so the precedence is
/// unit-testable without mutating process-global env/cwd state.
fn resolve_alert_custom_channel_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
    profile: &str,
) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    // 1. Env var — highest priority, mirroring runtime `parse_env_bool`. An
    //    invalid value (not true/false/1/0) is ignored and falls through.
    if let Some(val) = env_var("AUTUMN_ALERTS__CUSTOM_CHANNEL") {
        match val.as_str() {
            "true" | "1" => return true,
            "false" | "0" => return false,
            _ => {}
        }
    }
    // 2. Profile-specific `[profile.{profile}.alerts].custom_channel`.
    if !profile.is_empty()
        && let Some(v) = table
            .and_then(|t| t.get("profile"))
            .and_then(|v| v.get(profile))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("alerts"))
            .and_then(toml::Value::as_table)
            .and_then(|a| a.get("custom_channel"))
            .and_then(toml::Value::as_bool)
    {
        return v;
    }
    // 3. Base `[alerts].custom_channel`.
    if let Some(v) = table
        .and_then(|t| t.get("alerts"))
        .and_then(toml::Value::as_table)
        .and_then(|a| a.get("custom_channel"))
        .and_then(toml::Value::as_bool)
    {
        return v;
    }
    // 4. Unset everywhere → false (mirrors `AlertConfig::default()`).
    false
}

/// Resolve the RAW effective `[alerts] error_rate_threshold` (empty when unset)
/// against the same fully-merged config the runtime boots with, mirroring
/// [`resolve_alert_enabled`]'s profile selection and merged-table build.
///
/// The raw string (not a parsed number) is surfaced so
/// [`check_alert_error_rate_threshold_impl`] can both validate its range and name
/// a bad value in its warning. Env vars still take highest precedence.
fn resolve_alert_error_rate_threshold() -> String {
    let selected_input = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    let normalized_profile = normalize_alert_profile(&selected_input);
    // The merge flattens the active profile into the top-level `[alerts]`, so
    // resolve against an EMPTY profile (mirroring `resolve_alert_enabled`).
    let merged = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    resolve_alert_error_rate_threshold_from_sources(
        |key| std::env::var(key).ok(),
        Some(&merged),
        "",
    )
}

/// Resolve the RAW `[alerts] error_rate_threshold` under the same layer precedence
/// the runtime config merge applies (env `AUTUMN_ALERTS__ERROR_RATE_THRESHOLD` >
/// `[profile.{profile}.alerts].error_rate_threshold` > base
/// `[alerts].error_rate_threshold`), returning an empty string when unset — the
/// runtime then uses the default `0.05`.
///
/// The value is a TOML float in config files but a string via the env var, so it
/// is stringified regardless of its TOML scalar type (float/integer/string).
/// The highest-priority PRESENT layer wins; a present env var is terminal (even
/// empty, an explicit clear). Split out from [`resolve_alert_error_rate_threshold`]
/// so the precedence is unit-testable without mutating process-global env/cwd state.
fn resolve_alert_error_rate_threshold_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
    profile: &str,
) -> String
where
    F: Fn(&str) -> Option<String>,
{
    // Stringify a TOML scalar so a float (`0.05`), an integer (`1`), or a string
    // value all resolve to a comparable raw representation.
    let stringify = |v: &toml::Value| -> Option<String> {
        match v {
            toml::Value::Float(f) => Some(f.to_string()),
            toml::Value::Integer(i) => Some(i.to_string()),
            toml::Value::String(s) => Some(s.clone()),
            _ => None,
        }
    };
    // 1. Env var — highest priority. Present (even empty) is terminal.
    if let Some(val) = env_var("AUTUMN_ALERTS__ERROR_RATE_THRESHOLD") {
        return val;
    }
    // 2. Profile-specific `[profile.{profile}.alerts].error_rate_threshold`.
    if !profile.is_empty()
        && let Some(v) = table
            .and_then(|t| t.get("profile"))
            .and_then(|v| v.get(profile))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("alerts"))
            .and_then(toml::Value::as_table)
            .and_then(|a| a.get("error_rate_threshold"))
            .and_then(&stringify)
    {
        return v;
    }
    // 3. Base `[alerts].error_rate_threshold`.
    if let Some(v) = table
        .and_then(|t| t.get("alerts"))
        .and_then(toml::Value::as_table)
        .and_then(|a| a.get("error_rate_threshold"))
        .and_then(stringify)
    {
        return v;
    }
    // 4. Unset everywhere → empty (runtime uses the default 0.05).
    String::new()
}

/// Resolve whether the active profile is production from the environment or
/// `autumn.toml`.
fn resolve_is_production() -> bool {
    for var in ["AUTUMN_ENV", "AUTUMN_PROFILE"] {
        if let Ok(val) = std::env::var(var) {
            let v = val.trim().to_ascii_lowercase();
            if v == "prod" || v == "production" {
                return true;
            }
            if !v.is_empty() {
                return false;
            }
        }
    }
    // Fall back to build-mode detection: release binary implies prod
    false
}

/// Check whether Tailwind is enabled in `autumn.toml`.
///
/// Falls back to a heuristic: if `build.rs` exists the project is assumed to
/// use the Tailwind build pipeline.
fn tailwind_enabled() -> bool {
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| t.get("tailwind").and_then(toml::Value::as_bool))
        .unwrap_or_else(|| std::path::Path::new("build.rs").exists())
}

fn resolve_compression_enabled() -> bool {
    // 1. Env var takes highest precedence. Use the same accepted values as the
    //    runtime's parse_env_bool ("true"/"1"/"false"/"0") so doctor never
    //    reports a different state than the app would observe.
    if let Ok(val) = std::env::var("AUTUMN_COMPRESSION__ENABLED") {
        match val.as_str() {
            "true" | "1" => return true,
            "false" | "0" => return false,
            _ => {}
        }
    }

    // 2. Read TOML, applying profile-specific override when a profile is active
    //    (mirrors the five-layer config system so `[profile.prod] compression.enabled`
    //    doesn't cause a spurious doctor warning in production).
    let table = read_autumn_toml_table().unwrap_or_default();
    let profile = std::env::var("AUTUMN_ENV")
        .ok()
        .or_else(|| std::env::var("AUTUMN_PROFILE").ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let parse_enabled = |root: &toml::Table| {
        root.get("compression")
            .and_then(|v| v.get("enabled"))
            .and_then(toml::Value::as_bool)
    };

    // Profile-specific section takes precedence over base config.
    if !profile.is_empty()
        && let Some(enabled) = table
            .get("profile")
            .and_then(|v| v.get(&profile))
            .and_then(toml::Value::as_table)
            .and_then(&parse_enabled)
    {
        return enabled;
    }

    parse_enabled(&table).unwrap_or(false)
}

/// Map a shared `autumn deploy` preflight grader result into a doctor
/// [`CheckResult`] under the given (deploy-namespaced) name. A passing grader
/// becomes a `Pass`; a failing one becomes a `Fail` so `doctor --strict` treats
/// a broken deploy target as a hard error, consistent with the other
/// config-gated checks.
fn deploy_preflight_result(
    name: &'static str,
    check: crate::deploy::PreflightCheck,
) -> CheckResult {
    CheckResult {
        name,
        status: if check.passed {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: Some(check.detail),
        hint: check.hint,
    }
}

/// Resolve the `[deploy]` doctor branch from the merged active-profile runtime
/// table.
///
/// Returns `(Some(cfg), None)` when `[deploy]` is present and parses (run the
/// preflight graders), `(None, None)` when `[deploy]` is absent (skip the
/// preflight gracefully), and `(None, Some(fail))` when `[deploy]` is present
/// but a field has the wrong type. That last case is the important one: dropping
/// the parse error with `.ok()` would silently skip every deploy check while
/// `autumn deploy` (which loads the same `[deploy]` via `AutumnConfig::load()`)
/// fails on it — so `doctor --strict` would greenlight a config the deploy path
/// cannot parse. The `[deploy]` schema carries no secrets, so echoing the serde
/// error into the check detail is safe.
fn resolve_deploy_doctor_config(
    merged: &toml::Table,
    env: &dyn autumn_web::config::Env,
) -> (Option<DeployConfig>, Option<CheckResult>) {
    // First resolve the merged-profile `[deploy]` table (base autumn.toml +
    // [profile.<env>] + autumn-<env>.toml). A present-but-malformed table is a
    // hard fail regardless of env, because `AutumnConfig::load()` also fails to
    // deserialize it, so `autumn deploy` cannot run — surface it loudly instead
    // of silently skipping.
    let mut deploy_cfg = match merged.get("deploy").cloned() {
        Some(value) => match value.try_into::<DeployConfig>() {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                return (
                    None,
                    Some(CheckResult {
                        name: "deploy_config",
                        status: CheckStatus::Fail,
                        detail: Some(format!("[deploy] is present but invalid: {err}")),
                        hint: Some(
                            "Fix the [deploy] section in autumn.toml so its fields match the \
                             expected types (see `autumn deploy check`)",
                        ),
                    }),
                );
            }
        },
        None => None,
    };
    // Apply `AUTUMN_DEPLOY__*` env overrides on top of the merged-profile value,
    // exactly like `AutumnConfig::load()` does before `autumn deploy check`
    // grades `config.deploy`. This makes doctor grade the SAME deploy target as
    // `deploy check` for the same env + profile + TOML: an env-only
    // `AUTUMN_DEPLOY__HOST` materializes `Some(DeployConfig)` (so the preflight
    // runs instead of being skipped), and env values win over TOML. When no
    // `[deploy]` TOML and no `AUTUMN_DEPLOY__*` env exist, this stays `None` and
    // the preflight skips gracefully.
    autumn_web::config::apply_deploy_env_overrides(&mut deploy_cfg, env);
    (deploy_cfg, None)
}

/// Load guard mirroring what `autumn deploy check` does
/// (`deploy.rs::load_runtime_config`): attempt to load the full runtime
/// [`autumn_web::config::AutumnConfig`] under the DEPLOY profile via the SAME
/// `AutumnConfig::load_with_env_lenient_unknown_roots` call, using the
/// profile-forced env overlay (`deploy::deploy_profile_env_overlay`) the caller
/// already built.
///
/// `run()`'s deploy value graders build the merged TOML table with
/// [`get_merged_toml_table_runtime`], whose profile-override read swallows a
/// parse/IO error with `.ok()` — so a MALFORMED or unreadable
/// `autumn-<profile>.toml` override is silently DROPPED and doctor grades only
/// the base values, PASSING `--strict`. `autumn deploy check`, by contrast,
/// loads the config under the deploy profile and FAILS on the same override.
/// This guard closes that gap: when the deploy-profile load errors it returns a
/// FAILING `deploy_config` check reporting the load error; when it succeeds it
/// returns `None` and the existing value grading proceeds.
///
/// Lenient toward unknown top-level roots (#2063): as of the deploy CLI's move
/// to `AutumnConfig::load_with_env_lenient_unknown_roots`
/// (`deploy.rs::load_runtime_config`), `autumn deploy check`/`up` accept a
/// plugin-owned top-level root (e.g. `[media]`) as opaque and defer to app boot.
/// This guard MUST use the SAME lenient loader so `doctor --strict` and `deploy
/// check` agree: a plugin root that passes `deploy check` no longer FAILS the
/// doctor deploy-config check. Malformed TOML and typos inside a KNOWN/core
/// section stay fatal in the lenient loader, so those still fail here exactly as
/// they fail `deploy check`.
///
/// Additive: on a well-formed, deployable config the load succeeds — that is
/// exactly what lets `autumn deploy check` pass — so this never newly fails a
/// valid override.
fn deploy_profile_config_load_check(
    deploy_profile_raw: &str,
    env: &dyn autumn_web::config::Env,
) -> Option<CheckResult> {
    match autumn_web::config::AutumnConfig::load_with_env_lenient_unknown_roots(env) {
        Ok(_) => None,
        Err(err) => Some(CheckResult {
            name: "deploy_config",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "deploy profile config failed to load under `[deploy] profile = \
                 {deploy_profile_raw}`: {err} — `autumn deploy check` would reject this"
            )),
            hint: Some(
                "Fix the deploy-profile config (e.g. a malformed or unreadable \
                 autumn-<profile>.toml override) so it loads under the deploy profile \
                 (see `autumn deploy check`)",
            ),
        }),
    }
}

// ─── Main entry point ─────────────────────────────────────────────────────────

/// Remedy printed when `[deploy] host` / `[deploy] hosts` are spelled in a way the
/// shared validator refuses (both keys set, a blank entry, a duplicate).
///
/// A named constant so a unit test can pin its exact text: it is a `hint` field on
/// `CheckResult`, so it is emitted verbatim into `autumn doctor --json` as well as
/// the terminal, and a dropped `\` continuation would ship a run of spaces
/// mid-sentence on a machine-readable surface (issue #1621).
const DEPLOY_HOST_SPELLING_HINT: &str = "Fix the [deploy] host spelling in autumn.toml — \
     set either `host` (one server) or `hosts` (a fleet), never both (see `autumn deploy \
     check`)";

/// Remedy printed on the SSH-reachability row when the host spelling itself is
/// broken, so there is no list to probe. See [`DEPLOY_HOST_SPELLING_HINT`].
const DEPLOY_HOST_SPELLING_REACHABILITY_HINT: &str = "Fix the [deploy] host spelling in autumn.toml before probing reachability (see \
     `autumn deploy check`)";

/// Run all doctor checks and report results.
///
/// Checks are organised in two phases:
/// 1. **Config phase** (serial, fast) — file/env reads to decide which checks
///    to run and what data to pass them.
/// 2. **Check phase** (parallel) — every applicable check is spawned on its own
///    thread so that slow operations (TCP connect, subprocess calls) overlap.
///    Results are joined back in display order.
#[allow(clippy::too_many_lines)]
pub fn run(opts: DoctorOptions) {
    use std::thread;
    type Task = Box<dyn FnOnce() -> CheckResult + Send>;

    let cli_version = env!("CARGO_PKG_VERSION");

    if !opts.json {
        println!("\u{1F342} autumn doctor\n");
    }

    // ── Phase 1: config reads (serial, cheap) ────────────────────────────────
    let msrv = read_msrv().unwrap_or_else(|| "1.88.0".to_owned());
    let web_ver = read_autumn_web_version();
    let toml_result = std::fs::read_to_string("autumn.toml");
    // Resolve doctor's database topology and process role/jobs backend from the merged
    // active-profile config — base autumn.toml, `[profile.<env>]`, `autumn-<env>.toml`
    // — plus the `.env` overlay, the same layering the runtime, `detect_backend`, and
    // `autumn migrate` use, not the raw top-level `autumn.toml` table. A database URL,
    // `role`, or `jobs.backend` supplied only by an active profile or by `.env` is
    // invisible to the raw table, so a raw-table lookup under `AUTUMN_ENV=prod` would
    // miss a SQLite target and run the Postgres-only pg_dump/pg_restore and
    // pending-migration checks (F21). Env precedence is preserved: the resolver reads
    // the `.env`-backed overlay first, then the merged table, mirroring the sibling
    // deploy DB preflight below.
    let (db_canonical, db_selected, _) = resolve_active_profiles();
    let merged_db_toml = get_merged_toml_table_runtime(&db_canonical, &db_selected);
    let db_denv: Box<dyn autumn_web::config::Env> =
        match autumn_web::dotenv::os_env_with_dotenv_for_profile(&db_canonical) {
            Ok(e) => Box::new(e),
            Err(_) => Box::new(autumn_web::config::OsEnv),
        };
    let db_topology = resolve_database_topology_from_sources(
        |key| db_denv.var(key).ok().filter(|value| !value.is_empty()),
        Some(&merged_db_toml),
    );
    let (process_role, jobs_backend) = resolve_process_role_and_backend(Some(&merged_db_toml));
    let port = resolve_server_port();
    let tailwind = tailwind_enabled();
    let signing_secret = resolve_optional_signing_secret();
    let trusted_hosts = resolve_trusted_hosts();
    let is_production = resolve_is_production();
    let rate_limit_key_strategy = resolve_rate_limit_key_strategy();
    let auth_extractor_mounted = resolve_auth_extractor_mounted();
    let compression_enabled = resolve_compression_enabled();
    let (alert_email, alert_webhook_configured, alert_webhook_url) = resolve_alert_destination();
    let alerts_enabled = resolve_alert_enabled();
    let alert_custom_channel = resolve_alert_custom_channel();
    let alert_error_rate_threshold = resolve_alert_error_rate_threshold();
    let (alert_pd_routing_key, alert_pd_url, alert_slack_url, alert_discord_url) =
        resolve_alert_transports();
    let proxy_conflict_data = resolve_proxy_conflict_data();
    let (dotenv_has_example, dotenv_has_env, dotenv_uncovered) = resolve_dotenv_state();

    // ── Phase 2: build tasks in display order ────────────────────────────────
    let mut tasks: Vec<Task> = Vec::new();

    // 0. Platform support tier (#1616). First, because it frames every check
    // below: on Windows a developer needs to know which journeys are native
    // before they read a warning about one that is not.
    tasks.push(Box::new(|| {
        check_platform_support_impl(std::env::consts::OS)
    }));

    // 1. Rust toolchain
    tasks.push(Box::new(move || check_rust_toolchain(&msrv)));

    // 2. Version skew (only when autumn-web appears in the project's Cargo.toml)
    if let Some(web) = web_ver {
        tasks.push(Box::new(move || check_version_compat(cli_version, &web)));
    }

    // 3. autumn.toml
    match toml_result {
        Ok(content) => tasks.push(Box::new(move || check_toml_content(&content))),
        Err(_) => tasks.push(Box::new(|| CheckResult {
            name: "autumn_toml",
            status: CheckStatus::Warn,
            detail: Some("autumn.toml not found in current directory".into()),
            hint: Some("Run `autumn doctor` from your project root (where autumn.toml lives)"),
        })),
    }

    // 4+. Database topology and role-specific checks.
    let topology_for_check = db_topology.clone();
    tasks.push(Box::new(move || {
        check_database_topology_contract(&topology_for_check, is_production)
    }));

    // 4b. Process role vs. jobs backend: a split web/worker role can't share the
    // in-process `local` job queue across separate processes.
    tasks.push(Box::new(move || {
        check_split_topology_on_local(process_role, &jobs_backend)
    }));

    // 4c. Queue pinning zero-coverage guard (#1623): warn if jobs.pin leaves a
    // configured queue with no worker coverage anywhere in the topology. Build
    // from the merged active-profile table (base autumn.toml + [profile.<env>]
    // + autumn-<env>.toml) exactly like the runtime config loader — and like the
    // sibling profile-aware doctor checks (proxy conflict, mail unsubscribe) —
    // so profile-specific `jobs.queues` / `jobs.pin` layers are honored instead
    // of only the top-level `[jobs]` section.
    let (queue_canonical, queue_selected, _) = resolve_active_profiles();
    let merged_jobs_toml = get_merged_toml_table_runtime(&queue_canonical, &queue_selected);
    let (configured_queues, jobs_pin) = resolve_queues_and_pin(Some(&merged_jobs_toml));
    // Resolve the process role for this check from the same merged active-profile table
    // the queues and pin come from, not the raw top-level `toml_table` that feeds
    // `process_role` above. A `role` set only under `[profile.<env>]` or
    // `autumn-<env>.toml` — a web replica in prod, say — is invisible to the raw table,
    // so the role would resolve to `Combined`, the web-role skip-gate would never fire,
    // and `doctor --strict` could wrongly warn or fail on queue coverage for a process
    // that runs no job workers. Precedence mirrors the runtime exactly (`AUTUMN_ROLE`
    // env > merged-table `role` > `Combined`), reusing the shared resolver.
    let (queue_coverage_role, _) = resolve_process_role_and_backend(Some(&merged_jobs_toml));
    // Topology/registry inputs (#1756) that let the coverage check restore a
    // SOUND `--strict` hard-fail: the declared fleet topology (all worker tiers'
    // pins) and the compiled `#[job(queue)]` set (from a jobs manifest or an
    // inline list). Absent both, `check_queue_coverage_topology` delegates to the
    // informational-only per-process report, so existing deployments never
    // regress.
    let fleet_topology = resolve_fleet_topology(Some(&merged_jobs_toml));
    let declared_queues = resolve_declared_queues(Some(&merged_jobs_toml));
    tasks.push(Box::new(move || {
        check_queue_coverage_topology(
            queue_coverage_role,
            &configured_queues,
            &jobs_pin,
            &declared_queues,
            fleet_topology.as_ref(),
        )
    }));

    // A `sqlite://` primary boots only as the lone, single-backend, unsharded database
    // role. Pair it with a replica, add `[[database.shards]]`, keep a mismatched legacy
    // `database.url`, or mix backends, and `DatabaseConfig::validate` rejects it at
    // boot, so doctor must not Pass it (F19/F23/F26). Rather than re-derive that rule,
    // the bootability helper delegates to
    // `autumn_web::config::database_backend_consistency`, the function boot itself
    // uses, feeding it every resolved role: legacy `url`, `primary_url`, `replica_url`,
    // and shard presence. A `sqlite://` replica is never bootable, so its connectivity
    // check always passes `false`. Shard presence resolves through the same
    // profile-aware `.env` overlay and merged active-profile table as the topology,
    // reusing the migrate resolver so doctor and the real migration step agree.
    //
    // F28: a malformed `[[database.shards]]` entry resolves to `Err`. Collapsing that
    // to "no shards" let a SQLite primary with a broken shard entry keep the no-host
    // connectivity Pass, so `autumn doctor --strict` greenlit a config the app cannot
    // boot. Keep the `Result` so the `Err` surfaces as an authoritative Fail below and
    // the per-role connectivity Pass is withheld.
    let shard_resolution = crate::migrate::try_resolve_shard_database_urls_from_sources(
        |key| db_denv.var(key),
        Some(&merged_db_toml),
    );
    let db_has_shards = shard_resolution
        .as_ref()
        .is_ok_and(|shards| !shards.is_empty());
    let malformed_shard_error = shard_resolution.err();
    let shards_malformed = malformed_shard_error.is_some();
    // Finding F27: the boot-time backend-consistency rule
    // (`DatabaseConfig::validate`) must run as an UNCONDITIONAL doctor check,
    // independent of which backend is the effective primary. The SQLite-only
    // `sqlite_primary_target_is_bootable` gate below only fires for a SQLite
    // effective primary, so its mirror image — a Postgres primary with a mismatched
    // `sqlite://` legacy `database.url` (or a SQLite replica/shard) — slipped
    // through: with the Postgres primary reachable, the per-role connectivity Pass
    // greenlit a config the app rejects at boot. Compute the consistency verdict
    // once, on every resolved role, and make it AUTHORITATIVE: when it Fails, the
    // per-role connectivity Pass logic is withheld so a reachable primary can't
    // contradict the mismatch (findings F26 sqlite-primary+pg-legacy AND F27
    // pg-primary+sqlite-legacy, plus any role mismatch — replica, shards).
    let backend_consistent = autumn_web::config::database_backend_consistency(
        db_topology.legacy_url.as_deref(),
        db_topology.primary_url.as_deref(),
        db_topology.replica_url.as_deref(),
        db_has_shards,
    )
    .is_ok();
    // Only surface the check when a database is actually configured — a no-database
    // app has nothing to be inconsistent about. Any configured role (legacy `url`,
    // `primary_url`, `replica_url`, or shards) triggers it, so both mismatch
    // directions and role mismatches are covered.
    let db_configured = db_topology.primary_url.is_some()
        || db_topology.legacy_url.is_some()
        || db_topology.replica_url.is_some()
        || db_has_shards;
    if db_configured {
        let legacy = db_topology.legacy_url.clone();
        let primary = db_topology.primary_url.clone();
        let replica = db_topology.replica_url.clone();
        tasks.push(Box::new(move || {
            check_database_backend_consistency(
                legacy.as_deref(),
                primary.as_deref(),
                replica.as_deref(),
                db_has_shards,
            )
        }));
    }

    // Finding F28: surface a malformed `[[database.shards]]` entry as an
    // authoritative Fail — the resolver (and runtime/migrate) reject it, so it is
    // a boot-blocking problem, not "no shards". Emitted independently of
    // `db_configured`/`backend_consistent` (a shard-only config with a broken
    // entry resolves no other role) and, like the F27 gate below, it withholds
    // the per-role connectivity Pass so a "reachable" SQLite primary can't
    // contradict it.
    if let Some(err) = malformed_shard_error {
        tasks.push(Box::new(move || check_shard_config_resolvable(&err)));
    }

    let sqlite_primary_bootable = sqlite_primary_target_is_bootable(
        db_topology.legacy_url.as_deref(),
        db_topology.primary_url.as_deref(),
        db_topology.replica_url.as_deref(),
        db_has_shards,
    );
    // Withhold per-role connectivity Pass/Fail logic for a backend-inconsistent
    // config: the unconditional consistency FAIL above is the authoritative signal,
    // and a reachable primary must not double-report a contradictory "reachable"
    // Pass (finding F27, part 2). A backend-consistent config runs the ordinary
    // connectivity/pending-migration probes unchanged. Likewise withhold it when
    // the shard config is malformed (finding F28): the `db_shard_config` FAIL is
    // authoritative, so a SQLite primary must not receive a misleading no-host
    // connectivity Pass for a topology the app cannot boot.
    if backend_consistent && !shards_malformed {
        if let Some(url) = db_topology.primary_url.clone() {
            let primary_for_pending = url.clone();
            let primary_is_sqlite = autumn_web::config::DatabaseBackend::detect(&url)
                == Some(autumn_web::config::DatabaseBackend::Sqlite);
            tasks.push(Box::new(move || {
                check_db_connectivity(&url, sqlite_primary_bootable)
            }));
            // `check_pending_migrations` shells out to `diesel migration pending` and,
            // when the CLI is absent, tells users to install `diesel_cli ... --features
            // postgres` — a Postgres-specific warning / `--strict` failure that doesn't
            // apply to a SQLite app. SQLite migration tracking is deferred to #1906, so
            // emit an honest deferred note instead of the Postgres probe (finding F20),
            // gated off the SAME profile-merged backend detection as the pg_client_tools
            // skip and the connectivity check.
            if primary_is_sqlite {
                tasks.push(Box::new(check_pending_migrations_sqlite));
            } else {
                tasks.push(Box::new(move || {
                    check_pending_migrations(&primary_for_pending)
                }));
            }
        }

        if let Some(url) = db_topology.replica_url.clone() {
            tasks.push(Box::new(move || {
                check_db_role_connectivity("replica", &url, tcp_reachable, false)
            }));
        }

        if let (Some(primary_url), Some(replica_url)) = (
            db_topology.primary_url.clone(),
            db_topology.replica_url.clone(),
        ) {
            let fallback = db_topology.replica_fallback;
            tasks.push(Box::new(move || {
                check_replica_migrations(&primary_url, &replica_url, fallback)
            }));
        }
    }

    // 6. Port bindable
    tasks.push(Box::new(move || check_port_bindable(port)));

    // 7. Tailwind binary (only when build pipeline is present)
    if tailwind {
        tasks.push(Box::new(check_tailwind_binary));
    }

    // 7b. PostgreSQL client tools behind `autumn db backup` / `db restore`. On
    // a SQLite app (issue #1614) these Postgres-only tools don't apply, so a
    // missing `pg_dump`/`pg_restore` is not a problem — SQLite backup/restore
    // is tracked in #1909; don't emit a misleading Postgres-only warning.
    if db_topology
        .primary_url
        .as_deref()
        .and_then(autumn_web::config::DatabaseBackend::detect)
        == Some(autumn_web::config::DatabaseBackend::Sqlite)
    {
        tasks.push(Box::new(check_pg_client_tools_sqlite));
    } else {
        tasks.push(Box::new(check_pg_client_tools));
    }

    // 8. Signing-secret readiness (warn in dev, fail in prod when missing/weak)
    tasks.push(Box::new(move || {
        check_signing_secret_impl(signing_secret.as_deref(), is_production)
    }));
    tasks.push(Box::new(move || {
        check_trusted_hosts_impl(&trusted_hosts, is_production)
    }));

    // 8-deploy. Deploy preflight (#1607). Config-gated: it runs only when a
    // `[deploy]` section is present and skips gracefully otherwise, reusing the exact
    // graders behind `autumn deploy check`. The offline graders — signing secret,
    // database URL, migrate check — always run when deploy is configured; the
    // SSH-reachability probe is gated behind `--online`, so `doctor` stays offline and
    // non-flaky by default, matching the ACME probes.
    //
    // Resolve `[deploy]` from the merged active-profile runtime table (base
    // autumn.toml, `[profile.<env>]`, `autumn-<env>.toml`), like the sibling
    // profile-aware checks and the runtime loader, not the raw top-level `toml_table`.
    // A `[deploy]` block supplied only by an active profile is invisible to the raw
    // table, so a raw-table lookup would skip the preflight entirely — or `--online`
    // would probe the base host instead of the effective target — grading a deploy
    // config the runtime never loads.
    let (deploy_canonical, deploy_selected, _) = resolve_active_profiles();
    let merged_deploy_toml = get_merged_toml_table_runtime(&deploy_canonical, &deploy_selected);
    // Distinguish "[deploy] absent", which skips the preflight gracefully, from
    // "[deploy] present but malformed". Swallowing the parse error with `.ok()` would
    // silently drop every deploy check when a field has the wrong type (`ssh_port =
    // "22"`, `keep_releases = -1`), yet `autumn deploy` loads `AutumnConfig` and fails
    // on the same table, so `doctor --strict` would greenlight a config the deploy
    // path cannot parse. Surface a failing `deploy_config` check instead.
    //
    // `AUTUMN_DEPLOY__*` env overrides are then applied on top — env wins over TOML,
    // and an env-only host materializes a deploy config — matching
    // `AutumnConfig::load()`, so doctor grades the same target as `deploy check` for an
    // env-only or env-overridden deploy rather than skipping or probing a stale host.
    // They are read through the profile-aware `.env` overlay, since
    // `AutumnConfig::load` layers dotenv. Real OS env still wins; fall back to bare OS
    // env only if the overlay cannot be built from a malformed `.env`, matching the
    // TLS and offsite-backup checks.
    let deploy_denv: Box<dyn autumn_web::config::Env> =
        match autumn_web::dotenv::os_env_with_dotenv_for_profile(&deploy_canonical) {
            Ok(e) => Box::new(e),
            Err(_) => Box::new(autumn_web::config::OsEnv),
        };
    let (deploy_cfg, deploy_config_check) =
        resolve_deploy_doctor_config(&merged_deploy_toml, deploy_denv.as_ref());
    if let Some(check) = deploy_config_check {
        tasks.push(Box::new(move || check));
    }
    if let Some(deploy_cfg) = deploy_cfg {
        // Phase 2 (found via #1966 review): re-derive the value-resolution inputs —
        // the merged TOML table and the `.env` overlay — under the `[deploy] profile`,
        // not the ambient CLI runtime profile.
        //
        // Phase 1 above resolved `deploy_cfg` and its `.profile` from the `[deploy]`
        // table under the ambient profile, which is fine: that table decides the target
        // profile. But the deploy secret and DB values — signing secret, database URL,
        // shard URLs, db-backed-runtime DB requirement — must resolve under the deploy
        // profile the uploaded unit boots under. A value supplied only there
        // (`[profile.prod]`, `.env.prod`) is invisible to the ambient default `dev`
        // resolution, so grading it there would falsely report it missing or insecure
        // even though `autumn deploy check` resolves it correctly. Strictness grading
        // below already keys on the deploy profile.
        //
        // This mirrors `load_runtime_config`'s own two-phase reload in `deploy.rs`, the
        // same chicken-and-egg: learn the profile, then resolve under it. It reuses
        // `deploy::deploy_profile_env_overlay`, the exact overlay `load_runtime_config`
        // uses (`.env.<canonical>` selection plus a forced raw `AUTUMN_ENV`), rather
        // than replicating the layering, so doctor and `deploy check` cannot drift.
        // `trimmed_deploy_profile` yields the raw spelling exactly as
        // `ResolvedDeployConfig::resolve` stores it.
        let deploy_profile_raw = crate::deploy::trimmed_deploy_profile(&deploy_cfg.profile);
        let deploy_profile_canonical =
            crate::deploy::canonicalize_deploy_profile(&deploy_profile_raw);
        // Rebuild the merged TOML under the deploy profile (canonical + raw
        // selected), matching how `resolve_active_profiles`' (canonical, selected)
        // tuple feeds `get_merged_toml_table_runtime` above.
        let merged_deploy_toml =
            get_merged_toml_table_runtime(&deploy_profile_canonical, &deploy_profile_raw);
        // Rebuild the `.env` overlay under the deploy profile via the SAME path as
        // `load_runtime_config`, so `.env.<deploy>` loads and `AUTUMN_ENV` reads
        // as the deploy profile. Fall back to bare OS env on a malformed `.env`,
        // matching the phase-1 fallback above.
        let deploy_denv: Box<dyn autumn_web::config::Env> =
            match crate::deploy::deploy_profile_env_overlay(&deploy_profile_raw) {
                Ok(e) => Box::new(e),
                Err(_) => Box::new(autumn_web::config::OsEnv),
            };
        // Load guard (Codex review): the value graders below build
        // `merged_deploy_toml` via `get_merged_toml_table_runtime`, whose
        // profile-override read swallows a parse or IO error with `.ok()`. A malformed
        // or unreadable `autumn-<profile>.toml` override is therefore dropped silently,
        // and doctor would grade only the base values and pass `--strict` while `autumn
        // deploy check`, which loads under the deploy profile, fails on the same file.
        // Mirror `deploy check`'s load here — the same lenient loader
        // (`AutumnConfig::load_with_env_lenient_unknown_roots`, #2063) through the same
        // profile-forced `deploy_denv` overlay — and surface any error as a failing
        // `deploy_config` check, so the two agree: a plugin-owned top-level root such
        // as `[media]` that passes `deploy check` no longer fails `doctor --strict`,
        // while malformed TOML and known-section typos stay fatal in both. On a
        // deployable config the load succeeds, so this never newly fails a valid
        // override.
        if let Some(check) =
            deploy_profile_config_load_check(&deploy_profile_raw, deploy_denv.as_ref())
        {
            tasks.push(Box::new(move || check));
        }
        // Resolve the deploy signing secret from the SAME merged active-profile
        // table used for `deploy_cfg` and the DB URL (env first, then
        // `merged_deploy_toml`'s `security.signing_secret.secret`) — NOT the raw
        // top-level `autumn.toml` that `resolve_optional_signing_secret` reads. A
        // secret supplied only by the active profile is invisible to the raw
        // table, so a raw lookup would report it MISSING even though
        // `AutumnConfig::load()` and `autumn deploy check` see it.
        let deploy_signing =
            resolve_deploy_signing_secret(&merged_deploy_toml, deploy_denv.as_ref());
        // Rotation secrets accepted during a grace window are validated at boot
        // with the same rule as the current secret, so grade them here from the
        // same merged table (no env override exists for `previous_secrets`).
        let deploy_previous_signing_result =
            resolve_deploy_previous_signing_secrets(&merged_deploy_toml);
        // Derive the deploy DB-URL preflight input from the same merged active-profile
        // table used for `deploy_cfg`, like the sibling profile-aware checks above, not
        // the pre-merge `db_topology` built from the raw top-level `toml_table`. A
        // database URL supplied only by the active profile is invisible to the raw
        // table, so a raw-table lookup would fail `deploy_database_url` under `--strict`
        // even though `AutumnConfig::load()` and `autumn deploy check` see it. Env-var
        // precedence is preserved: `resolve_database_topology_from_sources` reads the
        // overlay first, then the merged table. Resolving through `deploy_denv`, the
        // profile-aware `.env` overlay, means a URL set only in `.env`/`.env.<profile>`
        // is seen by doctor exactly as `AutumnConfig::load()` sees it, rather than the
        // bare process env.
        let deploy_db_topology = resolve_database_topology_from_sources(
            |key| deploy_denv.var(key).ok().filter(|value| !value.is_empty()),
            Some(&merged_deploy_toml),
        );
        // Shard-only apps declare no control `primary_url`/`url` but still have a
        // usable database: `autumn migrate` targets each `[[database.shards]]` entry
        // (see `migrate::build_targets`). Resolve the shard URLs through the same
        // `deploy_denv` overlay and merged active-profile table, reusing the exact
        // migrate resolver so preflight and the real migration step agree, and fall
        // back to the first shard's primary URL when no control primary resolves. Only
        // writable targets count — `database.replica_url` is excluded, because `autumn
        // migrate` cannot migrate against a replica. The grader never prints the value.
        //
        // Use the fallible shard resolver here. The exiting
        // `resolve_shard_database_urls_from_sources` calls `std::process::exit(1)` on a
        // malformed `[[database.shards]]` entry, which — running while doctor tasks are
        // being built — would terminate `autumn doctor --json` before it emits its JSON
        // result, bypassing normal `CheckResult` reporting. Map an `Err` to a failing
        // `deploy_database_url` check below instead, so doctor still produces valid
        // output; `autumn migrate` keeps its print-and-exit behaviour via a thin
        // wrapper. The resolver is lifted into its own binding so it feeds both the
        // db-configured flag (shard presence) and the writable URL below.
        let deploy_shards_result = crate::migrate::try_resolve_shard_database_urls_from_sources(
            |key| deploy_denv.var(key),
            Some(&merged_deploy_toml),
        );
        // Derive db-configured from resolved URL/shard presence, mirroring
        // `deploy.rs` `collect_preflight` (`url || primary_url || replica_url ||
        // has_shards()`) — NOT mere `[database]` table presence, which flips true
        // on a tuning-only table (e.g. `pool_size`) and would falsely fail
        // `deploy_database_url` for a config `deploy check` accepts. `primary_url`
        // here folds `url`+`primary_url`. Computed BEFORE the writable-URL `.map()`
        // below moves `primary_url` (this only borrows it via `.is_some()`).
        let deploy_db_configured = deploy_db_topology.primary_url.is_some()
            || deploy_db_topology.replica_url.is_some()
            || deploy_shards_result
                .as_ref()
                .is_ok_and(|shards| !shards.is_empty());
        let deploy_db_url_result = deploy_shards_result.map(|deploy_shards| {
            deploy_db_topology
                .primary_url
                .or_else(|| deploy_shards.into_iter().next().map(|(_, url)| url))
        });
        // A DB-backed runtime feature (jobs.backend/scheduler.backend = postgres)
        // requires a configured pool at startup, so a writable DB URL is required
        // even with no migrations dir and no `[database]` section. Resolved from
        // the SAME merged active-profile table so `doctor` and `deploy check`
        // apply the identical DB-required rule.
        let deploy_db_backed_runtime =
            resolve_deploy_db_backed_runtime(&merged_deploy_toml, deploy_denv.as_ref());

        // Grade deploy-target checks against the resolved `[deploy] profile` (default
        // `prod`), not the ambient CLI runtime profile — `is_production`, read from
        // AUTUMN_ENV/AUTUMN_PROFILE, is false on a dev box. Otherwise `autumn doctor
        // --strict` greenlights a weak deploy signing secret that `autumn deploy
        // check`/`deploy up` fail, since those grade against the profile the uploaded
        // unit boots under. Normalize the profile as `deploy check` does, so alias
        // spellings (`PROD`, `Production`) still grade as production. Ambient
        // (non-deploy) checks keep using `is_production`.
        let deploy_is_production = crate::deploy::is_production_profile(Some(
            &crate::deploy::canonicalize_deploy_profile(&deploy_cfg.profile),
        ));

        // Host presence is validated offline, whenever `[deploy]` is configured: a
        // `[deploy]` table with a missing or blank `host` makes `autumn deploy check`
        // fail immediately, so default offline `doctor --strict` must fail on it too
        // rather than greenlighting a config the deploy path rejects. Only the TCP
        // connect probe is gated behind `--online`.
        //
        // #1621: the target is a list — `[deploy] host`, or `[deploy] hosts` as a fleet
        // in rollout order — and doctor enumerates it exactly as `deploy check` does,
        // through the same shared validator, so a fleet config is graded rather than
        // reported as "no target host configured". A malformed spelling — both keys set,
        // a blank entry, a duplicate — draws the same hard refusal `deploy check` makes,
        // surfaced here as a failing `deploy_host`.
        //
        // Doctor keeps one check per grader name even for a fleet: `CheckResult.name`
        // is a `&'static str` and an operator-visible `--json` key, so the per-host
        // detail rides in the detail text instead of multiplying names. See
        // `crate::deploy::DOCTOR_PREFLIGHT_GRADERS`.
        let deploy_hosts = crate::deploy::deploy_host_list(&deploy_cfg);
        tasks.push(Box::new({
            let deploy_hosts = deploy_hosts.clone();
            move || match &deploy_hosts {
                Ok(hosts) => deploy_preflight_result(
                    "deploy_host",
                    crate::deploy::grade_deploy_hosts_present(hosts),
                ),
                Err(message) => CheckResult {
                    name: "deploy_host",
                    status: CheckStatus::Fail,
                    detail: Some(message.clone()),
                    hint: Some(DEPLOY_HOST_SPELLING_HINT),
                },
            }
        }));
        if opts.online {
            let ssh_port = deploy_cfg.ssh_port;
            tasks.push(Box::new(move || match &deploy_hosts {
                Ok(hosts) => deploy_preflight_result(
                    crate::deploy::DOCTOR_PREFLIGHT_GRADERS[0],
                    crate::deploy::grade_fleet_ssh_reachability(
                        hosts,
                        ssh_port,
                        std::time::Duration::from_secs(5),
                    ),
                ),
                // The spelling itself is broken, so there is no list to probe; the
                // `deploy_host` check above already names the problem. Report the
                // reachability check as failing rather than silently omitting it —
                // omission would change the `--json` key set.
                Err(message) => CheckResult {
                    name: crate::deploy::DOCTOR_PREFLIGHT_GRADERS[0],
                    status: CheckStatus::Fail,
                    detail: Some(message.clone()),
                    hint: Some(DEPLOY_HOST_SPELLING_REACHABILITY_HINT),
                },
            }));
        }

        match deploy_previous_signing_result {
            Ok(deploy_previous_signing) => {
                tasks.push(Box::new(move || {
                    deploy_preflight_result(
                        crate::deploy::DOCTOR_PREFLIGHT_GRADERS[1],
                        crate::deploy::grade_signing_secret(
                            deploy_signing.as_deref(),
                            &deploy_previous_signing,
                            deploy_is_production,
                        ),
                    )
                }));
            }
            Err(msg) => {
                // A non-array `previous_secrets` or a non-string element: surface
                // it as a FAILING check on EVERY profile (not gated on
                // production), because `AutumnConfig::load()` deserializes the
                // field as `Vec<String>` and hard-fails the same config that
                // `autumn deploy check` loads. Silently dropping it would let a
                // strong current secret green-light a config the deploy path
                // rejects.
                tasks.push(Box::new(move || CheckResult {
                    name: crate::deploy::DOCTOR_PREFLIGHT_GRADERS[1],
                    status: CheckStatus::Fail,
                    detail: Some(format!(
                        "security.signing_secret.previous_secrets is present but invalid: {msg}"
                    )),
                    hint: Some(
                        "Set security.signing_secret.previous_secrets to an array of strings \
                         (see `autumn deploy check`).",
                    ),
                }));
            }
        }
        match deploy_db_url_result {
            Ok(deploy_db_url) => {
                tasks.push(Box::new(move || {
                    deploy_preflight_result(
                        crate::deploy::DOCTOR_PREFLIGHT_GRADERS[2],
                        crate::deploy::grade_database_url(
                            deploy_db_url.as_deref(),
                            std::path::Path::new("migrations"),
                            deploy_db_configured,
                            deploy_db_backed_runtime,
                        ),
                    )
                }));
            }
            Err(msg) => {
                // A malformed `[[database.shards]]` entry: surface it as a FAILING
                // check instead of exiting the process, so `autumn doctor --json`
                // still emits valid JSON with a clear, actionable failure.
                tasks.push(Box::new(move || CheckResult {
                    name: crate::deploy::DOCTOR_PREFLIGHT_GRADERS[2],
                    status: CheckStatus::Fail,
                    detail: Some(format!("[[database.shards]] is present but invalid: {msg}")),
                    hint: Some(
                        "Fix the [[database.shards]] entries in autumn.toml so each has a string \
                         `name` and `primary_url` with unique names (see `autumn deploy check`)",
                    ),
                }));
            }
        }
        tasks.push(Box::new(|| {
            deploy_preflight_result(
                crate::deploy::DOCTOR_PREFLIGHT_GRADERS[3],
                crate::deploy::grade_migrate_check(std::path::Path::new("migrations")),
            )
        }));
    }

    // 8a. Direct-HTTPS certificate readiness (#1603): validate `[server.tls]` cert and
    // key, warn on near-expiry, fail on expired.
    //
    // Skip this static-cert check for an ACME-only deployment — acme configured with
    // no static cert or key. The runtime's `TlsConfig::validate()` accepts ACME-only,
    // but `resolve_tls_paths()` sees the enclosing `[server.tls]` table and reports
    // empty paths, so `check_tls_impl` would emit a spurious "must set both cert_path
    // and key_path" Fail for every ACME deployment. The acme checks below grade that
    // mode instead.
    //
    // Resolve the ACME config from the merged active-profile runtime table, exactly as
    // `resolve_tls_paths()` does, not the raw top-level `toml_table`. A
    // `[server.tls.acme]` supplied only by an active profile or override file is
    // invisible to the raw table, so a raw-table lookup would leave `acme_configured`
    // false while `resolve_tls_paths()` still sees the enclosing `[server.tls]` with no
    // static paths — wrongly running, and failing, the static TLS check on a valid
    // profile-scoped ACME deployment.
    let (acme_canonical, acme_selected, _) = resolve_active_profiles();
    let merged_acme_toml = get_merged_toml_table_runtime(&acme_canonical, &acme_selected);
    let acme_config = resolve_acme_doctor_config(Some(&merged_acme_toml));
    // The profile whose encrypted credentials file the runtime would read, and the
    // tenancy base domain tenant subdomains resolve against — both read from the same
    // merged, profile-layered view the ACME config comes from, so the DNS-01 checks
    // below grade exactly what the app will see (#1620). Use the canonical profile,
    // not the raw environment variable: the runtime maps `production` → `prod` before
    // loading `config/credentials/<profile>.toml.enc` (`normalize_profile_name`), so
    // grading the raw spelling would read a different file than the server does, and
    // report "no credential found" for a correct deployment, or the reverse.
    let acme_credentials_profile = if acme_canonical.trim().is_empty() {
        "dev".to_owned()
    } else {
        acme_canonical.trim().to_ascii_lowercase()
    };
    let tenancy_base_domain = merged_acme_toml
        .get("tenancy")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("base_domain"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);

    // XOR inputs mirror `TlsConfig::validate`, which keys static-mode on
    // `cert_path.is_some()` / `key_path.is_some()`: base them on whether the
    // `cert_path`/`key_path` KEY is PRESENT (TOML or env override), independent of
    // an empty value. An explicitly-present-but-empty path is `Some("")` at
    // runtime, so `[server.tls.acme]` + `cert_path = ""` is a mixed static+ACME
    // config the runtime rejects — doctor must Fail it, not collapse the empty
    // path to "absent" and pass it as ACME-only.
    let (has_static_cert, has_static_key) = resolve_static_tls_presence();

    // Whether a genuine (non-empty) static cert/key PAIR exists — value-based,
    // gating the static-cert INSPECTION below. A present-but-empty path is not a
    // runnable static cert, so it stays absent here: an empty pure-static config
    // still Fails via the static check's "must set both" path, and an empty path
    // alongside acme skips the (now redundant) static inspection because the
    // mixed-mode Fail already fired.
    let (has_cert_path, has_key_path) = resolve_tls_paths()
        .map_or((false, false), |(cert, key)| {
            (!cert.as_os_str().is_empty(), !key.as_os_str().is_empty())
        });
    let static_cert_key_present = has_cert_path && has_key_path;

    // Mirror `TlsConfig::validate()`'s static-vs-ACME XOR + half-pair rules so
    // doctor FAILS exactly the combinations the runtime refuses to boot: static
    // cert/key AND [server.tls.acme] together (mutually exclusive), or only one
    // of cert_path/key_path (half pair). Without this, a config with BOTH a valid
    // static cert/key AND acme could PASS `--strict`, and a config with a single
    // static path plus acme would silently skip the static check — blessing a
    // config the app won't start.
    if let Some(mode_fail) =
        check_tls_mode_impl(has_static_cert, has_static_key, acme_config.is_some())
    {
        tasks.push(Box::new(move || mode_fail));
    }

    if should_run_static_tls_check(acme_config.is_some(), static_cert_key_present) {
        let tls_data = resolve_tls_doctor_data();
        tasks.push(Box::new(move || check_tls_impl(&tls_data)));
    }

    // 8a-bis. Automatic ACME provisioning (issue #1608). When [server.tls.acme]
    // is configured: always inspect the stored certificate offline (expiry), and
    // — only under --online — actively probe port reachability and DNS.
    if let Some(acme) = acme_config {
        // Mirror the runtime's ACME boot invariants FIRST: it refuses to boot an
        // ACME config with an invalid `directory` (deserialize failure), no
        // domains, a blank contact_email, or a wildcard domain. Without this,
        // doctor silently defaulted a bad directory to staging, and its
        // empty-domains path reported
        // `acme_stored_cert` as Pass with no probes, blessing a config that exits
        // at boot. Emit the FAIL and SKIP the misleading stored-cert / online
        // probes when the config is invalid (there is nothing valid to inspect).
        if let Some(config_fail) = check_acme_config_impl(&acme) {
            tasks.push(Box::new(move || config_fail));
        } else {
            // Offline: stored certificate expiry (reuses the #1603 inspect path).
            // Scan ONLY the configured directory namespace the runtime store loads
            // from ({cache_dir}/{directory_label}/), never sibling namespaces.
            let stored = resolve_acme_stored_cert_data(
                &acme.cache_dir,
                &acme.directory_label,
                &acme.domains,
            );
            tasks.push(Box::new(move || check_acme_stored_cert_impl(&stored)));

            // Offline: the private-CA trust anchor, if one is configured. A bad
            // path here fails every order at the TLS handshake, so it is graded
            // even though no network probe is involved.
            let ca_root = resolve_acme_ca_root_data(
                acme.ca_root_path.as_deref(),
                acme.ca_root_error.as_deref(),
                &acme.directory_label,
            );
            tasks.push(Box::new(move || check_acme_ca_root_impl(&ca_root)));

            // Offline: the DNS-01 provider credential (issue #1620). Read the
            // same way the runtime reads it — encrypted credentials store,
            // overlaid with AUTUMN_ACME_DNS_* — so a Pass here means the app
            // will find the same credential at boot.
            // Reading the credential needs the runtime's own resolution +
            // validation, which live behind autumn-web's `acme` feature (pulled
            // in by this crate's `tls` feature). Without it the check reports a
            // Warn rather than silently passing an unverified credential.
            #[cfg(feature = "tls")]
            {
                let credential = resolve_acme_dns_credential(
                    &acme,
                    &acme_credentials_profile,
                    std::path::Path::new("."),
                );
                tasks.push(Box::new(move || {
                    check_acme_dns_credential_impl(&credential)
                }));
            }
            #[cfg(not(feature = "tls"))]
            if acme.dns.is_some() || acme.dns_error.is_some() {
                tasks.push(Box::new(|| CheckResult {
                    name: "acme_dns_credential",
                    status: CheckStatus::Warn,
                    detail: Some(
                        "this autumn binary was built without the `tls` feature, so the DNS-01 \
                         provider credential cannot be read or validated"
                            .to_owned(),
                    ),
                    hint: Some("Rebuild the CLI with the `tls` feature to run this check"),
                }));
            }

            // Offline: does the certificate actually cover the tenant subdomains
            // `[tenancy] base_domain` will serve? A base domain the certificate
            // does not cover means every tenant host serves a name mismatch.
            let tenancy_base = tenancy_base_domain;
            let tenancy_domains = acme.domains.clone();
            let covers = tenancy_base
                .as_deref()
                .is_some_and(|base| acme_covers_tenant_subdomains(base, &tenancy_domains));
            tasks.push(Box::new(move || {
                check_acme_tenancy_domain_impl(tenancy_base.as_deref(), &tenancy_domains, covers)
            }));

            // Probe EVERY configured domain: issuance orders authorizations for
            // all `config.domains`, so a probe of only the first name can pass
            // doctor while issuance fails on an unprobed domain. One port + DNS
            // check per domain, each labeled with the domain in its detail; still
            // bounded and gated behind --online.
            if opts.online {
                let dns01 = acme.dns.is_some();
                for domain in acme_online_probe_domains(&acme) {
                    let d80 = domain.clone();
                    tasks.push(Box::new(move || {
                        let p80 = probe_port(&d80, 80);
                        let p443 = probe_port(&d80, 443);
                        check_acme_ports_for_challenge(&d80, p80, p443, dns01)
                    }));
                    let d_dns = domain;
                    tasks.push(Box::new(move || {
                        let outcome = resolve_dns_points_here(&d_dns);
                        check_acme_dns_for_challenge(&d_dns, &outcome, dns01)
                    }));
                }

                // Online: can public DNS answer for the zone's _acme-challenge
                // name at all? The CA reads the challenge record from public
                // DNS, so a broken delegation fails every DNS-01 order however
                // correctly the record is written (#1620).
                #[cfg(feature = "tls")]
                if let Some(dns_cfg) = acme.dns.clone() {
                    let resolvers = dns_cfg.resolver_addrs().unwrap_or_default();
                    for domain in acme_online_probe_domains(&acme) {
                        let fqdn = autumn_web::acme::dns::challenge_fqdn(&domain);
                        let resolvers = resolvers.clone();
                        tasks.push(Box::new(move || {
                            let visibility = resolve_challenge_dns_visibility(&fqdn, &resolvers);
                            check_acme_dns_propagation_impl(&fqdn, &visibility)
                        }));
                    }
                }
            }
        }
    }

    // 8b. Rate-limit key-strategy misconfiguration
    tasks.push(Box::new(move || {
        check_rate_limit_key_strategy_impl(&rate_limit_key_strategy, auth_extractor_mounted)
    }));

    // 8c. Conflicting trusted-proxy configuration (new vs. deprecated fields)
    tasks.push(Box::new(move || {
        check_proxy_conflict_impl(&proxy_conflict_data)
    }));

    // 8d. Deprecated config key usage
    let deprecated_keys = resolve_deprecations();
    tasks.push(Box::new(move || {
        check_deprecated_keys_impl(&deprecated_keys)
    }));

    // 9. OAuth2 provider checks (client_id and client_secret validation)
    let oauth2_providers = resolve_oauth2_providers();
    for p in oauth2_providers {
        tasks.push(Box::new(move || {
            check_oauth2_provider_impl(&p.name, &p.client_id, &p.client_secret, is_production)
        }));
    }

    // 9b. List-Unsubscribe wiring: fail closed in prod when a `#[mailer]` declares
    // `list_unsubscribe` but no unsubscribe destination is configured. Layer the
    // profile sources exactly as the runtime config loader does — alias-aware inline
    // precedence with the canonical spelling winning, and a single override file
    // preferring the selected spelling — so doctor evaluates what the app will boot
    // with rather than a stale legacy spelling. Profile selection mirrors
    // `resolve_profile_input`: a blank AUTUMN_ENV is ignored before falling back to
    // AUTUMN_PROFILE, so it cannot downgrade a prod selection to dev.
    let raw_mail_profile = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "dev".to_owned());
    let selected_input = raw_mail_profile.trim().to_owned();
    let normalized_profile = match selected_input.to_lowercase().as_str() {
        "prod" | "production" => "prod".to_owned(),
        "dev" | "development" | "" => "dev".to_owned(),
        other => other.to_owned(),
    };
    let merged_mail_toml = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    let (unsub_base_url, unsub_mailto) = resolve_mail_unsubscribe(Some(&merged_mail_toml));
    // Resolve the effective transport the same way the runtime does: a *valid* env
    // override (trimmed, case-insensitive) wins, then explicit `[mail].transport`,
    // then the profile smart-default (only `dev` defaults to `log`; every other
    // profile defaults to `disabled`). An invalid/whitespace env value is ignored,
    // matching `Transport::from_env_value`.
    let effective_transport = resolve_effective_mail_transport(
        std::env::var("AUTUMN_MAIL__TRANSPORT").ok(),
        merged_mail_toml
            .get("mail")
            .and_then(toml::Value::as_table)
            .and_then(|m| m.get("transport"))
            .and_then(toml::Value::as_str),
        &normalized_profile,
    );
    let mail_transport_disabled = effective_transport == "disabled";
    let unsub_token_ttl_days = resolve_unsubscribe_token_ttl_days(Some(&merged_mail_toml));
    let unsub_ttl_toml_type_invalid = unsubscribe_ttl_toml_type_invalid(Some(&merged_mail_toml));
    let unsub_dest_toml_type_invalid = unsubscribe_dest_toml_type_invalid(Some(&merged_mail_toml));
    let has_list_unsubscribe_usage = detect_list_unsubscribe_usage();
    tasks.push(Box::new(move || {
        // A present-but-non-string destination fails the runtime's typed config
        // load (Option<String>) before boot; doctor reads an untyped table, so a
        // non-string value would otherwise look absent. Flag it like the TTL guard.
        if unsub_dest_toml_type_invalid {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(
                    "mail.unsubscribe_base_url and mail.unsubscribe_mailto must be strings; a \
                     non-string TOML value (e.g. an integer or array) fails configuration loading \
                     before the app can boot"
                        .into(),
                ),
                hint: Some(
                    "Set mail.unsubscribe_base_url / mail.unsubscribe_mailto to quoted string values",
                ),
            };
        }
        // A present-but-non-integer TOML TTL fails the runtime's typed config load
        // before boot; doctor reads an untyped table, so flag it here rather than
        // letting it silently default in `resolve_unsubscribe_token_ttl_days`.
        if unsub_ttl_toml_type_invalid {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(
                    "mail.unsubscribe_token_ttl_days must be an integer number of days; a \
                     non-integer TOML value (e.g. a quoted string or a float) fails configuration \
                     loading before the app can boot"
                        .into(),
                ),
                hint: Some(
                    "Set mail.unsubscribe_token_ttl_days to a bare integer, e.g. unsubscribe_token_ttl_days = 30",
                ),
            };
        }
        check_mail_unsubscribe_config_impl(
            has_list_unsubscribe_usage,
            unsub_base_url.as_deref(),
            unsub_mailto.as_deref(),
            mail_transport_disabled,
            is_production,
            unsub_token_ttl_days,
        )
    }));

    // 10. Stale artifacts (warn only, never fail)
    tasks.push(Box::new(check_stale_artifacts));

    // 11. Maintenance mode state
    tasks.push(Box::new(check_maintenance_mode));

    // 12. Compression (warn in production when disabled)
    tasks.push(Box::new(move || {
        check_compression_impl(compression_enabled, is_production)
    }));

    // 12a. Operator alerts (warn in production when no destination configured).
    //      Gate the email destination on a usable mail transport, reusing the
    //      SAME `mail_transport_disabled` the mail-unsubscribe check resolved
    //      from the merged config above (a `Copy` bool, still readable after it
    //      was moved into the mail closure). This mirrors runtime
    //      `mailer.is_disabled()`: an email with a disabled transport installs
    //      no alert channel, so it must not count toward a valid destination.
    let mail_transport_usable = !mail_transport_disabled;
    // Also gate the email destination on a resolved, VALID `[mail] from` for
    // transports that require one (SMTP): the runtime's alert mail sets no
    // per-message `from`, so an SMTP email with no `[mail] from` — or a
    // present-but-unparsable one, which aborts boot in `MailerBuilder::build` —
    // fails to deliver. Pass the effective value (empty when unset) so the check
    // can both gate on validity and name a bad value. Reuse the SAME merged config
    // / effective transport the mail checks above resolved.
    let mail_from = resolve_mail_from(Some(&merged_mail_toml)).unwrap_or_default();
    let transport_requires_from = mail_transport_requires_from(&effective_transport);
    // A native transport (PagerDuty / Slack / Discord) counts as a configured
    // destination exactly as the runtime's `AlertConfig::has_destination` does, so
    // a native-only config does not trip the "no operator alert destination"
    // warning that would fail `autumn doctor --strict` while the runtime happily
    // registers the channel.
    let alert_native_configured = native_alert_transport_configured(
        &alert_pd_routing_key,
        &alert_slack_url,
        &alert_discord_url,
    );
    tasks.push(Box::new(move || {
        check_alert_destination_impl(
            alerts_enabled,
            &alert_email,
            alert_webhook_configured,
            &alert_webhook_url,
            mail_transport_usable,
            is_production,
            &mail_from,
            transport_requires_from,
            alert_custom_channel,
            alert_native_configured,
        )
    }));

    // 12a2. `[alerts] error_rate_threshold` sanity: a non-finite or out-of-(0,1]
    //       value silently breaks the 5xx alert (the runtime falls back to the
    //       default). Warn in production so the operator fixes the config.
    tasks.push(Box::new(move || {
        check_alert_error_rate_threshold_impl(&alert_error_rate_threshold, is_production)
    }));

    // 12a3. Native alert transports (issue #1630): a configured PagerDuty /
    //       Slack / Discord destination must carry the values it needs to
    //       deliver (a well-formed routing key, an absolute pagerduty_url, an
    //       absolute https Slack/Discord webhook URL). Warn in production so a
    //       transport that *looks* configured actually pages.
    tasks.push(Box::new(move || {
        check_alert_transports_impl(
            &alert_pd_routing_key,
            &alert_pd_url,
            &alert_slack_url,
            &alert_discord_url,
            is_production,
        )
    }));

    // 12b. Local-dev `.env` handling (warn on `.env.example` without `.env`,
    //      or a present-but-ungitignored `.env`).
    tasks.push(Box::new(move || {
        check_dotenv_impl(dotenv_has_example, dotenv_has_env, &dotenv_uncovered)
    }));

    // 12c. Offsite backup config presence (issue #1619): informational when
    //      unconfigured; flags an unset bucket, a shared bucket without opt-in,
    //      or credentials that aren't ready. Never prints credential values.
    tasks.push(Box::new(|| {
        check_offsite_backup_impl(&resolve_offsite_backup_data())
    }));

    // 13. System-test browser (warn if missing; not all projects use system tests)
    tasks.push(Box::new(check_system_test_browser));

    // 14. GDPR export/erasure registration (warn when auth starter is present
    //     but #[repository] models are not registered in GdprRegistry).
    //     File-system work runs inside the task so it overlaps with other checks.
    tasks.push(Box::new(|| {
        let has_auth_starter = std::path::Path::new("src/routes/auth.rs").exists();
        let unregistered = resolve_gdpr_unregistered_tables();
        check_gdpr_export_registration_impl(
            has_auth_starter,
            &unregistered.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }));

    // 15. Model column privacy (issue #1374): warn when a `#[model]` has a
    //     sensitively-named column (password/token/secret/*_hash) that is not
    //     marked `#[private]`, so it would leak into JSON responses.
    tasks.push(Box::new(|| {
        let found = resolve_unprivate_sensitive_columns();
        check_model_private_columns_impl(&found)
    }));

    // 16. Edge capsule toolchain (issue #1790): a project with `#[edge]` routes
    //     needs the wasm32-wasip1 std library installed or `autumn build` cannot
    //     emit the capsule. Both the source scan and the toolchain probe run
    //     inside the task so they overlap with the other checks.
    tasks.push(Box::new(|| {
        let scan = crate::edge_scan::resolve_edge_scan(std::path::Path::new("."));
        // Probe the toolchain only when the answer can matter: a project with no
        // #[edge] routes must not pay for a `rustc` spawn on every doctor run.
        let installed = !scan.is_empty() && crate::build::edge_target_installed();
        check_edge_target_impl(&scan, installed)
    }));

    // 17. Edge route wiring (issue #1790): an `#[edge]` handler that also
    //     carries an auth guard fails the build, an unregistered one is never
    //     served at the edge, and a missing `src/bin/edge-capsule.rs` leaves
    //     nothing to compile.
    tasks.push(Box::new(|| {
        let scan = crate::edge_scan::resolve_edge_scan(std::path::Path::new("."));
        let capsule_bin_exists = std::path::Path::new(EDGE_CAPSULE_BIN).exists();
        check_edge_routes_impl(&scan, capsule_bin_exists)
    }));

    // 18. Orphaned plugin residue (issue #1631): a dependency with no mount, a
    //     mount with no dependency, or migrations still applied for a plugin
    //     that is no longer in the app. The migration half is best effort —
    //     `__diesel_schema_migrations` has no source column, so the only way to
    //     attribute a version to a plugin is the plugin's own declared list,
    //     and reading the history at all needs a configured database plus the
    //     `diesel` CLI. Without either, the check still reports the two static
    //     findings and simply never raises the third.
    let residue_database_url = db_topology.primary_url.clone().or(db_topology.legacy_url);
    tasks.push(Box::new(move || {
        let wirings = resolve_plugin_wirings(std::path::Path::new("."), || {
            residue_database_url
                .as_deref()
                .map_or_else(Vec::new, applied_migration_versions)
        });
        check_plugin_residue_impl(&wirings)
    }));

    // 19. Dependency advisories and policy (issue #1633): the app's lockfile
    //     graded against its own `deny.toml` — the same policy file, waiver
    //     store and auditor that #1600's CI gate runs, so this verdict predicts
    //     that gate. Always offline — the advisory database is never fetched
    //     here — and bounded, so a `cargo metadata` waiting on Cargo's
    //     package-cache lock reports no verdict rather than hanging doctor.
    //     See `crate::deps` for what parity does and does not cover.
    tasks.push(Box::new(|| {
        check_dependencies_impl(&crate::deps::evaluate_within(
            std::path::Path::new("."),
            crate::deps::DOCTOR_BUDGET,
        ))
    }));

    // ── Phase 3: spawn all tasks concurrently ────────────────────────────────
    #[allow(clippy::needless_collect)]
    let handles: Vec<thread::JoinHandle<CheckResult>> =
        tasks.into_iter().map(thread::spawn).collect();

    // ── Phase 4: join in order (preserves display ordering) ──────────────────
    let results: Vec<CheckResult> = handles
        .into_iter()
        .map(|h| {
            h.join().unwrap_or_else(|_| CheckResult {
                name: "internal_error",
                status: CheckStatus::Fail,
                detail: Some("a check panicked unexpectedly".into()),
                hint: None,
            })
        })
        .collect();

    let summary = compute_summary(&results);
    let code = exit_code(&summary, opts.strict);

    if opts.json {
        println!("{}", to_json_output(&results, &summary));
    } else {
        for r in &results {
            println!("{}", format_check_line(r));
        }
        println!();
        println!("{}", format_summary_line(&summary, code));
    }

    std::process::exit(code);
}

/// Check whether maintenance mode is currently active.
///
/// Warns (not fails) so `autumn doctor` stays green during planned windows.
/// Check whether response compression is configured for a production profile.
///
/// Compression is off by default (for BREACH/CRIME safety). When running in
/// production without compression this check emits a `Warn` so operators know
/// they may want to enable it (or document the deliberate choice to rely on a
/// CDN / reverse-proxy for compression instead).
pub fn check_compression_impl(compression_enabled: bool, is_production: bool) -> CheckResult {
    if is_production && !compression_enabled {
        return CheckResult {
            name: "compression",
            status: CheckStatus::Warn,
            detail: Some(
                "response compression is disabled in production; \
                 text payloads (HTML/JSON/CSS) are served uncompressed"
                    .into(),
            ),
            hint: Some(
                "Set [compression] enabled = true in autumn.toml (or AUTUMN_COMPRESSION__ENABLED=true) \
                 if you are not using a CDN or reverse-proxy that compresses for you. \
                 Read the BREACH/CRIME tradeoff in docs/guide/compression.md before enabling.",
            ),
        };
    }
    CheckResult {
        name: "compression",
        status: CheckStatus::Pass,
        detail: Some(if compression_enabled {
            "response compression is enabled".into()
        } else {
            "response compression is disabled (off by default; use CDN or enable explicitly)".into()
        }),
        hint: None,
    }
}

/// Config-presence view of `[backup.offsite]` for [`check_offsite_backup_impl`].
/// Holds only names/booleans — never a credential value.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // a flat config-presence snapshot, not state
pub struct OffsiteBackupData {
    /// A `[backup.offsite]` section is present.
    pub configured: bool,
    /// The offsite bucket, when set.
    pub bucket: Option<String>,
    /// The configured access-key env var NAME (not value), when set.
    pub access_key_env: Option<String>,
    /// The configured secret-key env var NAME (not value), when set.
    pub secret_key_env: Option<String>,
    /// Whether the named access-key env var is actually present in the env.
    pub access_key_env_set: bool,
    /// Whether the named secret-key env var is actually present in the env.
    pub secret_key_env_set: bool,
    /// The offsite destination equals the app `[storage.s3]` bucket+endpoint and
    /// `allow_shared_bucket` was not set.
    pub shared_bucket_without_optin: bool,
}

/// Config-presence check for offsite backups (issue #1619). Never prints a
/// credential VALUE — only env-var names and the bucket. This is config-key
/// doctoring, not an alerting check.
#[must_use]
pub fn check_offsite_backup_impl(data: &OffsiteBackupData) -> CheckResult {
    const NAME: &str = "offsite_backup";
    if !data.configured {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some(
                "no offsite backup destination configured (a backup on the same disk as the \
                 database is not disaster recovery)"
                    .into(),
            ),
            hint: Some(
                "Add a [backup.offsite] section (see docs) and run `autumn db backup --upload` \
                 to keep a verified copy off the server.",
            ),
        };
    }
    let Some(bucket) = data.bucket.as_deref().filter(|b| !b.trim().is_empty()) else {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Fail,
            detail: Some(
                "[backup.offsite] is configured but backup.offsite.s3.bucket is unset".into(),
            ),
            hint: Some("Set backup.offsite.s3.bucket to the offsite bucket name."),
        };
    };
    if data.shared_bucket_without_optin {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Fail,
            detail: Some(format!(
                "offsite bucket {bucket:?} is the same as the app [storage.s3] bucket at the \
                 same endpoint, without opt-in"
            )),
            hint: Some(
                "Point the offsite backup at a distinct bucket, or set \
                 backup.offsite.allow_shared_bucket = true to acknowledge the shared bucket.",
            ),
        };
    }
    // Credential indirection: the env-var NAMES must be configured AND present.
    let mut missing: Vec<&str> = Vec::new();
    if data
        .access_key_env
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        missing.push("backup.offsite.s3.access_key_id_env (name unset)");
    } else if !data.access_key_env_set {
        missing.push("access key env var is not set in the environment");
    }
    if data
        .secret_key_env
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        missing.push("backup.offsite.s3.secret_access_key_env (name unset)");
    } else if !data.secret_key_env_set {
        missing.push("secret key env var is not set in the environment");
    }
    if !missing.is_empty() {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Warn,
            detail: Some(format!(
                "offsite bucket {bucket:?} configured, but credentials are not ready: {}",
                missing.join("; ")
            )),
            hint: Some(
                "Set backup.offsite.s3.access_key_id_env / secret_access_key_env to env var \
                 names, and export those variables with the S3 access key / secret.",
            ),
        };
    }
    CheckResult {
        name: NAME,
        status: CheckStatus::Pass,
        detail: Some(format!(
            "offsite backups configured (bucket {bucket:?}, credentials via env-var indirection)"
        )),
        hint: None,
    }
}

/// Resolve [`OffsiteBackupData`] from the loaded config (profile + env layered).
/// Best-effort: any config load error yields "not configured".
fn resolve_offsite_backup_data() -> OffsiteBackupData {
    let Ok(cfg) = autumn_web::config::AutumnConfig::load() else {
        return OffsiteBackupData::default();
    };
    let Some(offsite) = cfg.backup.offsite else {
        return OffsiteBackupData::default();
    };
    // Check credential presence through the SAME dotenv-aware, profile-aware
    // env `resolve_credentials` uses at runtime — real env layered with
    // `.env`/`.env.<profile>` — so a credential supplied via `.env.<profile>`
    // isn't a false "not ready" (issue #1619 P2 #10). Falls back to the plain
    // OS env only if the `.env` overlay can't be built (a malformed `.env`).
    let profile = cfg.profile.clone().unwrap_or_else(|| "dev".to_owned());
    let denv: Box<dyn autumn_web::config::Env> =
        match autumn_web::dotenv::os_env_with_dotenv_for_profile(&profile) {
            Ok(e) => Box::new(e),
            Err(_) => Box::new(autumn_web::config::OsEnv),
        };
    let bucket = offsite.s3.bucket.clone();
    // Evaluate the shared-bucket conflict with EXACTLY the runtime guard so doctor
    // and the CLI never disagree: the app `[storage.s3]` bucket counts only when
    // the resolved backend is actually S3 (P2 #16/#17), and buckets/endpoints are
    // compared through backup.rs `destinations_conflict` + `canonical_authority`
    // (P2 #19) — which collapses `None` and explicit `*.amazonaws.com` spellings.
    let storage_is_s3 = cfg.storage.backend == autumn_web::storage::StorageBackend::S3;
    let shares_app_bucket = offsite_shares_active_app_bucket(
        storage_is_s3,
        bucket.as_deref(),
        offsite.s3.endpoint.as_deref(),
        cfg.storage.s3.bucket.as_deref(),
        cfg.storage.s3.endpoint.as_deref(),
    );
    OffsiteBackupData {
        configured: true,
        bucket,
        access_key_env: offsite.s3.access_key_id_env.clone(),
        secret_key_env: offsite.s3.secret_access_key_env.clone(),
        access_key_env_set: cred_env_present(
            denv.as_ref(),
            offsite.s3.access_key_id_env.as_deref(),
        ),
        secret_key_env_set: cred_env_present(
            denv.as_ref(),
            offsite.s3.secret_access_key_env.as_deref(),
        ),
        shared_bucket_without_optin: shares_app_bucket && !offsite.allow_shared_bucket,
    }
}

/// Whether the offsite destination shares the app's ACTIVE S3 storage bucket,
/// evaluated with EXACTLY the runtime CLI guard
/// (`crate::db::backup::destinations_conflict`, including its `canonical_authority`
/// endpoint canonicalization) so `doctor` can never pass a config the CLI refuses,
/// nor flag one the CLI allows (issue #1619 P2 #19). The app bucket counts only
/// when `storage_is_s3` (P2 #16/#17) — a stale `[storage.s3]` bucket on a
/// local/disabled backend is inert. Pure for credential-safe unit testing.
fn offsite_shares_active_app_bucket(
    storage_is_s3: bool,
    offsite_bucket: Option<&str>,
    offsite_endpoint: Option<&str>,
    app_bucket: Option<&str>,
    app_endpoint: Option<&str>,
) -> bool {
    fn trimmed(b: Option<&str>) -> Option<&str> {
        b.map(str::trim).filter(|s| !s.is_empty())
    }
    if !storage_is_s3 {
        return false;
    }
    let Some(offsite_bucket) = trimmed(offsite_bucket) else {
        return false;
    };
    crate::db::backup::destinations_conflict(
        offsite_bucket,
        offsite_endpoint,
        trimmed(app_bucket),
        app_endpoint,
    )
}

/// Whether the environment variable NAMED by `name` is present and non-empty in
/// `env`. Uses the passed `Env` (the profile-aware dotenv overlay) rather than
/// bare `std::env`, so doctor's offsite-credential readiness matches runtime
/// resolution (which also reads `.env`/`.env.<profile>`), issue #1619 P2 #10.
/// Presence only — never returns or logs the value.
fn cred_env_present(env: &dyn autumn_web::config::Env, name: Option<&str>) -> bool {
    name.map(str::trim)
        .filter(|v| !v.is_empty())
        .is_some_and(|v| env.var(v).is_ok_and(|val| !val.is_empty()))
}

/// Resolve `.env` filesystem state for [`check_dotenv_impl`].
///
/// Returns `(has_env_example, has_env, uncovered_secret_files)`, where the last
/// element lists every always-secret dotenv file present in the project root
/// (`.env`, `.env.local`, and any `.env.<profile>.local`) that the project's
/// `.gitignore` does NOT cover. Committable `.env.<profile>` (non-local) files
/// are intentionally excluded from the secret set — they follow the Vite/Next
/// convention of being safe to commit — so doctor never flags them.
fn resolve_dotenv_state() -> (bool, bool, Vec<String>) {
    let has_env_example = std::path::Path::new(".env.example").exists();
    let has_env = std::path::Path::new(".env").exists();
    let gitignore = std::fs::read_to_string(".gitignore").unwrap_or_default();
    let uncovered = present_secret_dotenv_files(std::path::Path::new("."))
        .into_iter()
        .filter(|name| !gitignore_covers(&gitignore, name))
        .collect();
    (has_env_example, has_env, uncovered)
}

/// Enumerate the always-secret dotenv files present in `dir`: the root `.env`,
/// `.env.local`, and any `.env.<profile>.local`. Returned sorted for stable
/// output. Committable `.env.<profile>` (non-local) files are excluded — see
/// [`is_secret_dotenv_filename`].
fn present_secret_dotenv_files(dir: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            if !entry.path().is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str()
                && is_secret_dotenv_filename(name)
            {
                files.push(name.to_owned());
            }
        }
    }
    files.sort();
    files
}

/// Whether `name` is an always-secret dotenv file the loader can auto-load:
/// `.env`, `.env.local`, or a `.env.<profile>.local` variant.
///
/// A non-local `.env.<profile>` (e.g. `.env.dev`) is committable by convention
/// and returns `false`, as does the `.env.example` template.
fn is_secret_dotenv_filename(name: &str) -> bool {
    // Everything after the mandatory `.env` prefix.
    let Some(rest) = name.strip_prefix(".env") else {
        return false;
    };
    match rest {
        // Bare `.env` and the root `.env.local` are always secret.
        "" | ".local" => true,
        // `.env.<profile>.local` — a per-profile local override. `rest` here is
        // `.<profile>.local`; require a leading `.`, a `local` final segment,
        // and at least two `.` separators so `<profile>` is a real segment
        // (excludes committable non-local `.env.<profile>` files).
        _ => {
            rest.starts_with('.')
                && rest.rsplit('.').next() == Some("local")
                && rest.matches('.').count() >= 2
        }
    }
}

/// Whether a `.gitignore` body ignores a project-root file named `filename`.
///
/// Git-ignore semantics, restricted to the cases needed for root-level dotenv
/// files: comment (`#…`) and blank lines are skipped; every other line is a
/// pattern matched against the file's basename, where `*` matches any run of
/// non-`/` characters (including empty). A leading `!` marks a negation that
/// **re-includes** a previously excluded file. An optional leading `**/` (match
/// in any directory) then an optional leading `/` (anchor to the repository
/// root) are accepted and stripped before matching. A pattern that still
/// contains a `/` after stripping those prefixes — or ends in `/`
/// (directory-only) — is a path/dir pattern that cannot match a bare root-level
/// file and is skipped.
///
/// Per gitignore precedence, the **last** matching pattern decides: patterns
/// are evaluated top-to-bottom while tracking a running ignored state, so
/// `.env*` followed by `!.env` correctly reports `.env` as NOT covered, and
/// `!.env` followed by `.env` reports it as covered.
fn gitignore_covers(contents: &str, filename: &str) -> bool {
    let mut ignored = false;
    for line in contents.lines() {
        let entry = line.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        // A leading `!` re-includes (un-ignores) a matching file.
        let (negation, body) = entry
            .strip_prefix('!')
            .map_or((false, entry), |rest| (true, rest));
        let pattern = body.strip_prefix("**/").unwrap_or(body);
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.ends_with('/') || pattern.contains('/') {
            continue;
        }
        if glob_matches_basename(pattern, filename) {
            // Last match wins: a normal pattern ignores, a `!` negation
            // re-includes.
            ignored = !negation;
        }
    }
    ignored
}

/// Match a basename glob against `text`, anchored at both ends. The only special
/// character is `*`, which matches any run of characters (including empty).
fn glob_matches_basename(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative wildcard match with backtracking on the most recent `*`.
    let (mut pi, mut ti) = (0_usize, 0_usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0_usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Check that project-root `.env` handling is set up safely.
///
/// - Any always-secret dotenv file (`.env`, `.env.local`, `.env.<profile>.local`)
///   present but NOT gitignore-covered → Warn, naming the offending file(s).
///   This security finding takes priority: it is surfaced even when
///   `.env.example` exists without a root `.env` (so the copy-the-template hint
///   can never hide an ungitignored, committable secret file).
/// - `.env.example` present but no `.env` → Warn (developer likely needs to
///   copy the template).
/// - otherwise → Pass.
///
/// Warn-only by design (never Fail), consistent with the skill doc.
pub fn check_dotenv_impl(
    has_env_example: bool,
    has_env: bool,
    uncovered_secret_files: &[String],
) -> CheckResult {
    // Security first: an ungitignored always-secret dotenv file risks committing
    // secrets and must never be hidden behind the copy-the-template hint, so
    // check it before the `.env.example`-without-`.env` case. When both apply,
    // lead with the security issue and still mention copying the template.
    if !uncovered_secret_files.is_empty() {
        let mut detail = format!(
            "{} present but not gitignored — you risk committing secrets",
            uncovered_secret_files.join(", ")
        );
        if has_env_example && !has_env {
            detail.push_str("; also copy `.env.example` to `.env` to get started");
        }
        return CheckResult {
            name: "dotenv",
            status: CheckStatus::Warn,
            detail: Some(detail),
            hint: Some(
                "Add your local .env files to .gitignore (e.g. `.env`, `.env.local`, `.env.*.local`)",
            ),
        };
    }
    if has_env_example && !has_env {
        return CheckResult {
            name: "dotenv",
            status: CheckStatus::Warn,
            detail: Some("`.env.example` is present but no `.env` exists".into()),
            hint: Some("Copy `.env.example` to `.env` and fill in local values"),
        };
    }
    CheckResult {
        name: "dotenv",
        status: CheckStatus::Pass,
        detail: Some(if has_env {
            "`.env` handling looks correct".into()
        } else {
            ".env not configured".into()
        }),
        hint: None,
    }
}

pub fn check_maintenance_mode() -> CheckResult {
    use autumn_web::maintenance::{MAINTENANCE_FLAG_FILE, MaintenanceState};
    let path = std::path::Path::new(MAINTENANCE_FLAG_FILE);
    match MaintenanceState::load_from_file(path) {
        Ok(Some(config)) => {
            let detail = config.message.as_ref().map_or_else(
                || "maintenance mode is ON".to_owned(),
                |msg| format!("maintenance mode is ON — \"{msg}\""),
            );
            CheckResult {
                name: "maintenance_mode",
                status: CheckStatus::Warn,
                detail: Some(detail),
                hint: Some("Run `autumn maintenance off` to re-enable normal traffic"),
            }
        }
        Ok(None) | Err(_) => CheckResult {
            name: "maintenance_mode",
            status: CheckStatus::Pass,
            detail: Some("maintenance mode is off".into()),
            hint: None,
        },
    }
}

// ── System-test browser check ─────────────────────────────────────────────

/// Candidate paths probed for a Chromium binary, in resolution order.
///
/// Delegates to `autumn_web::browser_detect` so `autumn doctor` and the
/// `SystemTest` harness can never disagree about whether this host can run
/// system tests — they used to keep separate copies of this list and did
/// (#1456).
pub fn browser_candidate_paths() -> Vec<std::path::PathBuf> {
    autumn_web::browser_detect::browser_candidates()
}

/// Probe `path` for a browser version, using the same rules as the harness.
///
/// Returns `None` when the candidate is not usable, and
/// `autumn_web::browser_detect::UNKNOWN_VERSION` when it is usable but cannot
/// report a version (Windows `chrome.exe` is a GUI-subsystem binary that
/// prints nothing to the parent console).
fn probe_browser_version(path: &std::path::Path) -> Option<String> {
    autumn_web::browser_detect::probe_version(path)
}

/// Check whether a Chromium binary is available for system tests.
///
/// Returns `true` if the `[features]` table in `cargo_toml` declares `key`.
///
/// Scans only the `[features]` section (not dev-dependencies that may also
/// reference the feature) so a line like `autumn-web = { features = ["system-tests"] }`
/// does not produce a false positive.
fn cargo_toml_features_has_key(cargo_toml: &str, key: &str) -> bool {
    let quoted_key = format!("\"{key}\"");
    let mut in_features = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "[features]"
            || (trimmed.starts_with("[features]")
                && trimmed["[features]".len()..].trim_start().starts_with('#'))
        {
            in_features = true;
            continue;
        }
        if in_features {
            if trimmed.starts_with('[') {
                break;
            }
            let bare = trimmed
                .strip_prefix(key)
                .is_some_and(|r| r.trim_start().starts_with('='));
            let quoted = trimmed
                .strip_prefix(quoted_key.as_str())
                .is_some_and(|r| r.trim_start().starts_with('='));
            if bare || quoted {
                return true;
            }
        }
    }
    false
}

/// Reports `Warn` (not `Fail`) so that projects that don't use system tests
/// are not penalized. If you _do_ run system tests you'll get a clear
/// actionable message in `autumn doctor` output.
pub fn check_system_test_browser() -> CheckResult {
    let candidates = browser_candidate_paths();
    for path in &candidates {
        // No `is_file()` pre-check: `probe_browser_version` owns that
        // decision, and on Windows "the file exists" *is* the answer — a
        // GUI-subsystem `chrome.exe` can never report a version, so gating on
        // one here is what produced the false "no browser installed" (#1456).
        if let Some(version) = probe_browser_version(path) {
            return CheckResult {
                name: "system_test_browser",
                status: CheckStatus::Pass,
                detail: Some(format!(
                    "Chromium for system tests: {version} ({})",
                    path.display()
                )),
                hint: None,
            };
        }
    }

    // Only warn when the project has opted into system tests; otherwise a
    // missing browser is irrelevant and must not fail `autumn doctor --strict`.
    let project_uses_system_tests = std::env::current_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("Cargo.toml")).ok())
        .is_some_and(|s| cargo_toml_features_has_key(&s, "system-tests"));

    if project_uses_system_tests {
        CheckResult {
            name: "system_test_browser",
            status: CheckStatus::Warn,
            detail: Some("no Chromium binary found — system tests will be skipped".into()),
            hint: Some(
                "Install: apt-get install chromium-browser  \
                 or set AUTUMN_CHROMIUM=/path/to/chrome",
            ),
        }
    } else {
        CheckResult {
            name: "system_test_browser",
            status: CheckStatus::Pass,
            detail: Some("no Chromium binary found (project does not use system-tests)".into()),
            hint: None,
        }
    }
}

/// Resolve the list of `#[repository]`-annotated table names that are not yet
/// registered in a `GdprRegistry` in the project source.
///
/// Currently uses a lightweight heuristic: scans `src/schema.rs` for `diesel::table!`
/// declarations and returns those that have no matching `GdprRegistry::register` call
/// in any `*.rs` source file. Returns an empty list when the project has no schema or
/// when all tables appear to be registered.
fn resolve_gdpr_unregistered_tables() -> Vec<String> {
    let schema_path = std::path::Path::new("src/schema.rs");
    let Ok(schema) = std::fs::read_to_string(schema_path) else {
        return Vec::new();
    };

    // Collect table names declared as `diesel::table! { <name> (id) { ... } }`.
    let mut declared_tables: Vec<String> = Vec::new();
    for line in schema.lines() {
        let trimmed = line.trim();
        // Match lines like `diesel::table! {` followed by the table name on the next line,
        // or inline `pub table_name (id) {` patterns.  Use a simple prefix scan.
        if let Some(rest) = trimmed
            .strip_prefix("diesel::table!")
            .map(str::trim)
            .filter(|r| r.starts_with('{'))
        {
            // table name is on the same line or will appear shortly; skip for now
            let _ = rest;
        }
        // Diesel schema emits bare identifiers like `    table_name (pk) {`
        // where `pk` can be any column name (id, uuid, code, or composite).
        if let Some(open_paren) = trimmed.find('(')
            && (trimmed.ends_with('{') || trimmed.ends_with("{ "))
        {
            let name = trimmed[..open_paren].trim();
            if !name.is_empty() && !name.starts_with("//") && !name.starts_with("diesel") {
                declared_tables.push(name.to_owned());
            }
        }
    }

    if declared_tables.is_empty() {
        return Vec::new();
    }

    // Check which tables appear to be registered via GdprRegistry in the project.
    let registered_tables = collect_gdpr_registered_tables_from_source();

    declared_tables
        .into_iter()
        .filter(|t| !registered_tables.contains(t))
        .collect()
}

/// Scan all `*.rs` files under `src/` for `GdprRegistry` register calls and
/// return the set of table names found. Handles both single-line and
/// rustfmt-formatted multi-line `ModelRegistration::` calls.
fn collect_gdpr_registered_tables_from_source() -> std::collections::HashSet<String> {
    let mut found = std::collections::HashSet::new();
    for path in glob_rs_files(std::path::Path::new("src")) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        scan_source_for_gdpr_registrations(&content, &mut found);
    }
    found
}

/// Extract `ModelRegistration::` table names from source text, handling both
/// single-line calls and multi-line rustfmt-formatted calls.
fn scan_source_for_gdpr_registrations(
    content: &str,
    found: &mut std::collections::HashSet<String>,
) {
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(idx) = line.find("ModelRegistration::") {
            let rest = &line[idx..];
            // Try the same line first, then the next 3 lines for multi-line calls.
            if extract_quoted_table_name(rest, found) {
                continue;
            }
            for j in 1..=3 {
                if let Some(next) = lines.get(i + j)
                    && extract_quoted_table_name(next.trim(), found)
                {
                    break;
                }
            }
        }
    }
}

/// Extract the first double-quoted string from `s` as a table name.
/// Returns `true` if a non-empty, non-whitespace name was found and inserted.
fn extract_quoted_table_name(s: &str, found: &mut std::collections::HashSet<String>) -> bool {
    if let Some(start) = s.find('"')
        && let Some(end) = s[start + 1..].find('"')
    {
        let name = &s[start + 1..start + 1 + end];
        if !name.is_empty() && !name.contains(' ') {
            found.insert(name.to_owned());
            return true;
        }
    }
    false
}

/// Recursively collect all `*.rs` file paths under `dir`.
fn glob_rs_files(dir: impl AsRef<std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.append(&mut glob_rs_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Warn when the auth starter is present but one or more `#[repository]`-annotated
/// tables have not been registered in the GDPR export/erasure registry.
///
/// This check implements GDPR issue #820 AC #8:
/// > `autumn doctor` warns when repositories exist whose models are not registered
/// > for export/erasure once the auth starter is present.
///
/// `has_auth_starter` should be `true` when the project's `src/routes/auth.rs` exists
/// (indicating `autumn generate auth` was run).
/// `unregistered_tables` is the list of `#[repository]`-annotated table names that
/// have not been registered via `GdprRegistry`.
pub fn check_gdpr_export_registration_impl(
    has_auth_starter: bool,
    unregistered_tables: &[&str],
) -> CheckResult {
    if !has_auth_starter {
        return CheckResult {
            name: "gdpr_export_registration",
            status: CheckStatus::Pass,
            detail: Some("auth starter not present; GDPR registration not required".into()),
            hint: None,
        };
    }

    if unregistered_tables.is_empty() {
        return CheckResult {
            name: "gdpr_export_registration",
            status: CheckStatus::Pass,
            detail: Some("all repositories are registered for GDPR export/erasure".into()),
            hint: None,
        };
    }

    let tables = unregistered_tables.join(", ");
    CheckResult {
        name: "gdpr_export_registration",
        status: CheckStatus::Warn,
        detail: Some(format!(
            "{} repository model(s) not registered for GDPR export/erasure: {tables}",
            unregistered_tables.len()
        )),
        hint: Some(
            "Register each model via GdprRegistry in your application state. \
             See docs/guide/gdpr-compliance.md for the export/erasure API.",
        ),
    }
}

// ─── Model column privacy (#1374) ───────────────────────────────────────────

/// A `#[model]` column whose name looks sensitive but is not marked
/// `#[private]`. Input to [`check_model_private_columns_impl`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprivateSensitiveColumn {
    pub model: String,
    pub column: String,
}

/// The sensitive-name heuristic shared with the `#[model]` macro: matches
/// `password`, `token`, `secret` anywhere, or a `hash` / `_hash` suffix.
pub fn column_name_is_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.ends_with("_hash")
        || lower == "hash"
}

/// Warn when a `#[model]` column matches the sensitive-name heuristic but is
/// not marked `#[private]` (issue #1374 AC).
///
/// Pure and injectable for tests; a warning (never a hard failure) so it can't
/// break an existing build.
pub fn check_model_private_columns_impl(found: &[UnprivateSensitiveColumn]) -> CheckResult {
    if found.is_empty() {
        return CheckResult {
            name: "model_private_columns",
            status: CheckStatus::Pass,
            detail: Some("no sensitively-named model columns are missing `#[private]`".into()),
            hint: None,
        };
    }
    let lines: Vec<String> = found
        .iter()
        .map(|c| {
            format!(
                "{}.{} looks sensitive but is not marked `#[private]` (it will serialize into JSON responses)",
                c.model, c.column
            )
        })
        .collect();
    CheckResult {
        name: "model_private_columns",
        status: CheckStatus::Warn,
        detail: Some(lines.join("\n")),
        hint: Some(
            "Mark sensitive columns `#[private]` so they never leak into `Json`/`--api` \
             responses; the field stays writable and queryable.",
        ),
    }
}

/// Scan the project's `src/` for `#[model]` structs and return the sensitively
/// named columns that are not marked `#[private]`. Returns an empty list when
/// there is no `src/` directory or nothing is flagged.
fn resolve_unprivate_sensitive_columns() -> Vec<UnprivateSensitiveColumn> {
    let mut found = Vec::new();
    for path in glob_rs_files(std::path::Path::new("src")) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            scan_source_for_unprivate_sensitive_columns(&content, &mut found);
        }
    }
    found
}

/// Line-based scanner: for each `#[model]`-annotated struct, flag `pub <name>:`
/// fields whose name is sensitive and that are not immediately preceded by a
/// `#[private]` (or `#[encrypted]`, which is private-in-JSON by default)
/// attribute. Deliberately lightweight (no full parse) — consistent with the
/// other `autumn doctor` source heuristics.
/// Whether an attribute line applies the `#[model]` macro, including the
/// path-qualified forms `#[autumn_web::model(...)]` and `#[macros::model]`.
/// Matches when the attribute path's last `::`-segment is `model`.
fn attr_line_is_model(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("#[") else {
        return false;
    };
    // The attribute path runs up to the first argument list, close bracket,
    // whitespace, or comma (e.g. `autumn_web::model` in `#[autumn_web::model(...)]`).
    let path = rest
        .split(|c: char| c == '(' || c == ']' || c == ',' || c.is_whitespace())
        .next()
        .unwrap_or("");
    !path.is_empty() && path.rsplit("::").next() == Some("model")
}

fn scan_source_for_unprivate_sensitive_columns(
    content: &str,
    found: &mut Vec<UnprivateSensitiveColumn>,
) {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if !attr_line_is_model(lines[i]) {
            i += 1;
            continue;
        }
        // Find the struct name (the `struct X` line) and the opening brace.
        let mut j = i + 1;
        let mut model_name: Option<String> = None;
        while j < lines.len() {
            let t = lines[j].trim_start();
            if let Some(rest) = t
                .strip_prefix("pub struct ")
                .or_else(|| t.strip_prefix("struct "))
            {
                model_name = rest
                    .split(|c: char| c == '{' || c == '(' || c.is_whitespace())
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                break;
            }
            j += 1;
        }
        let Some(model_name) = model_name else {
            i += 1;
            continue;
        };
        // Walk the struct body until the matching closing brace at column 0-ish.
        let mut k = j + 1;
        let mut field_is_private = false;
        while k < lines.len() {
            let t = lines[k].trim();
            if t.starts_with('}') {
                // Closing brace terminates the struct even when followed by a
                // trailing comment (`} // User`) or a semicolon (`};`).
                break;
            }
            if t.starts_with("#[private") || t.starts_with("#[encrypted") {
                field_is_private = true;
            } else if let Some(field) = parse_pub_field_name(t) {
                if column_name_is_sensitive(&field) && !field_is_private {
                    found.push(UnprivateSensitiveColumn {
                        model: model_name.clone(),
                        column: field,
                    });
                }
                // Reset the attribute flag after consuming a field line.
                field_is_private = false;
            }
            k += 1;
        }
        i = k + 1;
    }
}

/// Extract the field name from a `pub <name>: <ty>,` struct-field line, or
/// `None` if the line is not a field declaration.
fn parse_pub_field_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub ")?;
    let colon = rest.find(':')?;
    let name = rest[..colon].trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_owned())
}

// ─── Edge capsule preflight (#1790) ──────────────────────────────────────────

/// The `src/bin/edge-capsule.rs` an app with `#[edge]` routes needs.
const EDGE_CAPSULE_BIN: &str = "src/bin/edge-capsule.rs";

/// Whether the project can compile its `#[edge]` routes at all: the
/// `wasm32-wasip1` standard library has to be installed for the active
/// toolchain, or `autumn build` cannot emit the edge capsule.
///
/// Pure and injectable: `scan` is the pre-resolved source scan and
/// `target_installed` the pre-resolved toolchain probe (both resolved inside the
/// task closure in [`run`]).
pub fn check_edge_target_impl(
    scan: &crate::edge_scan::EdgeScan,
    target_installed: bool,
) -> CheckResult {
    const NAME: &str = "edge_target";
    if scan.is_empty() {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some("no #[edge] routes".into()),
            hint: None,
        };
    }
    if target_installed {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some(format!(
                "{} #[edge] route(s); the {} target is installed",
                scan.functions.len(),
                crate::build::EDGE_TARGET,
            )),
            hint: None,
        };
    }
    let mut files: Vec<&str> = scan.functions.iter().map(|f| f.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    CheckResult {
        name: NAME,
        status: CheckStatus::Fail,
        detail: Some(format!(
            "{} #[edge] route(s) in {} need the {} target, which is not installed",
            scan.functions.len(),
            files.join(", "),
            crate::build::EDGE_TARGET,
        )),
        hint: Some(crate::build::EDGE_TARGET_HINT),
    }
}

/// Whether the project's `#[edge]` routes are wired the way the capsule needs.
///
/// Reported in precedence order, worst first, so the single line always names
/// the most urgent problem:
/// 1. an `#[edge]` handler that also carries an auth/rate-limit guard — the
///    `#[edge]` macro rejects that pair, so this is a build failure caught
///    before the build;
/// 2. a marked handler no `edge_routes![]` registers — it compiles, but the
///    capsule never serves it;
/// 3. edge routes with no `src/bin/edge-capsule.rs` — nothing to compile into.
///
/// Pure and injectable: `capsule_bin_exists` is resolved by the caller.
pub fn check_edge_routes_impl(
    scan: &crate::edge_scan::EdgeScan,
    capsule_bin_exists: bool,
) -> CheckResult {
    const NAME: &str = "edge_routes";
    if scan.is_empty() {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Pass,
            detail: Some("no #[edge] routes".into()),
            hint: None,
        };
    }

    let guarded = scan.guarded();
    if !guarded.is_empty() {
        let lines: Vec<String> = guarded
            .iter()
            .map(|f| format!("{} also carries #[{}]", f.location(), f.guards.join("]/#[")))
            .collect();
        return CheckResult {
            name: NAME,
            status: CheckStatus::Fail,
            detail: Some(lines.join("\n")),
            hint: Some(
                "Remove #[edge] or the conflicting attribute: edge routes are unauthenticated \
                 read-path routes served without origin middleware — the capsule has no session, \
                 auth, or rate-limit state, and #[intercept] layers do not run there.",
            ),
        };
    }

    let unregistered = scan.unregistered();
    if !unregistered.is_empty() {
        let lines: Vec<String> = unregistered
            .iter()
            .map(|f| format!("{} is not registered", f.location()))
            .collect();
        return CheckResult {
            name: NAME,
            status: CheckStatus::Warn,
            detail: Some(lines.join("\n")),
            hint: Some(
                "Add the handler to edge_routes![] and pass it to the edge-capsule bin; \
                 an unregistered #[edge] route is only ever served by the origin.",
            ),
        };
    }

    if !capsule_bin_exists {
        return CheckResult {
            name: NAME,
            status: CheckStatus::Warn,
            detail: Some(format!(
                "{} #[edge] route(s) registered but {EDGE_CAPSULE_BIN} is missing",
                scan.functions.len()
            )),
            hint: Some(
                "Create src/bin/edge-capsule.rs calling autumn_edge::serve(...) with your \
                 edge_routes![] list so `autumn build` can compile the capsule.",
            ),
        };
    }

    CheckResult {
        name: NAME,
        status: CheckStatus::Pass,
        detail: Some(format!(
            "{} #[edge] route(s) registered with edge_routes![]",
            scan.registered_fns().len()
        )),
        hint: None,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Edge capsule preflight (#1790) ───────────────────────────────────────

    fn edge_scan_of(src: &str) -> crate::edge_scan::EdgeScan {
        crate::edge_scan::scan_sources(&[("src/routes.rs", src)])
    }

    /// One marked, registered, guard-free handler — the healthy shape.
    fn healthy_edge_scan() -> crate::edge_scan::EdgeScan {
        edge_scan_of("#[edge]\nfn greet() {}\nfn wire() { edge_routes![greet]; }")
    }

    /// #1621 review finding 12. Both hints are `hint` fields on `CheckResult`, so
    /// they are serialized verbatim into `autumn doctor --json`; a dropped `\`
    /// line continuation bakes the source indentation into the message as a long
    /// run of spaces mid-sentence. Pin the exact text.
    #[test]
    fn deploy_host_spelling_hints_carry_no_line_continuation_gutter() {
        assert_eq!(
            DEPLOY_HOST_SPELLING_HINT,
            "Fix the [deploy] host spelling in autumn.toml \u{2014} set either `host` (one \
             server) or `hosts` (a fleet), never both (see `autumn deploy check`)",
        );
        assert_eq!(
            DEPLOY_HOST_SPELLING_REACHABILITY_HINT,
            "Fix the [deploy] host spelling in autumn.toml before probing reachability \
             (see `autumn deploy check`)",
        );
        for hint in [
            DEPLOY_HOST_SPELLING_HINT,
            DEPLOY_HOST_SPELLING_REACHABILITY_HINT,
        ] {
            assert!(
                !hint.contains("   "),
                "a run of spaces mid-sentence means a `\\` continuation was dropped: {hint}",
            );
        }
    }

    #[test]
    fn edge_target_passes_without_edge_routes() {
        let scan = edge_scan_of("#[get(\"/\")]\nfn home() {}");
        // Passes whether or not the wasm target happens to be installed.
        for installed in [true, false] {
            let r = check_edge_target_impl(&scan, installed);
            assert_eq!(r.status, CheckStatus::Pass);
            assert_eq!(r.detail.as_deref(), Some("no #[edge] routes"));
            assert!(r.hint.is_none());
        }
    }

    #[test]
    fn edge_target_passes_when_installed() {
        let r = check_edge_target_impl(&healthy_edge_scan(), true);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.unwrap().contains("wasm32-wasip1"));
    }

    #[test]
    fn edge_target_fails_with_rustup_hint_when_missing() {
        let r = check_edge_target_impl(&healthy_edge_scan(), false);
        assert_eq!(r.status, CheckStatus::Fail);
        let detail = r.detail.unwrap();
        assert!(detail.contains("1 #[edge] route(s)"), "{detail}");
        assert!(detail.contains("src/routes.rs"), "{detail}");
        assert_eq!(r.hint, Some("Run `rustup target add wasm32-wasip1`"));
    }

    #[test]
    fn edge_routes_passes_without_edge_routes() {
        let r = check_edge_routes_impl(&edge_scan_of("fn home() {}"), false);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.detail.as_deref(), Some("no #[edge] routes"));
    }

    #[test]
    fn edge_routes_passes_when_registered_with_a_capsule_bin() {
        let r = check_edge_routes_impl(&healthy_edge_scan(), true);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.unwrap().contains("1 #[edge] route(s) registered"));
    }

    #[test]
    fn edge_routes_warns_on_an_unregistered_handler() {
        let scan = edge_scan_of("#[edge]\nfn greet() {}");
        let r = check_edge_routes_impl(&scan, true);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.detail.unwrap().contains("greet @ src/routes.rs:2"),
            "the warning must name the handler and its location"
        );
        assert!(r.hint.unwrap().contains("edge_routes![]"));
    }

    #[test]
    fn edge_routes_fails_on_an_auth_guarded_handler() {
        // The `#[edge]` macro rejects this pair; doctor catches it pre-build.
        let scan =
            edge_scan_of("#[edge]\n#[secured]\nfn dash() {}\nfn wire() { edge_routes![dash]; }");
        let r = check_edge_routes_impl(&scan, true);
        assert_eq!(r.status, CheckStatus::Fail);
        let detail = r.detail.unwrap();
        assert!(detail.contains("dash @ src/routes.rs:3"), "{detail}");
        assert!(detail.contains("#[secured]"), "{detail}");
        assert!(r.hint.unwrap().contains("unauthenticated read-path"));
    }

    #[test]
    fn edge_routes_guard_failure_outranks_registration_warning() {
        // Both problems present: the build-breaking one must be reported.
        let scan = edge_scan_of("#[edge]\n#[authorize(\"admin\")]\nfn dash() {}");
        assert_eq!(
            check_edge_routes_impl(&scan, false).status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn edge_routes_warns_when_the_capsule_bin_is_missing() {
        let r = check_edge_routes_impl(&healthy_edge_scan(), false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.detail.unwrap().contains("src/bin/edge-capsule.rs"),
            "the warning must name the file to create"
        );
        assert!(r.hint.unwrap().contains("autumn_edge::serve"));
    }

    #[test]
    fn offsite_backup_pass_when_unconfigured() {
        let r = check_offsite_backup_impl(&OffsiteBackupData::default());
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.unwrap().contains("not disaster recovery"));
    }

    #[test]
    fn offsite_backup_fail_when_bucket_unset() {
        let data = OffsiteBackupData {
            configured: true,
            ..Default::default()
        };
        let r = check_offsite_backup_impl(&data);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.unwrap().contains("bucket is unset"));
    }

    #[test]
    fn offsite_backup_fail_on_shared_bucket_without_optin() {
        let data = OffsiteBackupData {
            configured: true,
            bucket: Some("shared".to_owned()),
            shared_bucket_without_optin: true,
            ..Default::default()
        };
        let r = check_offsite_backup_impl(&data);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.unwrap().contains("same as the app"));
    }

    #[test]
    fn doctor_shared_bucket_matches_runtime_guard() {
        // P2 #16/#17 backend gate: a stale [storage.s3] bucket equal to the offsite
        // bucket must NOT flag shared unless the backend is S3 — otherwise
        // `autumn doctor --strict` false-fails on a local/disabled app.
        assert!(!offsite_shares_active_app_bucket(
            false,
            Some("shared"),
            None,
            Some("shared"),
            None
        ));
        // Feeding that into the check: not shared → no --strict failure.
        let local_data = OffsiteBackupData {
            configured: true,
            bucket: Some("shared".to_owned()),
            access_key_env: Some("K".to_owned()),
            secret_key_env: Some("S".to_owned()),
            access_key_env_set: true,
            secret_key_env_set: true,
            shared_bucket_without_optin: false, // gated off by backend != S3
        };
        assert_ne!(
            check_offsite_backup_impl(&local_data).status,
            CheckStatus::Fail
        );

        // P2 #19: doctor must match the runtime `canonical_authority` guard — the
        // app storage endpoint left None (AWS default) vs the offsite naming the
        // SAME AWS bucket with an explicit AWS regional endpoint canonicalize equal,
        // so doctor DETECTS the shared bucket (the CLI would `SharedBucketRefused`).
        assert!(offsite_shares_active_app_bucket(
            true,
            Some("shared"),
            Some("https://s3.us-east-1.amazonaws.com"),
            Some("shared"),
            None,
        ));
        // Both endpoints None (AWS default on both sides), same bucket → shared.
        assert!(offsite_shares_active_app_bucket(
            true,
            Some("shared"),
            None,
            Some("shared"),
            None
        ));
        // Different bucket → not shared even on S3.
        assert!(!offsite_shares_active_app_bucket(
            true,
            Some("offsite"),
            None,
            Some("appbucket"),
            None
        ));
        // Genuinely different endpoints (AWS vs self-hosted MinIO) → not shared.
        assert!(!offsite_shares_active_app_bucket(
            true,
            Some("shared"),
            Some("https://minio.example:9000"),
            Some("shared"),
            None,
        ));
    }

    #[test]
    fn offsite_backup_warn_when_credentials_not_ready() {
        // Bucket set, env var names configured, but the vars aren't exported.
        let data = OffsiteBackupData {
            configured: true,
            bucket: Some("offsite".to_owned()),
            access_key_env: Some("OFF_KEY".to_owned()),
            secret_key_env: Some("OFF_SECRET".to_owned()),
            access_key_env_set: false,
            secret_key_env_set: false,
            shared_bucket_without_optin: false,
        };
        let r = check_offsite_backup_impl(&data);
        assert_eq!(r.status, CheckStatus::Warn);
        // Names the situation, never a credential value.
        let detail = r.detail.unwrap();
        assert!(detail.contains("credentials are not ready"));
        assert!(!detail.contains("supersecret"));
    }

    #[test]
    fn offsite_backup_pass_when_ready() {
        let data = OffsiteBackupData {
            configured: true,
            bucket: Some("offsite".to_owned()),
            access_key_env: Some("OFF_KEY".to_owned()),
            secret_key_env: Some("OFF_SECRET".to_owned()),
            access_key_env_set: true,
            secret_key_env_set: true,
            shared_bucket_without_optin: false,
        };
        let r = check_offsite_backup_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.unwrap().contains("offsite backups configured"));
    }

    #[test]
    fn cred_env_present_honors_dotenv_supplied_creds() {
        // P2 #10: readiness is checked through the profile-aware env overlay
        // (real env + `.env.<profile>`), so a credential supplied ONLY via
        // `.env.prod` (here simulated by the overlay surfacing the value) counts
        // as ready — a bare `std::env::var` check would report a false negative.
        let overlay = autumn_web::config::MockEnv::new().with("OFF_SECRET", "from-dot-env-prod");
        assert!(cred_env_present(&overlay, Some("OFF_SECRET")));
        // Absent / unnamed / blank-name / empty-value => not present.
        assert!(!cred_env_present(&overlay, Some("MISSING")));
        assert!(!cred_env_present(&overlay, None));
        assert!(!cred_env_present(&overlay, Some("   ")));
        let empty = autumn_web::config::MockEnv::new().with("OFF_SECRET", "");
        assert!(!cred_env_present(&empty, Some("OFF_SECRET")));
    }

    #[test]
    fn known_toml_sections_includes_backup() {
        assert!(KNOWN_TOML_SECTIONS.contains(&"backup"));
    }

    #[test]
    fn model_private_columns_pass_when_none_flagged() {
        let r = check_model_private_columns_impl(&[]);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn pg_client_tools_pass_when_all_present() {
        let r = check_pg_client_tools_impl(&[]);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.hint.is_none());
    }

    #[test]
    fn pg_client_tools_warn_lists_missing() {
        let r = check_pg_client_tools_impl(&["pg_dump", "pg_restore"]);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.unwrap();
        assert!(detail.contains("pg_dump"));
        assert!(detail.contains("pg_restore"));
        assert!(r.hint.unwrap().contains("AUTUMN_PG_BIN_DIR"));
    }

    /// Regression: doctor must honour the same tool-discovery precedence as
    /// `db backup`/`restore`. When the tools are reachable via a candidate `bin`
    /// directory (e.g. `AUTUMN_PG_BIN_DIR` or the managed-pg bundle) rather than
    /// PATH, `check_pg_client_tools` must not raise a false warning. Exercised
    /// through `db backup`'s own locator so doctor and backup can't drift.
    #[test]
    fn pg_client_tools_pass_when_found_via_bin_dir() {
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path();
        for tool in ["pg_dump", "pg_restore"] {
            let name = if cfg!(windows) {
                format!("{tool}.exe")
            } else {
                tool.to_owned()
            };
            std::fs::write(bin.join(name), b"").expect("write fake tool");
        }

        let tools = crate::db::backup::PgTools::with_dirs(vec![bin.to_path_buf()]);
        let r = check_pg_client_tools_with(&tools);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.hint.is_none());
    }

    #[test]
    fn model_private_columns_warn_lists_offenders() {
        let found = vec![
            UnprivateSensitiveColumn {
                model: "User".into(),
                column: "password_hash".into(),
            },
            UnprivateSensitiveColumn {
                model: "ApiKey".into(),
                column: "secret".into(),
            },
        ];
        let r = check_model_private_columns_impl(&found);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.unwrap();
        assert!(detail.contains("User.password_hash"));
        assert!(detail.contains("ApiKey.secret"));
    }

    #[test]
    fn sensitive_name_heuristic_matches_expected() {
        assert!(column_name_is_sensitive("password_hash"));
        assert!(column_name_is_sensitive("reset_token"));
        assert!(column_name_is_sensitive("api_secret"));
        assert!(column_name_is_sensitive("password"));
        assert!(!column_name_is_sensitive("email"));
        assert!(!column_name_is_sensitive("display_name"));
    }

    #[test]
    fn scan_flags_unprivate_sensitive_columns_only() {
        let src = r"
#[model]
pub struct User {
    #[id]
    pub id: i64,
    pub email: String,
    pub password_hash: String,
    #[private]
    pub reset_token: String,
}
";
        let mut found = Vec::new();
        scan_source_for_unprivate_sensitive_columns(src, &mut found);
        assert_eq!(
            found.len(),
            1,
            "only password_hash should be flagged: {found:?}"
        );
        assert_eq!(found[0].model, "User");
        assert_eq!(found[0].column, "password_hash");
    }

    #[test]
    fn scan_terminates_struct_on_brace_with_trailing_tokens() {
        // The closing brace may be followed by a trailing comment (`} // User`)
        // or a semicolon (`};`); the scan must still stop at the struct end and
        // not swallow following declarations.
        for closing in ["} // User", "};", "}  // trailing"] {
            let src = format!(
                "#[model]\npub struct User {{\n    #[id]\n    pub id: i64,\n    pub password_hash: String,\n{closing}\npub struct Other {{\n    pub api_secret: String,\n}}\n"
            );
            let mut found = Vec::new();
            scan_source_for_unprivate_sensitive_columns(&src, &mut found);
            assert_eq!(
                found.len(),
                1,
                "closing line `{closing}` must terminate the struct: {found:?}"
            );
            assert_eq!(found[0].model, "User");
            assert_eq!(found[0].column, "password_hash");
        }
    }

    #[test]
    fn scan_treats_encrypted_as_private_in_json() {
        let src = r"
#[model]
pub struct Vault {
    #[id]
    pub id: i64,
    #[encrypted]
    pub api_secret: String,
}
";
        let mut found = Vec::new();
        scan_source_for_unprivate_sensitive_columns(src, &mut found);
        assert!(
            found.is_empty(),
            "encrypted columns are private-in-JSON: {found:?}"
        );
    }

    #[test]
    fn scan_matches_path_qualified_model_attributes() {
        // Path-qualified forms (`#[autumn_web::model]`, `#[macros::model]`) must
        // be scanned like the bare `#[model]` — otherwise the privacy heuristic
        // silently does nothing for files that use them.
        for attr in [
            "#[autumn_web::model]",
            "#[macros::model]",
            "#[model(table = \"users\")]",
        ] {
            let src = format!(
                "{attr}\npub struct User {{\n    #[id]\n    pub id: i64,\n    pub password_hash: String,\n}}\n"
            );
            let mut found = Vec::new();
            scan_source_for_unprivate_sensitive_columns(&src, &mut found);
            assert_eq!(
                found.len(),
                1,
                "path-qualified `{attr}` must be scanned and flag password_hash: {found:?}"
            );
            assert_eq!(found[0].column, "password_hash");
        }

        // A helper self-check: only attributes whose last segment is `model`.
        assert!(attr_line_is_model("#[autumn_web::model(table = \"x\")]"));
        assert!(attr_line_is_model("  #[macros::model]"));
        assert!(attr_line_is_model("#[model]"));
        assert!(!attr_line_is_model("#[models]"));
        assert!(!attr_line_is_model("#[model_config]"));
        assert!(!attr_line_is_model("pub struct User {"));
    }

    #[test]
    fn test_recursive_toml_validation() {
        let valid_toml = r#"
            [server]
            port = 4000
            host = "0.0.0.0"

            [database]
            primary_url = "postgres://localhost/db"
        "#;
        let r = check_toml_content(valid_toml);
        assert_eq!(r.status, CheckStatus::Pass);

        let typo_toml = r#"
            [database]
            primry_url = "postgres://localhost/db"
        "#;
        let r = check_toml_content(typo_toml);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.as_ref().unwrap();
        assert!(detail.contains("database.primry_url"));
        assert!(detail.contains("did you mean \"database.primary_url\"?"));

        let unknown_toml = r"
            [server]
            xyz_completely_unknown_key = 123
        ";
        let r = check_toml_content(unknown_toml);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.as_ref().unwrap();
        assert!(detail.contains("server.xyz_completely_unknown_key"));
        assert!(!detail.contains("did you mean"));
    }

    // ── mail_unsubscribe check ─────────────────────────────────────────────────

    #[test]
    fn mail_unsubscribe_no_usage_passes() {
        let r = check_mail_unsubscribe_config_impl(false, None, None, false, true, 30);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn mail_unsubscribe_usage_without_config_fails_in_prod() {
        let r = check_mail_unsubscribe_config_impl(true, None, None, false, true, 30);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.unwrap().contains("list_unsubscribe"));
    }

    #[test]
    fn mail_unsubscribe_usage_without_config_warns_outside_prod() {
        let r = check_mail_unsubscribe_config_impl(true, None, Some("   "), false, false, 30);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn mail_unsubscribe_disabled_transport_passes() {
        // Disabled transport emits no list mail, so config isn't required.
        let r = check_mail_unsubscribe_config_impl(true, None, None, true, true, 30);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn mail_unsubscribe_base_url_warns_in_prod_but_passes_outside() {
        // In production a base_url additionally requires a suppression backend the
        // runtime fails closed without; doctor can't verify it, so it warns.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com"),
            None,
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.unwrap().contains("suppression backend"));
        // Outside production the runtime does not gate on the backend, so it passes.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com"),
            None,
            false,
            false,
            30,
        );
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn mail_unsubscribe_usage_with_mailto_passes() {
        let r = check_mail_unsubscribe_config_impl(
            true,
            None,
            Some("unsub@example.com"),
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn detects_mailer_attribute_but_not_builder_or_comment() {
        assert!(file_declares_list_unsubscribe(
            "#[mailer(list_unsubscribe = \"weekly_digest\")]"
        ));
        // whitespace / newlines inside the attribute still match
        assert!(file_declares_list_unsubscribe(
            "#[mailer(\n    list_unsubscribe = \"x\"\n)]"
        ));
        // builder call must NOT match
        assert!(!file_declares_list_unsubscribe(
            "Mail::builder().list_unsubscribe(\"x\").build()"
        ));
        // line comment must NOT match
        assert!(!file_declares_list_unsubscribe(
            "// remember to set list_unsubscribe later"
        ));
        // commented-out attribute (line and mid-line) must NOT match
        assert!(!file_declares_list_unsubscribe(
            "// #[mailer(list_unsubscribe = \"x\")]"
        ));
        assert!(!file_declares_list_unsubscribe(
            "let x = 1; // #[mailer(list_unsubscribe = \"x\")]"
        ));
        // block-commented attribute must NOT match
        assert!(!file_declares_list_unsubscribe(
            "/* #[mailer(list_unsubscribe = \"weekly\")] */"
        ));
        assert!(!file_declares_list_unsubscribe(
            "before /*\n#[mailer(list_unsubscribe = \"x\")]\n*/ after"
        ));
    }

    #[test]
    fn scan_finds_workspace_mailer_but_skips_vendored_copy() {
        let root = tempfile::tempdir().expect("temp dir");
        // A vendored dependency copy (e.g. `cargo vendor`) carries the framework's
        // own list mailer in its tests — this must NOT count as app usage.
        let vendored = root.path().join("vendor/autumn-web/tests");
        std::fs::create_dir_all(&vendored).expect("create vendor dir");
        std::fs::write(
            vendored.join("mail_unsubscribe.rs"),
            "#[mailer(list_unsubscribe = \"weekly_digest\")]\nfn x() {}",
        )
        .expect("write vendored source");
        assert!(
            !scan_dir_for_list_unsubscribe(root.path()),
            "vendored dependency sources must be skipped"
        );

        // Integration-test and example fixtures are not compiled into the app
        // binary, so the runtime inventory never registers them — they must be
        // skipped too.
        for tree in ["tests", "examples"] {
            let dir = root.path().join(tree);
            std::fs::create_dir_all(&dir).expect("create test/example dir");
            std::fs::write(
                dir.join("fixture.rs"),
                "#[mailer(list_unsubscribe = \"weekly_digest\")]\nfn f() {}",
            )
            .expect("write fixture source");
        }
        assert!(
            !scan_dir_for_list_unsubscribe(root.path()),
            "tests/ and examples/ fixtures must be skipped"
        );

        // A real workspace member that declares one is still detected.
        let app = root.path().join("crates/marketing/src");
        std::fs::create_dir_all(&app).expect("create app dir");
        std::fs::write(
            app.join("mailers.rs"),
            "#[mailer(list_unsubscribe = \"weekly_digest\")]\nfn y() {}",
        )
        .expect("write app source");
        assert!(
            scan_dir_for_list_unsubscribe(root.path()),
            "workspace member mailer must still be detected"
        );
    }

    #[test]
    fn inline_profile_lookup_order_matches_runtime() {
        // Canonical spelling applied last so it wins over the legacy alias —
        // matching autumn_web::config::profile_lookup_names.
        assert_eq!(
            inline_profile_lookup_names("prod"),
            vec!["production", "prod"]
        );
        assert_eq!(
            inline_profile_lookup_names("dev"),
            vec!["development", "dev"]
        );
        assert_eq!(inline_profile_lookup_names("staging"), vec!["staging"]);
    }

    #[test]
    fn override_file_lookup_order_prefers_selected_spelling() {
        // Default canonical selection prefers the canonical file.
        assert_eq!(
            override_file_lookup_names("prod", "prod"),
            vec!["prod".to_owned(), "production".to_owned()]
        );
        // When the operator selected the legacy spelling, that file is preferred.
        assert_eq!(
            override_file_lookup_names("prod", "production"),
            vec!["production".to_owned(), "prod".to_owned()]
        );
        assert_eq!(
            override_file_lookup_names("dev", "dev"),
            vec!["dev".to_owned(), "development".to_owned()]
        );
        assert_eq!(
            override_file_lookup_names("dev", "development"),
            vec!["development".to_owned(), "dev".to_owned()]
        );
        assert_eq!(
            override_file_lookup_names("staging", "staging"),
            vec!["staging".to_owned()]
        );
    }

    #[test]
    fn mail_unsubscribe_invalid_base_url_fails_in_prod() {
        // Non-empty but malformed: runtime validate() would reject this, so doctor
        // must not return Pass on it.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("http://app.example.com"),
            None,
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com:abc"),
            None,
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn mail_unsubscribe_invalid_mailto_fails_in_prod() {
        let r = check_mail_unsubscribe_config_impl(
            true,
            None,
            Some("unsubscribe example.com"),
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn mail_unsubscribe_invalid_value_is_lenient_outside_prod() {
        // Dev mirrors the runtime: malformed values are not a hard gate.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("http://app.example.com"),
            None,
            false,
            false,
            30,
        );
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn doctor_url_validation_matches_runtime_cases() {
        assert!(is_valid_https_base_url_doctor("https://app.example.com"));
        assert!(is_valid_https_base_url_doctor(
            "https://app.example.com:8443"
        ));
        assert!(is_valid_https_base_url_doctor(
            "https://app.example.com/base"
        ));
        assert!(!is_valid_https_base_url_doctor("http://app.example.com"));
        assert!(!is_valid_https_base_url_doctor(
            "https://app.example.com:abc"
        ));
        assert!(!is_valid_https_base_url_doctor("https://@/base"));
        assert!(!is_valid_https_base_url_doctor("https:///path"));
        assert!(!is_valid_https_base_url_doctor("https:/app.example.com"));
        assert!(!is_valid_https_base_url_doctor("https:app.example.com"));
        assert!(!is_valid_https_base_url_doctor(
            "https://user@app.example.com"
        ));
        assert!(!is_valid_https_base_url_doctor(
            "https://app.example.com?q=1"
        ));

        assert!(is_valid_mailto_address_doctor("unsub@example.com"));
        assert!(is_valid_mailto_address_doctor("mailto:unsub@example.com"));
        assert!(!is_valid_mailto_address_doctor("not-an-email"));
        assert!(!is_valid_mailto_address_doctor("unsubscribe example.com"));
        assert!(!is_valid_mailto_address_doctor("https://unsub@example.com"));
        assert!(!is_valid_mailto_address_doctor("unsub@https://example.com"));
    }

    #[test]
    fn is_absolute_http_url_doctor_matches_runtime_absoluteness() {
        // Runtime (`http_client::Client::build_request`) treats a URL as absolute
        // iff it starts with `http://` or `https://`. Accept BOTH schemes — the
        // runtime does not require TLS for a webhook, so neither may doctor
        // (rejecting `http://` would be a false-positive warning).
        assert!(is_absolute_http_url_doctor(
            "https://alerts.example.com/hooks"
        ));
        assert!(is_absolute_http_url_doctor(
            "http://alerts.example.com/hooks"
        ));
        // A query string / userinfo / non-standard port is fine: the runtime posts
        // the URL verbatim, so doctor must not reject what delivers (unlike the
        // stricter https-base-url validator).
        assert!(is_absolute_http_url_doctor(
            "https://user@h.example.com/x?a=1"
        ));
        assert!(is_absolute_http_url_doctor("http://h.example.com:8080/x"));
        // Relative values never carry the scheme prefix runtime requires, and an
        // alert webhook has no base-url alias to resolve them against.
        assert!(!is_absolute_http_url_doctor("hooks.example/x"));
        assert!(!is_absolute_http_url_doctor("/hooks/autumn"));
        assert!(!is_absolute_http_url_doctor("alerts.example.com"));
        assert!(!is_absolute_http_url_doctor(""));
        // Non-http(s) schemes are not dispatched by the client as absolute here.
        assert!(!is_absolute_http_url_doctor("ftp://h.example.com/x"));
        assert!(!is_absolute_http_url_doctor("ws://h.example.com/x"));
        // Carries the prefix but has no host reqwest can send to.
        assert!(!is_absolute_http_url_doctor("https://"));
        assert!(!is_absolute_http_url_doctor("http://"));
    }

    // Exercises the lettre-backed parser + its runtime parity, so it is gated to
    // the default (Postgres) build where the `mail` re-export is compiled. The
    // sqlite-build fallback (a coarse syntactic check) is deliberately not held to
    // lettre parity.
    #[cfg(feature = "postgres")]
    #[test]
    fn is_valid_alert_mailbox_matches_lettre_and_accepts_display_name() {
        // Bare recipient mailboxes are accepted (this is what lettre parses at
        // send time for `[alerts] email`).
        assert!(is_valid_alert_mailbox_doctor("oncall@example.com"));
        assert!(is_valid_alert_mailbox_doctor("a+b@sub.example.co"));
        // RFC 5322 display-name form is ACCEPTED — the runtime's lettre parser
        // accepts it and registers + delivers to it fine, so doctor must not
        // reject it (the regression this fix addresses: a false `--strict`
        // failure on a valid config).
        assert!(is_valid_alert_mailbox_doctor("Ops <ops@example.com>"));
        // The `mailto:` URI form is REJECTED (unlike the unsubscribe-mailto
        // validator): the runtime passes it verbatim to lettre, which rejects it
        // as a recipient. Doctor must not pass a config that can't deliver.
        assert!(!is_valid_alert_mailbox_doctor("mailto:oncall@example.com"));
        // Other scheme/URI forms and malformed values stay rejected.
        assert!(!is_valid_alert_mailbox_doctor("https://oncall@example.com"));
        assert!(!is_valid_alert_mailbox_doctor("not-an-address"));
        assert!(!is_valid_alert_mailbox_doctor("oncall example.com"));
        assert!(!is_valid_alert_mailbox_doctor(""));

        // Exact parity with the runtime: doctor's check must agree with the very
        // parser `autumn/src/alerts.rs`'s `is_valid_alert_mailbox` runs, for every
        // case above. This is what keeps doctor from being stricter (or looser)
        // than the SMTP send path.
        for case in [
            "oncall@example.com",
            "a+b@sub.example.co",
            "Ops <ops@example.com>",
            "mailto:oncall@example.com",
            "https://oncall@example.com",
            "not-an-address",
            "oncall example.com",
            "",
        ] {
            let runtime_ok = case
                .parse::<autumn_web::reexports::lettre::message::Mailbox>()
                .is_ok();
            assert_eq!(
                is_valid_alert_mailbox_doctor(case),
                runtime_ok,
                "doctor and the runtime lettre parser must agree on {case:?}"
            );
        }
    }

    #[test]
    fn mail_unsubscribe_usage_with_both_url_and_mailto_warns_in_prod() {
        // base_url presence drives the suppression-backend warning even when a
        // mailto fallback is also configured.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com"),
            Some("unsub@example.com"),
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn transport_resolver_trims_and_ignores_invalid_env() {
        // Valid env override wins (trimmed + case-insensitive).
        assert_eq!(
            resolve_effective_mail_transport(Some(" SMTP ".to_owned()), Some("disabled"), "prod"),
            "smtp"
        );
        // Whitespace-only "disabled" must resolve to disabled, not be treated raw.
        assert_eq!(
            resolve_effective_mail_transport(Some(" disabled ".to_owned()), None, "prod"),
            "disabled"
        );
        // Invalid env override is ignored → falls back to TOML.
        assert_eq!(
            resolve_effective_mail_transport(Some("bogus".to_owned()), Some("disabled"), "prod"),
            "disabled"
        );
        // No env, no TOML → profile smart-default.
        assert_eq!(resolve_effective_mail_transport(None, None, "dev"), "log");
        assert_eq!(
            resolve_effective_mail_transport(None, None, "prod"),
            "disabled"
        );
        // TOML value is parsed too (case-insensitive).
        assert_eq!(
            resolve_effective_mail_transport(None, Some("LOG"), "prod"),
            "log"
        );
    }

    #[test]
    fn ttl_resolver_ignores_invalid_env_like_runtime() {
        let table: toml::Table =
            toml::from_str("[mail]\nunsubscribe_token_ttl_days = 14\n").unwrap();
        // Absent env → TOML value.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(None, Some(&table)),
            14
        );
        // No TOML, no env → default 30.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(None, None),
            30
        );
        // Valid env overrides TOML.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some("7".to_owned()), Some(&table)),
            7
        );
        // Blank / non-integer env is ignored (runtime warns + keeps TOML/default),
        // so it must NOT resolve to 0 and falsely fail the positive-days check.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some(String::new()), Some(&table)),
            14
        );
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some("abc".to_owned()), Some(&table)),
            14
        );
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some("  ".to_owned()), None),
            30
        );
    }

    #[test]
    fn ttl_toml_type_invalid_flags_non_integer_values() {
        // Absent key → not invalid (the default applies).
        assert!(!unsubscribe_ttl_toml_type_invalid(None));
        let empty: toml::Table = toml::from_str("[mail]\n").unwrap();
        assert!(!unsubscribe_ttl_toml_type_invalid(Some(&empty)));
        // A bare integer is valid.
        let int: toml::Table = toml::from_str("[mail]\nunsubscribe_token_ttl_days = 30\n").unwrap();
        assert!(!unsubscribe_ttl_toml_type_invalid(Some(&int)));
        // A quoted string or a float is the wrong type — the runtime's typed i64
        // deserialize rejects it before boot, so doctor must flag it.
        let string: toml::Table =
            toml::from_str("[mail]\nunsubscribe_token_ttl_days = \"30\"\n").unwrap();
        assert!(unsubscribe_ttl_toml_type_invalid(Some(&string)));
        let float: toml::Table =
            toml::from_str("[mail]\nunsubscribe_token_ttl_days = 30.0\n").unwrap();
        assert!(unsubscribe_ttl_toml_type_invalid(Some(&float)));
    }

    #[test]
    fn dest_toml_type_invalid_flags_non_string_values() {
        assert!(!unsubscribe_dest_toml_type_invalid(None));
        let empty: toml::Table = toml::from_str("[mail]\n").unwrap();
        assert!(!unsubscribe_dest_toml_type_invalid(Some(&empty)));
        // String values are valid.
        let strings: toml::Table = toml::from_str(
            "[mail]\nunsubscribe_base_url = \"https://x\"\nunsubscribe_mailto = \"u@x.com\"\n",
        )
        .unwrap();
        assert!(!unsubscribe_dest_toml_type_invalid(Some(&strings)));
        // A present non-string (integer/array) is the wrong type — the runtime's
        // Option<String> deserialize rejects it before boot, so doctor must flag it.
        let int_url: toml::Table = toml::from_str("[mail]\nunsubscribe_base_url = 42\n").unwrap();
        assert!(unsubscribe_dest_toml_type_invalid(Some(&int_url)));
        let arr_mailto: toml::Table =
            toml::from_str("[mail]\nunsubscribe_mailto = [\"u@x.com\"]\n").unwrap();
        assert!(unsubscribe_dest_toml_type_invalid(Some(&arr_mailto)));
    }

    #[test]
    fn mail_unsubscribe_non_positive_ttl_fails_in_any_profile() {
        // Runtime validate() rejects a non-positive TTL in every profile, even
        // with no list usage or a disabled transport, so the app won't boot.
        for is_prod in [true, false] {
            let r = check_mail_unsubscribe_config_impl(false, None, None, true, is_prod, 0);
            assert_eq!(r.status, CheckStatus::Fail, "ttl=0 prod={is_prod}");
            let r = check_mail_unsubscribe_config_impl(false, None, None, true, is_prod, -5);
            assert_eq!(r.status, CheckStatus::Fail, "ttl=-5 prod={is_prod}");
        }
    }

    #[test]
    fn mail_unsubscribe_invalid_destination_fails_even_without_usage() {
        // Runtime validate() rejects a malformed destination in prod regardless of
        // whether any #[mailer] declares list_unsubscribe, so doctor must not Pass
        // before validating it.
        let r = check_mail_unsubscribe_config_impl(
            false,
            Some("http://app.example.com"),
            None,
            true,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
        let r = check_mail_unsubscribe_config_impl(
            false,
            None,
            Some("unsubscribe example.com"),
            true,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn strip_rust_comments_handles_unterminated_block_comment() {
        // Should not panic; the remaining unclosed block is simply stripped.
        let result = strip_rust_comments("code /* unclosed comment");
        assert!(result.contains("code"));
        assert!(!result.contains("unclosed"));
    }

    #[test]
    fn strip_rust_comments_removes_line_comments_and_preserves_code() {
        // Code before a comment is retained; everything after `//` is dropped.
        let result = strip_rust_comments("let x = 1; // this comment should disappear");
        assert!(
            result.contains("let x = 1;"),
            "code before comment preserved: {result}"
        );
        assert!(
            !result.contains("disappear"),
            "line comment stripped: {result}"
        );
    }

    // ── glyph ────────────────────────────────────────────────────────────────

    #[test]
    fn glyph_pass() {
        assert_eq!(glyph(&CheckStatus::Pass), "✅");
    }

    #[test]
    fn glyph_warn() {
        assert_eq!(glyph(&CheckStatus::Warn), "⚠️ ");
    }

    #[test]
    fn glyph_fail() {
        assert_eq!(glyph(&CheckStatus::Fail), "❌");
    }

    #[test]
    fn trusted_hosts_fail_in_production_when_empty() {
        let result = check_trusted_hosts_impl(&[], true);
        assert_eq!(result.name, "trusted_hosts");
        assert!(matches!(result.status, CheckStatus::Fail));
    }

    #[test]
    fn trusted_hosts_warn_on_wildcard() {
        let result = check_trusted_hosts_impl(&["*".to_owned()], true);
        assert_eq!(result.name, "trusted_hosts");
        assert!(matches!(result.status, CheckStatus::Warn));
    }

    #[test]
    fn trusted_hosts_warn_on_wildcard_when_mixed_with_other_entries() {
        let result = check_trusted_hosts_impl(&["example.com".to_owned(), "*".to_owned()], true);
        assert_eq!(result.name, "trusted_hosts");
        assert!(matches!(result.status, CheckStatus::Warn));
    }

    // ── check_tls_impl (issue #1603) ─────────────────────────────────────────

    #[test]
    fn tls_pass_when_not_configured() {
        let r = check_tls_impl(&TlsDoctorData::NotConfigured);
        assert_eq!(r.name, "tls");
        assert!(matches!(r.status, CheckStatus::Pass));
        assert!(r.hint.is_none());
    }

    #[test]
    fn tls_fail_when_expired() {
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: true,
            days_until_expiry: -3,
        });
        assert!(matches!(r.status, CheckStatus::Fail));
        assert!(r.detail.as_deref().unwrap().contains("expired"));
        assert!(r.hint.is_some());
    }

    // A not-yet-valid leaf (notBefore in the future) fails every handshake until
    // its window opens, so the runtime rejects it at startup — doctor must grade
    // it a Fail, symmetric to the expired path (#1603, Codex review). The
    // not_yet_valid flag wins even when the expiry fields would otherwise Pass.
    #[test]
    fn tls_fail_when_not_yet_valid() {
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: true,
            expired: false,
            days_until_expiry: 365,
        });
        assert!(matches!(r.status, CheckStatus::Fail));
        assert!(r.detail.as_deref().unwrap().contains("not yet valid"));
        assert!(r.hint.is_some());
    }

    #[test]
    fn tls_warn_when_near_expiry() {
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: false,
            days_until_expiry: 10,
        });
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(r.detail.as_deref().unwrap().contains("10 day"));
    }

    #[test]
    fn tls_warn_at_exactly_thirty_days() {
        // The 30-day boundary is inclusive of the warning window.
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: false,
            days_until_expiry: 30,
        });
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn tls_pass_when_healthy() {
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: false,
            days_until_expiry: 365,
        });
        assert!(matches!(r.status, CheckStatus::Pass));
        assert!(r.hint.is_none());
    }

    // A certificate that expired less than 24h ago truncates to `0` days but
    // must grade as a FAIL, not a near-expiry WARN — the runtime rejects it at
    // startup (issue #1603, Codex review). We grade off the injected notAfter
    // via `LeafInspection::is_expired`, so no time-dependent fixture is needed.
    #[cfg(feature = "tls")]
    #[test]
    fn tls_grades_from_raw_timestamp_not_day_bucket() {
        use autumn_web::tls::LeafInspection;

        let now: i64 = 1_700_000_000;

        // A notBefore a day in the past keeps every fixture below currently
        // valid (not_yet_valid == false), isolating the expiry grading.
        let not_before = now - 86_400;

        // Expired 1h ago: buckets to 0 days, but is_expired → Fail.
        let just_expired = LeafInspection {
            not_before_unix: not_before,
            not_after_unix: now - 3_600,
        };
        assert!(just_expired.is_expired(now));
        assert!(!just_expired.is_not_yet_valid(now));
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: just_expired.is_not_yet_valid(now),
            expired: just_expired.is_expired(now),
            days_until_expiry: just_expired.days_until_expiry(now),
        });
        assert!(
            matches!(r.status, CheckStatus::Fail),
            "cert expired <24h ago must Fail, got {:?}",
            r.status
        );

        // Expires in 1h (not yet expired): near-expiry Warn.
        let expiring_soon = LeafInspection {
            not_before_unix: not_before,
            not_after_unix: now + 3_600,
        };
        assert!(!expiring_soon.is_expired(now));
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: expiring_soon.is_not_yet_valid(now),
            expired: expiring_soon.is_expired(now),
            days_until_expiry: expiring_soon.days_until_expiry(now),
        });
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "cert expiring in <24h (not expired) must Warn, got {:?}",
            r.status
        );

        // Expires in 40 days: healthy Pass.
        let healthy = LeafInspection {
            not_before_unix: not_before,
            not_after_unix: now + 40 * 86_400,
        };
        assert!(!healthy.is_expired(now));
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: healthy.is_not_yet_valid(now),
            expired: healthy.is_expired(now),
            days_until_expiry: healthy.days_until_expiry(now),
        });
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "cert with 40 days left must Pass, got {:?}",
            r.status
        );

        // Not yet valid: notBefore an hour in the future → Fail, even though
        // notAfter is far off (days_until_expiry would otherwise Pass).
        let not_yet_valid = LeafInspection {
            not_before_unix: now + 3_600,
            not_after_unix: now + 40 * 86_400,
        };
        assert!(not_yet_valid.is_not_yet_valid(now));
        assert!(!not_yet_valid.is_expired(now));
        let r = check_tls_impl(&TlsDoctorData::Healthy {
            not_yet_valid: not_yet_valid.is_not_yet_valid(now),
            expired: not_yet_valid.is_expired(now),
            days_until_expiry: not_yet_valid.days_until_expiry(now),
        });
        assert!(
            matches!(r.status, CheckStatus::Fail),
            "not-yet-valid cert must Fail, got {:?}",
            r.status
        );
        assert!(r.detail.as_deref().unwrap().contains("not yet valid"));
    }

    #[test]
    fn tls_fail_when_invalid() {
        let r = check_tls_impl(&TlsDoctorData::Invalid {
            detail: "cert file not found".to_owned(),
        });
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.detail.as_deref(), Some("cert file not found"));
        assert!(r.hint.is_some());
    }

    // Regression (#1603, Codex): a tuning-only TLS env deployment — e.g. only
    // `AUTUMN_SERVER__TLS__RELOAD_INTERVAL_SECS` set, no cert/key — makes the
    // runtime materialize `[server.tls]` and fail fast. Doctor must treat it as
    // configured (Some with empty paths) so it emits the actionable "must set
    // both" Fail, not a false Pass/NotConfigured. Tested via the pure sources
    // helper to avoid mutating the process-global environment.
    #[test]
    fn tls_tuning_only_env_resolves_as_incomplete_config() {
        let resolved = resolve_tls_paths_from_sources(
            false, // no [server.tls] section
            None,  // toml_cert
            None,  // toml_key
            None,  // env_cert (CERT_PATH unset)
            None,  // env_key (KEY_PATH unset)
            true,  // any_tls_env: a tuning-only var such as RELOAD_INTERVAL_SECS is set
        );
        let (cert, key) = resolved.expect("tuning-only TLS env must resolve as configured");
        assert!(
            cert.as_os_str().is_empty() && key.as_os_str().is_empty(),
            "cert/key must be empty so the caller reports 'must set both'"
        );

        // Empty paths are exactly what `resolve_tls_doctor_data` maps to the
        // incomplete-config Invalid, which grades as a Fail.
        assert!(
            cert.as_os_str().is_empty() || key.as_os_str().is_empty(),
            "mirrors resolve_tls_doctor_data's incomplete-config guard"
        );
        let data = TlsDoctorData::Invalid {
            detail: "[server.tls] must set both cert_path and key_path".to_owned(),
        };
        let r = check_tls_impl(&data);
        assert!(matches!(r.status, CheckStatus::Fail));
        assert!(r.detail.as_deref().unwrap().contains("must set both"));
    }

    // Regression (#1603, Codex): the runtime prefers `AUTUMN_MANIFEST_DIR` for
    // `autumn.toml`, so a `[server.tls]` section present ONLY in the manifest-dir
    // config must be graded — not reported NotConfigured because doctor read the
    // CWD. `resolve_tls_paths` resolves the merged table through
    // `resolve_config_path_manifest_aware`; here we inject the manifest dir
    // directly (no process-env mutation) by pointing `resolve_path` at a temp dir
    // and confirm the section flows through `resolve_tls_paths_from_sources`.
    #[test]
    fn tls_section_in_manifest_dir_config_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server.tls]\ncert_path = \"/etc/tls/cert.pem\"\nkey_path = \"/etc/tls/key.pem\"\n",
        )
        .unwrap();

        // Resolve the merged table the way `resolve_tls_paths` does when
        // `AUTUMN_MANIFEST_DIR` points at `dir`: each filename resolves under it.
        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("dev", "dev", move |f| base.join(f));

        let tls = merged
            .get("server")
            .and_then(toml::Value::as_table)
            .and_then(|s| s.get("tls"))
            .and_then(toml::Value::as_table);
        assert!(
            tls.is_some(),
            "a [server.tls] section in the manifest-dir autumn.toml must be visible"
        );

        let toml_cert = tls
            .and_then(|t| t.get("cert_path"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        let toml_key = tls
            .and_then(|t| t.get("key_path"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned);

        let (cert, key) =
            resolve_tls_paths_from_sources(tls.is_some(), toml_cert, toml_key, None, None, false)
                .expect("manifest-dir [server.tls] must resolve as configured, not NotConfigured");
        assert_eq!(cert, std::path::PathBuf::from("/etc/tls/cert.pem"));
        assert_eq!(key, std::path::PathBuf::from("/etc/tls/key.pem"));
    }

    // Complementary: with no section and NO TLS env var, TLS is unconfigured.
    #[test]
    fn tls_paths_none_when_unconfigured() {
        assert!(
            resolve_tls_paths_from_sources(false, None, None, None, None, false).is_none(),
            "no section and no TLS env means not configured"
        );
    }

    // Regression (#1603, Codex): detection must be limited to the FOUR recognized
    // `TLS_ENV_VAR_NAMES` — the exact set the runtime materializes `[server.tls]`
    // from. An unrelated/mistyped var sharing the prefix, e.g.
    // `AUTUMN_SERVER__TLS__ENABLED=false`, must NOT mark TLS configured: the
    // server serves plain HTTP for it, so treating it as configured would make
    // `doctor --strict` fail on a missing cert/key for a config the server
    // accepts. A recognized tuning var (`HANDSHAKE_TIMEOUT_SECS`) must still count.
    #[test]
    fn unrecognized_tls_env_var_is_not_detected() {
        use autumn_web::config::MockEnv;

        // Unrecognized prefix var, no cert/key: NOT detected → not configured.
        let unrecognized = MockEnv::new().with("AUTUMN_SERVER__TLS__ENABLED", "false");
        assert!(
            !any_tls_env_var_set_in(&unrecognized),
            "an unrecognized AUTUMN_SERVER__TLS__* var must not mark TLS configured"
        );
        assert!(
            resolve_tls_paths_from_sources(
                false, // no [server.tls] section
                None,
                None,
                None,
                None,
                any_tls_env_var_set_in(&unrecognized),
            )
            .is_none(),
            "an unrecognized TLS env var must resolve as not-configured (no spurious Fail)"
        );

        // A recognized tuning var IS still detected (materializes the table).
        let recognized = MockEnv::new().with("AUTUMN_SERVER__TLS__HANDSHAKE_TIMEOUT_SECS", "5");
        assert!(
            any_tls_env_var_set_in(&recognized),
            "a recognized TLS tuning var must still mark TLS configured"
        );
    }

    // Env cert/key win over toml, and a fully-specified pair resolves as-is.
    #[test]
    fn tls_paths_env_overrides_toml() {
        let resolved = resolve_tls_paths_from_sources(
            true,
            Some("/toml/c.pem".to_owned()),
            Some("/toml/k.pem".to_owned()),
            Some("/env/c.pem".to_owned()),
            Some("/env/k.pem".to_owned()),
            true,
        )
        .expect("configured");
        assert_eq!(resolved.0, std::path::PathBuf::from("/env/c.pem"));
        assert_eq!(resolved.1, std::path::PathBuf::from("/env/k.pem"));
    }

    // Regression (#1603, Codex): a PRESENT-but-EMPTY `AUTUMN_SERVER__TLS__CERT_PATH`
    // (e.g. a deployment that exports `AUTUMN_SERVER__TLS__CERT_PATH=`) is a
    // higher-priority override the runtime materializes as an empty `PathBuf`,
    // OVERWRITING the valid TOML cert path and failing fast on the missing file.
    // Doctor must read env vars with `.ok()` (no empty-filter) so it resolves to
    // the SAME empty path and grades a Fail — not silently fall back to the TOML
    // path and pass a config the server rejects. Extract exactly as
    // `resolve_tls_paths` now does (`env.var(...).ok()`) from a map-backed `Env`.
    #[test]
    fn tls_empty_env_override_clears_toml_path() {
        use autumn_web::config::{Env, MockEnv};

        // TOML has a valid cert/key pair, but the deployment exports an EMPTY
        // CERT_PATH override. The runtime overwrites cert_path with an empty
        // PathBuf; doctor must mirror that presence-vs-empty rule.
        let env = MockEnv::new().with("AUTUMN_SERVER__TLS__CERT_PATH", "");
        // Mirror `resolve_tls_paths`: `.ok()` with NO `.filter(non-empty)`, so a
        // present-but-empty var is `Some("")` (a clearing override), while an
        // unset var stays `None` (falls through to TOML).
        let env_cert = env.var("AUTUMN_SERVER__TLS__CERT_PATH").ok();
        let env_key = env.var("AUTUMN_SERVER__TLS__KEY_PATH").ok();
        assert_eq!(
            env_cert.as_deref(),
            Some(""),
            "a present-but-empty CERT_PATH must survive as Some(\"\"), not be dropped"
        );
        assert!(env_key.is_none(), "an unset KEY_PATH stays None");

        let (cert, key) = resolve_tls_paths_from_sources(
            true,                              // [server.tls] section present
            Some("/toml/cert.pem".to_owned()), // valid TOML cert
            Some("/toml/key.pem".to_owned()),  // valid TOML key
            env_cert,                          // present-but-empty override
            env_key,                           // unset -> falls back to TOML key
            any_tls_env_var_set_in(&env),
        )
        .expect("a present TLS env var means TLS is configured");

        // The empty override wins over the TOML cert, matching the runtime.
        assert!(
            cert.as_os_str().is_empty(),
            "empty CERT_PATH override must clear the TOML cert path"
        );
        assert_eq!(
            key,
            std::path::PathBuf::from("/toml/key.pem"),
            "the unset KEY_PATH keeps the TOML value"
        );

        // Empty cert path is exactly what `resolve_tls_doctor_data` maps to the
        // incomplete-config Invalid, which grades a Fail — same failure the
        // server surfaces at startup.
        let r = check_tls_impl(&TlsDoctorData::Invalid {
            detail: "[server.tls] must set both cert_path and key_path".to_owned(),
        });
        assert!(matches!(r.status, CheckStatus::Fail));
    }

    // Regression (#1603, Codex): TLS vars supplied through the `.env`/
    // `.env.<profile>` overlay the runtime loads (not the process env) must be
    // visible to doctor. `resolve_tls_paths` now reads them through the same
    // profile-aware `Env` overlay the sibling checks use; feed that overlay a
    // `MockEnv` (a map-backed `Env`) with TLS vars set and prove detection.
    #[test]
    fn dotenv_supplied_tls_var_is_detected() {
        use autumn_web::config::{Env, MockEnv};

        // A cert/key pair visible ONLY through the overlay (never the process
        // env) must resolve as configured — this is exactly the source
        // `resolve_tls_paths` now reads the `AUTUMN_SERVER__TLS__*` vars from.
        let env = MockEnv::new()
            .with("AUTUMN_SERVER__TLS__CERT_PATH", "/dotenv/cert.pem")
            .with("AUTUMN_SERVER__TLS__KEY_PATH", "/dotenv/key.pem");
        assert!(
            any_tls_env_var_set_in(&env),
            "a dotenv-supplied TLS var must count as configured"
        );
        let env_cert = env
            .var("AUTUMN_SERVER__TLS__CERT_PATH")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let env_key = env
            .var("AUTUMN_SERVER__TLS__KEY_PATH")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let resolved = resolve_tls_paths_from_sources(
            false, // no [server.tls] section in autumn.toml
            None,  // toml_cert
            None,  // toml_key
            env_cert,
            env_key,
            any_tls_env_var_set_in(&env),
        )
        .expect("dotenv-supplied TLS cert/key must resolve as configured");
        assert_eq!(resolved.0, std::path::PathBuf::from("/dotenv/cert.pem"));
        assert_eq!(resolved.1, std::path::PathBuf::from("/dotenv/key.pem"));

        // A tuning-only dotenv var (no cert/key) is otherwise invisible, yet
        // must still mark TLS configured so doctor emits the actionable "must
        // set both" Fail instead of a false Pass.
        let tuning_only = MockEnv::new().with("AUTUMN_SERVER__TLS__RELOAD_INTERVAL_SECS", "30");
        assert!(
            any_tls_env_var_set_in(&tuning_only),
            "a dotenv-supplied tuning var must count as configured"
        );
        let (cert, key) = resolve_tls_paths_from_sources(
            false,
            None,
            None,
            None,
            None,
            any_tls_env_var_set_in(&tuning_only),
        )
        .expect("tuning-only dotenv TLS var must resolve as configured");
        assert!(
            cert.as_os_str().is_empty() && key.as_os_str().is_empty(),
            "cert/key must be empty so the caller reports 'must set both'"
        );
    }

    #[test]
    fn tls_warn_when_feature_disabled() {
        let r = check_tls_impl(&TlsDoctorData::FeatureDisabled);
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(r.detail.as_deref().unwrap().contains("tls"));
    }

    // ── ACME preflight graders (issue #1608) ─────────────────────────────────

    #[test]
    fn acme_ports_pass_when_both_open() {
        let r = check_acme_ports_for_challenge(
            "app.example.com",
            PortReachability::Open,
            PortReachability::Open,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn acme_ports_fail_when_80_unreachable() {
        for p80 in [
            PortReachability::Refused,
            PortReachability::TimedOut,
            PortReachability::Error,
        ] {
            let r = check_acme_ports_for_challenge(
                "app.example.com",
                p80,
                PortReachability::Open,
                false,
            );
            assert!(matches!(r.status, CheckStatus::Fail), "port80={p80:?}");
            assert!(r.detail.as_deref().unwrap().contains("port 80"));
        }
    }

    #[test]
    fn acme_ports_warn_when_only_443_down() {
        let r = check_acme_ports_for_challenge(
            "app.example.com",
            PortReachability::Open,
            PortReachability::Refused,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn acme_dns_pass_when_points_here() {
        let r = check_acme_dns_for_challenge("app.example.com", &DnsPointsHere::Matches, false);
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn acme_dns_fail_when_resolves_elsewhere() {
        let r = check_acme_dns_for_challenge(
            "app.example.com",
            &DnsPointsHere::ResolvesElsewhere {
                resolved: vec!["203.0.113.7".to_owned()],
            },
            false,
        );
        assert!(matches!(r.status, CheckStatus::Fail));
        assert!(r.detail.as_deref().unwrap().contains("203.0.113.7"));
    }

    #[test]
    fn acme_dns_warn_when_indeterminate() {
        for outcome in [DnsPointsHere::Unresolved, DnsPointsHere::LocalIpsUnknown] {
            let r = check_acme_dns_for_challenge("app.example.com", &outcome, false);
            assert!(matches!(r.status, CheckStatus::Warn), "outcome={outcome:?}");
        }
    }

    // Regression (#1608, Codex): in NAT/container deployments the discovered local
    // source IP is private/CGNAT/link-local while the domain resolves to a public
    // LB/elastic IP. Comparing those must NOT report `ResolvesElsewhere` (which
    // fails `--online --strict`); with no PUBLIC local IP the result is the
    // inconclusive `LocalIpsUnknown`.
    #[test]
    fn dns_private_local_ips_are_inconclusive_not_elsewhere() {
        let resolved: Vec<std::net::IpAddr> = vec!["203.0.113.10".parse().unwrap()];
        let local: Vec<std::net::IpAddr> = vec![
            "10.0.0.5".parse().unwrap(),     // RFC1918
            "172.16.4.9".parse().unwrap(),   // RFC1918
            "192.168.1.20".parse().unwrap(), // RFC1918
            "100.72.0.1".parse().unwrap(),   // CGNAT 100.64/10
            "169.254.3.4".parse().unwrap(),  // link-local
            "127.0.0.1".parse().unwrap(),    // loopback
            "fe80::1".parse().unwrap(),      // IPv6 link-local
            "fd00::1".parse().unwrap(),      // IPv6 unique-local
        ];
        assert_eq!(
            grade_dns_points_here(&resolved, &local),
            DnsPointsHere::LocalIpsUnknown,
            "only private/link-local local IPs must be inconclusive, never ResolvesElsewhere"
        );
    }

    #[test]
    fn dns_public_local_ip_matching_result_points_here() {
        let ip: std::net::IpAddr = "203.0.113.10".parse().unwrap();
        // A private IP alongside a matching public one must not suppress the match.
        let local: Vec<std::net::IpAddr> = vec!["192.168.1.20".parse().unwrap(), ip];
        assert_eq!(grade_dns_points_here(&[ip], &local), DnsPointsHere::Matches);
    }

    #[test]
    fn dns_public_local_ip_not_in_result_resolves_elsewhere() {
        let resolved: Vec<std::net::IpAddr> = vec!["203.0.113.10".parse().unwrap()];
        // A genuine PUBLIC local IP that does not appear in the DNS result is a
        // real mismatch.
        let local: Vec<std::net::IpAddr> = vec!["198.51.100.7".parse().unwrap()];
        assert_eq!(
            grade_dns_points_here(&resolved, &local),
            DnsPointsHere::ResolvesElsewhere {
                resolved: vec!["203.0.113.10".to_owned()],
            }
        );
    }

    #[test]
    fn dns_unresolved_when_no_addresses() {
        let local: Vec<std::net::IpAddr> = vec!["203.0.113.9".parse().unwrap()];
        assert_eq!(
            grade_dns_points_here(&[], &local),
            DnsPointsHere::Unresolved
        );
    }

    // Regression (#1608, Codex P2): the domain resolves to TWO public addresses,
    // only one of which is this host. HTTP-01's challenge token is per-process, so
    // the CA validating against the unmatched address would 404 — a partial match
    // must NOT grade as a clean Pass; it reports the unmatched address as a Warn.
    #[test]
    fn dns_partial_match_flags_unmatched_address() {
        let here: std::net::IpAddr = "203.0.113.10".parse().unwrap();
        let elsewhere: std::net::IpAddr = "198.51.100.7".parse().unwrap();
        // Two resolved public addresses; only `here` is a local public IP.
        let resolved: Vec<std::net::IpAddr> = vec![here, elsewhere];
        let local: Vec<std::net::IpAddr> = vec![here];

        // The grader must not collapse to Matches just because one address matched.
        assert_eq!(
            grade_dns_points_here(&resolved, &local),
            DnsPointsHere::PartialMatch {
                unmatched: vec!["198.51.100.7".to_owned()],
            },
            "an unmatched resolved address must not be swallowed by a first-match Pass"
        );

        // And the check grades a partial match as a Warn that names the stray
        // address — not a clean Pass.
        let outcome = grade_dns_points_here(&resolved, &local);
        let r = check_acme_dns_for_challenge("app.example.com", &outcome, false);
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "a partial DNS match must be a Warn, not a clean Pass"
        );
        assert!(
            r.detail
                .as_deref()
                .unwrap_or_default()
                .contains("198.51.100.7"),
            "the unmatched address must be reported: {:?}",
            r.detail
        );
    }

    #[test]
    fn is_public_ip_classifies_ranges() {
        for public in ["203.0.113.10", "198.51.100.7", "2001:4860:4860::8888"] {
            assert!(
                is_public_ip(public.parse().unwrap()),
                "{public} should be public"
            );
        }
        for private in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.1.1",
            "127.0.0.1",
            "0.0.0.0",
            "fe80::1",
            "fc00::1",
            "fdff::1",
            "::1",
        ] {
            assert!(
                !is_public_ip(private.parse().unwrap()),
                "{private} should NOT be public"
            );
        }
        // Just outside CGNAT (100.64/10) is public again.
        assert!(is_public_ip("100.128.0.1".parse().unwrap()));
        assert!(is_public_ip("100.63.255.255".parse().unwrap()));
    }

    #[test]
    fn acme_ca_root_unset_against_a_custom_directory_warns() {
        // The symmetric case to `acme_ca_root_with_a_public_directory_warns`:
        // a private directory whose root is not configured (and not installed
        // host-wide) fails every order at the TLS handshake, so grading it Pass
        // would let `doctor --strict` bless a dead deployment.
        let r = check_acme_ca_root_impl(&AcmeCaRootData::MissingForPrivateDirectory);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.name, "acme_ca_root");
        assert!(r.hint.unwrap().contains("ca_root_path"));
    }

    #[test]
    fn acme_ca_root_unset_passes() {
        // Let's Encrypt (staging and production) serves its directory under a
        // publicly-trusted certificate, so the platform trust store is right.
        let r = check_acme_ca_root_impl(&AcmeCaRootData::NotConfigured);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.name, "acme_ca_root");
    }

    #[test]
    fn acme_ca_root_usable_passes() {
        let r = check_acme_ca_root_impl(&AcmeCaRootData::Usable);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn acme_ca_root_unreadable_fails() {
        // Setting ca_root_path REPLACES the client's trust anchors, so an
        // unreadable file leaves it trusting nothing: every order dies at the
        // TLS handshake. That is a Fail, not a Warn.
        let r = check_acme_ca_root_impl(&AcmeCaRootData::Unreadable {
            path: "/etc/autumn/pebble-root.pem".to_owned(),
            reason: "No such file or directory".to_owned(),
        });
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.unwrap().contains("/etc/autumn/pebble-root.pem"));
        assert!(r.hint.is_some());
    }

    #[test]
    fn acme_ca_root_non_certificate_fails() {
        let r = check_acme_ca_root_impl(&AcmeCaRootData::NotACertificate {
            path: "/etc/autumn/notes.txt".to_owned(),
            reason: "no items found".to_owned(),
        });
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.unwrap().contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn acme_ca_root_malformed_toml_value_fails() {
        // The runtime's `Option<PathBuf>` rejects a non-string, so the server
        // will not boot; doctor must not report the "unset" Pass for it.
        let r = check_acme_ca_root_impl(&AcmeCaRootData::Malformed {
            value: "123".to_owned(),
        });
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.unwrap().contains("not a string"));
    }

    #[test]
    fn acme_ca_root_bundle_warns_that_only_the_first_cert_is_used() {
        // `builder_with_root` installs only the FIRST PEM section, and a root is
        // conventionally LAST in a fullchain bundle — so a bundle that "looks
        // fine" pins the leaf as the anchor and fails every order.
        let r = check_acme_ca_root_impl(&AcmeCaRootData::ExtraCertificatesIgnored {
            path: "/etc/autumn/fullchain.pem".to_owned(),
            certificates: 3,
        });
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.unwrap();
        assert!(detail.contains('3'));
        assert!(detail.contains("FIRST"));
    }

    #[test]
    fn acme_ca_root_with_a_public_directory_warns() {
        let r = check_acme_ca_root_impl(&AcmeCaRootData::UnneededForPublicDirectory {
            path: "/etc/autumn/private-root.pem".to_owned(),
            directory: "production".to_owned(),
        });
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.unwrap().contains("production"));
    }

    /// The resolver must agree with `instant_acme::Account::builder_with_root`,
    /// which is `CertificateDer::from_pem_file` (FIRST section only) plus
    /// `RootCertStore::add`. Anything doctor passes that the runtime rejects is
    /// a false green light on a config that can only produce failed orders.
    #[test]
    fn acme_ca_root_resolver_matches_what_the_runtime_accepts() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Unset against a BUILT-IN directory is the correct, quiet default.
        assert_eq!(
            resolve_acme_ca_root_data(None, None, "production"),
            AcmeCaRootData::NotConfigured
        );
        // Unset against a CUSTOM directory is the failure this whole feature
        // exists to prevent, so it must not grade Pass.
        assert_eq!(
            resolve_acme_ca_root_data(None, None, "custom-abc"),
            AcmeCaRootData::MissingForPrivateDirectory
        );
        assert!(matches!(
            resolve_acme_ca_root_data(None, Some("123"), "custom-abc"),
            AcmeCaRootData::Malformed { .. }
        ));

        let missing = dir.path().join("absent.pem");
        assert!(matches!(
            resolve_acme_ca_root_data(Some(&missing), None, "custom-abc"),
            AcmeCaRootData::Unreadable { .. }
        ));

        // A directory is not a root PEM, and must not be read as one.
        assert!(matches!(
            resolve_acme_ca_root_data(Some(dir.path()), None, "custom-abc"),
            AcmeCaRootData::Unreadable { .. }
        ));

        let junk = dir.path().join("junk.pem");
        std::fs::write(&junk, "not a certificate").expect("write");
        assert!(matches!(
            resolve_acme_ca_root_data(Some(&junk), None, "custom-abc"),
            AcmeCaRootData::NotACertificate { .. }
        ));

        // A well-formed PEM block wrapping non-DER gets past the PEM decoder but
        // not past `RootCertStore::add` — exactly where the runtime fails.
        let bogus = dir.path().join("bogus.pem");
        std::fs::write(
            &bogus,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .expect("write");
        assert!(
            matches!(
                resolve_acme_ca_root_data(Some(&bogus), None, "custom-abc"),
                AcmeCaRootData::NotACertificate { .. }
            ),
            "a PEM block holding junk must not pass; the runtime rejects it"
        );

        // A real self-signed root is usable.
        let (root_pem, _) = self_signed_root_pem();
        let root = dir.path().join("root.pem");
        std::fs::write(&root, &root_pem).expect("write");
        assert_eq!(
            resolve_acme_ca_root_data(Some(&root), None, "custom-abc"),
            AcmeCaRootData::Usable
        );

        // ...but pinned against a public Let's Encrypt directory it is a Warn.
        assert!(matches!(
            resolve_acme_ca_root_data(Some(&root), None, "production"),
            AcmeCaRootData::UnneededForPublicDirectory { .. }
        ));

        // A bundle warns: only the first section becomes an anchor.
        let bundle = dir.path().join("fullchain.pem");
        std::fs::write(&bundle, format!("{root_pem}{root_pem}")).expect("write");
        assert!(matches!(
            resolve_acme_ca_root_data(Some(&bundle), None, "custom-abc"),
            AcmeCaRootData::ExtraCertificatesIgnored {
                certificates: 2,
                ..
            }
        ));

        // A bundle AND a public directory: the public-directory case wins. The
        // bundle remedy ("trim it to the root alone") would leave ca_root_path
        // set against Let's Encrypt, which is the actual failure — so the more
        // actionable "remove ca_root_path" must be what the operator is told.
        assert!(
            matches!(
                resolve_acme_ca_root_data(Some(&bundle), None, "production"),
                AcmeCaRootData::UnneededForPublicDirectory { .. }
            ),
            "a bundle must not mask the public-directory warning"
        );
    }

    /// A self-signed CA certificate PEM, for the `ca_root_path` resolver tests.
    fn self_signed_root_pem() -> (String, String) {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let cert = params.self_signed(&key).expect("self-sign");
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn acme_stored_cert_pass_when_none_yet() {
        let r = check_acme_stored_cert_impl(&TlsDoctorData::NotConfigured);
        assert!(matches!(r.status, CheckStatus::Pass));
        assert!(r.detail.as_deref().unwrap().contains("first boot"));
    }

    #[test]
    fn acme_stored_cert_fail_when_expired() {
        let r = check_acme_stored_cert_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: true,
            days_until_expiry: -3,
        });
        assert!(matches!(r.status, CheckStatus::Fail));
    }

    #[test]
    fn acme_stored_cert_fail_when_not_yet_valid() {
        // Mirrors check_tls_impl: a stored cert whose notBefore is in the future
        // fails every handshake until its window opens, so grade it a Fail even
        // when the expiry fields would otherwise Pass.
        let r = check_acme_stored_cert_impl(&TlsDoctorData::Healthy {
            not_yet_valid: true,
            expired: false,
            days_until_expiry: 365,
        });
        assert!(matches!(r.status, CheckStatus::Fail));
        assert!(r.detail.as_deref().unwrap().contains("not yet valid"));
    }

    #[test]
    fn acme_stored_cert_warn_when_near_expiry() {
        let r = check_acme_stored_cert_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: false,
            days_until_expiry: 10,
        });
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn acme_stored_cert_pass_when_healthy() {
        let r = check_acme_stored_cert_impl(&TlsDoctorData::Healthy {
            not_yet_valid: false,
            expired: false,
            days_until_expiry: 75,
        });
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    /// A minimal, valid [`AcmeDoctorConfig`] for the pure `check_acme_config_impl`
    /// tests, so each case names only the field it exercises. Mirrors the runtime
    /// defaults an absent key resolves to (port 80, renew-before 30 days,
    /// `config/acme`, staging) with no recorded deserialize errors.
    fn acme_doctor_cfg(domains: &[&str], contact_email: &str) -> AcmeDoctorConfig {
        AcmeDoctorConfig {
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
            contact_email: contact_email.to_owned(),
            http_challenge_port: 80,
            renew_before_days: 30,
            cache_dir: std::path::PathBuf::from("config/acme"),
            directory_label: "staging".to_owned(),
            directory_error: None,
            port_error: None,
            ca_root_path: None,
            ca_root_error: None,
            domains_error: None,
            renew_before_days_error: None,
            dns: None,
            dns_error: None,
        }
    }

    // ── DNS-01 / wildcard preflight (issue #1620) ─────────────────────────────

    fn acme_dns_cfg(
        provider: autumn_web::config::AcmeDnsProvider,
    ) -> autumn_web::config::AcmeDnsConfig {
        toml::from_str(&format!("provider = \"{}\"\n", provider.as_str()))
            .expect("the minimal DNS section parses")
    }

    #[test]
    fn dns_credential_check_passes_when_no_dns_section_is_configured() {
        let result = check_acme_dns_credential_impl(&DnsCredentialOutcome::NotConfigured);
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(
            result.detail.unwrap().contains("HTTP-01"),
            "the pass must explain why no credential is needed"
        );
    }

    // AC7: "`autumn doctor` diagnoses … missing or invalid provider credential".
    // A Fail, not a Warn: without it every issuance and renewal fails, and
    // nothing about the running app reveals that until expiry.
    #[test]
    fn a_missing_dns_credential_fails_and_says_where_to_put_it() {
        let outcome = DnsCredentialOutcome::Unusable(
            "no Cloudflare API token found for [server.tls.acme.dns] credential `acme_dns`: add \
             it under `[acme_dns]` in the encrypted credentials store (`autumn credentials edit`) \
             as `api_token = \"...\"`, or set the AUTUMN_ACME_DNS_API_TOKEN environment variable"
                .to_owned(),
        );
        let result = check_acme_dns_credential_impl(&outcome);
        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap();
        assert!(detail.contains("api_token"), "got: {detail}");
        assert!(detail.contains("autumn credentials edit"), "got: {detail}");
    }

    // The runtime refuses to boot on a `[server.tls.acme.dns]` that does not
    // deserialize — most usefully, one carrying an inline `api_token`. Doctor
    // must FAIL that rather than report "no DNS provider configured".
    #[test]
    fn a_malformed_dns_section_fails_and_names_the_problem() {
        let result = check_acme_dns_credential_impl(&DnsCredentialOutcome::Malformed(
            "unknown field `api_token`".to_owned(),
        ));
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap().contains("api_token"));
        assert!(result.hint.unwrap().contains("never in autumn.toml"));
    }

    #[test]
    fn a_usable_dns_credential_passes_and_names_the_provider_and_key() {
        let result = check_acme_dns_credential_impl(&DnsCredentialOutcome::Usable {
            provider: "cloudflare".to_owned(),
            key: "acme_dns".to_owned(),
        });
        assert_eq!(result.status, CheckStatus::Pass);
        let detail = result.detail.unwrap();
        assert!(detail.contains("cloudflare"), "got: {detail}");
        assert!(detail.contains("acme_dns"), "got: {detail}");
    }

    // AC7: "challenge TXT record not visible in public DNS". A name the resolvers
    // cannot answer for at all defeats a correctly-written record, so it is a
    // Fail; leftover records are only a Warn.
    #[test]
    fn challenge_dns_visibility_grades_answerability_then_leftovers() {
        let fqdn = "_acme-challenge.myapp.com";

        let pass =
            check_acme_dns_propagation_impl(fqdn, &ChallengeDnsVisibility::Answered { values: 0 });
        assert_eq!(pass.status, CheckStatus::Pass);

        let warn =
            check_acme_dns_propagation_impl(fqdn, &ChallengeDnsVisibility::Stale { values: 2 });
        assert_eq!(warn.status, CheckStatus::Warn);
        assert!(warn.detail.unwrap().contains('2'));

        let fail = check_acme_dns_propagation_impl(
            fqdn,
            &ChallengeDnsVisibility::Unanswerable(
                "SERVFAIL — the zone's nameservers did not answer".to_owned(),
            ),
        );
        assert_eq!(fail.status, CheckStatus::Fail);
        let detail = fail.detail.unwrap();
        assert!(
            detail.contains(fqdn),
            "the message must name the record: {detail}"
        );
        assert!(detail.contains("SERVFAIL"), "got: {detail}");
    }

    // AC7: "a `tenancy.base_domain` that the configured certificate domain does
    // not cover" — the check that catches a subdomain-per-tenant deployment with
    // a single-hostname certificate, where every tenant serves a name mismatch.
    #[test]
    fn tenancy_base_domain_must_be_covered_by_the_certificate() {
        let single = vec!["myapp.com".to_owned()];
        let covers = acme_covers_tenant_subdomains("myapp.com", &single);
        assert!(!covers, "an apex-only certificate covers no subdomain");
        let result = check_acme_tenancy_domain_impl(Some("myapp.com"), &single, covers);
        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap();
        assert!(detail.contains("myapp.com"), "got: {detail}");
        assert!(result.hint.unwrap().contains("*.<base_domain>"));

        let wildcard = vec!["myapp.com".to_owned(), "*.myapp.com".to_owned()];
        let covers = acme_covers_tenant_subdomains("myapp.com", &wildcard);
        assert!(covers);
        assert_eq!(
            check_acme_tenancy_domain_impl(Some("myapp.com"), &wildcard, covers).status,
            CheckStatus::Pass
        );

        // No tenancy configured: nothing to cover.
        assert_eq!(
            check_acme_tenancy_domain_impl(None, &single, false).status,
            CheckStatus::Pass
        );
        assert_eq!(
            check_acme_tenancy_domain_impl(Some("   "), &single, false).status,
            CheckStatus::Pass
        );
    }

    // An explicitly-listed tenant hostname is NOT coverage for tenant N+1 — the
    // probe host must be one no explicit SAN could plausibly name.
    #[test]
    fn one_explicit_tenant_san_is_not_subdomain_coverage() {
        let explicit = vec!["myapp.com".to_owned(), "tenant1.myapp.com".to_owned()];
        assert!(!acme_covers_tenant_subdomains("myapp.com", &explicit));
        // …and a wildcard for a DIFFERENT zone is not coverage either.
        assert!(!acme_covers_tenant_subdomains(
            "myapp.com",
            &["*.other.com".to_owned()]
        ));
    }

    // Regression (Codex, #1620): the port check was softened for DNS-01 but the
    // address check was not, so a wildcard deployment behind a load balancer —
    // the normal shape for this feature — still failed `--online --strict` on a
    // rule that is HTTP-01-specific by construction.
    #[test]
    fn resolving_elsewhere_is_not_a_failure_under_dns01() {
        let elsewhere = DnsPointsHere::ResolvesElsewhere {
            resolved: vec!["203.0.113.10".to_owned()],
        };

        // HTTP-01: the CA fetches the token from whatever the name resolves to,
        // so pointing elsewhere means issuance hits the wrong server.
        assert_eq!(
            check_acme_dns_for_challenge("myapp.com", &elsewhere, false).status,
            CheckStatus::Fail
        );

        // DNS-01: the CA reads a TXT record and never connects here at all.
        let dns01 = check_acme_dns_for_challenge("myapp.com", &elsewhere, true);
        assert_eq!(dns01.status, CheckStatus::Pass);
        assert!(
            dns01.detail.unwrap().contains("TXT record"),
            "the pass must say why the mismatch does not matter"
        );

        // A partial match and an unknowable local IP are the same story.
        for inconclusive in [
            DnsPointsHere::PartialMatch {
                unmatched: vec!["203.0.113.10".to_owned()],
            },
            DnsPointsHere::LocalIpsUnknown,
        ] {
            assert_eq!(
                check_acme_dns_for_challenge("myapp.com", &inconclusive, true).status,
                CheckStatus::Pass
            );
        }

        // A name that resolves nowhere is still worth a Warn: issuance would
        // succeed, but no visitor could reach the app.
        let unresolved =
            check_acme_dns_for_challenge("myapp.com", &DnsPointsHere::Unresolved, true);
        assert_eq!(unresolved.status, CheckStatus::Warn);
        assert!(unresolved.detail.unwrap().contains("visitors"));

        // Resolving here is a plain pass either way.
        for dns01 in [false, true] {
            assert_eq!(
                check_acme_dns_for_challenge("myapp.com", &DnsPointsHere::Matches, dns01).status,
                CheckStatus::Pass
            );
        }
    }

    // Under DNS-01 the CA never connects to :80, so an unreachable port 80 costs
    // only the HTTP→HTTPS redirect. Failing it would make `doctor --online
    // --strict` reject a correct wildcard deployment on a :443-only host.
    #[test]
    fn port_80_is_only_a_warning_under_dns01() {
        let http01 = check_acme_ports_for_challenge(
            "myapp.com",
            PortReachability::Refused,
            PortReachability::Open,
            false,
        );
        assert_eq!(http01.status, CheckStatus::Fail);

        let dns01 = check_acme_ports_for_challenge(
            "myapp.com",
            PortReachability::Refused,
            PortReachability::Open,
            true,
        );
        assert_eq!(dns01.status, CheckStatus::Warn);
        assert!(
            dns01.detail.unwrap().contains("redirect"),
            "the warning must say what is actually lost"
        );

        // With both ports open the grading is identical either way.
        for dns in [false, true] {
            assert_eq!(
                check_acme_ports_for_challenge(
                    "myapp.com",
                    PortReachability::Open,
                    PortReachability::Open,
                    dns
                )
                .status,
                CheckStatus::Pass
            );
        }
    }

    // A `*.myapp.com` entry has no address record of its own: probing it
    // literally would report a permanent, meaningless Warn on every wildcard
    // deployment. It is probed as the base domain it covers, deduplicated
    // against an explicitly-listed apex.
    #[test]
    fn wildcard_domains_are_probed_as_their_base_domain() {
        let mut cfg = acme_doctor_cfg(&["myapp.com", "*.myapp.com"], "ops@myapp.com");
        cfg.dns = Some(acme_dns_cfg(
            autumn_web::config::AcmeDnsProvider::Cloudflare,
        ));
        assert_eq!(
            acme_online_probe_domains(&cfg),
            vec!["myapp.com".to_owned()],
            "the apex is probed once, and the wildcard resolves to it"
        );

        // A wildcard with no explicit apex still probes the base domain.
        let cfg = acme_doctor_cfg(&["*.myapp.com"], "ops@myapp.com");
        assert_eq!(
            acme_online_probe_domains(&cfg),
            vec!["myapp.com".to_owned()]
        );

        // Non-wildcard configs are unchanged.
        let cfg = acme_doctor_cfg(&["a.example.com", "b.example.com"], "ops@example.com");
        assert_eq!(
            acme_online_probe_domains(&cfg),
            vec!["a.example.com".to_owned(), "b.example.com".to_owned()]
        );
    }

    // Regression (#1608, Codex): doctor must mirror AcmeConfig::validate(). An
    // ACME config with no/empty `domains` is REJECTED by the runtime at boot, so
    // doctor must FAIL it rather than silently pass acme_stored_cert.
    #[test]
    fn acme_config_fail_when_domains_empty() {
        let r = check_acme_config_impl(&acme_doctor_cfg(&[], "ops@example.com"))
            .expect("empty domains must be a FAIL, not Pass");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
    }

    #[test]
    fn acme_config_fail_when_contact_email_blank() {
        let r = check_acme_config_impl(&acme_doctor_cfg(&["app.example.com"], "   "))
            .expect("blank contact_email must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
    }

    #[test]
    fn acme_config_fail_when_wildcard_domain_has_no_dns_provider() {
        let r = check_acme_config_impl(&acme_doctor_cfg(&["*.example.com"], "ops@example.com"))
            .expect("a wildcard with no DNS-01 provider must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        // Names the section that fixes it, mirroring AcmeConfig::validate().
        let detail = r.detail.as_deref().unwrap_or_default();
        assert!(detail.contains("[server.tls.acme.dns]"), "got: {detail}");
        assert!(detail.contains("DNS-01"), "got: {detail}");

        // …and with the section present the same config is accepted, because
        // DNS-01 is exactly what makes a wildcard issuable (#1620).
        let mut cfg = acme_doctor_cfg(&["*.example.com"], "ops@example.com");
        cfg.dns = Some(acme_dns_cfg(
            autumn_web::config::AcmeDnsProvider::Cloudflare,
        ));
        assert!(
            check_acme_config_impl(&cfg).is_none(),
            "a wildcard WITH a DNS-01 provider is a valid config"
        );
    }

    #[test]
    fn acme_config_ok_when_valid() {
        // A valid ACME config produces no acme_config failure.
        assert!(
            check_acme_config_impl(&acme_doctor_cfg(&["app.example.com"], "ops@example.com"))
                .is_none(),
            "a valid ACME config must not raise an acme_config failure"
        );
    }

    // Regression (#1608, Codex P2): mirror `AcmeConfig::validate()` rejecting a
    // blank/whitespace-only domain entry. `domains = [""]` passes the empty-list
    // check (length 1) but the runtime then orders a cert for an empty DNS
    // identifier, so doctor must FAIL it rather than pass acme_stored_cert.
    #[test]
    fn acme_config_fail_when_domain_entry_blank() {
        let r = check_acme_config_impl(&acme_doctor_cfg(&[""], "ops@example.com"))
            .expect("a blank domain entry must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        assert!(
            r.detail.as_deref().unwrap_or_default().contains("blank"),
            "detail must mention blank entries: {:?}",
            r.detail
        );

        // Whitespace-only is rejected the same way.
        let r = check_acme_config_impl(&acme_doctor_cfg(&["   "], "ops@example.com"))
            .expect("a whitespace-only domain entry must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
    }

    // Regression (#1608, Codex P2): mirror `AcmeConfig::validate()` rejecting
    // `http_challenge_port = 0`. Port 0 binds an ephemeral OS port the HTTP-01
    // validator (always port 80) can never reach, so every issuance fails while
    // the process stays up — doctor must FAIL it.
    #[test]
    fn acme_config_fail_when_http_challenge_port_zero() {
        let mut cfg = acme_doctor_cfg(&["app.example.com"], "ops@example.com");
        cfg.http_challenge_port = 0;
        let r = check_acme_config_impl(&cfg).expect("http_challenge_port = 0 must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        assert!(
            r.detail
                .as_deref()
                .unwrap_or_default()
                .contains("http_challenge_port"),
            "detail must mention http_challenge_port: {:?}",
            r.detail
        );
    }

    // Regression (#1608, Codex P2): a malformed `http_challenge_port` must FAIL,
    // not silently fall back to 80. The runtime's typed `AcmeConfig` deserializes
    // the field as a `u16`, so an out-of-range value like `70000` or a non-integer
    // like a quoted string is rejected before boot; doctor previously swallowed the
    // parse error (`as_integer().map_or(80, |v| try_from(v).unwrap_or(80))`) and
    // graded a config the server won't start. `resolve_acme_doctor_config` now
    // records the bad value and `check_acme_config_impl` surfaces it as a FAIL.
    #[test]
    fn acme_config_fail_when_http_challenge_port_malformed() {
        // Out-of-`u16`-range integer.
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
http_challenge_port = 70000
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(
            acme.port_error.is_some(),
            "an out-of-range http_challenge_port must be recorded, not defaulted to 80"
        );
        let r = check_acme_config_impl(&acme)
            .expect("an out-of-range http_challenge_port must be a FAIL, not silently 80");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        let detail = r.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("http_challenge_port") && detail.contains("70000"),
            "detail must name the bad port value: {detail}"
        );

        // A non-integer (quoted string) is rejected the same way.
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
http_challenge_port = \"80\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(
            acme.port_error.is_some(),
            "a quoted-string http_challenge_port must be recorded, not defaulted to 80"
        );
        let r = check_acme_config_impl(&acme)
            .expect("a non-integer http_challenge_port must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
    }

    // A valid explicit `http_challenge_port` records no error and does not FAIL.
    #[test]
    fn acme_config_ok_when_http_challenge_port_valid() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
http_challenge_port = 8080
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(acme.port_error.is_none());
        assert_eq!(acme.http_challenge_port, 8080);
        assert!(
            check_acme_config_impl(&acme).is_none(),
            "a valid http_challenge_port must not raise an acme_config FAIL"
        );
    }

    // Regression (#1608, Codex P2): a `renew_before_days` >= the issued cert's
    // lifetime (~90 days for a public CA) keeps a freshly-issued cert perpetually
    // "due", so the renewal loop re-orders every tick and burns CA rate limits.
    // The runtime `AcmeConfig::validate()` rejects it, so doctor must mirror that
    // as an `acme_config` FAIL rather than silently defaulting.
    #[test]
    fn acme_config_fail_when_renew_before_days_at_or_above_cert_lifetime() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
renew_before_days = 100
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert_eq!(acme.renew_before_days, 100);
        let r = check_acme_config_impl(&acme).expect("renew_before_days >= 90 must be a FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        let detail = r.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("renew_before_days") && detail.contains("100"),
            "detail must name the bad renew_before_days value: {detail}"
        );

        // A sane sub-lifetime value must NOT FAIL.
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
renew_before_days = 30
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert_eq!(acme.renew_before_days, 30);
        assert!(
            check_acme_config_impl(&acme).is_none(),
            "a valid renew_before_days must not raise an acme_config FAIL"
        );
    }

    // Regression (#1874, item 1): a `renew_before_days` the runtime's typed
    // `AcmeConfig` cannot deserialize as a `u32` — a quoted string, a float, a
    // bool, a negative, or an out-of-`u32`-range integer — must FAIL, not silently
    // default to 30. Doctor's `as_integer()` chain treated a non-integer as
    // ABSENT, so `renew_before_days = "30"` graded Pass while the server exits at
    // boot on the same file.
    #[test]
    fn acme_config_fail_when_renew_before_days_malformed() {
        for (bad_line, needle) in [
            // The QUOTING is the bug here, so the FAIL must echo it: a wrong fix
            // that reused the `>= 90` detail would still contain a bare `30`.
            ("renew_before_days = \"30\"", "\"30\""),
            ("renew_before_days = 30.5", "30.5"),
            ("renew_before_days = true", "true"),
            ("renew_before_days = -1", "-1"),
            ("renew_before_days = 4294967296", "4294967296"),
        ] {
            let raw: toml::Table = toml::from_str(&format!(
                "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
{bad_line}
"
            ))
            .unwrap();
            let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
            assert!(
                acme.renew_before_days_error.is_some(),
                "`{bad_line}` must be recorded, not silently defaulted to 30"
            );
            let r = check_acme_config_impl(&acme)
                .unwrap_or_else(|| panic!("`{bad_line}` must be an acme_config FAIL"));
            assert!(matches!(r.status, CheckStatus::Fail));
            assert_eq!(r.name, "acme_config");
            let detail = r.detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("renew_before_days") && detail.contains(needle),
                "detail must name the bad renew_before_days value: {detail}"
            );
            // The deserialize tier, not the `>= 90` renewal-window rule: the
            // runtime never gets far enough to compare this against 90.
            assert!(
                detail.contains("is not a valid renewal window"),
                "`{bad_line}` must FAIL as a malformed value, not as a >= 90 \
                 renewal window: {detail}"
            );
        }
    }

    // Companion: a valid explicit `renew_before_days` records no error, and an
    // absent key still falls back to the runtime default (30) with no FAIL.
    #[test]
    fn acme_config_ok_when_renew_before_days_valid_or_absent() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
renew_before_days = 45
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(acme.renew_before_days_error.is_none());
        assert_eq!(acme.renew_before_days, 45);
        assert!(check_acme_config_impl(&acme).is_none());

        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(acme.renew_before_days_error.is_none());
        assert_eq!(
            acme.renew_before_days, 30,
            "absent key uses the runtime default"
        );
        assert!(check_acme_config_impl(&acme).is_none());
    }

    // Regression (#1874, item 2): doctor must mirror `AcmeConfig::validate()`'s
    // rejection of a whitespace-padded domain. Without this, `doctor --strict`
    // passes a config the runtime refuses to boot on — the same parity gap in the
    // opposite direction.
    #[test]
    fn acme_config_fail_when_domain_entry_whitespace_padded() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\" app.example.com \"]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        let r = check_acme_config_impl(&acme)
            .expect("a whitespace-padded domain must be an acme_config FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        let detail = r.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("whitespace") && detail.contains("` app.example.com `"),
            "detail must name the problem and echo the RAW padded entry (not just \
             the trimmed spelling it suggests): {detail}"
        );

        // Leading-only, trailing-only and tab/newline padding are rejected too.
        for line in [
            "domains = [\" app.example.com\"]",
            "domains = [\"app.example.com \"]",
            "domains = [\"\\tapp.example.com\\n\"]",
        ] {
            let raw: toml::Table = toml::from_str(&format!(
                "\
[server.tls.acme]
{line}
contact_email = \"ops@example.com\"
"
            ))
            .unwrap();
            let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
            let r = check_acme_config_impl(&acme)
                .unwrap_or_else(|| panic!("`{line}` must be an acme_config FAIL"));
            assert!(
                r.detail
                    .as_deref()
                    .unwrap_or_default()
                    .contains("whitespace"),
                "`{line}` must FAIL on whitespace: {:?}",
                r.detail
            );
        }

        // A padded entry is caught behind a well-formed one, and the FAIL names
        // the right index.
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\", \" www.example.com\"]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        let r = check_acme_config_impl(&acme).expect("a padded entry at index 1 must FAIL");
        assert!(
            r.detail.as_deref().unwrap_or_default().contains("index 1"),
            "detail must name the offending index: {:?}",
            r.detail
        );

        // And, as in the runtime, the padded rule does not shadow the blank or
        // wildcard rules.
        for (line, needle) in [
            ("domains = [\"   \"]", "blank entries"),
            ("domains = [\" *.example.com \"]", "wildcard"),
        ] {
            let raw: toml::Table = toml::from_str(&format!(
                "\
[server.tls.acme]
{line}
contact_email = \"ops@example.com\"
"
            ))
            .unwrap();
            let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
            let r = check_acme_config_impl(&acme).expect("must FAIL");
            let detail = r.detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains(needle),
                "`{line}` should report `{needle}`, got: {detail}"
            );
        }
    }

    // The doctor grader is a hand-copy of `AcmeConfig::validate()`, and the two have
    // drifted three times (#1608, and both items of #1874). Every other test here pins
    // one side against a hard-coded expectation; this one pins the two sides against
    // each other, so the next divergence fails a test instead of shipping. Verdicts
    // only — the messages deliberately differ in shape, since doctor splits the
    // runtime's closing advice into a separate `hint`. `ca_root_path` is excluded on
    // purpose: `validate()` grades a blank one, but doctor routes that whole story
    // through `check_acme_ca_root_impl` (see `check_acme_config_impl`'s docs).
    #[test]
    fn acme_config_grader_agrees_with_runtime_validate() {
        let cases: &[(&[&str], &str, u16, u32)] = &[
            (&["app.example.com"], "ops@example.com", 80, 30),
            (
                &["app.example.com", "www.example.com"],
                "ops@example.com",
                8080,
                89,
            ),
            // Item 2 (#1874) and its neighbours: padded, wildcard, blank.
            (&[" app.example.com "], "ops@example.com", 80, 30),
            (&["app.example.com\t"], "ops@example.com", 80, 30),
            (&["\u{a0}app.example.com"], "ops@example.com", 80, 30),
            (&[" *.example.com "], "ops@example.com", 80, 30),
            (&["   "], "ops@example.com", 80, 30),
            (
                &["app.example.com", " www.example.com"],
                "ops@example.com",
                80,
                30,
            ),
            // The pre-existing invariants.
            (&[], "ops@example.com", 80, 30),
            (&["app.example.com"], "  ", 80, 30),
            (&["app.example.com"], "ops@example.com", 0, 30),
            (&["app.example.com"], "ops@example.com", 80, 0),
            (&["app.example.com"], "ops@example.com", 80, 90),
            (&["app.example.com"], "ops@example.com", 80, 100),
            // Whitespace INSIDE a name is out of scope for both sides today; this
            // row records that they agree about it rather than that it is allowed.
            (&["app .example.com"], "ops@example.com", 80, 30),
        ];

        // Every case is run BOTH with and without a `[server.tls.acme.dns]`
        // section, because since #1620 the section is what decides whether a
        // wildcard entry is valid — so a doctor that ignored it would pass a
        // wildcard config the server refuses (or fail one it accepts).
        for (domains, contact_email, http_challenge_port, renew_before_days) in cases {
            for dns in [
                None,
                Some(acme_dns_cfg(
                    autumn_web::config::AcmeDnsProvider::Cloudflare,
                )),
                // A DNS section the runtime itself rejects: `exec` with no
                // command. Doctor must fail it too.
                Some(
                    toml::from_str::<autumn_web::config::AcmeDnsConfig>("provider = \"exec\"\n")
                        .expect("parses; validate() is what rejects it"),
                ),
            ] {
                let mut doctor_cfg = acme_doctor_cfg(domains, contact_email);
                doctor_cfg.http_challenge_port = *http_challenge_port;
                doctor_cfg.renew_before_days = *renew_before_days;
                doctor_cfg.dns = dns.clone();

                let runtime_cfg = autumn_web::config::AcmeConfig {
                    domains: domains.iter().map(|d| (*d).to_owned()).collect(),
                    contact_email: (*contact_email).to_owned(),
                    directory: autumn_web::config::AcmeDirectory::Staging,
                    cache_dir: std::path::PathBuf::from("config/acme"),
                    http_challenge_port: *http_challenge_port,
                    renew_before_days: *renew_before_days,
                    ca_root_path: None,
                    dns: dns.clone(),
                };

                assert_eq!(
                    check_acme_config_impl(&doctor_cfg).is_some(),
                    runtime_cfg.validate().is_err(),
                    "doctor and AcmeConfig::validate disagree on domains={domains:?} \
                     contact_email={contact_email:?} port={http_challenge_port} \
                     renew_before_days={renew_before_days} dns={:?} (runtime said {:?})",
                    dns.as_ref().map(|d| d.provider),
                    runtime_cfg.validate()
                );
            }
        }
    }

    // #1620's own rows for the parity test above: a wildcard is valid exactly
    // when a DNS-01 provider is configured, and a malformed one never is.
    #[test]
    fn wildcard_domain_parity_depends_on_the_dns_section() {
        let wildcard_cases: &[&[&str]] = &[
            &["*.myapp.com"],
            &["myapp.com", "*.myapp.com"],
            &["*"],
            &["*."],
            &["*myapp.com"],
            &["app.*.myapp.com"],
            &["*.*.myapp.com"],
        ];
        for domains in wildcard_cases {
            for dns in [
                None,
                Some(acme_dns_cfg(
                    autumn_web::config::AcmeDnsProvider::Cloudflare,
                )),
            ] {
                let mut doctor_cfg = acme_doctor_cfg(domains, "ops@myapp.com");
                doctor_cfg.dns = dns.clone();
                let runtime_cfg = autumn_web::config::AcmeConfig {
                    domains: domains.iter().map(|d| (*d).to_owned()).collect(),
                    contact_email: "ops@myapp.com".to_owned(),
                    directory: autumn_web::config::AcmeDirectory::Staging,
                    cache_dir: std::path::PathBuf::from("config/acme"),
                    http_challenge_port: 80,
                    renew_before_days: 30,
                    ca_root_path: None,
                    dns: dns.clone(),
                };
                assert_eq!(
                    check_acme_config_impl(&doctor_cfg).is_some(),
                    runtime_cfg.validate().is_err(),
                    "doctor and AcmeConfig::validate disagree on domains={domains:?} with \
                     dns={} (runtime said {:?})",
                    dns.is_some(),
                    runtime_cfg.validate()
                );
            }
        }
    }

    // Regression (#1608, Codex P2): a NON-STRING entry in the `domains` array
    // (`domains = ["app.example.com", 123]`) fails the runtime's typed
    // `Vec<String>` deserialization and the server won't boot, but doctor
    // previously `filter_map`ped it away — keeping the valid name and passing.
    // `resolve_acme_doctor_config` now records the bad entry and the grader FAILs.
    #[test]
    fn acme_config_fail_when_domain_entry_not_a_string() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\", 123]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(
            acme.domains_error.is_some(),
            "a non-string domain entry must be recorded, not silently dropped"
        );
        let r = check_acme_config_impl(&acme)
            .expect("a non-string domain entry must be a FAIL, not silently dropped");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        let detail = r.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("domains") && detail.contains("123"),
            "detail must name the non-string domains entry: {detail}"
        );
    }

    // Regression (#1608, Codex P2): a malformed/typo `acme.directory` must FAIL,
    // not silently default to staging. `resolve_acme_doctor_config` deserializes
    // `directory` the way the runtime does; the runtime FAILS TO BOOT on a value
    // that does not deserialize as `AcmeDirectory`, while doctor previously
    // swallowed the error (`.ok().unwrap_or_default()`) and blessed the config by
    // inspecting the (wrong) staging cache. Now the bad value surfaces as an
    // `acme_config` FAIL, folded into the same grader as the other ACME invariants.
    #[test]
    fn acme_config_fail_when_directory_invalid() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
directory = \"prod\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        // The bad value is recorded (not silently defaulted to staging).
        assert!(
            acme.directory_error.is_some(),
            "a malformed `directory` must be recorded as an error, not defaulted"
        );
        let r = check_acme_config_impl(&acme)
            .expect("an invalid `directory` must be a FAIL, not silently staging");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "acme_config");
        // The FAIL names the offending value so the operator can fix it.
        let detail = r.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("prod"),
            "detail must name the bad value: {detail}"
        );
        assert!(
            detail.contains("directory"),
            "detail must mention `directory`: {detail}"
        );
    }

    // Companion to the above: valid `directory` values (staging, production, a
    // custom directory table, and an absent key defaulting to staging) must NOT
    // raise the invalid-directory FAIL.
    #[test]
    fn acme_config_ok_for_valid_directories() {
        let cases = [
            ("directory = \"staging\"", "staging"),
            ("directory = \"production\"", "production"),
            (
                "directory = { custom = { url = \"https://acme.example.com/directory\" } }",
                "custom-",
            ),
        ];
        for (line, label_prefix) in cases {
            let raw: toml::Table = toml::from_str(&format!(
                "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
{line}
"
            ))
            .unwrap();
            let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
            assert!(
                acme.directory_error.is_none(),
                "valid directory `{line}` must not be an error"
            );
            assert!(
                acme.directory_label.starts_with(label_prefix),
                "directory label for `{line}` should start with `{label_prefix}`, got `{}`",
                acme.directory_label
            );
            assert!(
                check_acme_config_impl(&acme).is_none(),
                "valid directory `{line}` must not raise an acme_config FAIL"
            );
        }

        // Absent `directory` → runtime default (staging), no error.
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        assert!(acme.directory_error.is_none());
        assert_eq!(acme.directory_label, "staging");
    }

    // Regression (#1608, Codex): an ACME-only deployment ([server.tls.acme]
    // configured, no static cert_path/key_path) must NOT run the static
    // [server.tls] cert/key check. resolve_tls_paths() sees the enclosing
    // [server.tls] table and reports empty paths, which check_tls_impl grades
    // as a "must set both cert_path and key_path" Fail — a false positive for
    // every ACME deployment, since TlsConfig::validate() accepts ACME-only.
    #[test]
    fn acme_only_config_skips_static_tls_check() {
        // acme configured, no static cert/key present → skip the static check,
        // so the spurious "must set both" failure is never produced.
        assert!(
            !should_run_static_tls_check(true, false),
            "ACME-only deployment must skip the static [server.tls] cert/key check"
        );

        // Genuine static-TLS mode (cert/key present) is still checked, even when
        // acme is also configured.
        assert!(should_run_static_tls_check(true, true));
        assert!(should_run_static_tls_check(false, true));

        // No acme, no static cert/key: run the static check so the incomplete
        // static-TLS config still surfaces the actionable "must set both" Fail.
        assert!(should_run_static_tls_check(false, false));
    }

    // Regression (#1608, Codex): doctor must mirror `TlsConfig::validate`'s
    // static-vs-ACME XOR + half-pair rules so it FAILS exactly the combinations
    // the runtime refuses to boot, instead of blessing them under `--strict`.
    #[test]
    fn tls_mode_fails_static_and_acme_together() {
        // Both a static cert/key AND acme configured → mutually exclusive Fail,
        // even though both static paths are present (would otherwise pass).
        let r = check_tls_mode_impl(true, true, true).expect("both modes must Fail");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "tls_mode");
        assert!(r.detail.as_deref().unwrap().contains("[server.tls.acme]"));

        // A single static path plus acme is still the mutually-exclusive case
        // (any static path counts as static-configured), and previously the
        // static check was silently SKIPPED, hiding it.
        assert!(check_tls_mode_impl(true, false, true).is_some());
        assert!(check_tls_mode_impl(false, true, true).is_some());
    }

    #[test]
    fn tls_mode_fails_half_static_pair_without_acme() {
        // Exactly one of cert_path/key_path set, no acme → "must set both" Fail.
        for (has_cert, has_key) in [(true, false), (false, true)] {
            let r = check_tls_mode_impl(has_cert, has_key, false)
                .unwrap_or_else(|| panic!("half pair ({has_cert},{has_key}) must Fail"));
            assert!(matches!(r.status, CheckStatus::Fail));
            assert!(
                r.detail
                    .as_deref()
                    .unwrap()
                    .contains("must be set together")
            );
        }
    }

    #[test]
    fn tls_mode_passes_valid_single_mode_configs() {
        // Valid ACME-only: no static paths, acme configured → no mode Fail (the
        // dedicated acme checks grade it).
        assert!(
            check_tls_mode_impl(false, false, true).is_none(),
            "ACME-only is a valid single mode"
        );
        // Valid static-only: both paths, no acme → no mode Fail.
        assert!(
            check_tls_mode_impl(true, true, false).is_none(),
            "static-only with both paths is a valid single mode"
        );
        // Neither configured: TLS is optional → no mode Fail.
        assert!(check_tls_mode_impl(false, false, false).is_none());
    }

    // Regression (#1608, Codex P2): the static-vs-ACME XOR must key on static-path
    // PRESENCE, not non-emptiness. The runtime keys `TlsConfig::validate` on
    // `cert_path.is_some()`, so `[server.tls.acme]` + `cert_path = ""` is a mixed
    // static+ACME config it rejects at boot. Doctor previously collapsed the empty
    // path to "absent", treated the config as ACME-only, skipped the static check,
    // and could pass `--strict`. `static_tls_presence_from` now reports a
    // present-but-empty path as PRESENT, so the mixed-mode FAIL fires.
    #[test]
    fn empty_static_path_with_acme_is_mixed_mode_not_acme_only() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls]
cert_path = \"\"

[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let tls = raw
            .get("server")
            .and_then(toml::Value::as_table)
            .and_then(|s| s.get("tls"))
            .and_then(toml::Value::as_table);

        // The present-but-empty `cert_path` KEY counts as static-mode present.
        let (has_static_cert, has_static_key) = static_tls_presence_from(tls, false, false);
        assert!(
            has_static_cert,
            "a present-but-empty `cert_path` must count as static-mode present"
        );
        assert!(!has_static_key, "no `key_path` key is set");

        // acme is configured, so this is a mixed static+ACME config → FAIL, exactly
        // as `TlsConfig::validate` rejects it (NOT skipped as ACME-only).
        let acme = resolve_acme_doctor_config(Some(&raw)).expect("[server.tls.acme] present");
        let r = check_tls_mode_impl(has_static_cert, has_static_key, true)
            .expect("mixed static+ACME (present-but-empty cert_path) must FAIL");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "tls_mode");
        assert!(
            r.detail
                .as_deref()
                .unwrap_or_default()
                .contains("[server.tls.acme]"),
            "the FAIL must name the mutually-exclusive [server.tls.acme]"
        );
        // Sanity: acme itself resolves (the domains/email are valid).
        assert_eq!(acme.domains, ["app.example.com"]);
    }

    // Companion: an ACME-only deployment with NO `cert_path`/`key_path` key at all
    // is a valid single mode — neither static key present → no mixed-mode FAIL.
    #[test]
    fn acme_only_without_static_keys_is_valid() {
        let raw: toml::Table = toml::from_str(
            "\
[server.tls.acme]
domains = [\"app.example.com\"]
contact_email = \"ops@example.com\"
",
        )
        .unwrap();
        let tls = raw
            .get("server")
            .and_then(toml::Value::as_table)
            .and_then(|s| s.get("tls"))
            .and_then(toml::Value::as_table);

        let (has_static_cert, has_static_key) = static_tls_presence_from(tls, false, false);
        assert!(
            !has_static_cert && !has_static_key,
            "ACME-only config must report NO static cert/key keys present"
        );
        assert!(
            check_tls_mode_impl(has_static_cert, has_static_key, true).is_none(),
            "ACME-only (no static keys) is a valid single mode — no mixed-mode FAIL"
        );
    }

    // Regression (#1608, Codex P2): the static-vs-ACME XOR must read the static
    // cert/key env overrides through the SAME profile-aware dotenv overlay the
    // sibling `resolve_tls_paths()` uses — not the bare process env. A cert/key
    // supplied via `.env`/`.env.<profile>` alongside `[server.tls.acme]` is a mixed
    // static+ACME config the runtime rejects at boot; reading only the process env
    // would miss it and pass `doctor --strict`. `static_tls_env_presence_in` reads
    // the overlay (fed a `MockEnv` here, standing in for the dotenv overlay), so
    // the dotenv-supplied cert/key are seen as present and the mixed-mode FAIL fires.
    #[test]
    fn tls_mode_fails_when_dotenv_overlay_supplies_cert_key_with_acme() {
        use autumn_web::config::MockEnv;

        // cert/key present ONLY through the overlay (never TOML), one of them an
        // empty clearing override — both must count as PRESENT, mirroring the
        // runtime's `env.var(..).ok()`.
        let env = MockEnv::new()
            .with("AUTUMN_SERVER__TLS__CERT_PATH", "/dotenv/cert.pem")
            .with("AUTUMN_SERVER__TLS__KEY_PATH", "");
        let (cert_env, key_env) = static_tls_env_presence_in(&env);
        assert!(
            cert_env,
            "a dotenv-supplied CERT_PATH must be seen as present"
        );
        assert!(
            key_env,
            "a present-but-empty dotenv KEY_PATH override is still present"
        );

        // With no static cert/key in TOML but both supplied via the overlay, the
        // static keys are present — combined with acme, that is the mixed mode the
        // runtime rejects, so the tls_mode grader FAILs.
        let (has_static_cert, has_static_key) = static_tls_presence_from(None, cert_env, key_env);
        assert!(has_static_cert && has_static_key);
        let r = check_tls_mode_impl(has_static_cert, has_static_key, true)
            .expect("dotenv-supplied static cert/key + [server.tls.acme] must FAIL as mixed mode");
        assert!(matches!(r.status, CheckStatus::Fail));
        assert_eq!(r.name, "tls_mode");
        assert!(
            r.detail
                .as_deref()
                .unwrap_or_default()
                .contains("[server.tls.acme]"),
            "the FAIL must name the mutually-exclusive [server.tls.acme]"
        );

        // Sanity: with the overlay reporting NO TLS env vars, the same TOML-less
        // config is a valid ACME-only deployment (no mixed-mode FAIL).
        let empty = MockEnv::new();
        let (c, k) = static_tls_env_presence_in(&empty);
        let (hc, hk) = static_tls_presence_from(None, c, k);
        assert!(
            check_tls_mode_impl(hc, hk, true).is_none(),
            "no dotenv-supplied cert/key + acme is a valid ACME-only mode"
        );
    }

    // An env override for `cert_path` — even an empty one — counts as static-mode
    // present, mirroring the runtime applying `AUTUMN_SERVER__TLS__CERT_PATH` as
    // `Some("")`.
    #[test]
    fn static_presence_counts_empty_env_override() {
        // No [server.tls] table at all, but the cert env override is set.
        let (has_static_cert, has_static_key) = static_tls_presence_from(None, true, false);
        assert!(
            has_static_cert,
            "an env cert override must count as present"
        );
        assert!(!has_static_key);
    }

    // `[server.tls.acme]` supplied only by an active profile (`[profile.prod]`,
    // `autumn-prod.toml`) must be detected by doctor (#1608). A raw top-level table
    // misses it, so `resolve_tls_paths()` would still see the enclosing,
    // static-path-less `[server.tls]` and the static TLS check would spuriously fail a
    // valid ACME deployment. The merged runtime table sees the acme config, so doctor
    // skips the static check. This mirrors `get_merged_toml_table_runtime`'s profile
    // merge — a deep merge of `[profile.<env>]` onto the base top-level — feeding the
    // testable `resolve_acme_doctor_config` helper.
    #[test]
    fn profile_only_acme_skips_static_tls_check() {
        // [server.tls.acme] appears ONLY under [profile.prod]; the base top-level
        // [server.tls] carries no acme and no static cert/key.
        let raw: toml::Table = toml::from_str(
            "\
[server.tls]

[profile.prod.server.tls.acme]
domains = [\"app.example.com\"]
directory = \"production\"
",
        )
        .unwrap();

        // The raw top-level lookup (the buggy source) does NOT see the
        // profile-scoped acme, so it would leave the static TLS check running.
        assert!(
            resolve_acme_doctor_config(Some(&raw)).is_none(),
            "raw top-level table must miss profile-scoped [server.tls.acme]"
        );

        // Build the MERGED runtime table exactly as get_merged_toml_table_runtime
        // does for the active profile: base top-level, then deep_merge
        // [profile.prod].
        let mut merged = toml::Table::new();
        deep_merge(&mut merged, &raw);
        let prof = raw
            .get("profile")
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get("prod"))
            .and_then(toml::Value::as_table)
            .expect("[profile.prod] table");
        deep_merge(&mut merged, prof);

        // The merged table (what resolve_tls_paths and the fixed doctor path use)
        // DOES surface the acme config.
        let acme = resolve_acme_doctor_config(Some(&merged))
            .expect("merged/profile table must surface [server.tls.acme]");
        assert_eq!(acme.domains, ["app.example.com"]);
        assert_eq!(acme.directory_label, "production");

        // With acme detected and NO static cert/key present, the static
        // [server.tls] check is skipped — no spurious "must set both cert_path and
        // key_path" Fail for the profile-scoped ACME deployment.
        assert!(
            !should_run_static_tls_check(
                resolve_acme_doctor_config(Some(&merged)).is_some(),
                false,
            ),
            "profile-scoped ACME deployment must skip the static TLS check"
        );
    }

    // Regression (#1608, Codex): --online must probe EVERY configured ACME
    // domain, not just the first. Issuance orders an authorization for each name,
    // so probing only the first can pass doctor while issuance fails on an
    // unprobed domain. A 2-domain config must yield a port + DNS check per domain.
    #[test]
    fn online_probes_every_configured_domain() {
        let mut config = acme_doctor_cfg(&["ok.example.com", "bad.example.com"], "ops@example.com");
        config.directory_label = "production".to_owned();

        // Every configured domain is scheduled for probing, not just the first.
        let domains = acme_online_probe_domains(&config);
        assert_eq!(
            domains,
            vec!["ok.example.com".to_owned(), "bad.example.com".to_owned()],
            "every configured domain must be probed, not just the first"
        );

        // Each domain flows through the pure graders to a domain-labeled check,
        // mirroring the port + DNS tasks run() enqueues per domain.
        let mut ports_checks = Vec::new();
        let mut dns_checks = Vec::new();
        for domain in &domains {
            ports_checks.push(check_acme_ports_for_challenge(
                domain,
                PortReachability::Open,
                PortReachability::Open,
                false,
            ));
            dns_checks.push(check_acme_dns_for_challenge(
                domain,
                &DnsPointsHere::Matches,
                false,
            ));
        }

        assert_eq!(ports_checks.len(), 2, "one acme_ports check per domain");
        assert_eq!(dns_checks.len(), 2, "one acme_dns check per domain");
        for domain in &domains {
            assert!(
                ports_checks
                    .iter()
                    .any(|c| c.detail.as_deref().is_some_and(|d| d.contains(domain))),
                "{domain} must be represented by a labeled acme_ports check"
            );
            assert!(
                dns_checks
                    .iter()
                    .any(|c| c.detail.as_deref().is_some_and(|d| d.contains(domain))),
                "{domain} must be represented by a labeled acme_dns check"
            );
        }
    }

    // The doctor's ACME directory label must match FsAcmeStore's namespacing so
    // the stored-cert scan reads the SAME subdirectory the runtime loads.
    #[test]
    fn acme_directory_label_matches_store() {
        let label_of = |src: &str| {
            let table: toml::Table = toml::from_str(src).unwrap();
            acme_directory_label(&parse_acme_directory(&table).expect("valid directory"))
        };

        assert_eq!(label_of("directory = \"staging\""), "staging");
        assert_eq!(label_of("directory = \"production\""), "production");

        // Unset directory defaults to staging (the runtime default).
        assert_eq!(
            acme_directory_label(&parse_acme_directory(&toml::Table::new()).unwrap()),
            "staging"
        );

        // A custom directory hashes its URL, mirroring the store's `custom-<hash>`.
        let label = label_of("directory = { custom = { url = \"https://pebble.test/dir\" } }");
        assert!(label.starts_with("custom-"), "got {label}");
    }

    // Regression (#1608, Codex): doctor must inspect the cert id derived from the
    // ACTIVE `acme.domains` (what `build_acme_tls_listener` loads), NOT whichever
    // `*.chain.pem` is newest. If `domains` changed and old cert files remain, an
    // old cert for other domains must not be reported as the stored cert, and an
    // absent configured cert is a benign first-run state — not a "healthy old
    // cert" or an expired-old-cert `--strict` failure.
    #[test]
    fn acme_scan_inspects_configured_domain_cert() {
        let dir = tempfile::tempdir().unwrap();
        let label = "production";
        let cert_dir = dir.path().join(label);
        std::fs::create_dir_all(&cert_dir).unwrap();

        let configured = vec!["app.example.com".to_owned()];
        let old = vec!["old.example.com".to_owned()];
        let configured_id = acme_cert_id(&configured);
        let old_id = acme_cert_id(&old);
        assert_ne!(configured_id, old_id);

        let write_pair = |id: &str| {
            std::fs::write(cert_dir.join(format!("{id}.chain.pem")), b"chain").unwrap();
            std::fs::write(cert_dir.join(format!("{id}.key.pem")), b"key").unwrap();
        };

        // Only the OLD (different-domains) cert present: the configured-domains
        // lookup finds nothing, so the old cert is NOT reported as the stored one.
        write_pair(&old_id);
        assert!(
            configured_acme_cert_pair(dir.path(), label, &configured).is_none(),
            "a cert for different domains must not be reported as the stored cert"
        );

        // Now add the configured-domains cert alongside the old one: the lookup
        // targets EXACTLY the configured id, ignoring the old file.
        write_pair(&configured_id);
        let picked = configured_acme_cert_pair(dir.path(), label, &configured)
            .expect("configured-domains cert must be found");
        assert!(
            picked.0.to_string_lossy().contains(&configured_id),
            "must inspect the configured-domains cert, got {:?}",
            picked.0
        );
        assert!(
            !picked.0.to_string_lossy().contains(&old_id),
            "must NOT inspect the old different-domains cert, got {:?}",
            picked.0
        );
    }

    // A missing configured-domains cert (only a partial pair, or nothing) reads as
    // "not stored yet" — a benign first-run state, never a hard scan miss.
    #[test]
    fn acme_scan_absent_configured_cert_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let label = "production";
        let cert_dir = dir.path().join(label);
        std::fs::create_dir_all(&cert_dir).unwrap();
        let domains = vec!["app.example.com".to_owned()];
        let id = acme_cert_id(&domains);

        // Nothing stored yet.
        assert!(configured_acme_cert_pair(dir.path(), label, &domains).is_none());

        // Only the chain (no key) — mirrors FsAcmeStore treating a partial pair as
        // absent.
        std::fs::write(cert_dir.join(format!("{id}.chain.pem")), b"chain").unwrap();
        assert!(configured_acme_cert_pair(dir.path(), label, &domains).is_none());

        // The graded result for an absent cert is the benign "not configured yet".
        assert_eq!(
            resolve_acme_stored_cert_data(dir.path(), label, &domains),
            TlsDoctorData::NotConfigured
        );
    }

    // Codex review (#1864): a directory (or any other non-regular node)
    // occupying a cert path is a genuinely broken cache, not an absent one.
    // It must grade as Invalid (a --strict FAIL), never the benign
    // NotConfigured Pass — otherwise doctor blesses a cache that will fail to
    // load at boot.
    #[cfg(feature = "tls")]
    #[test]
    fn acme_scan_occupied_directory_is_invalid_not_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let label = "production";
        let cert_dir = dir.path().join(label);
        let domains = vec!["app.example.com".to_owned()];
        let id = acme_cert_id(&domains);

        // The chain path is occupied by a directory instead of a file; the
        // key path is a normal (if bogus) file.
        std::fs::create_dir_all(cert_dir.join(format!("{id}.chain.pem"))).unwrap();
        std::fs::write(cert_dir.join(format!("{id}.key.pem")), b"key").unwrap();

        assert!(
            configured_acme_cert_pair(dir.path(), label, &domains).is_some(),
            "an occupied path must be reported as present, not absent"
        );
        assert!(
            matches!(
                resolve_acme_stored_cert_data(dir.path(), label, &domains),
                TlsDoctorData::Invalid { .. }
            ),
            "an occupied-but-unreadable cert must FAIL --strict, not pass as NotConfigured"
        );
    }

    // Drift guard (#1608, Codex): the doctor's feature-less replica of the cert-id
    // hashing MUST stay byte-for-byte equal to the store's
    // `CertId::from_domains`. Only compiled with this crate's `tls` feature
    // (on by default — it pulls in `autumn_web::acme`; this crate's own,
    // separate `acme` feature is not what gates it, see #1864), so the two
    // derivations are compared directly.
    #[cfg(feature = "tls")]
    #[test]
    fn doctor_cert_id_matches_store() {
        let cases: &[&[&str]] = &[
            &["app.example.com"],
            &["b.example.com", "a.example.com"],
            &["a.example.com", "b.example.com", "a.example.com"],
            &["one.example.com", "two.example.net", "three.example.org"],
        ];
        for case in cases {
            let domains: Vec<String> = case.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(
                acme_cert_id(&domains),
                autumn_web::acme::store::CertId::from_domains(&domains).as_str(),
                "doctor cert-id replica drifted from CertId::from_domains for {domains:?}"
            );
        }
    }

    // ── check_alert_destination_impl (issue #1610) ───────────────────────────

    #[test]
    fn alert_destination_warns_in_production_when_none() {
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert_eq!(r.name, "alert_destination");
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(r.hint.is_some());
    }

    #[test]
    fn alert_destination_passes_in_production_with_native_transport_only() {
        // A native transport (PagerDuty routing key, or a Slack/Discord webhook)
        // is a valid destination: the runtime registers the channel and
        // `has_destination()` returns true, so doctor must NOT warn "no operator
        // alert destination" — otherwise a native-only prod config fails
        // `--strict` while it delivers fine at runtime. The only destination here
        // is the native transport (email/webhook/custom all absent).
        let r = check_alert_destination_impl(
            true,  // alerts_enabled
            "",    // email
            false, // webhook_configured
            "",    // webhook_url
            true,  // mail_transport_usable
            true,  // is_production
            "noreply@example.com",
            true,  // transport_requires_from
            false, // custom_channel
            true,  // native_transport_configured
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "a native-transport-only config must pass the destination gate"
        );
        assert!(
            !r.detail
                .as_deref()
                .unwrap_or_default()
                .contains("no operator alert destination"),
            "must not warn about a missing destination when a native transport is configured"
        );
        // Sanity: with the native flag off and nothing else configured, the same
        // production config DOES warn — proving the flag is what flips the gate.
        let off = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(off.status, CheckStatus::Warn));
    }

    #[test]
    fn native_alert_transport_configured_matches_runtime_predicate() {
        // Mirrors AlertConfig::has_destination for native transports: any of the
        // three configs present (non-empty after trim) counts.
        assert!(!native_alert_transport_configured("", "", ""));
        assert!(!native_alert_transport_configured("  ", "\t", " "));
        assert!(native_alert_transport_configured("R0UT1NGKEY", "", ""));
        assert!(native_alert_transport_configured(
            "",
            "https://hooks.slack.com/services/x",
            ""
        ));
        assert!(native_alert_transport_configured(
            "",
            "",
            "https://discord.com/api/webhooks/x/slack"
        ));
    }

    #[test]
    fn alert_destination_passes_in_production_with_email() {
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn alert_destination_passes_in_production_with_webhook() {
        let r = check_alert_destination_impl(
            true,
            "",
            true,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn alert_destination_passes_in_dev_without_destination() {
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            false,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    // ── email destination requires a usable mail transport (issue #1610) ──────
    // Runtime `alerts::install_from_config` skips `MailAlertChannel` when
    // `mailer.is_disabled()` (the disabled transport, which is the default
    // outside dev), so doctor must not count an email against a disabled
    // transport. These wire `resolve_effective_mail_transport` (the exact helper
    // the doctor mail check uses) to the destination check, mirroring `run()`.

    #[test]
    fn alert_destination_email_only_with_disabled_transport_warns_in_prod() {
        // `[alerts] email` is set, but there is no `[mail] transport` — in a prod
        // profile that resolves to the `disabled` default, so the runtime installs
        // NO email alert channel. Doctor must NOT count the email and must warn.
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(email, "the [alerts] email itself resolves as present");
        assert!(!webhook);
        // No env override, no `[mail] transport`, prod profile → "disabled".
        let transport = resolve_effective_mail_transport(None, None, "prod");
        assert_eq!(transport, "disabled");
        let usable = transport != "disabled";
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            webhook,
            "",
            usable,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "email with a disabled mail transport delivers nothing → must warn in prod"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("transport is disabled")),
            "the dedicated warning must name the disabled transport as the cause"
        );
    }

    #[test]
    fn alert_destination_email_with_usable_transport_passes_in_prod() {
        // Same email, but `[mail] transport = "smtp"` (a real, usable backend) →
        // the runtime installs the channel, so doctor counts the email and passes.
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(email, "the [alerts] email resolves as present");
        let transport = resolve_effective_mail_transport(None, Some("smtp"), "prod");
        assert_eq!(transport, "smtp");
        // SMTP requires a `from`; this happy path has one set, so it still passes.
        let requires_from = mail_transport_requires_from(&transport);
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            webhook,
            "",
            transport != "disabled",
            true,
            "noreply@example.com",
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "email with a usable (smtp) transport is a valid destination in prod"
        );
    }

    #[test]
    fn mail_transport_requires_from_only_for_smtp() {
        // Only SMTP builds an RFC 5322 message (`lettre_message`), which errors
        // without a `from`. `log`/`file` render `from` only when present and
        // deliver without it; `disabled` delivers nothing at all.
        assert!(mail_transport_requires_from("smtp"));
        assert!(!mail_transport_requires_from("log"));
        assert!(!mail_transport_requires_from("file"));
        assert!(!mail_transport_requires_from("disabled"));
    }

    // ── email destination requires a mail `from` for SMTP (issue #1610) ───────
    // The runtime's `MailAlertChannel` builds alert mail with NO per-message
    // `from`, so it falls back to the mailer default (`[mail] from`). Under
    // `transport = "smtp"` with no `[mail] from`, delivery fails in
    // autumn-web's `lettre_message` with "mail from address is required". Doctor
    // mirrors this: an SMTP email with no `from` is not a working destination.
    // `log`/`file` deliver without a `from`, so they must NOT be over-gated.

    #[test]
    fn alert_destination_email_smtp_with_from_passes_in_prod() {
        // (a) email + smtp + a `[mail] from` set → the runtime installs a channel
        // that can send, so doctor counts the email and passes in prod.
        let transport = resolve_effective_mail_transport(None, Some("smtp"), "prod");
        assert_eq!(transport, "smtp");
        let requires_from = mail_transport_requires_from(&transport);
        assert!(requires_from, "smtp must be flagged as requiring a from");
        let mail_from = "noreply@example.com";
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            true,
            mail_from,
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "email + smtp + [mail] from is a valid destination in prod"
        );
    }

    #[test]
    fn alert_destination_email_smtp_without_from_warns_in_prod() {
        // (b) email + smtp + NO `[mail] from` → SMTP delivery fails with "mail
        // from address is required", so doctor must NOT count the email and must
        // warn with the dedicated `from` message (distinct from the
        // disabled-transport and no-destination warnings).
        let transport = resolve_effective_mail_transport(None, Some("smtp"), "prod");
        let requires_from = mail_transport_requires_from(&transport);
        let mail_from = "";
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            true,
            mail_from,
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "email on smtp with no [mail] from delivers nothing → must warn in prod"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("`[mail] from` is not set")),
            "the dedicated warning must name the missing [mail] from as the cause"
        );
    }

    #[test]
    fn alert_destination_email_log_transport_without_from_still_passes() {
        // (c) email + `log` transport + NO `[mail] from` → the log transport
        // renders `from` only when present and delivers without it, so a missing
        // `from` must NOT down-grade the destination. Do not over-gate.
        let transport = resolve_effective_mail_transport(None, Some("log"), "prod");
        assert_eq!(transport, "log");
        let requires_from = mail_transport_requires_from(&transport);
        assert!(
            !requires_from,
            "log must NOT be flagged as requiring a from"
        );
        let mail_from = "";
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            true,
            mail_from,
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "email on a log transport delivers without a from → must not warn"
        );
    }

    // ── invalid `[mail] from` for SMTP (issue #1610 follow-up) ────────────────
    // A present-but-unparsable `[mail] from` on an SMTP transport makes the
    // runtime ABORT boot (`MailerBuilder::build` runs `parse_mailbox`), and the
    // alert mail — which carries no per-message `from` — has no valid default to
    // fall back to. Doctor must catch this PRE-deploy: NOT count the email and
    // surface a dedicated warning naming the value, distinct from the
    // missing-`from` case. Valid display-name senders must NOT be over-rejected,
    // and non-SMTP transports (which never parse `from`) stay lenient.

    #[test]
    fn alert_destination_email_smtp_with_invalid_from_warns_in_prod() {
        let transport = resolve_effective_mail_transport(None, Some("smtp"), "prod");
        let requires_from = mail_transport_requires_from(&transport);
        assert!(requires_from, "smtp must require a from");
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            true,
            "not-a-mailbox",
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "smtp with an unparsable [mail] from delivers nothing → must warn in prod"
        );
        let detail = r.detail.unwrap_or_default();
        assert!(
            detail.contains("`[mail] from`")
                && detail.contains("not a valid email address")
                && detail.contains("not-a-mailbox"),
            "the dedicated invalid-from warning must fire and name the value, got: {detail}"
        );
    }

    #[test]
    fn alert_destination_email_smtp_with_display_name_from_passes_in_prod() {
        // A valid RFC 5322 display-name `from` (`Ops <ops@example.com>`) is what
        // the runtime's `parse_mailbox` accepts, so doctor must accept it too and
        // still count the email destination — no over-rejection.
        let transport = resolve_effective_mail_transport(None, Some("smtp"), "prod");
        let requires_from = mail_transport_requires_from(&transport);
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            true,
            "Ops <ops@example.com>",
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "a valid display-name [mail] from is a working sender → must not warn"
        );
    }

    #[test]
    fn alert_destination_email_smtp_with_invalid_from_is_lenient_in_dev() {
        // Dev-mode leniency mirrors the rest of the check: an invalid `[mail] from`
        // is not a hard problem locally, so it must NOT warn outside production.
        let transport = resolve_effective_mail_transport(None, Some("smtp"), "prod");
        let requires_from = mail_transport_requires_from(&transport);
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            false, // dev
            "not-a-mailbox",
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "dev-mode leniency: an invalid [mail] from must not warn outside production"
        );
    }

    #[test]
    fn alert_destination_email_log_transport_with_invalid_from_still_passes() {
        // A `log` transport never parses `[mail] from`, so even a garbage value
        // must NOT down-grade the destination. Do not over-gate non-SMTP
        // transports (parity with the runtime, which requires a from only for smtp).
        let transport = resolve_effective_mail_transport(None, Some("log"), "prod");
        assert_eq!(transport, "log");
        let requires_from = mail_transport_requires_from(&transport);
        assert!(!requires_from, "log must NOT require a from");
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            false,
            "",
            transport != "disabled",
            true,
            "not-a-mailbox",
            requires_from,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "log transport ignores [mail] from → an invalid value must not warn"
        );
    }

    // ── invalid `[alerts] email` (issue #1610 follow-up) ──────────────────────
    // lettre parses the recipient only at SEND time (`lettre_message`), not when
    // the alert `Mail` is built, so a present-but-unparsable address passes every
    // earlier gate yet fails EVERY delivery at runtime with
    // `MailError::InvalidAddress`. Doctor must count such an email as no usable
    // destination and surface a dedicated warning naming the bad value.

    #[test]
    fn alert_destination_invalid_email_warns_in_prod() {
        // A non-empty but syntactically invalid address on an otherwise-usable
        // SMTP transport with a `[mail] from` set: without this gate doctor would
        // PASS, yet every runtime send fails with an invalid-address error.
        let r = check_alert_destination_impl(
            true,
            "not-an-address",
            false,
            "",
            true,                  // usable transport
            true,                  // production
            "noreply@example.com", // [mail] from present
            true,                  // smtp requires from
            false,                 // custom_channel
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "an invalid alert email delivers nothing at runtime → must warn in prod"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("not a valid email address")),
            "the dedicated warning must name the invalid address as the cause"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("not-an-address")),
            "the warning should echo the offending value"
        );
    }

    #[test]
    fn alert_destination_invalid_email_is_lenient_in_dev() {
        // Dev-mode leniency mirrors the rest of the check: an invalid email is not
        // a hard problem locally, so it must NOT warn outside production.
        let r = check_alert_destination_impl(
            true,
            "not-an-address",
            false,
            "",
            true,
            false, // dev
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "dev-mode leniency: an invalid alert email must not warn outside production"
        );
    }

    #[test]
    fn alert_destination_valid_plus_addressing_passes_in_prod() {
        // A perfectly valid address using `+` sub-addressing and a multi-label
        // domain must NOT be rejected — the validity check must not over-reject.
        let r = check_alert_destination_impl(
            true,
            "a+b@sub.example.co",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "a valid `+`-addressed, multi-label-domain email is a working destination"
        );
    }

    #[test]
    fn alert_destination_mailto_uri_email_warns_in_prod() {
        // `[alerts] email = "mailto:oncall@example.com"` is handed verbatim to
        // lettre, which rejects the `mailto:` URI as a bare recipient — so every
        // send fails at runtime. A prior fix used the unsubscribe-mailto validator
        // (which strips `mailto:` and returned true), letting doctor PASS a config
        // that can never deliver. The bare-mailbox check must reject it and surface
        // the dedicated invalid-email warning.
        let r = check_alert_destination_impl(
            true,
            "mailto:oncall@example.com",
            false,
            "",
            true,                  // usable transport
            true,                  // production
            "noreply@example.com", // [mail] from present
            true,                  // smtp requires from
            false,                 // custom_channel
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "a mailto: URI alert email delivers nothing at runtime → must warn in prod"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("not a valid email address")),
            "the dedicated invalid-email warning must fire for the mailto: URI form"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("mailto:oncall@example.com")),
            "the warning should echo the offending value"
        );
    }

    #[test]
    fn alert_destination_invalid_email_not_a_destination_when_alerts_disabled() {
        // With alerting disabled, an invalid email is not a (real) destination, so
        // the disabled-switch warning must NOT claim a destination is configured.
        let r = check_alert_destination_impl(
            false, // alerting disabled
            "not-an-address",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("alerting is disabled")
                    && !d.contains("even though a destination is configured")),
            "an invalid email must not be counted as a configured destination"
        );
    }

    #[test]
    fn alert_destination_webhook_survives_disabled_transport() {
        // The prod profile clears the email, but a signed webhook is present. The
        // webhook path is independent of the mailer, so a disabled mail transport
        // must not down-grade an otherwise-valid webhook destination.
        let table: toml::Table = toml::from_str(
            "[alerts]\nemail = \"ops@example.com\"\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n[profile.prod.alerts]\nemail = \"\"\n",
        )
        .unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!email, "the prod profile cleared the email");
        assert!(webhook, "the signed webhook remains configured");
        // Mail transport disabled — irrelevant to the webhook path.
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "",
            false,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "a signed webhook is a valid destination regardless of mail transport"
        );
    }

    // ── resolve_alert_destination_from_sources precedence (issue #1610) ───────

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn resolve_alert_destination_base_email_is_detected() {
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"dev@example.com\"\n").unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(email, "base [alerts] email should resolve as configured");
        assert!(!webhook);
    }

    #[test]
    fn resolve_alert_destination_prod_profile_clears_base_email() {
        // Base sets an email; the prod profile explicitly clears it with an empty
        // string. A production run must resolve to "no destination" so doctor warns.
        let table: toml::Table = toml::from_str(
            "[alerts]\nemail = \"dev@example.com\"\n[profile.prod.alerts]\nemail = \"\"\n",
        )
        .unwrap();
        let (email, _webhook) =
            resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(
            !email,
            "an empty prod-profile override must CLEAR the base email, not OR it back in"
        );

        // And the surfaced doctor check warns in production for this config.
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn resolve_alert_destination_dev_profile_does_not_clear_base_for_prod_run() {
        // A dev-profile override must not affect a prod run: only the active
        // profile's layer participates.
        let table: toml::Table = toml::from_str(
            "[alerts]\nemail = \"dev@example.com\"\n[profile.dev.alerts]\nemail = \"\"\n",
        )
        .unwrap();
        let (email, _webhook) =
            resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(email, "dev-profile clear must not affect a prod run");
    }

    #[test]
    fn resolve_alert_destination_empty_env_clears_base_email() {
        // An explicitly-empty AUTUMN_ALERTS__EMAIL is the highest-priority layer
        // and must CLEAR a base value; an unset webhook var falls through to base.
        // The base carries BOTH a webhook_url and a webhook_secret so the webhook
        // is a real (signed) destination.
        let table: toml::Table = toml::from_str(
            "[alerts]\nemail = \"dev@example.com\"\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let env = |key: &str| (key == "AUTUMN_ALERTS__EMAIL").then(String::new);
        let (email, webhook) = resolve_alert_destination_from_sources(env, Some(&table), "prod");
        assert!(
            !email,
            "empty AUTUMN_ALERTS__EMAIL must clear the base email"
        );
        assert!(webhook, "unset webhook env should fall through to base");
    }

    #[test]
    fn resolve_alert_destination_nonempty_env_wins_over_absent_toml() {
        // Env supplies both the webhook URL and its signing secret, so the webhook
        // resolves as a real signed destination with no TOML present.
        let env = |key: &str| match key {
            "AUTUMN_ALERTS__WEBHOOK_URL" => Some("https://hooks.example/y".to_owned()),
            "AUTUMN_ALERTS__WEBHOOK_SECRET" => Some("s3cr3t".to_owned()),
            _ => None,
        };
        let (email, webhook) = resolve_alert_destination_from_sources(env, None, "prod");
        assert!(!email);
        assert!(
            webhook,
            "env webhook should resolve as configured with no TOML"
        );
    }

    #[test]
    fn resolve_alert_destination_webhook_requires_secret() {
        // (a) URL + secret both set at base → webhook is a real destination and a
        // production run passes.
        let table: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!email);
        assert!(
            webhook,
            "webhook_url + webhook_secret must count as configured"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn resolve_alert_destination_webhook_url_without_secret_is_not_configured() {
        // (b) URL set but secret MISSING → runtime installs no (unsigned) channel,
        // so doctor must NOT count it and must warn in production.
        let table: toml::Table =
            toml::from_str("[alerts]\nwebhook_url = \"https://hooks.example/x\"\n").unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!email);
        assert!(
            !webhook,
            "a webhook_url with no signing secret is not a destination"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "URL-without-secret in production must warn"
        );
    }

    #[test]
    fn resolve_alert_destination_prod_profile_clears_webhook_secret() {
        // (c) URL + secret set at base, but the prod profile CLEARS the secret with
        // an empty string → the higher (profile) layer wins, so the webhook is not
        // a signed destination and doctor warns in production.
        let table: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n[profile.prod.alerts]\nwebhook_secret = \"\"\n",
        )
        .unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!email);
        assert!(
            !webhook,
            "a prod-profile empty webhook_secret must clear the base secret"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));

        // And an empty AUTUMN_ALERTS__WEBHOOK_SECRET env var clears it just the same.
        let env = |key: &str| (key == "AUTUMN_ALERTS__WEBHOOK_SECRET").then(String::new);
        let (_email, webhook_env) =
            resolve_alert_destination_from_sources(env, Some(&table), "prod");
        assert!(
            !webhook_env,
            "an empty AUTUMN_ALERTS__WEBHOOK_SECRET must clear the secret"
        );
    }

    #[test]
    fn resolve_alert_destination_absolute_webhook_url_with_secret_passes() {
        // An absolute https URL + secret is a real destination: the runtime's HTTP
        // client dispatches it and the signed POST is deliverable → prod passes.
        let table: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!email);
        assert!(webhook, "absolute https webhook_url + secret is configured");
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "https://hooks.example/x",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn resolve_alert_destination_absolute_http_webhook_url_with_secret_passes() {
        // Runtime treats `http://…` as absolute too (it does not require TLS), so
        // doctor must accept it — rejecting it would be a false-positive warning.
        let table: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"http://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let (_email, webhook) =
            resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(
            webhook,
            "an absolute http:// webhook_url + secret must count, matching runtime"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "http://hooks.example/x",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn resolve_alert_destination_relative_webhook_url_is_not_configured_and_warns() {
        // A relative `webhook_url` (no scheme) + secret: the runtime's HTTP client
        // only dispatches URLs starting with http(s)://, and an alert webhook has
        // no base-url alias to resolve a relative value against, so EVERY signed
        // POST fails. doctor must NOT count it and must emit its DEDICATED warning
        // naming the value in production — not the generic "no destination" one.
        let table: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!email);
        assert!(
            !webhook,
            "a relative webhook_url is not an absolute http(s) URL, so it is not a destination"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "hooks.example/x",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "a relative webhook_url in production must warn"
        );
        let detail = r.detail.unwrap_or_default();
        assert!(
            detail.contains("not an absolute http(s) URL") && detail.contains("hooks.example/x"),
            "the warning must name the offending value and its cause, got: {detail}"
        );
    }

    #[test]
    fn alert_destination_relative_webhook_url_is_lenient_in_dev() {
        // Outside production the check stays lenient — a malformed webhook URL is
        // not fatal locally, so it passes quietly like the rest of this check.
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "hooks.example/x",
            true,
            false, // dev
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    // ── merged-config view (issue #1610) ──────────────────────────────────────
    // The wrapper now resolves against the fully-merged effective config the
    // runtime boots with — base `autumn.toml` layered with `[profile.{name}]` and
    // the `autumn-{profile}.toml` override file, alias-normalized — reading the
    // flattened top-level `[alerts]` with an EMPTY profile. These tests build the
    // merged table with the same `deep_merge` the runtime loader uses (the doctor
    // bin deliberately avoids process-global cwd mutation, so the file merge is
    // exercised through its merge primitive rather than real on-disk files).

    #[test]
    fn normalize_alert_profile_resolves_aliases() {
        // Mirrors `autumn_web::config::normalize_profile_name`'s alias table so
        // doctor merges the same profile sources the app will boot with.
        assert_eq!(normalize_alert_profile("production"), "prod");
        assert_eq!(normalize_alert_profile("PRODUCTION"), "prod");
        assert_eq!(normalize_alert_profile("  prod "), "prod");
        assert_eq!(normalize_alert_profile("development"), "dev");
        assert_eq!(normalize_alert_profile("DEV"), "dev");
        // Custom profiles keep their operator-specified spelling; blank stays blank.
        assert_eq!(normalize_alert_profile("staging"), "staging");
        assert_eq!(normalize_alert_profile(""), "");
        assert_eq!(normalize_alert_profile("   "), "");
    }

    #[test]
    fn resolve_alert_destination_merged_override_file_clears_base_email() {
        // Base `autumn.toml` sets the only alert channel; an `autumn-prod.toml`
        // override file CLEARS it with an empty string. `deep_merge` flattens the
        // override over the base into the top-level `[alerts]`, exactly as
        // `get_merged_toml_table_runtime` produces. Resolving against this flattened
        // view with an EMPTY profile must see NO destination → doctor warns in prod.
        let mut merged: toml::Table =
            toml::from_str("[alerts]\nemail = \"dev@example.com\"\n").unwrap();
        let override_file: toml::Table = toml::from_str("[alerts]\nemail = \"\"\n").unwrap();
        deep_merge(&mut merged, &override_file);

        let (email, _webhook) = resolve_alert_destination_from_sources(no_env, Some(&merged), "");
        assert!(
            !email,
            "an autumn-prod.toml that clears [alerts].email must resolve as no destination"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "a production deploy whose override file cleared the only alert channel must WARN"
        );
    }

    #[test]
    fn resolve_alert_destination_merged_ignores_stale_profile_section() {
        // The merged table still carries the ORIGINAL raw `[profile.prod.alerts]`
        // (the base is deep-merged wholesale), but the override file already
        // flattened the effective value onto the top-level `[alerts]`. Resolving
        // with an EMPTY profile must read the flattened top-level, NOT the stale
        // `[profile.prod.alerts]` — otherwise a non-empty stale section would
        // spuriously mask a file-cleared destination and re-open the #1610 hole.
        let mut merged: toml::Table = toml::from_str(
            "[alerts]\nemail = \"dev@example.com\"\n[profile.prod.alerts]\nemail = \"ops@example.com\"\n",
        )
        .unwrap();
        // autumn-prod.toml clears email; deep_merge flattens it onto top-level [alerts].
        let override_file: toml::Table = toml::from_str("[alerts]\nemail = \"\"\n").unwrap();
        deep_merge(&mut merged, &override_file);

        let (email, _webhook) = resolve_alert_destination_from_sources(no_env, Some(&merged), "");
        assert!(
            !email,
            "the flattened top-level [alerts] wins; a stale [profile.prod.alerts] must not mask a cleared destination"
        );
    }

    #[test]
    fn resolve_alert_destination_merged_base_email_survives_without_override() {
        // No override / profile clear: the base email flows through the merged view
        // and a production run passes.
        let merged: toml::Table =
            toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&merged), "");
        assert!(email);
        assert!(!webhook);
        let r = check_alert_destination_impl(
            true,
            "ops@example.com",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn resolve_alert_destination_merged_webhook_requires_both_url_and_secret() {
        // Base configures a signed webhook (url + secret); the override file clears
        // the SECRET. Through the flattened merged view the webhook is no longer a
        // signed destination, so doctor must not count it and must warn in prod.
        let mut merged: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let override_file: toml::Table =
            toml::from_str("[alerts]\nwebhook_secret = \"\"\n").unwrap();
        deep_merge(&mut merged, &override_file);

        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&merged), "");
        assert!(!email);
        assert!(
            !webhook,
            "a webhook whose secret was cleared by the override file is not a signed destination"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));

        // With both left intact through the merged view, the webhook still counts.
        let intact: toml::Table = toml::from_str(
            "[alerts]\nwebhook_url = \"https://hooks.example/x\"\nwebhook_secret = \"s3cr3t\"\n",
        )
        .unwrap();
        let (_e, webhook_ok) = resolve_alert_destination_from_sources(no_env, Some(&intact), "");
        assert!(
            webhook_ok,
            "url + secret through the merged view is a signed destination"
        );
    }

    // ── [alerts] enabled master switch (issue #1610 follow-up) ────────────────
    // The runtime `alerts::install_from_config` returns BEFORE installing any
    // Alerter when `config.enabled` is false, so a prod deploy with alerting
    // disabled delivers nothing even with a destination configured. Doctor must
    // honour the same master switch (env > profile > base > default `true`).

    #[test]
    fn resolve_alert_enabled_defaults_true_when_unset() {
        // No `enabled` key anywhere → the runtime default (`true`) applies.
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(no_env, Some(&table), "prod");
        assert!(enabled);
        // Nothing configured at all still defaults to enabled.
        assert!(resolve_alert_enabled_from_sources(no_env, None, "prod"));
    }

    #[test]
    fn resolve_alert_enabled_base_false_is_detected() {
        let table: toml::Table =
            toml::from_str("[alerts]\nenabled = false\nemail = \"ops@example.com\"\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(no_env, Some(&table), "prod");
        assert!(!enabled);
    }

    #[test]
    fn resolve_alert_enabled_env_true_overrides_base_false() {
        // Env parsing mirrors runtime `parse_env_bool`: "true"/"1" enable.
        let env = |k: &str| (k == "AUTUMN_ALERTS__ENABLED").then(|| "true".to_owned());
        let table: toml::Table = toml::from_str("[alerts]\nenabled = false\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(env, Some(&table), "prod");
        assert!(enabled);
        let env1 = |k: &str| (k == "AUTUMN_ALERTS__ENABLED").then(|| "1".to_owned());
        let enabled_one = resolve_alert_enabled_from_sources(env1, Some(&table), "prod");
        assert!(enabled_one);
    }

    #[test]
    fn resolve_alert_enabled_invalid_env_falls_through_to_toml() {
        // A non-boolean env value is ignored (mirrors runtime), so the base
        // `enabled = false` still wins.
        let env = |k: &str| (k == "AUTUMN_ALERTS__ENABLED").then(|| "yes".to_owned());
        let table: toml::Table = toml::from_str("[alerts]\nenabled = false\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(env, Some(&table), "prod");
        assert!(!enabled);
    }

    #[test]
    fn resolve_alert_enabled_profile_false_overrides_base_true() {
        // A profile-specific `enabled = false` disables under that profile while
        // the base (empty-profile resolution) stays enabled.
        let table: toml::Table = toml::from_str(
            "[alerts]\nemail = \"ops@example.com\"\n[profile.prod.alerts]\nenabled = false\n",
        )
        .unwrap();
        let under_profile = resolve_alert_enabled_from_sources(no_env, Some(&table), "prod");
        assert!(!under_profile);
        let under_base = resolve_alert_enabled_from_sources(no_env, Some(&table), "");
        assert!(under_base);
    }

    #[test]
    fn alert_disabled_at_base_in_prod_with_email_warns() {
        // (a) `[alerts] enabled = false` at base, email set, production run → warn.
        let table: toml::Table =
            toml::from_str("[alerts]\nenabled = false\nemail = \"ops@example.com\"\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(no_env, Some(&table), "prod");
        let (email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!enabled);
        assert!(email, "the email itself is still configured");
        let r = check_alert_destination_impl(
            enabled,
            "ops@example.com",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "disabled alerting with a configured destination must warn in prod"
        );
        assert!(
            r.detail.as_deref().is_some_and(|d| d.contains("disabled")),
            "the warning must name the disabled master switch"
        );
    }

    #[test]
    fn alert_disabled_via_env_in_prod_warns() {
        // (b) `AUTUMN_ALERTS__ENABLED=false` env override in production → warn,
        // even though the base leaves alerting enabled with an email set.
        let env = |k: &str| (k == "AUTUMN_ALERTS__ENABLED").then(|| "false".to_owned());
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(env, Some(&table), "prod");
        let (_email, webhook) =
            resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(!enabled);
        let r = check_alert_destination_impl(
            enabled,
            "ops@example.com",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn alert_enabled_default_true_preserves_existing_prod_pass() {
        // (c) `enabled` unset → defaults true; a configured email + usable
        // transport in prod still passes exactly as before this change.
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(no_env, Some(&table), "prod");
        let (_email, webhook) =
            resolve_alert_destination_from_sources(no_env, Some(&table), "prod");
        assert!(enabled);
        let r = check_alert_destination_impl(
            enabled,
            "ops@example.com",
            webhook,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn alert_disabled_in_dev_does_not_warn() {
        // (d) Disabled in DEV stays lenient (no warning) like the rest of the check.
        let table: toml::Table =
            toml::from_str("[alerts]\nenabled = false\nemail = \"ops@example.com\"\n").unwrap();
        let enabled = resolve_alert_enabled_from_sources(no_env, Some(&table), "dev");
        let (_email, webhook) = resolve_alert_destination_from_sources(no_env, Some(&table), "dev");
        assert!(!enabled);
        let r = check_alert_destination_impl(
            enabled,
            "ops@example.com",
            webhook,
            "",
            true,
            false,
            "noreply@example.com",
            true,
            false,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "dev-mode leniency: disabled alerting must not warn outside production"
        );
    }

    // ── [alerts] custom_channel declaration (issue #1610 follow-up) ───────────
    // An app that delivers alerts ONLY via `AppBuilder::with_alert_channel`
    // (code, not config) still triggered doctor's no-destination Warn, which
    // `--strict` turns into exit 1. `[alerts] custom_channel = true` lets the
    // operator DECLARE the code-registered channel so the pure no-destination
    // fall-through passes — while a *broken configured* destination still warns.

    #[test]
    fn alert_destination_custom_channel_passes_in_prod_without_destination() {
        // custom_channel = true + no email/webhook in prod → PASS (not Warn), so
        // `autumn doctor --strict` succeeds for a code-only alert setup.
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            /* custom_channel */ true,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Pass),
            "custom_channel = true declares a code-registered channel → no-destination must pass"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("custom_channel")),
            "the pass detail should name the custom_channel declaration"
        );
    }

    #[test]
    fn alert_destination_without_custom_channel_still_warns_in_prod() {
        // custom_channel = false (default) + no destination in prod → still Warn,
        // preserving the pre-existing behavior; the warning now points at the flag.
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            /* custom_channel */ false,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("custom_channel")),
            "the no-destination warning should mention the custom_channel escape hatch"
        );
    }

    #[test]
    fn alert_destination_custom_channel_does_not_mask_broken_webhook() {
        // custom_channel = true but a CONFIGURED webhook_url is relative/invalid →
        // STILL warns about the broken value. custom_channel only suppresses the
        // pure "no destination configured" fall-through, never a misconfigured one.
        let r = check_alert_destination_impl(
            true,
            "",
            false, // relative URL is not a usable webhook, so webhook_configured is false
            "not-a-url/hooks", // present but non-absolute → dedicated invalid warning
            true,
            true,
            "noreply@example.com",
            true,
            /* custom_channel */ true,
            false,
        );
        assert!(
            matches!(r.status, CheckStatus::Warn),
            "a broken configured webhook_url must still warn even with custom_channel = true"
        );
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("not an absolute")),
            "the broken-webhook warning must fire, not the custom_channel pass"
        );
    }

    #[test]
    fn resolve_alert_custom_channel_defaults_false_when_unset() {
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        assert!(!resolve_alert_custom_channel_from_sources(
            no_env,
            Some(&table),
            "prod"
        ));
        assert!(!resolve_alert_custom_channel_from_sources(
            no_env, None, "prod"
        ));
    }

    #[test]
    fn resolve_alert_custom_channel_base_true_is_detected() {
        let table: toml::Table = toml::from_str("[alerts]\ncustom_channel = true\n").unwrap();
        assert!(resolve_alert_custom_channel_from_sources(
            no_env,
            Some(&table),
            "prod"
        ));
    }

    #[test]
    fn resolve_alert_custom_channel_env_true_overrides_base_false() {
        let table: toml::Table = toml::from_str("[alerts]\ncustom_channel = false\n").unwrap();
        let env = |k: &str| (k == "AUTUMN_ALERTS__CUSTOM_CHANNEL").then(|| "true".to_owned());
        assert!(resolve_alert_custom_channel_from_sources(
            env,
            Some(&table),
            "prod"
        ));
        let env1 = |k: &str| (k == "AUTUMN_ALERTS__CUSTOM_CHANNEL").then(|| "1".to_owned());
        assert!(resolve_alert_custom_channel_from_sources(
            env1,
            Some(&table),
            "prod"
        ));
    }

    #[test]
    fn resolve_alert_custom_channel_env_resolves_and_passes() {
        // End-to-end at the resolver + check boundary: env sets the flag true, and
        // with no configured destination in prod the check passes.
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let env = |k: &str| (k == "AUTUMN_ALERTS__CUSTOM_CHANNEL").then(|| "true".to_owned());
        let custom = resolve_alert_custom_channel_from_sources(env, Some(&table), "prod");
        assert!(
            custom,
            "AUTUMN_ALERTS__CUSTOM_CHANNEL=true must resolve true"
        );
        let r = check_alert_destination_impl(
            true,
            "",
            false,
            "",
            true,
            true,
            "noreply@example.com",
            true,
            custom,
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn resolve_alert_custom_channel_profile_true_overrides_base_false() {
        let table: toml::Table = toml::from_str(
            "[alerts]\ncustom_channel = false\n[profile.prod.alerts]\ncustom_channel = true\n",
        )
        .unwrap();
        assert!(resolve_alert_custom_channel_from_sources(
            no_env,
            Some(&table),
            "prod"
        ));
        assert!(!resolve_alert_custom_channel_from_sources(
            no_env,
            Some(&table),
            ""
        ));
    }

    // ── check_alert_error_rate_threshold_impl (issue #1610) ──────────────────
    // The threshold is compared against a FRACTION in [0, 1] (rate = err/req), so
    // only (0, 1] is meaningful. A non-finite value or one > 1 makes the 5xx alert
    // never fire; a value <= 0 makes it fire on 0%-error windows. In production
    // doctor warns; outside production it passes quietly (alerts are optional).

    #[test]
    fn alert_threshold_unset_passes() {
        // No value set: the runtime uses the valid default (0.05).
        let r = check_alert_error_rate_threshold_impl("", true);
        assert!(matches!(r.status, CheckStatus::Pass));
        let r = check_alert_error_rate_threshold_impl("   ", true);
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn alert_threshold_valid_in_range_passes() {
        for v in ["0.05", "0.2", "1", "1.0", "0.001"] {
            let r = check_alert_error_rate_threshold_impl(v, true);
            assert!(
                matches!(r.status, CheckStatus::Pass),
                "{v} is a valid (0, 1] rate and must pass"
            );
        }
    }

    #[test]
    fn alert_threshold_nan_warns_in_prod() {
        let r = check_alert_error_rate_threshold_impl("nan", true);
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail.as_deref().is_some_and(|d| d.contains("nan")),
            "the warning must name the offending value"
        );
        assert!(r.hint.is_some());
    }

    #[test]
    fn alert_threshold_out_of_range_warns_in_prod() {
        // > 1: the alert can never fire. <= 0: it fires on 0%-error windows.
        for v in ["1.5", "0", "-0.1", "inf"] {
            let r = check_alert_error_rate_threshold_impl(v, true);
            assert!(
                matches!(r.status, CheckStatus::Warn),
                "{v} is out of (0, 1] and must warn in production"
            );
        }
    }

    #[test]
    fn alert_threshold_unparsable_warns_in_prod() {
        let r = check_alert_error_rate_threshold_impl("high", true);
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn alert_threshold_invalid_is_lenient_outside_prod() {
        // Dev leniency, consistent with the other alert checks: still pass.
        for v in ["nan", "1.5", "0", "-0.1"] {
            let r = check_alert_error_rate_threshold_impl(v, false);
            assert!(
                matches!(r.status, CheckStatus::Pass),
                "{v} must pass quietly outside production"
            );
        }
    }

    #[test]
    fn resolve_alert_error_rate_threshold_reads_base_float() {
        let table: toml::Table = toml::from_str("[alerts]\nerror_rate_threshold = 0.2\n").unwrap();
        let raw = resolve_alert_error_rate_threshold_from_sources(no_env, Some(&table), "prod");
        assert_eq!(raw, "0.2");
        let r = check_alert_error_rate_threshold_impl(&raw, true);
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn resolve_alert_error_rate_threshold_out_of_range_float_warns() {
        // A present, out-of-range float resolves and the check warns in prod.
        let table: toml::Table = toml::from_str("[alerts]\nerror_rate_threshold = 1.5\n").unwrap();
        let raw = resolve_alert_error_rate_threshold_from_sources(no_env, Some(&table), "prod");
        assert_eq!(raw, "1.5");
        let r = check_alert_error_rate_threshold_impl(&raw, true);
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn resolve_alert_error_rate_threshold_env_overrides_base() {
        // A present env var (even a bad one) wins over the base TOML.
        let table: toml::Table = toml::from_str("[alerts]\nerror_rate_threshold = 0.05\n").unwrap();
        let env = |k: &str| (k == "AUTUMN_ALERTS__ERROR_RATE_THRESHOLD").then(|| "nan".to_owned());
        let raw = resolve_alert_error_rate_threshold_from_sources(env, Some(&table), "prod");
        assert_eq!(raw, "nan");
        assert!(matches!(
            check_alert_error_rate_threshold_impl(&raw, true).status,
            CheckStatus::Warn
        ));
    }

    #[test]
    fn resolve_alert_error_rate_threshold_profile_overrides_base() {
        let table: toml::Table = toml::from_str(
            "[alerts]\nerror_rate_threshold = 0.05\n[profile.prod.alerts]\nerror_rate_threshold = 2.0\n",
        )
        .unwrap();
        let under_profile =
            resolve_alert_error_rate_threshold_from_sources(no_env, Some(&table), "prod");
        assert_eq!(under_profile, "2");
        let under_base = resolve_alert_error_rate_threshold_from_sources(no_env, Some(&table), "");
        assert_eq!(under_base, "0.05");
    }

    #[test]
    fn resolve_alert_error_rate_threshold_unset_is_empty() {
        let table: toml::Table = toml::from_str("[alerts]\nemail = \"ops@example.com\"\n").unwrap();
        let raw = resolve_alert_error_rate_threshold_from_sources(no_env, Some(&table), "prod");
        assert_eq!(raw, "");
        assert!(matches!(
            check_alert_error_rate_threshold_impl(&raw, true).status,
            CheckStatus::Pass
        ));
    }

    // ── check_alert_transports_impl (issue #1630) ────────────────────────────
    // A configured native transport (PagerDuty / Slack / Discord) must carry the
    // values it needs to deliver. Production warns on a malformed routing key, a
    // non-absolute pagerduty_url, or a non-absolute-https Slack/Discord URL; dev
    // passes quietly (these transports are optional locally).

    #[test]
    fn alert_transports_none_configured_passes() {
        let r = check_alert_transports_impl("", "", "", "", true);
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn alert_transports_all_valid_pass() {
        let r = check_alert_transports_impl(
            "R0UT1NGKEY0000000000000000000000",
            "https://events.pagerduty.com/v2/enqueue",
            "https://hooks.slack.com/services/T/B/x",
            "https://discord.com/api/webhooks/1/abc/slack",
            true,
        );
        assert!(matches!(r.status, CheckStatus::Pass), "{:?}", r.detail);
        assert_eq!(r.name, "alert_transports");
    }

    #[test]
    fn alert_transports_malformed_routing_key_warns_in_prod() {
        let r = check_alert_transports_impl("bad key with spaces", "", "", "", true);
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("routing key")),
            "must name the malformed routing key"
        );
    }

    #[test]
    fn alert_transports_pagerduty_url_without_key_warns() {
        let r = check_alert_transports_impl(
            "",
            "https://events.pagerduty.com/v2/enqueue",
            "",
            "",
            true,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("routing_key"))
        );
    }

    #[test]
    fn alert_transports_non_absolute_pagerduty_url_warns() {
        let r = check_alert_transports_impl(
            "R0UT1NGKEY",
            "events.pagerduty.com/v2/enqueue",
            "",
            "",
            true,
        );
        assert!(matches!(r.status, CheckStatus::Warn));
    }

    #[test]
    fn alert_transports_non_https_slack_url_warns() {
        // Plaintext http is rejected — Slack only exposes https endpoints.
        let r = check_alert_transports_impl("", "", "http://hooks.slack.com/services/x", "", true);
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("slack_webhook_url"))
        );
    }

    #[test]
    fn alert_transports_relative_discord_url_warns() {
        let r = check_alert_transports_impl("", "", "", "/webhooks/x/slack", true);
        assert!(matches!(r.status, CheckStatus::Warn));
        assert!(
            r.detail
                .as_deref()
                .is_some_and(|d| d.contains("discord_webhook_url"))
        );
    }

    #[test]
    fn alert_transports_invalid_is_lenient_outside_prod() {
        // Every invalid case passes quietly in dev, like the other alert checks.
        let r = check_alert_transports_impl(
            "bad key",
            "not-a-url",
            "http://hooks.slack.com/x",
            "/relative",
            false,
        );
        assert!(matches!(r.status, CheckStatus::Pass));
    }

    #[test]
    fn is_absolute_https_url_doctor_requires_https_and_host() {
        assert!(is_absolute_https_url_doctor(
            "https://hooks.slack.com/services/x"
        ));
        assert!(!is_absolute_https_url_doctor(
            "http://hooks.slack.com/services/x"
        ));
        assert!(!is_absolute_https_url_doctor("hooks.slack.com/x"));
        assert!(!is_absolute_https_url_doctor("https://"));
        assert!(!is_absolute_https_url_doctor(""));
    }

    // ── compute_summary ──────────────────────────────────────────────────────

    #[test]
    fn compute_summary_all_pass() {
        let results = vec![
            CheckResult {
                name: "a",
                status: CheckStatus::Pass,
                detail: None,
                hint: None,
            },
            CheckResult {
                name: "b",
                status: CheckStatus::Pass,
                detail: None,
                hint: None,
            },
        ];
        let s = compute_summary(&results);
        assert_eq!(s.passed, 2);
        assert_eq!(s.warned, 0);
        assert_eq!(s.failed, 0);
    }

    #[test]
    fn compute_summary_mixed() {
        let results = vec![
            CheckResult {
                name: "a",
                status: CheckStatus::Pass,
                detail: None,
                hint: None,
            },
            CheckResult {
                name: "b",
                status: CheckStatus::Warn,
                detail: None,
                hint: None,
            },
            CheckResult {
                name: "c",
                status: CheckStatus::Fail,
                detail: None,
                hint: None,
            },
        ];
        let s = compute_summary(&results);
        assert_eq!(s.passed, 1);
        assert_eq!(s.warned, 1);
        assert_eq!(s.failed, 1);
    }

    #[test]
    fn compute_summary_empty() {
        let s = compute_summary(&[]);
        assert_eq!(s.passed, 0);
        assert_eq!(s.warned, 0);
        assert_eq!(s.failed, 0);
    }

    // ── exit_code ────────────────────────────────────────────────────────────

    #[test]
    fn exit_code_no_failures() {
        let s = Summary {
            passed: 3,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&s, false), 0);
    }

    #[test]
    fn exit_code_with_failure() {
        let s = Summary {
            passed: 2,
            warned: 0,
            failed: 1,
        };
        assert_eq!(exit_code(&s, false), 1);
    }

    #[test]
    fn exit_code_warn_non_strict() {
        let s = Summary {
            passed: 2,
            warned: 1,
            failed: 0,
        };
        assert_eq!(exit_code(&s, false), 0);
    }

    #[test]
    fn exit_code_warn_strict() {
        let s = Summary {
            passed: 2,
            warned: 1,
            failed: 0,
        };
        assert_eq!(exit_code(&s, true), 1);
    }

    #[test]
    fn exit_code_zero_when_all_pass_strict() {
        let s = Summary {
            passed: 5,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&s, true), 0);
    }

    // ── format_check_line ────────────────────────────────────────────────────

    #[test]
    fn format_check_line_pass_contains_glyph_and_name() {
        let r = CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Pass,
            detail: Some("1.88.0".into()),
            hint: None,
        };
        let line = format_check_line(&r);
        assert!(line.contains("✅"));
        assert!(line.contains("rust_toolchain"));
    }

    #[test]
    fn format_check_line_fail_includes_hint() {
        let r = CheckResult {
            name: "port_bindable",
            status: CheckStatus::Fail,
            detail: Some("port 3000 in use".into()),
            hint: Some("Kill the process using port 3000 or change server.port in autumn.toml"),
        };
        let line = format_check_line(&r);
        assert!(line.contains("❌"));
        assert!(line.contains("port_bindable"));
        assert!(line.contains("Kill the process"));
    }

    #[test]
    fn format_check_line_pass_omits_hint() {
        let r = CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Pass,
            detail: None,
            hint: Some("some hint that should not appear on pass"),
        };
        let line = format_check_line(&r);
        assert!(!line.contains("some hint"));
    }

    #[test]
    fn format_check_line_warn_includes_hint() {
        let r = CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some("patch skew".into()),
            hint: Some("Update your dependency"),
        };
        let line = format_check_line(&r);
        assert!(line.contains("⚠️"));
        assert!(line.contains("Update your dependency"));
    }

    // ── format_summary_line ──────────────────────────────────────────────────

    #[test]
    fn format_summary_all_pass() {
        let s = Summary {
            passed: 7,
            warned: 0,
            failed: 0,
        };
        let line = format_summary_line(&s, 0);
        assert!(line.contains("7 passed"));
        assert!(line.contains("0 warnings"));
        assert!(line.contains("0 failed"));
        assert!(line.contains("all clear"));
    }

    #[test]
    fn format_summary_with_failure() {
        let s = Summary {
            passed: 5,
            warned: 1,
            failed: 1,
        };
        let line = format_summary_line(&s, 1);
        assert!(line.contains("5 passed"));
        assert!(line.contains("1 warning"));
        assert!(line.contains("1 failed"));
        assert!(line.contains("problems found"));
    }

    #[test]
    fn format_summary_singular_warning_label() {
        let s = Summary {
            passed: 3,
            warned: 1,
            failed: 0,
        };
        let line = format_summary_line(&s, 0);
        assert!(line.contains("1 warning,"));
    }

    #[test]
    fn format_summary_plural_warning_label() {
        let s = Summary {
            passed: 1,
            warned: 2,
            failed: 0,
        };
        let line = format_summary_line(&s, 0);
        assert!(line.contains("2 warnings,"));
    }

    // ── check_toml_content ───────────────────────────────────────────────────

    #[test]
    fn check_toml_content_valid() {
        let content = r#"
[server]
port = 3000

[database]
url = "postgres://localhost/mydb"
"#;
        let r = check_toml_content(content);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_toml_content_syntax_error() {
        let content = "[[[[invalid toml";
        let r = check_toml_content(content);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_toml_content_unknown_key_warns() {
        let content = r#"
[server]
port = 3000

[totally_unknown_section]
foo = "bar"
"#;
        let r = check_toml_content(content);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.detail
                .as_deref()
                .unwrap_or("")
                .contains("totally_unknown_section"),
            "detail should mention the unknown key"
        );
    }

    #[test]
    fn check_toml_content_empty_is_pass() {
        let r = check_toml_content("");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    // `KNOWN_TOML_SECTIONS` lists `mail`, but autumn-web's config schema
    // (`get_schema_keys`) drops the `[mail]` section under the sqlite backend-flip
    // build (mail feature off), so this static-list-vs-runtime-schema parity check
    // is gated to the default (Postgres) build where `[mail]` is compiled in.
    #[cfg(feature = "postgres")]
    #[test]
    fn check_toml_content_all_known_sections_pass() {
        let content: String = KNOWN_TOML_SECTIONS
            .iter()
            .flat_map(|&s| ["[", s, "]\n"])
            .collect();
        let r = check_toml_content(&content);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "Test failed with result: {r:?}"
        );
    }

    // ── check_version_compat ─────────────────────────────────────────────────

    #[test]
    fn check_version_compat_matching() {
        let r = check_version_compat("0.3.0", "0.3.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_version_compat_minor_skew_fails() {
        let r = check_version_compat("0.3.0", "0.4.0");
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_version_compat_patch_skew_warns() {
        let r = check_version_compat("0.3.0", "0.3.1");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn check_version_compat_caret_requirement_stripped() {
        let r = check_version_compat("0.3.0", "^0.3");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_version_compat_tilde_requirement_stripped() {
        let r = check_version_compat("0.3.0", "~0.3.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_version_compat_exact_requirement_stripped() {
        let r = check_version_compat("0.3.0", "=0.3.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    // ── check_port_bindable_impl ─────────────────────────────────────────────

    #[test]
    fn check_port_bindable_impl_success() {
        let r = check_port_bindable_impl(3000, |_port| true);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.as_deref().unwrap_or("").contains("available"));
    }

    #[test]
    fn check_port_bindable_impl_failure() {
        let r = check_port_bindable_impl(3000, |_port| false);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
        assert!(r.detail.as_deref().unwrap_or("").contains("in use"));
    }

    #[test]
    fn check_port_bindable_impl_reports_correct_port() {
        let r = check_port_bindable_impl(8080, |_| true);
        assert!(r.detail.as_deref().unwrap_or("").contains("8080"));
    }

    // ── check_rust_toolchain_impl ────────────────────────────────────────────

    #[test]
    fn check_rust_toolchain_impl_pass() {
        let r = check_rust_toolchain_impl("rustc 1.88.0 (abc123 2026-04-01)", "1.88.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_rust_toolchain_impl_fail() {
        let r = check_rust_toolchain_impl("rustc 1.80.0 (abc123 2025-01-01)", "1.88.0");
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_rust_toolchain_impl_newer_passes() {
        let r = check_rust_toolchain_impl("rustc 1.90.0 (abc 2026-01-01)", "1.88.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_rust_toolchain_impl_exact_msrv_passes() {
        let r = check_rust_toolchain_impl("1.88.0", "1.88.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_rust_toolchain_impl_unparseable_warns() {
        let r = check_rust_toolchain_impl("not-a-version", "1.88.0");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    // ── parse_db_host_port ───────────────────────────────────────────────────

    #[test]
    fn parse_db_host_port_full_url() {
        let (host, port) = parse_db_host_port("postgres://user:pass@localhost:5432/mydb").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_no_credentials() {
        let (host, port) = parse_db_host_port("postgres://localhost/mydb").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_default_port() {
        let (host, port) = parse_db_host_port("postgres://user:pass@db.example.com/mydb").unwrap();
        assert_eq!(host, "db.example.com");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_custom_port() {
        let (host, port) = parse_db_host_port("postgres://localhost:6543/test").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 6543);
    }

    #[test]
    fn parse_db_host_port_unbracketed_ipv6_defaults_port() {
        let (host, port) = parse_db_host_port("postgres://::1/db").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_bracketed_ipv6_custom_port() {
        let (host, port) = parse_db_host_port("postgres://[::1]:6543/db").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 6543);
    }

    #[test]
    fn parse_db_host_port_postgresql_scheme() {
        let (host, port) = parse_db_host_port("postgresql://localhost:5432/db").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_invalid_scheme() {
        assert!(parse_db_host_port("mysql://localhost/db").is_none());
    }

    // ── check_tailwind_binary_at ─────────────────────────────────────────────

    #[test]
    fn database_topology_doctor_fails_replica_without_primary() {
        let topology = DoctorDatabaseTopology {
            primary_url: None,
            replica_url: Some("postgres://user:secret@replica:5432/app".to_owned()),
            legacy_url: None,
            auto_migrate_in_production: false,
            replica_fallback: DoctorReplicaFallback::FailReadiness,
        };

        let result = check_database_topology_contract(&topology, true);

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("replica"));
        assert!(result.hint.unwrap_or_default().contains("primary"));
    }

    #[test]
    fn database_topology_doctor_fails_prod_replica_with_startup_migrations() {
        let topology = DoctorDatabaseTopology {
            primary_url: Some("postgres://user:secret@primary:5432/app".to_owned()),
            replica_url: Some("postgres://user:secret@replica:5432/app".to_owned()),
            legacy_url: None,
            auto_migrate_in_production: true,
            replica_fallback: DoctorReplicaFallback::FailReadiness,
        };

        let result = check_database_topology_contract(&topology, true);

        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("migration"));
        assert!(!detail.contains("secret"));
    }

    #[test]
    fn database_topology_doctor_fails_stale_replica_when_readiness_must_fail() {
        let result = check_replica_migration_versions(
            Some("20260510000000"),
            Some("20260509000000"),
            DoctorReplicaFallback::FailReadiness,
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("replica"));
        assert!(result.hint.unwrap_or_default().contains("replay"));
    }

    #[test]
    fn database_topology_doctor_warns_stale_replica_when_falling_back_to_primary() {
        let result = check_replica_migration_versions(
            Some("20260510000000"),
            Some("20260509000000"),
            DoctorReplicaFallback::Primary,
        );

        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.unwrap_or_default().contains("primary"));
    }

    // ── process role vs. jobs backend ─────────────────────────────────────────

    #[test]
    fn split_topology_fails_on_web_role_with_local_backend() {
        let result = check_split_topology_on_local(ProcessRole::Web, "local");
        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(
            detail.contains("web"),
            "detail must name the role: {detail}"
        );
        assert!(
            detail.contains("local"),
            "detail must name the backend: {detail}"
        );
        assert!(result.hint.unwrap_or_default().contains("postgres"));
    }

    #[test]
    fn split_topology_fails_on_worker_role_with_local_backend() {
        let result = check_split_topology_on_local(ProcessRole::Worker, "local");
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("worker"));
    }

    #[test]
    fn split_topology_passes_for_combined_role_on_local_backend() {
        let result = check_split_topology_on_local(ProcessRole::Combined, "local");
        assert_eq!(result.status, CheckStatus::Pass);
        // Combined never splits, so the detail need not mention the backend.
        assert_eq!(result.detail.as_deref(), Some("role=combined"));
        assert!(result.hint.is_none());
    }

    #[test]
    fn split_topology_passes_for_web_role_on_postgres_backend() {
        let result = check_split_topology_on_local(ProcessRole::Web, "postgres");
        assert_eq!(result.status, CheckStatus::Pass);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("role=web"), "got: {detail}");
        assert!(detail.contains("postgres"), "got: {detail}");
    }

    #[test]
    fn split_topology_passes_for_worker_role_on_redis_backend() {
        let result = check_split_topology_on_local(ProcessRole::Worker, "redis");
        assert_eq!(result.status, CheckStatus::Pass);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("role=worker"), "got: {detail}");
        assert!(detail.contains("redis"), "got: {detail}");
    }

    #[test]
    fn split_topology_fails_on_web_role_with_backend_typo() {
        // A typo like `postgresql` is not a recognized durable backend: it falls
        // through to the in-process local runtime, so a split role must be
        // rejected exactly as it is for the literal `local`.
        let result = check_split_topology_on_local(ProcessRole::Web, "postgresql");
        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(
            detail.contains("web"),
            "detail must name the role: {detail}"
        );
        assert!(
            detail.contains("postgresql"),
            "detail must name the offending backend: {detail}"
        );
        assert!(result.hint.unwrap_or_default().contains("postgres"));
    }

    #[test]
    fn split_topology_fails_on_web_role_with_blank_backend() {
        // A blank backend also falls through to the local runtime.
        let result = check_split_topology_on_local(ProcessRole::Web, "");
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("web"));
    }

    #[test]
    fn resolve_process_role_prefers_env_over_toml() {
        let table: toml::Table =
            toml::from_str("role = \"web\"\n[jobs]\nbackend = \"local\"\n").expect("parse toml");
        // Env overrides the toml `role`, and reads the jobs backend from toml.
        let (role, backend) = resolve_process_role_and_backend_from_sources(
            |key| (key == "AUTUMN_ROLE").then(|| "worker".to_owned()),
            Some(&table),
        );
        assert_eq!(role, ProcessRole::Worker);
        assert_eq!(backend, "local");
    }

    #[test]
    fn resolve_process_role_defaults_to_combined_and_local() {
        let (role, backend) = resolve_process_role_and_backend_from_sources(|_| None, None);
        assert_eq!(role, ProcessRole::Combined);
        assert_eq!(backend, "local");
    }

    #[test]
    fn queue_coverage_passes_when_pin_unset() {
        let result = check_queue_coverage(
            ProcessRole::Combined,
            &["critical".into(), "low".into()],
            &[],
        );
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn queue_coverage_passes_when_pin_covers_all_queues() {
        let queues = vec!["critical".to_string(), "low".to_string()];
        let pin = vec!["critical".to_string(), "low".to_string()];
        assert_eq!(
            check_queue_coverage(ProcessRole::Combined, &queues, &pin).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn queue_coverage_passes_for_valid_subset_pin() {
        // REVISED for the queue-coverage redesign (#1461): a subset pin that
        // still claims at least one known queue is a legitimate multi-tier
        // deployment, not a failure. It passes and reports the unclaimed queues
        // as informational detail (must NOT fail `--strict`).
        let queues = vec!["critical".to_string(), "low".to_string()];
        let pin = vec!["critical".to_string()];
        let result = check_queue_coverage(ProcessRole::Combined, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.detail.unwrap().contains("low"));
    }

    #[test]
    fn queue_coverage_passes_for_web_role_even_with_failing_pin() {
        // A web-only replica (`AUTUMN_ROLE=web` / `role = "web"`) runs ZERO job
        // workers, so queue-pinning coverage is irrelevant to it: doctor must not
        // fail `--strict` on queue coverage for a web tier. Use a pin that would
        // otherwise Warn on a worker-bearing role — an empty effective schedule
        // (pin references only unknown queues). The gate on
        // `ProcessRole::runs_workers()` makes the web role Pass regardless.
        let queues = vec!["default".to_string()];
        let pin = vec!["critical".to_string()]; // matches no known queue → would Warn on a worker

        // Web role: coverage is not applicable → Pass, and `--strict` stays green.
        let web = check_queue_coverage(ProcessRole::Web, &queues, &pin);
        assert_eq!(web.status, CheckStatus::Pass);
        assert!(
            web.detail
                .as_deref()
                .unwrap_or_default()
                .contains("web-only role"),
            "web-role detail should explain the check is not applicable: {:?}",
            web.detail
        );
        let web_summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&web_summary, true), 0);

        // Sanity: the SAME pin on a worker-bearing role is now INFORMATIONAL-ONLY
        // — it Passes and never fails `--strict`. doctor is config-only and
        // per-process, so it cannot prove a genuine zero-coverage gap; the
        // authoritative check is the runtime startup warning.
        let worker = check_queue_coverage(ProcessRole::Worker, &queues, &pin);
        assert_eq!(worker.status, CheckStatus::Pass);
        let worker_summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&worker_summary, true), 0);

        // Combined behaves like Worker here (also runs workers).
        assert_eq!(
            check_queue_coverage(ProcessRole::Combined, &queues, &pin).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn resolve_queues_zero_config_uses_implicit_default_queue() {
        // No `[jobs.queues]`: the runtime still creates the implicit `default`
        // queue, so doctor must resolve it too (#1623).
        let table: toml::Table = toml::from_str("[jobs]\npin = [\"critical\"]\n").expect("parse");
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&table));
        assert_eq!(queues, vec!["default"]);
        assert_eq!(pin, vec!["critical"]);
    }

    #[test]
    fn queue_coverage_reports_default_when_zero_config_app_is_pinned_elsewhere() {
        // A zero-config app's only configured queue is the implicit `default`. Pinning
        // to `critical`, absent from `[jobs.queues]`, is informational only: doctor is
        // config-only and cannot know whether `critical` is a `#[job(queue =
        // "critical")]`-declared queue the runtime appends to the effective schedule
        // and drains, nor whether a sibling worker tier covers `default`. So the check
        // passes, never failing `--strict`, and reports both facts. The authoritative
        // zero-coverage guard is the runtime startup warning
        // (`warn_pinned_uncovered_queues`). Before this change the case warned and
        // failed `--strict`.
        let table: toml::Table = toml::from_str("[jobs]\npin = [\"critical\"]\n").expect("parse");
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&table));
        let result = check_queue_coverage(ProcessRole::Combined, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        let detail = result.detail.unwrap();
        // Reports that configured `default` is not claimed by this pin...
        assert!(
            detail.contains("default"),
            "detail should name the unclaimed configured queue: {detail}"
        );
        // ...and that pinned `critical` is not in [jobs.queues] and may be
        // job-declared / drained by the runtime.
        assert!(
            detail.contains("critical"),
            "detail should name the config-unknown pinned queue: {detail}"
        );
        assert!(
            detail.contains("job(queue"),
            "detail should note the pinned queue may be #[job(queue=…)]-declared: {detail}"
        );
        // Informational only → `--strict` stays green.
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn queue_coverage_passes_for_zero_config_app_without_pin() {
        // No pin on a zero-config app: `default` is drained, no false positive.
        let table: toml::Table = toml::from_str("").expect("parse");
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&table));
        assert_eq!(queues, vec!["default"]);
        assert!(pin.is_empty());
        assert_eq!(
            check_queue_coverage(ProcessRole::Combined, &queues, &pin).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn queue_coverage_honors_profile_layer_jobs_config() {
        // Regression for the #1623 review finding: when queue config is
        // profile-specific, the coverage check must read the merged
        // active-profile table (base + `[profile.<env>]` + `autumn-<env>.toml`)
        // exactly like the runtime — not just the raw top-level `[jobs]`.
        //
        // Scenario: base `autumn.toml` has no top-level `[jobs]`; the prod
        // profile pins `critical` while declaring `critical` + `bulk` queues,
        // leaving `bulk` uncovered.
        let base: toml::Table = toml::from_str(
            "[profile.prod.jobs]\npin = [\"critical\"]\nqueues = [\"critical\", \"bulk\"]\n",
        )
        .expect("parse toml");

        // Resolving from the raw top-level table sees no `[jobs]` at all, so it
        // falls back to the implicit `default` queue with no pin — proving that
        // WITHOUT merging the profile layer doctor never observes the profile's
        // `jobs.pin`/`jobs.queues`.
        let (raw_queues, raw_pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&base));
        assert_eq!(raw_queues, vec!["default"]);
        assert!(raw_pin.is_empty());
        assert_eq!(
            check_queue_coverage(ProcessRole::Combined, &raw_queues, &raw_pin).status,
            CheckStatus::Pass
        );

        // Merge the active `[profile.prod]` layer the same way
        // `get_merged_toml_table_runtime` does, then resolve: the prod
        // `jobs.queues` / `jobs.pin` are now honored. Under the queue-coverage
        // redesign (#1461) pinning `critical` (a KNOWN queue) is a valid,
        // non-empty subset schedule, so the check PASSES and reports the
        // unclaimed `bulk` as informational detail (never fails `--strict`).
        // Asserting the detail names `bulk` proves the profile layer's queues
        // were actually merged and read.
        let mut merged = toml::Table::new();
        deep_merge(&mut merged, &base);
        for profile_name in inline_profile_lookup_names("prod") {
            if let Some(prof) = base
                .get("profile")
                .and_then(toml::Value::as_table)
                .and_then(|p| p.get(profile_name))
                .and_then(toml::Value::as_table)
            {
                deep_merge(&mut merged, prof);
            }
        }
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&merged));
        assert_eq!(queues, vec!["critical", "bulk"]);
        assert_eq!(pin, vec!["critical"]);
        let result = check_queue_coverage(ProcessRole::Combined, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.detail.unwrap().contains("bulk"));
        // A valid subset pin must NOT fail `--strict`.
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn queue_coverage_resolves_web_role_from_profile_layer() {
        // Regression for the doctor.rs P2: the queue-coverage check reads
        // `jobs.queues`/`jobs.pin` from the merged active-profile table, but the `role`
        // it was gated on came from the raw top-level table. A `role` declared only
        // under `[profile.prod]`, with AUTUMN_ENV=prod, was missed, so a web replica
        // was treated as `Combined`: the web-role skip-gate never fired and a pin
        // matching no known queue wrongly warned or failed `--strict`. The fix resolves
        // the role from the same merged table. Scenario: base `autumn.toml` has no
        // top-level `role`; the prod profile sets `role = "web"` and pins a queue
        // matching nothing configured.
        let base: toml::Table = toml::from_str(
            "[profile.prod]\nrole = \"web\"\n[profile.prod.jobs]\npin = [\"ghost\"]\nqueues = [\"critical\"]\n",
        )
        .expect("parse toml");

        // BEFORE (bug): resolving the role from the RAW top-level table sees no
        // `role`, so it defaults to `Combined`. A worker-bearing role with a pin
        // that matches no known queue Warns → `--strict` fails. This is exactly
        // the false failure the fix eliminates.
        let (raw_role, _) = resolve_process_role_and_backend_from_sources(|_| None, Some(&base));
        assert_eq!(raw_role, ProcessRole::Combined);
        let (raw_queues, raw_pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&base));
        let raw_result = check_queue_coverage(raw_role, &raw_queues, &raw_pin);
        // (raw table also misses the profile's queues/pin, but the load-bearing
        // point is the role: Combined runs workers, so the gate does not skip.)
        assert!(raw_role.runs_workers());
        let _ = raw_result;

        // AFTER (fix): merge the active `[profile.prod]` layer the same way
        // `get_merged_toml_table_runtime` does, then resolve the role from the
        // merged table. The prod `role = "web"` is now honored.
        let mut merged = toml::Table::new();
        deep_merge(&mut merged, &base);
        for profile_name in inline_profile_lookup_names("prod") {
            if let Some(prof) = base
                .get("profile")
                .and_then(toml::Value::as_table)
                .and_then(|p| p.get(profile_name))
                .and_then(toml::Value::as_table)
            {
                deep_merge(&mut merged, prof);
            }
        }
        let (merged_role, _) =
            resolve_process_role_and_backend_from_sources(|_| None, Some(&merged));
        assert_eq!(
            merged_role,
            ProcessRole::Web,
            "role must be resolved from the merged profile layer"
        );
        assert!(!merged_role.runs_workers());

        // The web role runs no workers, so the coverage check skips the pin
        // evaluation entirely and PASSES even though the pin matches no queue.
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&merged));
        let result = check_queue_coverage(merged_role, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(
            result
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("web-only role"),
            "web-role coverage detail should explain the check is skipped: {:?}",
            result.detail
        );
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);

        // AUTUMN_ROLE env still wins over the merged-table role (runtime
        // precedence): a `worker` override on the same config runs workers. The
        // pin matching no configured queue is now INFORMATIONAL-ONLY → Pass; the
        // runtime startup warning is the authoritative coverage check.
        let (env_role, _) = resolve_process_role_and_backend_from_sources(
            |key| (key == "AUTUMN_ROLE").then(|| "worker".to_owned()),
            Some(&merged),
        );
        assert_eq!(env_role, ProcessRole::Worker);
        assert_eq!(
            check_queue_coverage(env_role, &queues, &pin).status,
            CheckStatus::Pass,
        );
    }

    #[test]
    fn queue_coverage_web_role_from_top_level_role_still_passes() {
        // No-regression companion: a top-level (non-profile) `role = "web"` must
        // still resolve to `Web` and skip the coverage check — the profile-aware
        // fix must not break the common flat-config case.
        let table: toml::Table =
            toml::from_str("role = \"web\"\n[jobs]\npin = [\"ghost\"]\nqueues = [\"critical\"]\n")
                .expect("parse toml");
        let (role, _) = resolve_process_role_and_backend_from_sources(|_| None, Some(&table));
        assert_eq!(role, ProcessRole::Web);
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&table));
        let result = check_queue_coverage(role, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn queue_coverage_worker_role_from_profile_with_empty_schedule_is_informational() {
        // A prod `role = "worker"` (worker-bearing) resolved from the profile
        // layer, with a pin matching no queue in [jobs.queues], is now
        // INFORMATIONAL-ONLY: doctor cannot see `#[job(queue=…)]`-declared queues
        // or sibling tiers, so it PASSES (never fails `--strict`) and reports the
        // gap. The authoritative guard is the runtime startup warning.
        let base: toml::Table = toml::from_str(
            "[profile.prod]\nrole = \"worker\"\n[profile.prod.jobs]\npin = [\"ghost\"]\nqueues = [\"critical\"]\n",
        )
        .expect("parse toml");
        let mut merged = toml::Table::new();
        deep_merge(&mut merged, &base);
        for profile_name in inline_profile_lookup_names("prod") {
            if let Some(prof) = base
                .get("profile")
                .and_then(toml::Value::as_table)
                .and_then(|p| p.get(profile_name))
                .and_then(toml::Value::as_table)
            {
                deep_merge(&mut merged, prof);
            }
        }
        let (role, _) = resolve_process_role_and_backend_from_sources(|_| None, Some(&merged));
        assert_eq!(role, ProcessRole::Worker);
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&merged));
        let result = check_queue_coverage(role, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn queue_coverage_passes_for_multi_tier_subset_pins() {
        // #1461: a multi-tier deployment intentionally splits queues across
        // worker tiers (e.g. one process pins `critical`, another pins
        // `bulk,default`). Each tier's subset pin is legitimate and must NOT
        // fail `--strict`, and a single doctor run cannot see sibling tiers.
        let queues = vec![
            "critical".to_string(),
            "bulk".to_string(),
            "default".to_string(),
        ];

        // Critical tier: pins only `critical`; the other queues are covered by
        // another tier. Passes, and the informational detail names them.
        let critical_tier =
            check_queue_coverage(ProcessRole::Combined, &queues, &["critical".to_string()]);
        assert_eq!(critical_tier.status, CheckStatus::Pass);
        let detail = critical_tier.detail.unwrap();
        assert!(detail.contains("bulk"), "detail should list bulk: {detail}");
        assert!(
            detail.contains("default"),
            "detail should list default: {detail}"
        );

        // Bulk tier: pins `bulk,default`; also a valid subset → Pass.
        let bulk_tier = check_queue_coverage(
            ProcessRole::Combined,
            &queues,
            &["bulk".to_string(), "default".to_string()],
        );
        assert_eq!(bulk_tier.status, CheckStatus::Pass);

        // Neither tier warns, so `--strict` stays green.
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    // ── #1756: sound, topology/registry-aware `--strict` hard-fail ──────────────

    #[test]
    fn topology_absent_delegates_to_informational_pass() {
        // Regression guard: with NO fleet topology declared the topology-aware
        // check must behave exactly like the informational-only per-process
        // report — a subset pin Passes and never fails `--strict`.
        let queues = vec!["critical".to_string(), "bulk".to_string()];
        let pin = vec!["critical".to_string()];
        let result = check_queue_coverage_topology(
            ProcessRole::Combined,
            &queues,
            &pin,
            &[],  // no declared queues
            None, // no fleet topology → informational fallback
        );
        assert_eq!(result.status, CheckStatus::Pass);
        // Same detail the informational-only path produces (names the unclaimed).
        assert!(result.detail.unwrap().contains("bulk"));
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn topology_with_full_union_coverage_passes() {
        // The union of all declared tiers covers every needed queue → Pass.
        let queues = vec![
            "critical".to_string(),
            "bulk".to_string(),
            "default".to_string(),
        ];
        let fleet = FleetTopology {
            tiers: vec![
                vec!["critical".to_string()],
                vec!["bulk".to_string(), "default".to_string()],
            ],
            malformed: false,
        };
        let result = check_queue_coverage_topology(
            ProcessRole::Combined,
            &queues,
            &["critical".to_string()],
            &[],
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.detail);
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn topology_valid_multi_tier_subset_split_does_not_fail() {
        // The #1746 false-positive case: two tiers split the queues between them.
        // A single process sees only its own pin (`critical`), which looks like a
        // gap — but with the fleet topology declared the union covers everything,
        // so `--strict` must NOT fail.
        let queues = vec![
            "critical".to_string(),
            "bulk".to_string(),
            "default".to_string(),
        ];
        let fleet = FleetTopology {
            tiers: vec![
                vec!["critical".to_string()],
                vec!["bulk".to_string(), "default".to_string()],
            ],
            malformed: false,
        };
        // Run doctor as the critical tier (pin = critical): still Pass.
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["critical".to_string()],
            &[],
            Some(&fleet),
        );
        assert_ne!(
            result.status,
            CheckStatus::Fail,
            "a valid multi-tier subset split must not fail: {:?}",
            result.detail
        );
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn topology_with_genuine_gap_fails_strict() {
        // A configured queue drained by NO declared tier is a provable gap → Fail,
        // which `--strict` promotes to exit 1.
        let queues = vec![
            "critical".to_string(),
            "bulk".to_string(),
            "default".to_string(),
        ];
        let fleet = FleetTopology {
            // No tier drains `bulk`.
            tiers: vec![vec!["critical".to_string()], vec!["default".to_string()]],
            malformed: false,
        };
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["critical".to_string()],
            &[],
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Fail, "{:?}", result.detail);
        assert!(
            result.detail.unwrap().contains("bulk"),
            "the failure must name the uncovered queue"
        );
        assert!(result.hint.is_some());
        let summary = Summary {
            passed: 0,
            warned: 0,
            failed: 1,
        };
        assert_eq!(exit_code(&summary, true), 1);
    }

    #[test]
    fn topology_unpinned_tier_covers_everything() {
        // An unpinned tier (empty pin) drains every queue, so union coverage is
        // total even when other tiers are narrow subsets → Pass.
        let queues = vec!["critical".to_string(), "bulk".to_string()];
        let fleet = FleetTopology {
            tiers: vec![
                vec!["critical".to_string()],
                Vec::new(), // unpinned tier: drains everything
            ],
            malformed: false,
        };
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["critical".to_string()],
            &[],
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.detail);
    }

    #[test]
    fn topology_job_declared_queue_pinned_but_absent_from_config_passes() {
        // The #1756 blindspot RESOLVED, not failed: a queue named in a tier's pin
        // that is a `#[job(queue = "email")]`-declared queue (absent from
        // [jobs.queues]) must Pass. With the declared set surfaced, `email` is in
        // the needed set AND covered by the pinning tier → no gap; even without
        // the declared set it is simply not needed → no gap.
        let queues = vec!["default".to_string()];
        let declared = vec!["email".to_string()];
        let fleet = FleetTopology {
            tiers: vec![vec!["default".to_string(), "email".to_string()]],
            malformed: false,
        };
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["default".to_string(), "email".to_string()],
            &declared,
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.detail);

        // And with the declared set UNKNOWN (empty), still Pass — the job-declared
        // pin never manufactures a gap.
        let result_no_manifest = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["default".to_string(), "email".to_string()],
            &[],
            Some(&fleet),
        );
        assert_eq!(
            result_no_manifest.status,
            CheckStatus::Pass,
            "{:?}",
            result_no_manifest.detail
        );
    }

    /// Every `[jobs.fleet]` example in the jobs guide must be a config that
    /// `autumn doctor` actually passes.
    ///
    /// The first draft of that section shipped a `declared_queues =
    /// ["thumbnails"]` example whose `tiers` pinned no tier to `thumbnails` —
    /// a reader who copied it verbatim got exit 1 from the very check the
    /// section is teaching them to use. Prose review did not catch it because
    /// the example reads correctly; only running it does. So run it: pull each
    /// `[jobs.fleet]` TOML block straight out of the guide and push it through
    /// the same resolvers and the same check `autumn doctor` uses.
    #[test]
    fn documented_fleet_topology_examples_pass_the_coverage_check() {
        const GUIDE: &str = include_str!("../../docs/guide/jobs.md");
        // The same example is mirrored in the `JobFleetConfig` rustdoc, where it
        // is just as copy-pasteable; scan both so they cannot drift apart.
        const CONFIG_RS: &str = include_str!("../../autumn/src/config.rs");

        // The sources interleave `[jobs.queues]` and `[jobs.fleet]`; a block may
        // carry either or both. Only blocks that declare a topology are checked
        // — the rest have nothing for this check to act on. Rustdoc fences carry
        // a `/// ` prefix on every line, so strip it.
        let mut blocks: Vec<String> = GUIDE
            .split("```toml")
            .skip(1)
            .filter_map(|rest| rest.split("```").next())
            .filter(|block| block.contains("[jobs.fleet]"))
            .map(str::to_owned)
            .collect();
        blocks.extend(
            CONFIG_RS
                .split("/// ```toml")
                .skip(1)
                .filter_map(|rest| rest.split("/// ```").next())
                .filter(|block| block.contains("[jobs.fleet]"))
                .map(|block| {
                    block
                        .lines()
                        .map(|l| l.trim_start().trim_start_matches("///").trim_start())
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
        );
        assert!(
            blocks.len() >= 2,
            "expected a [jobs.fleet] example in both docs/guide/jobs.md and the \
             JobFleetConfig rustdoc, found {} — either an example was removed or the \
             fence style changed (fix the extractor), but do not leave this test \
             silently checking nothing",
            blocks.len(),
        );

        for block in &blocks {
            let block = block.as_str();
            let table: toml::Table = toml::from_str(block).unwrap_or_else(|e| {
                panic!("documented [jobs.fleet] example is not valid TOML: {e}\n{block}")
            });
            // The app must accept it too, or the example fails boot under
            // `server.strict_config_enforce_all`.
            let fleet_value = table
                .get("jobs")
                .and_then(|j| j.get("fleet"))
                .cloned()
                .expect("block was selected for containing [jobs.fleet]");
            let _: autumn_web::config::JobFleetConfig =
                fleet_value.try_into().unwrap_or_else(|e| {
                    panic!("documented [jobs.fleet] example is not valid config: {e}\n{block}")
                });

            let fleet = resolve_fleet_topology(Some(&table))
                .expect("a block containing [jobs.fleet] tiers declares a topology");
            let declared = resolve_declared_queues_from_sources(|_| None, Some(&table));
            let configured: Vec<String> = table
                .get("jobs")
                .and_then(|j| j.get("queues"))
                .and_then(toml::Value::as_table)
                .map(|q| q.keys().cloned().collect())
                .unwrap_or_default();
            // Read as the tier the example's first entry describes.
            let pin = fleet.tiers.first().cloned().unwrap_or_default();

            let result = check_queue_coverage_topology(
                ProcessRole::Worker,
                &configured,
                &pin,
                &declared,
                Some(&fleet),
            );
            assert_eq!(
                result.status,
                CheckStatus::Pass,
                "a [jobs.fleet] example in docs/guide/jobs.md fails `autumn doctor`: {:?}\n{block}",
                result.detail,
            );
        }
    }

    #[test]
    fn topology_declared_gap_on_job_declared_queue_fails() {
        // Once the declared set is known, a job-declared queue that NO tier drains
        // is a provable gap the runtime would strand → Fail. This is the blindspot
        // being *resolved* (surfaced as a real gap), not a false positive.
        let queues = vec!["default".to_string()];
        let declared = vec!["email".to_string()];
        let fleet = FleetTopology {
            // No tier pins `email`.
            tiers: vec![vec!["default".to_string()]],
            malformed: false,
        };
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["default".to_string()],
            &declared,
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Fail, "{:?}", result.detail);
        assert!(result.detail.unwrap().contains("email"));
    }

    #[test]
    fn topology_empty_tiers_stays_informational() {
        // A `[jobs.fleet]` with no tiers declares nothing coverable, so it must
        // not fail — it falls back to informational Pass.
        let queues = vec!["critical".to_string(), "bulk".to_string()];
        let fleet = FleetTopology {
            tiers: Vec::new(),
            malformed: false,
        };
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &queues,
            &["critical".to_string()],
            &[],
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.detail);
    }

    #[test]
    fn resolve_fleet_topology_reads_tiers_array() {
        let table: toml::Table =
            toml::from_str("[jobs.fleet]\ntiers = [[\"critical\"], [\"bulk\", \"default\"], []]\n")
                .expect("parse toml");
        let fleet = resolve_fleet_topology(Some(&table)).expect("topology declared");
        assert_eq!(
            fleet.tiers,
            vec![
                vec!["critical".to_string()],
                vec!["bulk".to_string(), "default".to_string()],
                Vec::<String>::new(),
            ]
        );
        assert!(fleet.has_unpinned_tier());
    }

    /// Anti-drift guard (#1623). `doctor` reads `[jobs.fleet]` out of the merged
    /// raw TOML table (it needs profile-aware merging the typed loader doesn't
    /// give it), while the app boots the same file through
    /// `autumn_web::config::JobFleetConfig`. Two readers, one spelling: if either
    /// side renames a key, the app rejects a topology doctor accepts (or the
    /// reverse), and the AC6 hard-fail silently stops covering real deployments.
    /// So parse one document both ways and require the same answer.
    #[test]
    fn doctor_and_app_agree_on_the_fleet_topology_spelling() {
        const DOC: &str = "[jobs.fleet]\n\
                           tiers = [[\"critical\"], [\"bulk\", \"default\"], []]\n\
                           manifest = \"target/autumn-jobs.toml\"\n\
                           declared_queues = [\"thumbnails\"]\n";

        let table: toml::Table = toml::from_str(DOC).expect("parse toml");
        let doctor_view = resolve_fleet_topology(Some(&table)).expect("topology declared");
        let app_view: autumn_web::config::JobFleetConfig = toml::from_str::<toml::Table>(DOC)
            .expect("parse toml")
            .get("jobs")
            .and_then(|j| j.get("fleet"))
            .cloned()
            .expect("jobs.fleet present")
            .try_into()
            .expect("app config accepts the same [jobs.fleet] doctor reads");

        assert_eq!(
            doctor_view.tiers, app_view.tiers,
            "doctor and the app must read the same `tiers` from one autumn.toml",
        );
        assert_eq!(
            resolve_declared_queues_from_sources(|_| None, Some(&table)),
            app_view.declared_queues,
            "doctor and the app must read the same `declared_queues`",
        );
        assert_eq!(
            app_view.manifest.as_deref(),
            Some("target/autumn-jobs.toml"),
            "the app must accept the `manifest` key doctor resolves",
        );
    }

    /// A `tiers` that is present but is not a list of lists — most likely the
    /// flat `tiers = ["critical"]`, since `jobs.pin` uses exactly that shape a
    /// few lines above it in the same file — must not be read as "no topology
    /// declared". Doing so silently switches the AC6 hard-fail back off: the
    /// check falls through to the informational-only report and passes, and
    /// nothing anywhere says coverage is no longer being enforced. The app
    /// rejects the same input at boot, so a pre-deploy gate that passes it has
    /// the ordering exactly backwards.
    #[test]
    fn a_malformed_tiers_fails_instead_of_silently_disabling_the_check() {
        // Every shape the app's typed `JobFleetConfig` rejects at boot. Each one
        // must be reported here, not read as "no topology declared" — that would
        // fall through to the informational-only report and pass, switching the
        // AC6 hard-fail off with nothing saying so.
        for doc in [
            // The flat shape `jobs.pin` uses a few lines above it.
            "[jobs.fleet]\ntiers = [\"critical\", \"bulk\"]\n",
            // A bare value instead of a list at all.
            "[jobs.fleet]\ntiers = \"critical\"\n",
            "[jobs.fleet]\ntiers = 3\n",
            // Mixed: one well-formed tier and one that is not.
            "[jobs.fleet]\ntiers = [[\"critical\"], \"bulk\"]\n",
            // A non-string queue name. `[[1]]` is the dangerous one: dropping
            // the entry leaves an EMPTY tier, which reads as "drains
            // everything" and would make the check pass unconditionally.
            "[jobs.fleet]\ntiers = [[1]]\n",
            "[jobs.fleet]\ntiers = [[\"critical\", 1]]\n",
            "[jobs.fleet]\ntiers = [[\"critical\"], [[\"bulk\"]]]\n",
        ] {
            let table: toml::Table = toml::from_str(doc).expect("parse toml");
            let fleet = resolve_fleet_topology(Some(&table)).unwrap_or_else(|| {
                panic!("a present-but-malformed tiers must be reported, not dropped: {doc}")
            });
            assert!(fleet.malformed, "{doc}");

            let result = check_queue_coverage_topology(
                ProcessRole::Worker,
                &["critical".to_string(), "bulk".to_string()],
                &["critical".to_string()],
                &[],
                Some(&fleet),
            );
            assert_eq!(
                result.status,
                CheckStatus::Fail,
                "{doc}: {:?}",
                result.detail
            );
            assert!(
                result.detail.unwrap_or_default().contains("list of lists"),
                "the failure must say what is wrong with the value: {doc}",
            );

            // The app refuses the same document, so both gates agree.
            let fleet_value = table
                .get("jobs")
                .and_then(|j| j.get("fleet"))
                .cloned()
                .expect("jobs.fleet present");
            let parsed: Result<autumn_web::config::JobFleetConfig, _> = fleet_value.try_into();
            assert!(
                parsed.is_err(),
                "the typed config must reject it too: {doc}"
            );
        }

        // The specific hazard for `[[1]]`: silently dropping the non-string
        // leaves an empty tier, and an empty tier is a *legitimate* declaration
        // meaning "unpinned, drains everything" — so the check would have
        // reported total coverage and passed. Pin that an empty tier really does
        // mean that, which is why the non-string case can never be allowed to
        // decay into one.
        let unpinned = FleetTopology {
            tiers: vec![Vec::new()],
            malformed: false,
        };
        assert!(unpinned.has_unpinned_tier());
        assert_eq!(
            check_queue_coverage_topology(
                ProcessRole::Worker,
                &["critical".to_string(), "bulk".to_string()],
                &[],
                &[],
                Some(&unpinned),
            )
            .status,
            CheckStatus::Pass,
            "an empty tier means 'drains everything' — so a malformed tier must never \
             be allowed to collapse into one",
        );

        // `[jobs.fleet]` itself not a table is the same class of mistake.
        let table: toml::Table =
            toml::from_str("[jobs]\nfleet = \"critical\"\n").expect("parse toml");
        let fleet = resolve_fleet_topology(Some(&table))
            .expect("a present-but-malformed [jobs.fleet] must be reported");
        assert!(fleet.malformed);

        // …while a genuinely absent section stays "nothing declared".
        let absent: toml::Table =
            toml::from_str("[jobs]\npin = [\"critical\"]\n").expect("parse toml");
        assert!(resolve_fleet_topology(Some(&absent)).is_none());
        let no_tiers: toml::Table =
            toml::from_str("[jobs.fleet]\ndeclared_queues = [\"a\"]\n").expect("parse toml");
        assert!(resolve_fleet_topology(Some(&no_tiers)).is_none());
    }

    #[test]
    fn resolve_fleet_topology_absent_is_none() {
        let table: toml::Table =
            toml::from_str("[jobs]\nqueues = [\"critical\"]\n").expect("parse toml");
        assert!(resolve_fleet_topology(Some(&table)).is_none());
        // Present but empty tiers → still None (nothing declared).
        let empty: toml::Table = toml::from_str("[jobs.fleet]\ntiers = []\n").expect("parse toml");
        assert!(resolve_fleet_topology(Some(&empty)).is_none());
    }

    #[test]
    fn resolve_declared_queues_reads_inline_list_and_manifest() {
        // Inline list (MVP).
        let inline: toml::Table = toml::from_str(
            "[jobs.fleet]\ntiers = [[\"default\"]]\ndeclared_queues = [\"email\", \"sms\"]\n",
        )
        .expect("parse toml");
        let declared = resolve_declared_queues_from_sources(|_| None, Some(&inline));
        assert_eq!(declared, vec!["email".to_string(), "sms".to_string()]);

        // Emitted manifest takes precedence over the inline list.
        let with_manifest: toml::Table = toml::from_str(
            "[jobs.fleet]\nmanifest = \"target/jobs-manifest.toml\"\ndeclared_queues = [\"stale\"]\n",
        )
        .expect("parse toml");
        let declared_from_manifest = resolve_declared_queues_from_sources(
            |path| {
                (path == "target/jobs-manifest.toml")
                    .then(|| "queues = [\"critical\", \"email\"]\n".to_string())
            },
            Some(&with_manifest),
        );
        assert_eq!(
            declared_from_manifest,
            vec!["critical".to_string(), "email".to_string()]
        );

        // An EMPTY manifest array is the app answering "no job-declared queues",
        // not failing to answer, so it wins over a stale inline list. Falling
        // through here would let that stale entry manufacture a coverage failure
        // against the ground truth — and would contradict the documented
        // precedence.
        let empty_manifest = resolve_declared_queues_from_sources(
            |path| (path == "target/jobs-manifest.toml").then(|| "queues = []\n".to_string()),
            Some(&with_manifest),
        );
        assert!(
            empty_manifest.is_empty(),
            "an empty manifest must win over declared_queues, got {empty_manifest:?}",
        );

        // A manifest that genuinely says nothing DOES fall through: unreadable
        // file, unparseable TOML, or no `queues` array at all.
        for (label, read) in [
            ("unreadable", None::<String>),
            ("unparseable", Some("this is not toml =".to_string())),
            ("no queues key", Some("other = 1\n".to_string())),
        ] {
            let fell_through =
                resolve_declared_queues_from_sources(|_| read.clone(), Some(&with_manifest));
            assert_eq!(
                fell_through,
                vec!["stale".to_string()],
                "a manifest that says nothing ({label}) must fall through to declared_queues",
            );
        }

        // No `[jobs.fleet]` → empty.
        let none: toml::Table =
            toml::from_str("[jobs]\nqueues = [\"critical\"]\n").expect("parse toml");
        assert!(resolve_declared_queues_from_sources(|_| None, Some(&none)).is_empty());
    }

    // ── Jobs manifest emit → consume loop (#1756) ──────────────────────────
    //
    // These prove the full round trip: the manifest string `autumn jobs manifest`
    // emits (verified byte-for-byte by autumn-web's `jobs_manifest_renders_*`
    // test) is written to disk, read back through `[jobs.fleet] manifest = ...`,
    // and drives the topology-aware coverage verdict.

    /// The exact bytes `render_jobs_manifest` emits for a `[jobs.queues] =
    /// ["critical"]` app carrying one `#[job(queue = "email")]`.
    const EMITTED_MANIFEST: &str = "queues = [\"critical\", \"email\"]\n";

    #[test]
    fn manifest_emit_consume_loop_fails_on_genuine_gap() {
        // Emit → write to disk → doctor reads it back and PROVES a gap: the
        // manifest surfaces the job-declared `email`, which no tier drains, so a
        // topology-aware `--strict` run turns it into a hard Fail (exit 1).
        let temp = tempfile::tempdir().expect("temp dir");
        let manifest_path = temp.path().join("jobs-manifest.toml");
        std::fs::write(&manifest_path, EMITTED_MANIFEST).expect("write manifest");

        let table: toml::Table = toml::from_str(&format!(
            "[jobs]\nqueues = [\"critical\"]\n\n[jobs.fleet]\nmanifest = {path:?}\ntiers = [[\"critical\"]]\n",
            path = manifest_path.to_str().expect("utf8 path"),
        ))
        .expect("parse toml");

        // Doctor reads the emitted manifest, not the stale inline list.
        let declared = resolve_declared_queues(Some(&table));
        assert_eq!(
            declared,
            vec!["critical".to_string(), "email".to_string()],
            "the emitted manifest is the declared-queue source of truth"
        );

        let fleet = resolve_fleet_topology(Some(&table)).expect("topology declared");
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &["critical".to_string()],
            &["critical".to_string()],
            &declared,
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Fail, "{:?}", result.detail);
        assert!(
            result.detail.unwrap().contains("email"),
            "the failure must name the manifest-surfaced uncovered queue"
        );
        let summary = Summary {
            passed: 0,
            warned: 0,
            failed: 1,
        };
        assert_eq!(exit_code(&summary, true), 1);
    }

    #[test]
    fn manifest_emit_consume_loop_passes_on_full_coverage() {
        // Same emitted manifest, but now a dedicated tier drains `email` too →
        // union coverage is total → Pass, with no false negative.
        let temp = tempfile::tempdir().expect("temp dir");
        let manifest_path = temp.path().join("jobs-manifest.toml");
        std::fs::write(&manifest_path, EMITTED_MANIFEST).expect("write manifest");

        let table: toml::Table = toml::from_str(&format!(
            "[jobs]\nqueues = [\"critical\"]\n\n[jobs.fleet]\nmanifest = {path:?}\ntiers = [[\"critical\"], [\"email\"]]\n",
            path = manifest_path.to_str().expect("utf8 path"),
        ))
        .expect("parse toml");

        let declared = resolve_declared_queues(Some(&table));
        let fleet = resolve_fleet_topology(Some(&table)).expect("topology declared");
        let result = check_queue_coverage_topology(
            ProcessRole::Worker,
            &["critical".to_string()],
            &["critical".to_string()],
            &declared,
            Some(&fleet),
        );
        assert_eq!(result.status, CheckStatus::Pass, "{:?}", result.detail);
    }

    #[test]
    fn resolve_queues_empty_pin_env_overrides_toml_pin() {
        // #2290: a PRESENT but empty `AUTUMN_JOBS__PIN` is an explicit override
        // that clears the pin (process unpinned, drains every queue), mirroring
        // the runtime's `if let Ok(val) = env.var("AUTUMN_JOBS__PIN")` semantics.
        // Doctor must NOT fall back to the TOML `jobs.pin` in that case.
        let table: toml::Table = toml::from_str("[jobs]\npin = [\"critical\"]\n").expect("parse");
        let (queues, pin) = resolve_queues_and_pin_from_sources(
            |key| (key == "AUTUMN_JOBS__PIN").then(String::new),
            Some(&table),
        );
        assert_eq!(queues, vec!["default"]);
        assert!(pin.is_empty(), "empty env pin must clear the TOML pin");
        let result = check_queue_coverage(ProcessRole::Combined, &queues, &pin);
        assert_eq!(result.status, CheckStatus::Pass);
        // Previously this false-failed (fell back to `pin=[critical]` →
        // empty-schedule Warn); now it passes cleanly under `--strict`.
        let summary = Summary {
            passed: 1,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&summary, true), 0);
    }

    #[test]
    fn resolve_queues_and_pin_reads_array_queues_and_toml_pin() {
        let table: toml::Table =
            toml::from_str("[jobs]\nqueues = [\"critical\", \"low\"]\npin = [\"critical\"]\n")
                .expect("parse toml");
        let (queues, pin) = resolve_queues_and_pin_from_sources(|_| None, Some(&table));
        assert_eq!(queues, vec!["critical", "low"]);
        assert_eq!(pin, vec!["critical"]);
    }

    #[test]
    fn resolve_queues_and_pin_reads_table_queue_names_and_env_pin() {
        let table: toml::Table =
            toml::from_str("[jobs.queues]\ncritical = 4\nlow = 1\n").expect("parse toml");
        let (mut queues, pin) = resolve_queues_and_pin_from_sources(
            |key| (key == "AUTUMN_JOBS__PIN").then(|| "critical, low".to_owned()),
            Some(&table),
        );
        queues.sort();
        assert_eq!(queues, vec!["critical", "low"]);
        // Env pin overrides (there is no TOML pin here).
        assert_eq!(pin, vec!["critical", "low"]);
    }

    #[test]
    fn resolve_process_role_reads_backend_from_env() {
        let table: toml::Table = toml::from_str("role = \"web\"\n").expect("parse toml");
        let (role, backend) = resolve_process_role_and_backend_from_sources(
            |key| (key == "AUTUMN_JOBS__BACKEND").then(|| "postgres".to_owned()),
            Some(&table),
        );
        assert_eq!(role, ProcessRole::Web);
        assert_eq!(backend, "postgres");
    }

    #[test]
    fn resolve_process_role_falls_back_to_toml_when_env_role_invalid() {
        // An invalid AUTUMN_ROLE must be ignored in favor of the configured
        // TOML role, exactly like the runtime's apply_role_env_overrides_with_env.
        let table: toml::Table =
            toml::from_str("role = \"worker\"\n[jobs]\nbackend = \"local\"\n").expect("parse toml");
        let (role, backend) = resolve_process_role_and_backend_from_sources(
            |key| (key == "AUTUMN_ROLE").then(|| "garbage".to_owned()),
            Some(&table),
        );
        assert_eq!(role, ProcessRole::Worker);
        assert_eq!(backend, "local");

        // ...and doctor therefore flags the split-on-local misconfiguration the
        // runtime rejects at startup, instead of wrongly passing.
        let result = check_split_topology_on_local(role, &backend);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("worker"));
    }

    #[test]
    fn database_topology_parses_latest_applied_migration_version() {
        let output = r"
            [X] 20260509000000_create_widgets
            [ ] 20260510000000_add_widget_index
            [X] 20260508000000_create_users
        ";

        assert_eq!(
            parse_latest_applied_migration_version(output).as_deref(),
            Some("20260509000000_create_widgets")
        );
    }

    #[test]
    fn database_topology_connectivity_names_role_without_credentials() {
        let result = check_db_role_connectivity(
            "primary",
            "postgres://user:secret@db:5432/app",
            |host, port| {
                assert_eq!(host, "db");
                assert_eq!(port, 5432);
                false
            },
            false,
        );

        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("primary"));
        assert!(detail.contains("db:5432"));
        assert!(!detail.contains("user"));
        assert!(!detail.contains("secret"));
    }

    /// `SQLite` foundation (issue #1614): a bootable single-role `sqlite://`
    /// primary is valid, so the connectivity check must Pass and never nudge the
    /// user to "use postgres://" nor Fail on an unreachable host (there is no
    /// host).
    #[test]
    fn db_connectivity_sqlite_target_passes_without_postgres_hint() {
        for url in ["sqlite://app.db", "sqlite:data/app.db", "file:app.db"] {
            let result = check_db_role_connectivity(
                "primary",
                url,
                |_, _| panic!("SQLite target must not attempt TCP reachability"),
                true,
            );
            assert_eq!(result.status, CheckStatus::Pass, "url={url}");
            let detail = result.detail.unwrap_or_default();
            assert!(detail.contains("SQLite"), "detail={detail}");
            assert!(
                result.hint.is_none(),
                "SQLite target must not emit a postgres:// hint (url={url})"
            );
        }
    }

    /// A Postgres target keeps the historical behavior — the `SQLite`
    /// short-circuit must not swallow a real connectivity failure.
    #[test]
    fn db_connectivity_postgres_target_still_evaluated() {
        let result =
            check_db_role_connectivity("primary", "postgres://db:5432/app", |_, _| false, false);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    /// Finding F19: `doctor` must agree with `DatabaseConfig::validate`, which
    /// rejects any config that pairs `SQLite` with a read replica or mixes
    /// backends. A `sqlite://` target that is NOT the bootable lone primary must
    /// Fail (not blanket-Pass), so `doctor --strict` cannot greenlight a config
    /// that cannot boot.
    #[test]
    fn db_connectivity_sqlite_replica_or_mixed_backend_fails() {
        // A SQLite replica is never bootable (read replicas require postgres).
        let replica = check_db_role_connectivity(
            "replica",
            "sqlite://replica.db",
            |_, _| panic!("SQLite target must not attempt TCP reachability"),
            false,
        );
        assert_eq!(replica.status, CheckStatus::Fail);
        let detail = replica.detail.unwrap_or_default();
        assert!(detail.contains("replica"), "detail={detail}");
        assert!(detail.contains("same backend"), "detail={detail}");

        // A SQLite primary is not bootable when a replica is configured.
        let primary = check_db_role_connectivity(
            "primary",
            "sqlite://app.db",
            |_, _| panic!("SQLite target must not attempt TCP reachability"),
            false,
        );
        assert_eq!(primary.status, CheckStatus::Fail);
    }

    /// Finding F19: the bootability rule mirrors
    /// `DatabaseConfig::validate_backend_consistency` for the primary/replica
    /// topology — a `sqlite://` primary boots only as the single database role.
    #[test]
    fn sqlite_primary_bootable_only_as_single_role() {
        // Signature: (legacy_url, primary_url, replica_url, has_shards).
        // SQLite primary, no legacy url, no replica, no shards → bootable.
        assert!(sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            None,
            false
        ));
        // SQLite primary + any replica → not bootable (replicas need postgres).
        assert!(!sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            Some("postgres://db:5432/app"),
            false
        ));
        assert!(!sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            Some("sqlite://replica.db"),
            false
        ));
        // Postgres primary (even with a SQLite replica) is never gated by this
        // SQLite-only helper — the replica's own check handles the mismatch.
        assert!(!sqlite_primary_target_is_bootable(
            None,
            Some("postgres://db:5432/app"),
            Some("sqlite://replica.db"),
            false
        ));
        // All-postgres is untouched by the SQLite gate.
        assert!(!sqlite_primary_target_is_bootable(
            None,
            Some("postgres://db:5432/app"),
            Some("postgres://replica:5432/app"),
            false
        ));
    }

    /// Finding F23: the bootability rule must also mirror the shard clause of
    /// `DatabaseConfig::validate_backend_consistency` — a `sqlite://` primary with
    /// any `[[database.shards]]` is rejected at boot (database shards require the
    /// postgres backend), so doctor must Fail it rather than skip the Postgres
    /// probes. A lone `SQLite` primary with no replica and no shards still Passes.
    #[test]
    fn sqlite_primary_with_shards_is_not_bootable() {
        // Signature: (legacy_url, primary_url, replica_url, has_shards).
        // SQLite primary + shards → not bootable (shards need postgres).
        assert!(!sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            None,
            true
        ));
        // Replica AND shards together are likewise not bootable.
        assert!(!sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            Some("sqlite://replica.db"),
            true
        ));
        // Lone SQLite primary (no replica, no shards) stays bootable.
        assert!(sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            None,
            false
        ));
    }

    /// Finding F26: the bootability decision must also mirror the legacy
    /// `database.url` clause of the backend-consistency rule. A config with
    /// `primary_url = "sqlite://…"` while the legacy `database.url` stays
    /// `postgres://…` is a mixed-backend topology `DatabaseConfig::validate`
    /// rejects at boot; doctor collapses to the effective (`SQLite`) primary, so
    /// without threading the legacy URL it would greenlight a config the app
    /// refuses to load. Delegating to `database_backend_consistency` fixes this.
    #[test]
    fn sqlite_primary_with_mismatched_legacy_url_is_not_bootable() {
        // Signature: (legacy_url, primary_url, replica_url, has_shards).
        // SQLite primary + Postgres legacy url → mixed backends, NOT bootable.
        assert!(!sqlite_primary_target_is_bootable(
            Some("postgres://db:5432/app"),
            Some("sqlite://app.db"),
            None,
            false
        ));
        // A clean sqlite:// primary with a redundant sqlite:// legacy url of the
        // same backend still Passes (no mismatch).
        assert!(sqlite_primary_target_is_bootable(
            Some("sqlite://app.db"),
            Some("sqlite://app.db"),
            None,
            false
        ));
        // A clean lone sqlite:// primary (no legacy url, replica, or shards)
        // still Passes.
        assert!(sqlite_primary_target_is_bootable(
            None,
            Some("sqlite://app.db"),
            None,
            false
        ));
    }

    /// Finding F26, end-to-end through the doctor connectivity check: with the
    /// effective primary resolving to `sqlite://` but the legacy `database.url`
    /// naming Postgres, the `SQLite` Pass must be withheld and `check_db_connectivity`
    /// must Fail, matching what `DatabaseConfig::validate` does at boot.
    #[test]
    fn sqlite_primary_with_mismatched_legacy_url_connectivity_fails() {
        let bootable = sqlite_primary_target_is_bootable(
            Some("postgres://db:5432/app"),
            Some("sqlite://app.db"),
            None,
            false,
        );
        assert!(!bootable, "mismatched legacy url must not be bootable");
        let r = check_db_connectivity("sqlite://app.db", bootable);
        assert_eq!(r.status, CheckStatus::Fail);
        // The real boot-time message names the mismatched legacy role and
        // backend; doctor and boot must agree on rejecting it.
        assert_eq!(
            autumn_web::config::database_backend_consistency(
                Some("postgres://db:5432/app"),
                Some("sqlite://app.db"),
                None,
                false,
            ),
            Err(
                "database.url does not match the primary database backend (sqlite); \
                 every configured database role must use the same backend"
                    .to_owned()
            )
        );
    }

    /// Finding F27: the backend-consistency check is UNCONDITIONAL — it must Fail
    /// a mismatched config whatever the effective primary backend, so a reachable
    /// Postgres primary can't greenlight `doctor --strict` for a config
    /// `DatabaseConfig::validate` rejects at boot. This is the mirror image of F26.
    #[test]
    fn backend_consistency_check_catches_mismatch_for_any_primary() {
        // Signature: (legacy_url, primary_url, replica_url, has_shards).

        // F27: Postgres primary + SQLite legacy `database.url` → mixed backends.
        // The SQLite-only bootability gate never fires (primary is Postgres), so
        // this unconditional check is what catches it.
        let r = check_database_backend_consistency(
            Some("sqlite://app.db"),
            Some("postgres://db:5432/app"),
            None,
            false,
        );
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "F27: pg primary + sqlite legacy"
        );
        assert!(
            r.detail.unwrap_or_default().contains("backend"),
            "the FAIL detail must explain the backend mismatch"
        );

        // F26 (mirror image): SQLite primary + Postgres legacy `database.url`.
        let r = check_database_backend_consistency(
            Some("postgres://db:5432/app"),
            Some("sqlite://app.db"),
            None,
            false,
        );
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "F26: sqlite primary + pg legacy"
        );

        // Replica mismatch: Postgres primary + SQLite replica → Fail.
        let r = check_database_backend_consistency(
            None,
            Some("postgres://db:5432/app"),
            Some("sqlite://replica.db"),
            false,
        );
        assert_eq!(r.status, CheckStatus::Fail, "pg primary + sqlite replica");

        // Shard mismatch: SQLite primary + shards → Fail.
        let r = check_database_backend_consistency(None, Some("sqlite://app.db"), None, true);
        assert_eq!(r.status, CheckStatus::Fail, "sqlite primary + shards");

        // Clean all-Postgres (primary + replica, no mismatch) → Pass.
        let r = check_database_backend_consistency(
            None,
            Some("postgres://db:5432/app"),
            Some("postgres://replica:5432/app"),
            false,
        );
        assert_eq!(r.status, CheckStatus::Pass, "clean all-postgres");

        // Clean single SQLite primary (no legacy/replica/shards) → Pass.
        let r = check_database_backend_consistency(None, Some("sqlite://app.db"), None, false);
        assert_eq!(r.status, CheckStatus::Pass, "clean single sqlite");

        // A redundant same-backend legacy `database.url` alongside the primary is
        // consistent → Pass (both directions).
        let r = check_database_backend_consistency(
            Some("postgres://db:5432/app"),
            Some("postgres://db:5432/app"),
            None,
            false,
        );
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "redundant pg legacy == pg primary"
        );
    }

    /// Finding F23: `check_db_connectivity` must Fail (not Pass) for a `SQLite`
    /// primary that is not bootable because shards are present, mirroring
    /// `validate_backend_consistency`. The Fail detail names shards as a
    /// postgres-only topology so the operator sees why the config cannot boot.
    #[test]
    fn sqlite_primary_with_shards_connectivity_fails_and_cites_shards() {
        let r = check_db_connectivity("sqlite://app.db", false);
        assert_eq!(r.status, CheckStatus::Fail);
        let detail = r.detail.unwrap_or_default();
        assert!(detail.contains("shards"), "detail={detail}");
    }

    /// `SQLite` foundation (issue #1614): the pg-client-tools check must not warn
    /// about missing `pg_dump`/`pg_restore` on a `SQLite` app; it Passes and
    /// points at #1909 (`SQLite` backup/restore is not yet wired).
    #[test]
    fn pg_client_tools_sqlite_variant_passes_and_cites_1909() {
        let r = check_pg_client_tools_sqlite();
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.hint.unwrap_or_default().contains("1909"));
    }

    /// `SQLite` foundation (issue #1614), finding F20: the pending-migration
    /// probe must not shell out to `diesel migration pending` or emit the
    /// Postgres `diesel_cli --features postgres` install hint on a `SQLite` app.
    /// The `SQLite` variant Passes with an honest deferred note citing #1906
    /// rather than silently claiming migrations are up to date.
    #[test]
    fn pending_migrations_sqlite_variant_passes_and_cites_1906() {
        let r = check_pending_migrations_sqlite();
        assert_eq!(r.status, CheckStatus::Pass);
        let detail = r.detail.unwrap_or_default();
        assert!(
            detail.contains("not yet wired"),
            "detail must be an honest deferred note, got {detail:?}"
        );
        assert!(
            !detail.to_lowercase().contains("no pending"),
            "must not claim migrations are up to date, got {detail:?}"
        );
        assert!(r.hint.unwrap_or_default().contains("1906"));
    }

    /// Finding F21: doctor must resolve its database topology from the MERGED
    /// active-profile config, not the raw top-level `autumn.toml`. A `sqlite://`
    /// primary supplied ONLY under `[profile.prod.database]` (active via
    /// `AUTUMN_ENV=prod`) must be detected as the `SQLite` target — otherwise the
    /// `pg_dump`/`pg_restore` + pending-migration probes run against a Postgres
    /// assumption. Mirrors `tls_section_in_manifest_dir_config_is_detected`:
    /// builds the merged table from a temp dir (no process-env mutation) and
    /// feeds `resolve_database_topology_from_sources` the way `run()` does.
    #[test]
    fn sqlite_primary_in_active_profile_is_detected_as_sqlite() {
        use autumn_web::config::DatabaseBackend;
        let dir = tempfile::tempdir().unwrap();
        // Base autumn.toml has NO [database]; the URL lives only under the prod
        // profile overlay — exactly the config the raw-table lookup would miss.
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[profile.prod.database]\nprimary_url = \"sqlite://app.db\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));
        // Env returns nothing, so the URL must come from the merged profile table.
        let topology = resolve_database_topology_from_sources(|_| None, Some(&merged));

        assert_eq!(
            topology.primary_url.as_deref(),
            Some("sqlite://app.db"),
            "the [profile.prod.database] primary_url must be resolved from the merged table"
        );
        assert_eq!(
            topology
                .primary_url
                .as_deref()
                .and_then(DatabaseBackend::detect),
            Some(DatabaseBackend::Sqlite),
            "profile-merged SQLite primary must be detected as SQLite, gating pg_client_tools \
             and the pending-migration probe"
        );
        assert!(
            sqlite_primary_target_is_bootable(
                topology.legacy_url.as_deref(),
                topology.primary_url.as_deref(),
                None,
                false
            ),
            "a lone SQLite primary from the active profile is a bootable single-role target"
        );
    }

    /// Finding F21, override-file variant: a `sqlite://` primary supplied only by
    /// the `autumn-prod.toml` override file (selected by `AUTUMN_ENV=prod`) must
    /// likewise be resolved from the merged table.
    #[test]
    fn sqlite_primary_in_override_file_is_detected_as_sqlite() {
        use autumn_web::config::DatabaseBackend;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("autumn.toml"), "[server]\nport = 8080\n").unwrap();
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[database]\nprimary_url = \"sqlite://prod.db\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));
        let topology = resolve_database_topology_from_sources(|_| None, Some(&merged));

        assert_eq!(
            topology
                .primary_url
                .as_deref()
                .and_then(DatabaseBackend::detect),
            Some(DatabaseBackend::Sqlite),
            "autumn-prod.toml SQLite primary must be detected as SQLite"
        );
    }

    /// Finding F21 counter-case: a base-file Postgres app (no profile overlay)
    /// must still resolve as Postgres so doctor keeps running the Postgres
    /// connectivity / `pg_client_tools` / pending-migration checks.
    #[test]
    fn base_file_postgres_primary_still_detected_as_postgres() {
        use autumn_web::config::DatabaseBackend;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://db:5432/app\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));
        let topology = resolve_database_topology_from_sources(|_| None, Some(&merged));

        let backend = topology
            .primary_url
            .as_deref()
            .and_then(DatabaseBackend::detect);
        assert_eq!(
            backend,
            Some(DatabaseBackend::Postgres),
            "a base-file Postgres app must stay Postgres and run the Postgres checks"
        );
        assert!(
            !sqlite_primary_target_is_bootable(
                topology.legacy_url.as_deref(),
                topology.primary_url.as_deref(),
                None,
                false
            ),
            "a Postgres primary is not a SQLite target"
        );
    }

    /// Finding F27, end-to-end through topology resolution: a config with a
    /// Postgres `primary_url` and a mismatched legacy `sqlite://` `database.url`
    /// must resolve BOTH roles from the merged table, and the unconditional
    /// backend-consistency check must Fail — mirroring `DatabaseConfig::validate`,
    /// which rejects this mixed-backend config at boot. Before F27 doctor collapsed
    /// to the (reachable) Postgres primary and never validated the legacy `SQLite`
    /// role, so `doctor --strict` could greenlight a config the app refuses to load.
    #[test]
    fn postgres_primary_with_mismatched_sqlite_legacy_url_fails_consistency() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://db:5432/app\"\nurl = \"sqlite://app.db\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));
        let topology = resolve_database_topology_from_sources(|_| None, Some(&merged));

        // The effective primary collapses to Postgres, but the legacy SQLite role
        // must be threaded through so the consistency check can see the mismatch.
        assert_eq!(
            topology.primary_url.as_deref(),
            Some("postgres://db:5432/app"),
            "primary_url wins for the effective primary"
        );
        assert_eq!(
            topology.legacy_url.as_deref(),
            Some("sqlite://app.db"),
            "the legacy database.url must be resolved separately for the mismatch check"
        );

        // The SQLite-only bootability gate does NOT fire (effective primary is
        // Postgres) — this is exactly why F27 needs the unconditional check.
        assert!(!sqlite_primary_target_is_bootable(
            topology.legacy_url.as_deref(),
            topology.primary_url.as_deref(),
            topology.replica_url.as_deref(),
            false,
        ));

        // The unconditional consistency check catches it.
        let r = check_database_backend_consistency(
            topology.legacy_url.as_deref(),
            topology.primary_url.as_deref(),
            topology.replica_url.as_deref(),
            false,
        );
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "F27: pg primary + sqlite legacy url must Fail the unconditional check"
        );
    }

    /// Finding F28: a MALFORMED `[[database.shards]]` entry (here missing
    /// `primary_url`) on a `sqlite://` primary must Fail doctor, not slip through
    /// as "no shards". The runtime config loader and `autumn migrate` reject the
    /// same entry, so doctor must too. Before F28 the resolver's `Err` collapsed
    /// to `has_shards = false`, leaving the `SQLite` primary a bootable lone-primary
    /// target — the `db_shard_config` Fail is what now blocks that misleading Pass.
    #[test]
    fn sqlite_primary_with_malformed_shard_entry_fails() {
        let dir = tempfile::tempdir().unwrap();
        // `name` present but `primary_url` missing → the resolver rejects it.
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n\n\
             [[database.shards]]\nname = \"shard-a\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));

        // `run()` resolves shard presence with the exact migrate resolver.
        let shard_resolution = crate::migrate::try_resolve_shard_database_urls_from_sources(
            |_| Err(std::env::VarError::NotPresent),
            Some(&merged),
        );
        let err = shard_resolution
            .expect_err("a shard entry missing primary_url must be rejected by the resolver");
        assert!(
            err.contains("primary_url"),
            "the resolver error must name the missing field: {err}"
        );

        // The bug: collapsed to `has_shards = false`, the SQLite primary looks like
        // a bootable lone primary — so doctor MUST instead Fail on the shard config.
        assert!(
            sqlite_primary_target_is_bootable(None, Some("sqlite://app.db"), None, false),
            "demonstrates the F28 trap: with the malformed entry dropped, the SQLite \
             primary would wrongly read as bootable"
        );

        let r = check_shard_config_resolvable(&err);
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "a malformed [[database.shards]] entry must Fail doctor"
        );
        let detail = r.detail.unwrap_or_default();
        assert!(
            detail.contains("database.shards") && detail.contains("primary_url"),
            "the Fail detail must name the malformed shard config: {detail}"
        );
    }

    /// Finding F28 counter-case: a WELL-FORMED `[[database.shards]]` entry on a
    /// `sqlite://` primary must still Fail — via the existing backend-consistency
    /// path (F23/F27), because shards require the postgres backend. The shard
    /// resolver accepts the entry (`db_has_shards = true`), so no `db_shard_config`
    /// Fail fires; the consistency check is what rejects the topology.
    #[test]
    fn sqlite_primary_with_wellformed_shard_still_fails_via_consistency() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n\n\
             [[database.shards]]\nname = \"shard-a\"\nprimary_url = \"sqlite://shard-a.db\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));

        let shards = crate::migrate::try_resolve_shard_database_urls_from_sources(
            |_| Err(std::env::VarError::NotPresent),
            Some(&merged),
        )
        .expect("a well-formed shard entry must resolve");
        assert_eq!(shards.len(), 1, "the well-formed shard must be resolved");

        // No malformed-shard Fail; the shards-require-postgres rule catches it.
        let r = check_database_backend_consistency(None, Some("sqlite://app.db"), None, true);
        assert_eq!(
            r.status,
            CheckStatus::Fail,
            "a SQLite primary with shards must Fail: shards require postgres"
        );
    }

    /// Finding F28 counter-case: a clean single-role `sqlite://` primary with NO
    /// shards must still Pass — the resolver returns an empty shard list, so no
    /// `db_shard_config` Fail fires and the lone `SQLite` primary stays bootable.
    #[test]
    fn clean_sqlite_primary_without_shards_still_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[database]\nprimary_url = \"sqlite://app.db\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));

        let shards = crate::migrate::try_resolve_shard_database_urls_from_sources(
            |_| Err(std::env::VarError::NotPresent),
            Some(&merged),
        )
        .expect("no shard table must resolve to an empty list, not an error");
        assert!(shards.is_empty(), "a shard-free config has no shards");

        let r = check_database_backend_consistency(None, Some("sqlite://app.db"), None, false);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "a lone SQLite primary is a bootable single-role config"
        );
        assert!(
            sqlite_primary_target_is_bootable(None, Some("sqlite://app.db"), None, false),
            "the lone SQLite primary target is bootable"
        );
    }

    /// Finding F28 counter-case: a WELL-FORMED Postgres shard config is unchanged —
    /// the resolver accepts every entry, so no `db_shard_config` Fail fires, and
    /// backend consistency Passes because postgres supports horizontal sharding.
    #[test]
    fn wellformed_postgres_shard_config_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[database]\nprimary_url = \"postgres://db:5432/app\"\n\n\
             [[database.shards]]\nname = \"shard-a\"\nprimary_url = \"postgres://db:5432/a\"\n\n\
             [[database.shards]]\nname = \"shard-b\"\nprimary_url = \"postgres://db:5432/b\"\n",
        )
        .unwrap();

        let base = dir.path().to_path_buf();
        let merged = get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));

        let shards = crate::migrate::try_resolve_shard_database_urls_from_sources(
            |_| Err(std::env::VarError::NotPresent),
            Some(&merged),
        )
        .expect("well-formed postgres shards must resolve without error");
        assert_eq!(shards.len(), 2, "both postgres shards must resolve");

        let r =
            check_database_backend_consistency(None, Some("postgres://db:5432/app"), None, true);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "a postgres primary with shards is a consistent, bootable topology"
        );
    }

    fn temp_tailwind_path(temp: &tempfile::TempDir) -> std::path::PathBuf {
        let tailwind_name = if cfg!(windows) {
            "tailwindcss.exe"
        } else {
            "tailwindcss"
        };
        temp.path()
            .join("target")
            .join("autumn")
            .join(tailwind_name)
    }

    fn create_executable_tailwind_fixture(path: &std::path::Path) {
        let parent = path.parent().expect("tailwind path has parent");
        std::fs::create_dir_all(parent).expect("create tailwind parent");
        std::fs::copy(std::env::current_exe().expect("current test exe"), path)
            .expect("copy executable fixture");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(path)
                .expect("tailwind fixture metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("make tailwind fixture executable");
        }
    }

    #[test]
    fn check_tailwind_expected_path_directory_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        std::fs::create_dir_all(&tailwind_path).expect("create directory at tailwind path");

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.detail.as_deref().unwrap_or("").contains("directory"),
            "detail should identify directory path, got {r:?}"
        );
    }

    #[test]
    fn check_tailwind_not_found() {
        let temp = tempfile::tempdir().expect("temp dir");
        let r = check_tailwind_binary_at(&temp_tailwind_path(&temp));
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("not found"));
        assert!(r.hint.unwrap_or("").contains("autumn setup"));
    }

    #[test]
    fn check_tailwind_regular_file_without_execute_permission_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        std::fs::create_dir_all(tailwind_path.parent().expect("tailwind path has parent"))
            .expect("create tailwind parent");
        std::fs::write(&tailwind_path, "not an executable").expect("write non-executable file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(&tailwind_path)
                .expect("tailwind file metadata")
                .permissions();
            permissions.set_mode(0o644);
            std::fs::set_permissions(&tailwind_path, permissions)
                .expect("clear executable permission");
        }

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("not executable"));
        assert!(r.hint.unwrap_or("").contains("--force"));
    }

    #[test]
    fn check_tailwind_existing_file_fails_when_current_user_cannot_execute() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        create_executable_tailwind_fixture(&tailwind_path);

        let r = check_tailwind_binary_at_with_executable_probe(&tailwind_path, |_, _| false);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("not executable"));
    }

    #[test]
    fn check_tailwind_executable_file_passes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        create_executable_tailwind_fixture(&tailwind_path);

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.as_deref().unwrap_or("").contains("executable"));
    }

    #[cfg(unix)]
    #[test]
    fn check_tailwind_broken_symlink_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        std::fs::create_dir_all(tailwind_path.parent().expect("tailwind path has parent"))
            .expect("create tailwind parent");
        std::os::unix::fs::symlink(temp.path().join("missing-tailwind"), &tailwind_path)
            .expect("create broken symlink");

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("broken symlink"));
    }

    // ── check_signing_secret_impl (RED phase) ────────────────────────────────

    #[test]
    fn check_signing_secret_impl_name_is_signing_secret() {
        let r = check_signing_secret_impl(None, false);
        assert_eq!(r.name, "signing_secret");
    }

    #[test]
    fn check_signing_secret_impl_dev_no_secret_warns() {
        let r = check_signing_secret_impl(None, false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.as_deref().unwrap_or("").contains("ephemeral"));
    }

    #[test]
    fn check_signing_secret_impl_dev_with_valid_secret_passes() {
        let r = check_signing_secret_impl(Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"), false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_signing_secret_impl_prod_missing_fails() {
        let r = check_signing_secret_impl(None, true);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_signing_secret_impl_prod_too_short_fails() {
        let r = check_signing_secret_impl(Some("short"), true);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("short"));
    }

    #[test]
    fn check_signing_secret_impl_prod_demo_value_fails() {
        let r = check_signing_secret_impl(Some("changeme"), true);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_signing_secret_impl_prod_valid_secret_passes() {
        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
        let r = check_signing_secret_impl(Some(secret), true);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_signing_secret_impl_prod_hint_mentions_openssl() {
        let r = check_signing_secret_impl(None, true);
        let hint = r.hint.unwrap_or("");
        assert!(hint.contains("openssl") || hint.contains("rand"));
    }

    #[test]
    fn check_signing_secret_impl_dev_warn_has_hint() {
        let r = check_signing_secret_impl(None, false);
        assert!(r.hint.is_some());
    }

    // ── to_json_output ───────────────────────────────────────────────────────

    #[test]
    fn json_output_contains_checks_and_summary() {
        let results = vec![
            CheckResult {
                name: "rust_toolchain",
                status: CheckStatus::Pass,
                detail: Some("1.88.0".into()),
                hint: None,
            },
            CheckResult {
                name: "port_bindable",
                status: CheckStatus::Fail,
                detail: None,
                hint: Some("hint text"),
            },
        ];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);

        assert!(json.contains("rust_toolchain"));
        assert!(json.contains("port_bindable"));
        assert!(json.contains("\"passed\": 1"));
        assert!(json.contains("\"failed\": 1"));
    }

    #[test]
    fn json_output_valid_json() {
        let results = vec![CheckResult {
            name: "test",
            status: CheckStatus::Pass,
            detail: None,
            hint: None,
        }];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);
        // Should parse as valid JSON
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }

    // ── check_rate_limit_key_strategy ────────────────────────────────────────

    #[test]
    fn rate_limit_key_strategy_ip_always_passes() {
        let r = check_rate_limit_key_strategy_impl("ip", false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_api_token_always_passes() {
        let r = check_rate_limit_key_strategy_impl("api_token", false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_authenticated_principal_without_auth_warns_in_strict() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principal", false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.hint.is_some());
        assert!(
            r.detail.as_deref().unwrap_or("").contains("auth extractor"),
            "detail should mention auth extractor"
        );
    }

    #[test]
    fn rate_limit_key_strategy_authenticated_principal_with_auth_passes() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principal", true);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_name_is_stable() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principal", false);
        assert_eq!(r.name, "rate_limit_key_strategy");
    }

    #[test]
    fn rate_limit_key_strategy_empty_strategy_passes() {
        // Unconfigured / empty strategy is treated as default (ip).
        let r = check_rate_limit_key_strategy_impl("", false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_invalid_value_fails() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principals", false);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.detail
                .as_deref()
                .unwrap_or("")
                .contains("not a valid strategy")
        );
    }

    // ── check_maintenance_mode ────────────────────────────────────────────────

    #[test]
    fn check_maintenance_mode_passes_when_off() {
        // No flag file in the test dir → maintenance is off → Pass
        let result = check_maintenance_mode();
        // In CI there should be no flag file; if there is, accept Warn too.
        assert!(
            result.status == CheckStatus::Pass || result.status == CheckStatus::Warn,
            "unexpected status: {:?}",
            result.status
        );
    }

    // ── check_oauth2 (RED phase) ──────────────────────────────────────────────

    #[test]
    fn check_oauth2_empty_client_id_in_production_fails() {
        let result = check_oauth2_provider_impl("github", "", "real-secret-value", true);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "empty client_id in production must fail: {result:?}",
        );
        assert!(
            result.detail.as_deref().unwrap_or("").contains("client_id"),
            "detail must mention client_id: {:?}",
            result.detail
        );
    }

    #[test]
    fn check_oauth2_empty_client_secret_in_production_fails() {
        let result = check_oauth2_provider_impl("github", "cid", "", true);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "empty client_secret in production must fail: {result:?}",
        );
        assert!(
            result
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("client_secret"),
            "detail must mention client_secret: {:?}",
            result.detail
        );
    }

    #[test]
    fn check_oauth2_empty_client_secret_in_dev_warns() {
        let result = check_oauth2_provider_impl("github", "cid", "", false);
        assert_eq!(
            result.status,
            CheckStatus::Warn,
            "empty client_secret outside production must warn: {result:?}",
        );
    }

    #[test]
    fn check_oauth2_non_empty_client_secret_passes() {
        let result = check_oauth2_provider_impl("github", "cid", "real-secret-value", true);
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "non-empty client_secret must pass: {result:?}",
        );
    }

    #[test]
    fn check_oauth2_provider_name_appears_in_check_name() {
        let result = check_oauth2_provider_impl("google", "cid", "", true);
        assert!(
            result.name.contains("oauth2") || result.name.contains("google"),
            "check name must identify the provider: {}",
            result.name
        );
    }

    #[test]
    fn resolve_oauth2_providers_prefers_env_var_over_empty_toml_secret() {
        let toml: toml::Table = toml::from_str(
            r#"
[auth.oauth2.github]
client_id = "cid"
client_secret = ""
authorize_url = "https://github.com/login/oauth/authorize"
token_url = "https://github.com/login/oauth/access_token"
redirect_uri = "http://localhost/callback"
"#,
        )
        .unwrap();

        let providers = resolve_oauth2_providers_from_sources(
            |key| {
                if key == "AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRET" {
                    Some("ghp_test_secret".to_owned())
                } else {
                    None
                }
            },
            Some(&toml),
            "dev",
        );

        let p = providers.into_iter().find(|p| p.name == "github").unwrap();
        assert_eq!(
            p.client_secret, "ghp_test_secret",
            "env var must override empty TOML client_secret"
        );
    }

    // ── check_compression ────────────────────────────────────────────────────

    #[test]
    fn compression_warns_in_production_when_disabled() {
        let result = check_compression_impl(false, true);
        assert_eq!(result.name, "compression");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
    }

    #[test]
    fn compression_passes_in_production_when_enabled() {
        let result = check_compression_impl(true, true);
        assert_eq!(result.name, "compression");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass, got {:?}",
            result.status
        );
    }

    #[test]
    fn compression_passes_in_dev_when_disabled() {
        let result = check_compression_impl(false, false);
        assert_eq!(result.name, "compression");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass in dev profile, got {:?}",
            result.status
        );
    }

    // ── check_dotenv ─────────────────────────────────────────────────────────

    #[test]
    fn dotenv_warns_when_example_present_without_env() {
        let result = check_dotenv_impl(true, false, &[]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        assert!(result.detail.as_deref().unwrap().contains(".env.example"));
    }

    #[test]
    fn dotenv_warns_when_env_present_but_not_gitignored() {
        let result = check_dotenv_impl(true, true, &[".env".to_owned()]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        let detail = result.detail.as_deref().unwrap();
        assert!(detail.contains("not gitignored"), "got: {detail:?}");
        assert!(detail.contains(".env"), "got: {detail:?}");
    }

    #[test]
    fn dotenv_warns_when_env_local_present_but_not_gitignored() {
        // A `.env.local` with no root `.env` must still warn: the loader reads
        // it and it is always secret.
        let result = check_dotenv_impl(false, false, &[".env.local".to_owned()]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        assert!(
            result.detail.as_deref().unwrap().contains(".env.local"),
            "detail should name the offending file: {:?}",
            result.detail
        );
    }

    #[test]
    fn dotenv_uncovered_secret_takes_priority_over_missing_env() {
        // Combined case: `.env.example` present, no root `.env`, and a present
        // `.env.local` that is NOT gitignored. The ungitignored-secret warning
        // must win (naming the file) rather than being hidden behind the
        // copy-the-template hint.
        let result = check_dotenv_impl(true, false, &[".env.local".to_owned()]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        let detail = result.detail.as_deref().unwrap();
        assert!(
            detail.contains(".env.local"),
            "detail must name the uncovered secret file: {detail:?}"
        );
        assert!(
            detail.contains("not gitignored"),
            "detail must lead with the security issue: {detail:?}"
        );
        // The gitignore hint is the actionable one for the security issue.
        assert!(
            result.hint.unwrap().contains(".gitignore"),
            "hint should point at .gitignore: {:?}",
            result.hint
        );
    }

    #[test]
    fn dotenv_warns_when_profile_local_present_but_not_gitignored() {
        let result = check_dotenv_impl(false, false, &[".env.dev.local".to_owned()]);
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        assert!(
            result.detail.as_deref().unwrap().contains(".env.dev.local"),
            "got: {:?}",
            result.detail
        );
    }

    #[test]
    fn dotenv_passes_when_all_present_secret_files_gitignored() {
        // No uncovered secret files → Pass, even with a root `.env` present.
        let result = check_dotenv_impl(true, true, &[]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass, got {:?}",
            result.status
        );
    }

    #[test]
    fn dotenv_passes_when_nothing_configured() {
        let result = check_dotenv_impl(false, false, &[]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass, got {:?}",
            result.status
        );
    }

    #[test]
    fn is_secret_dotenv_filename_classifies_local_vs_committable() {
        assert!(is_secret_dotenv_filename(".env"));
        assert!(is_secret_dotenv_filename(".env.local"));
        assert!(is_secret_dotenv_filename(".env.dev.local"));
        assert!(is_secret_dotenv_filename(".env.production.local"));
        // Committable per-profile (non-local) files are NOT secret.
        assert!(!is_secret_dotenv_filename(".env.dev"));
        assert!(!is_secret_dotenv_filename(".env.production"));
        // The template is not itself a secret.
        assert!(!is_secret_dotenv_filename(".env.example"));
        assert!(!is_secret_dotenv_filename("env.local"));
    }

    #[test]
    fn gitignore_covers_truth_table() {
        // `.env` covers `.env` but NOT `.env.local`.
        assert!(gitignore_covers(".env\n", ".env"));
        assert!(!gitignore_covers(".env\n", ".env.local"));
        // `.env.*` covers `.env.local` but NOT `.env`.
        assert!(gitignore_covers(".env.*\n", ".env.local"));
        assert!(!gitignore_covers(".env.*\n", ".env"));
        // `.env*` covers both.
        assert!(gitignore_covers(".env*\n", ".env"));
        assert!(gitignore_covers(".env*\n", ".env.local"));
        // `*.local` and `.env.*.local` cover `.env.dev.local`.
        assert!(gitignore_covers("*.local\n", ".env.dev.local"));
        assert!(gitignore_covers(".env.*.local\n", ".env.dev.local"));
        // `.env.example` covers neither `.env` nor `.env.local`.
        assert!(!gitignore_covers(".env.example\n", ".env"));
        assert!(!gitignore_covers(".env.example\n", ".env.local"));
        // `**/.env` covers `.env`.
        assert!(gitignore_covers("**/.env\n", ".env"));
        // Leading `/` anchor and surrounding whitespace.
        assert!(gitignore_covers("/target\n.env\n", ".env"));
        assert!(gitignore_covers("  .env  \n", ".env"));
        // Comment, blank, and negation lines never count as coverage.
        assert!(!gitignore_covers("# .env\n\n", ".env"));
        assert!(!gitignore_covers("!.env\n", ".env"));
        assert!(!gitignore_covers("", ".env"));
        // A path pattern in a subdirectory does not cover a root-level file.
        assert!(!gitignore_covers("config/.env\n", ".env"));
        // A directory-only pattern does not cover a regular file.
        assert!(!gitignore_covers(".env/\n", ".env"));
        // Last-match-wins with `!` negations (gitignore precedence): a `!.env`
        // re-include after a broader ignore means `.env` is NOT covered.
        assert!(!gitignore_covers(".env*\n!.env\n", ".env"));
        assert!(!gitignore_covers(".env\n!.env\n", ".env"));
        // `.env*` still covers a sibling the negation does not re-include.
        assert!(gitignore_covers(".env*\n!.env\n", ".env.local"));
        // Order matters: a later ignore re-covers a file an earlier `!` freed.
        assert!(gitignore_covers("!.env\n.env\n", ".env"));
    }

    #[test]
    fn parse_config_bool_handles_false_values() {
        assert_eq!(parse_config_bool("false"), Some(false));
        assert_eq!(parse_config_bool("0"), Some(false));
        assert_eq!(parse_config_bool("no"), Some(false));
        assert_eq!(parse_config_bool("off"), Some(false));
        assert_eq!(parse_config_bool("true"), Some(true));
        assert_eq!(parse_config_bool("1"), Some(true));
        assert_eq!(parse_config_bool("yes"), Some(true));
        assert_eq!(parse_config_bool("on"), Some(true));
        assert_eq!(parse_config_bool("garbage"), None);
    }

    // ── check_proxy_conflict ─────────────────────────────────────────────────

    #[test]
    fn proxy_conflict_passes_when_only_new_fields_set() {
        let data = ProxyConflictData {
            new_ranges: vec!["10.0.0.0/8".into()],
            new_trust_fwd: true,
            new_hops: None,
            old_ranges: vec![],
            old_trust_fwd: false,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn proxy_conflict_passes_when_only_old_fields_set() {
        let data = ProxyConflictData {
            new_ranges: vec![],
            new_trust_fwd: false,
            new_hops: None,
            old_ranges: vec!["10.0.0.0/8".into()],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn proxy_conflict_warns_when_both_set_with_different_ranges() {
        let data = ProxyConflictData {
            new_ranges: vec!["10.0.0.0/8".into()],
            new_trust_fwd: true,
            new_hops: None,
            old_ranges: vec!["192.168.0.0/16".into()],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.hint.is_some());
    }

    #[test]
    fn proxy_conflict_warns_when_trusted_hops_set_alongside_old_fields() {
        let data = ProxyConflictData {
            new_ranges: vec![],
            new_trust_fwd: true,
            new_hops: Some(1),
            old_ranges: vec![],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn proxy_conflict_passes_when_matching_values_no_hops() {
        let data = ProxyConflictData {
            new_ranges: vec!["10.0.0.0/8".into()],
            new_trust_fwd: true,
            new_hops: None,
            old_ranges: vec!["10.0.0.0/8".into()],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    // ── system_test_browser ───────────────────────────────────────────────────

    #[test]
    fn browser_check_not_found_is_warn_not_fail() {
        // Simulate: no browser in an empty candidate list.  The check must
        // return Warn so projects that don't use system tests aren't penalized.
        let result = check_system_test_browser();
        // Accept Pass (if Chrome is on the host) or Warn (if not).
        assert!(
            result.status == CheckStatus::Pass || result.status == CheckStatus::Warn,
            "browser check must be Pass or Warn, got {:?}",
            result.status
        );
    }

    #[test]
    fn browser_candidate_paths_includes_common_locations() {
        let paths = browser_candidate_paths();
        let as_strs: Vec<_> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            as_strs
                .iter()
                .any(|s| s.contains("chromium") || s.contains("chrome")),
            "candidate list must include common Chrome paths; got {as_strs:?}"
        );
    }

    #[test]
    fn browser_check_hint_mentions_apt_get() {
        let result = check_system_test_browser();
        if result.status == CheckStatus::Warn {
            let hint = result.hint.unwrap_or("");
            assert!(
                hint.contains("apt-get") || hint.contains("AUTUMN_CHROMIUM"),
                "hint must mention install command; got: {hint}"
            );
        }
    }

    // ── cargo_toml_features_has_key ───────────────────────────────────────────

    #[test]
    fn features_has_key_detects_bare_key() {
        let toml = "[features]\nsystem-tests = [\"autumn-web/system-tests\"]\n";
        assert!(cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_detects_quoted_key() {
        let toml = "[features]\n\"system-tests\" = [\"autumn-web/system-tests\"]\n";
        assert!(cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_ignores_dev_dependency_mention() {
        // The key appears in [dev-dependencies] but NOT in [features].
        let toml = "[dev-dependencies]\nautumn-web = { features = [\"system-tests\"] }\n";
        assert!(!cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_no_features_section() {
        let toml = "[package]\nname = \"x\"\n";
        assert!(!cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_commented_header() {
        let toml = "[features] # project features\nsystem-tests = []\n";
        assert!(cargo_toml_features_has_key(toml, "system-tests"));
    }

    // ── check_gdpr_export_registration ───────────────────────────────────────

    #[test]
    fn gdpr_check_passes_when_no_auth_starter() {
        let r = check_gdpr_export_registration_impl(false, &[]);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "no auth starter → should always pass: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_passes_when_no_auth_starter_even_with_unregistered_tables() {
        let r = check_gdpr_export_registration_impl(false, &["posts", "comments"]);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "no auth starter → must not warn about unregistered tables: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_passes_when_all_tables_registered() {
        let r = check_gdpr_export_registration_impl(true, &[]);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "auth starter present + all tables registered → should pass: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_warns_when_unregistered_tables_present() {
        let r = check_gdpr_export_registration_impl(true, &["posts", "comments"]);
        assert_eq!(
            r.status,
            CheckStatus::Warn,
            "unregistered tables with auth starter must warn: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_name_is_stable() {
        let r = check_gdpr_export_registration_impl(false, &[]);
        assert_eq!(r.name, "gdpr_export_registration");
    }

    #[test]
    fn gdpr_check_detail_lists_unregistered_table_names() {
        let r = check_gdpr_export_registration_impl(true, &["posts", "comments"]);
        let detail = r.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("posts"),
            "detail must mention unregistered table 'posts': {detail}"
        );
        assert!(
            detail.contains("comments"),
            "detail must mention unregistered table 'comments': {detail}"
        );
    }

    #[test]
    fn gdpr_check_hint_references_registry_api() {
        let r = check_gdpr_export_registration_impl(true, &["orders"]);
        let hint = r.hint.unwrap_or("");
        assert!(
            hint.contains("GdprRegistry") || hint.contains("gdpr"),
            "hint must reference the GdprRegistry API: {hint}"
        );
    }

    #[test]
    fn gdpr_check_detail_mentions_count_of_unregistered_tables() {
        let r = check_gdpr_export_registration_impl(true, &["t1", "t2", "t3"]);
        let detail = r.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains('3') || detail.contains("3 "),
            "detail must mention the count of unregistered tables: {detail}"
        );
    }

    // ── scan_source_for_gdpr_registrations / extract_quoted_table_name ─────────

    #[test]
    fn scanner_detects_single_line_hard_delete() {
        let src = r#"registry.register(ModelRegistration::hard_delete("posts"));"#;
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("posts"),
            "should detect single-line hard_delete: {found:?}"
        );
    }

    #[test]
    fn scanner_detects_multiline_retain_call() {
        let src = "registry.register(ModelRegistration::retain(\n    \"invoices\",\n    \"7-year financial hold\",\n));";
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("invoices"),
            "should detect multi-line retain registration: {found:?}"
        );
    }

    #[test]
    fn scanner_detects_multiline_anonymize_call() {
        let src = "registry.register(ModelRegistration::anonymize(\n    \"comments\",\n));";
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("comments"),
            "should detect multi-line anonymize registration: {found:?}"
        );
    }

    #[test]
    fn scanner_ignores_string_before_token() {
        let src = r#"let _x = "not_a_table"; ModelRegistration::hard_delete("real_table");"#;
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("real_table"),
            "should contain real_table: {found:?}"
        );
        assert!(
            !found.contains("not_a_table"),
            "should not contain string before token: {found:?}"
        );
    }

    // ── Deprecated-keys check tests ───────────────────────────────────────────

    #[test]
    fn red_check_deprecated_keys_empty_is_pass() {
        let result = check_deprecated_keys_impl(&[]);
        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.name, "deprecated_keys");
    }

    #[test]
    fn red_check_deprecated_keys_warns_one_line_per_key() {
        let found = vec![
            DoctorDeprecation {
                path: "security.rate_limit.trusted_proxies".into(),
                replacement: Some("security.trusted_proxies.ranges".into()),
                since: "0.5.0".into(),
                remove_in: "1.0.0".into(),
            },
            DoctorDeprecation {
                path: "security.rate_limit.trust_forwarded_headers".into(),
                replacement: Some("security.trusted_proxies.trust_forwarded_headers".into()),
                since: "0.5.0".into(),
                remove_in: "1.0.0".into(),
            },
        ];
        let result = check_deprecated_keys_impl(&found);
        assert_eq!(result.status, CheckStatus::Warn);
        assert_eq!(result.name, "deprecated_keys");
        let detail = result.detail.unwrap();
        let lines: Vec<&str> = detail.lines().collect();
        assert_eq!(lines.len(), 2, "one line per key: {detail:?}");
        assert!(lines[0].contains("security.rate_limit.trusted_proxies"));
        assert!(lines[0].contains("security.trusted_proxies.ranges"));
        assert!(
            lines[0].contains("0.5.0"),
            "detail must include since version"
        );
        assert!(lines[0].contains("1.0.0"));
        assert!(lines[1].contains("security.rate_limit.trust_forwarded_headers"));
    }

    #[test]
    fn red_check_deprecated_keys_json_includes_detail() {
        let found = vec![DoctorDeprecation {
            path: "security.rate_limit.trusted_proxies".into(),
            replacement: Some("security.trusted_proxies.ranges".into()),
            since: "0.5.0".into(),
            remove_in: "1.0.0".into(),
        }];
        let results = vec![check_deprecated_keys_impl(&found)];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);
        assert!(
            json.contains("\"deprecated_keys\""),
            "JSON must name the check"
        );
        assert!(json.contains("\"warn\""), "JSON must include warn status");
        assert!(
            json.contains("security.rate_limit.trusted_proxies"),
            "JSON must contain the deprecated key path"
        );
    }

    #[test]
    fn deploy_doctor_config_absent_is_skipped() {
        use autumn_web::config::MockEnv;
        // No [deploy] table and no AUTUMN_DEPLOY__* env → no config parsed, no
        // failing check (graceful skip).
        let table: toml::Table = "".parse().unwrap();
        let (cfg, check) = resolve_deploy_doctor_config(&table, &MockEnv::new());
        assert!(cfg.is_none(), "absent [deploy] must not yield a config");
        assert!(
            check.is_none(),
            "absent [deploy] must not emit a failing check"
        );
    }

    #[test]
    fn deploy_doctor_config_valid_parses() {
        use autumn_web::config::MockEnv;
        let table: toml::Table = "[deploy]\nhost = \"203.0.113.10\"\nssh_port = 2222\n"
            .parse()
            .unwrap();
        let (cfg, check) = resolve_deploy_doctor_config(&table, &MockEnv::new());
        assert!(check.is_none(), "valid [deploy] must not emit a fail check");
        let cfg = cfg.expect("valid [deploy] must parse into a config");
        assert_eq!(cfg.host.as_deref(), Some("203.0.113.10"));
        assert_eq!(cfg.ssh_port, 2222);
    }

    #[test]
    fn deploy_doctor_config_env_only_host_materializes_config() {
        use autumn_web::config::MockEnv;
        // No [deploy] in TOML, but AUTUMN_DEPLOY__HOST is set: doctor must
        // materialize a deploy config (Some) so the preflight runs against that
        // host instead of being skipped — matching `autumn deploy check`, which
        // grades `AutumnConfig::load().deploy` (env applied).
        let table: toml::Table = "".parse().unwrap();
        let env = MockEnv::new()
            .with("AUTUMN_DEPLOY__HOST", "203.0.113.99")
            .with("AUTUMN_DEPLOY__SSH_PORT", "2201");
        let (cfg, check) = resolve_deploy_doctor_config(&table, &env);
        assert!(
            check.is_none(),
            "env-only deploy must not emit a fail check"
        );
        let cfg = cfg.expect("env-only AUTUMN_DEPLOY__HOST must materialize a config");
        assert_eq!(cfg.host.as_deref(), Some("203.0.113.99"));
        assert_eq!(cfg.ssh_port, 2201);
    }

    #[test]
    fn deploy_doctor_config_env_overrides_toml_host() {
        use autumn_web::config::MockEnv;
        // A TOML host is present, but AUTUMN_DEPLOY__HOST wins — doctor grades
        // the same effective target as `autumn deploy check`.
        let table: toml::Table = "[deploy]\nhost = \"toml.example\"\nssh_port = 22\n"
            .parse()
            .unwrap();
        let env = MockEnv::new().with("AUTUMN_DEPLOY__HOST", "env.example");
        let (cfg, check) = resolve_deploy_doctor_config(&table, &env);
        assert!(check.is_none());
        let cfg = cfg.expect("config must be present");
        assert_eq!(
            cfg.host.as_deref(),
            Some("env.example"),
            "AUTUMN_DEPLOY__HOST must override the TOML host"
        );
        // Unset env fields fall back to the TOML value.
        assert_eq!(cfg.ssh_port, 22);
    }

    #[test]
    fn deploy_doctor_config_bad_type_fails_not_skipped() {
        use autumn_web::config::MockEnv;
        // `ssh_port` as a string is a present-but-invalid table: the old `.ok()`
        // path silently skipped ALL deploy checks; now it must FAIL loudly.
        let table: toml::Table = "[deploy]\nhost = \"h\"\nssh_port = \"22\"\n"
            .parse()
            .unwrap();
        let (cfg, check) = resolve_deploy_doctor_config(&table, &MockEnv::new());
        assert!(
            cfg.is_none(),
            "an invalid [deploy] must not yield a config to preflight"
        );
        let check = check.expect("an invalid [deploy] must emit a failing check");
        assert_eq!(check.name, "deploy_config");
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(
            check.detail.unwrap().contains("present but invalid"),
            "detail must explain the [deploy] table is present but invalid"
        );
    }

    /// Issue found via #1966 review: doctor must resolve the deploy SIGNING
    /// SECRET under the `[deploy] profile`, NOT the ambient CLI runtime profile
    /// (default `dev`). A secret supplied only under `[profile.prod]` is invisible
    /// to the ambient (dev) resolution, so grading it there falsely reports it
    /// MISSING even though `autumn deploy check` (fixed in #1966) resolves it.
    /// This mirrors `run()`'s two-phase deploy resolution the way the F21 DB tests
    /// mirror `run()`'s DB resolution — via the injectable
    /// `get_merged_toml_table_runtime_with`, no process-env mutation.
    #[test]
    fn deploy_signing_secret_in_deploy_profile_is_resolved_under_that_profile() {
        use autumn_web::config::MockEnv;
        let dir = tempfile::tempdir().unwrap();
        // Base declares `[deploy] profile = "prod"`; the signing secret lives ONLY
        // under `[profile.prod]` — exactly the value the ambient (dev) lookup misses.
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[deploy]\nhost = \"deploy.example.com\"\nprofile = \"prod\"\n\
             [profile.prod.security.signing_secret]\n\
             secret = \"prod-only-32-char-strong-secret-value-xyz\"\n",
        )
        .unwrap();
        let base = dir.path().to_path_buf();

        // PHASE 1 (unchanged): learn the deploy target profile from the `[deploy]`
        // table resolved under the AMBIENT (dev) merged table, as `run()` does.
        let base_dev = base.clone();
        let ambient_merged =
            get_merged_toml_table_runtime_with("dev", "dev", move |f| base_dev.join(f));
        let (deploy_cfg, config_check) =
            resolve_deploy_doctor_config(&ambient_merged, &MockEnv::new());
        assert!(
            config_check.is_none(),
            "the [deploy] table parses under the ambient profile"
        );
        let deploy_cfg = deploy_cfg.expect("[deploy] present");
        assert_eq!(
            crate::deploy::trimmed_deploy_profile(&deploy_cfg.profile),
            "prod",
            "deploy target profile is learned from the ambient [deploy] table"
        );

        // Regression guard (the BUG): resolving the secret under the ambient (dev)
        // merged table does NOT see the prod-only secret — this is what falsely
        // reported it missing before the fix.
        assert_eq!(
            resolve_deploy_signing_secret(&ambient_merged, &MockEnv::new()),
            None,
            "prod-only secret is invisible under the ambient dev profile (the bug)"
        );

        // PHASE 2 (the FIX): re-derive the merged table under the `[deploy] profile`
        // and resolve the secret there — the exact re-derivation `run()` now does.
        let deploy_profile_raw = crate::deploy::trimmed_deploy_profile(&deploy_cfg.profile);
        let deploy_profile_canonical =
            crate::deploy::canonicalize_deploy_profile(&deploy_profile_raw);
        let deploy_merged = get_merged_toml_table_runtime_with(
            &deploy_profile_canonical,
            &deploy_profile_raw,
            move |f| base.join(f),
        );
        assert_eq!(
            resolve_deploy_signing_secret(&deploy_merged, &MockEnv::new()).as_deref(),
            Some("prod-only-32-char-strong-secret-value-xyz"),
            "the fix: doctor resolves the deploy signing secret under the [deploy] profile"
        );
    }

    /// Companion to the signing-secret case for the deploy DATABASE URL: a
    /// `primary_url` supplied only under `[profile.prod.database]` must be resolved
    /// under the `[deploy] profile`, not the ambient (dev) profile — otherwise
    /// `deploy_database_url` falsely fails under `--strict` even though `autumn
    /// deploy check` (fixed in #1966) resolves it.
    #[test]
    fn deploy_database_url_in_deploy_profile_is_resolved_under_that_profile() {
        use autumn_web::config::MockEnv;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[deploy]\nhost = \"deploy.example.com\"\nprofile = \"prod\"\n\
             [profile.prod.database]\n\
             primary_url = \"postgres://prod-host/appdb\"\n",
        )
        .unwrap();
        let base = dir.path().to_path_buf();

        // PHASE 1: learn the deploy profile from the ambient `[deploy]` table.
        let base_dev = base.clone();
        let ambient_merged =
            get_merged_toml_table_runtime_with("dev", "dev", move |f| base_dev.join(f));
        let (deploy_cfg, _) = resolve_deploy_doctor_config(&ambient_merged, &MockEnv::new());
        let deploy_cfg = deploy_cfg.expect("[deploy] present");

        // Bug parity: the prod-only DB URL is invisible under the ambient dev merge.
        let ambient_topology =
            resolve_database_topology_from_sources(|_| None, Some(&ambient_merged));
        assert_eq!(
            ambient_topology.primary_url, None,
            "prod-only DB URL is invisible under the ambient dev profile (the bug)"
        );

        // PHASE 2 (the FIX): resolve the DB topology under the `[deploy] profile`.
        let deploy_profile_raw = crate::deploy::trimmed_deploy_profile(&deploy_cfg.profile);
        let deploy_profile_canonical =
            crate::deploy::canonicalize_deploy_profile(&deploy_profile_raw);
        let deploy_merged = get_merged_toml_table_runtime_with(
            &deploy_profile_canonical,
            &deploy_profile_raw,
            move |f| base.join(f),
        );
        let deploy_topology =
            resolve_database_topology_from_sources(|_| None, Some(&deploy_merged));
        assert_eq!(
            deploy_topology.primary_url.as_deref(),
            Some("postgres://prod-host/appdb"),
            "the fix: doctor resolves the deploy DB URL under the [deploy] profile"
        );
    }

    /// Negative/parity: a signing secret present ONLY under the AMBIENT (dev)
    /// profile — while `[deploy] profile = "prod"` — must NOT be what the deploy
    /// grade keys on. The deploy grade reflects the `[deploy] profile`'s config, so
    /// resolving under prod does not see the dev-only value. This proves the fix
    /// keys strictly on the deploy profile, not a leaky ambient value.
    #[test]
    fn deploy_signing_secret_only_under_ambient_profile_is_not_keyed_by_deploy_grade() {
        use autumn_web::config::MockEnv;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[deploy]\nhost = \"deploy.example.com\"\nprofile = \"prod\"\n\
             [profile.dev.security.signing_secret]\n\
             secret = \"dev-only-secret-not-for-prod-grade-xyz\"\n",
        )
        .unwrap();
        let base = dir.path().to_path_buf();

        // The deploy grade resolves under the `[deploy] profile` (prod), so the
        // dev-only secret is NOT seen — the deploy grade reflects prod config only.
        let deploy_merged =
            get_merged_toml_table_runtime_with("prod", "prod", move |f| base.join(f));
        assert_eq!(
            resolve_deploy_signing_secret(&deploy_merged, &MockEnv::new()),
            None,
            "a dev-only secret is not keyed by the deploy grade under the prod profile"
        );
    }

    /// Codex review (P2): doctor's deploy value graders build the merged TOML via
    /// `get_merged_toml_table_runtime`, whose profile-override read uses `.ok()`,
    /// so a MALFORMED `autumn-<profile>.toml` is silently dropped — doctor would
    /// then grade only the base values and PASS `--strict`, while `autumn deploy
    /// check` (which loads the config under the deploy profile) FAILS on the same
    /// file. `deploy_profile_config_load_check` closes that gap:
    /// a malformed deploy-profile override FAILS the `deploy_config` check; a
    /// well-formed one does NOT newly fail. Uses an `AUTUMN_MANIFEST_DIR`-scoped
    /// `MockEnv` (the same mechanism the runtime's own config tests use) so
    /// `load_with_env` reads the temp-dir files with no process-env mutation.
    #[test]
    fn malformed_deploy_profile_override_fails_deploy_config_load_check() {
        use autumn_web::config::MockEnv;
        let dir = tempfile::tempdir().unwrap();
        // `[deploy] profile = "prod"`, ambient = dev. The prod override file is
        // present but MALFORMED (invalid TOML) — exactly the case
        // `get_merged_toml_table_runtime`'s `.ok()` merge silently drops.
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[deploy]\nhost = \"deploy.example.com\"\nprofile = \"prod\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "this line has no equals sign and is not valid toml\n",
        )
        .unwrap();

        // Resolve config files from the temp dir under the prod (deploy) profile
        // without mutating the process env — the same mechanism the runtime's
        // config tests use (`AUTUMN_MANIFEST_DIR` selects the dir, `AUTUMN_ENV`
        // selects the profile that pulls in `autumn-prod.toml`).
        let env = MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap())
            .with("AUTUMN_ENV", "prod");

        let check = deploy_profile_config_load_check("prod", &env)
            .expect("a malformed autumn-prod.toml must fail the deploy_config load check");
        assert_eq!(check.name, "deploy_config");
        assert!(
            matches!(check.status, CheckStatus::Fail),
            "the malformed override must FAIL, not pass"
        );
        let detail = check.detail.unwrap_or_default();
        assert!(
            detail.contains("prod"),
            "the failure names the deploy profile: {detail}"
        );
        assert!(
            detail.contains("failed to load"),
            "the failure surfaces the load error: {detail}"
        );

        // Companion: a WELL-FORMED prod override under the same layout does NOT
        // trigger this new failure — the normal case still passes (mirroring how
        // `autumn deploy check` loads it without error).
        std::fs::write(
            dir.path().join("autumn-prod.toml"),
            "[server]\nport = 8443\n",
        )
        .unwrap();
        assert!(
            deploy_profile_config_load_check("prod", &env).is_none(),
            "a well-formed autumn-prod.toml must not newly fail the deploy_config check"
        );
    }

    /// Codex review (P2, #2067): the deploy CLI now loads config leniently toward
    /// unknown top-level roots (`deploy.rs::load_runtime_config` uses
    /// `AutumnConfig::load_with_env_lenient_unknown_roots`), so `autumn deploy
    /// check`/`up` accept a plugin-owned root like `[media]` under `strict_config`
    /// and defer to app boot. `deploy_profile_config_load_check` must mirror that
    /// SAME lenient loader, or `doctor --strict` would turn the very config
    /// `deploy check` passes into a FAILING `deploy_config` check — the two
    /// deploy workflows disagreeing. This asserts a `[media]` + `strict_config`
    /// project does NOT fail the doctor deploy-config guard, while a typo inside a
    /// KNOWN section (`[database] primry_url`) still FAILS it (leniency is scoped
    /// to unknown top-level roots only, matching the loader-level guarantee).
    #[test]
    fn plugin_owned_root_does_not_fail_deploy_config_load_check_but_known_typo_does() {
        use autumn_web::config::MockEnv;
        let dir = tempfile::tempdir().unwrap();
        // `[deploy] profile = "prod"`, `strict_config` on, plus a plugin-owned
        // `[media]` top-level root the CLI cannot know about — exactly the case
        // #2067 made lenient for `autumn deploy check`.
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n\
             [deploy]\nhost = \"deploy.example.com\"\nprofile = \"prod\"\n\n\
             [media]\nmediamtx_host = \"cdn.example\"\n",
        )
        .unwrap();
        let env = MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap())
            .with("AUTUMN_ENV", "prod");

        // The plugin root must NOT fail the doctor deploy-config guard — this is
        // the config `autumn deploy check` passes, so `doctor --strict` must agree.
        assert!(
            deploy_profile_config_load_check("prod", &env).is_none(),
            "a plugin-owned [media] top-level root under strict_config must not fail \
             the deploy_config load check (it passes `autumn deploy check`)"
        );

        // But a typo INSIDE a known/core section stays fatal in the lenient loader,
        // so the doctor guard still FAILS it — exactly as `autumn deploy check` does.
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nstrict_config = true\n\n\
             [deploy]\nhost = \"deploy.example.com\"\nprofile = \"prod\"\n\n\
             [database]\nprimry_url = \"postgres://localhost/db\"\n",
        )
        .unwrap();
        let check = deploy_profile_config_load_check("prod", &env)
            .expect("a known-section typo must still fail the deploy_config load check");
        assert_eq!(check.name, "deploy_config");
        assert!(
            matches!(check.status, CheckStatus::Fail),
            "the known-section typo must FAIL, not pass"
        );
    }

    #[test]
    fn deploy_previous_secrets_non_string_element_is_malformed() {
        // `previous_secrets = [123]` deserializes as `Vec<String>` failing in
        // `AutumnConfig::load()`; doctor must treat it as malformed, not drop it.
        let table: toml::Table = "[security.signing_secret]\nprevious_secrets = [123]\n"
            .parse()
            .unwrap();
        assert!(
            resolve_deploy_previous_signing_secrets(&table).is_err(),
            "a non-string previous_secrets element must be malformed"
        );
    }

    #[test]
    fn deploy_previous_secrets_non_array_is_malformed() {
        // A scalar `previous_secrets` cannot deserialize into `Vec<String>`.
        let table: toml::Table = "[security.signing_secret]\nprevious_secrets = \"nope\"\n"
            .parse()
            .unwrap();
        assert!(
            resolve_deploy_previous_signing_secrets(&table).is_err(),
            "a non-array previous_secrets must be malformed"
        );
    }

    #[test]
    fn deploy_previous_secrets_absent_or_strings_ok() {
        // Absent key → empty vec.
        let empty: toml::Table = toml::Table::new();
        assert_eq!(
            resolve_deploy_previous_signing_secrets(&empty).unwrap(),
            Vec::<String>::new()
        );
        // Array of strings → collected; empty strings are NOT rejected (load()
        // accepts them; value-validation is downstream in the grader).
        let table: toml::Table = "[security.signing_secret]\nprevious_secrets = [\"a\", \"\"]\n"
            .parse()
            .unwrap();
        assert_eq!(
            resolve_deploy_previous_signing_secrets(&table).unwrap(),
            vec!["a".to_owned(), String::new()]
        );
    }

    #[test]
    fn deploy_doctor_config_negative_keep_releases_fails() {
        use autumn_web::config::MockEnv;
        // `keep_releases = -1` cannot deserialize into u32 → fail, not skip.
        let table: toml::Table = "[deploy]\nhost = \"h\"\nkeep_releases = -1\n"
            .parse()
            .unwrap();
        let (cfg, check) = resolve_deploy_doctor_config(&table, &MockEnv::new());
        assert!(cfg.is_none());
        assert_eq!(
            check.expect("negative keep_releases must fail").status,
            CheckStatus::Fail
        );
    }
    // ── `plugin_residue` (issue #1631, AC #7) ────────────────────────────────

    fn wiring(crate_name: &str, dependency: bool, mount: bool) -> PluginWiring {
        PluginWiring {
            crate_name: crate_name.to_owned(),
            community: false,
            dependency,
            mount,
            mount_qualified: mount,
            orphaned_migrations: Vec::new(),
        }
    }

    #[test]
    fn plugin_residue_passes_when_every_plugin_is_wired_consistently() {
        let result = check_plugin_residue_impl(&[
            wiring("autumn-admin-plugin", true, true),
            wiring("autumn-search", false, false),
        ]);
        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.name, "plugin_residue");
    }

    /// A dependency with no mount is the residue `plugin remove` exists to
    /// prevent, and what a half-finished manual install leaves behind.
    #[test]
    fn plugin_residue_warns_on_a_dependency_with_no_mount() {
        let result = check_plugin_residue_impl(&[wiring("autumn-admin-plugin", true, false)]);
        assert_eq!(result.status, CheckStatus::Warn);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("autumn-admin-plugin"), "{detail}");
        assert!(detail.contains("mount"), "{detail}");
    }

    /// A mount with no dependency does not compile — that is a failure, not a
    /// warning, whatever `--strict` is set to.
    #[test]
    fn plugin_residue_fails_on_a_mount_with_no_dependency() {
        let result = check_plugin_residue_impl(&[wiring("autumn-search", false, true)]);
        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("autumn-search"), "{detail}");
    }

    /// The third residue kind: the plugin is gone from the code, but its
    /// migrations are still recorded as applied.
    #[test]
    fn plugin_residue_warns_on_migrations_whose_plugin_is_gone() {
        let result = check_plugin_residue_impl(&[PluginWiring {
            crate_name: "autumn-media-plugin".to_owned(),
            community: false,
            dependency: false,
            mount: false,
            mount_qualified: false,
            orphaned_migrations: vec!["20260720000000".to_owned()],
        }]);
        assert_eq!(result.status, CheckStatus::Warn);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("20260720000000"), "{detail}");
        assert!(detail.contains("autumn-media-plugin"), "{detail}");
    }

    /// A plugin that is still installed is not residue, however many of its
    /// migrations are applied — that is just a working install.
    #[test]
    fn plugin_residue_ignores_migrations_of_an_installed_plugin() {
        let result = check_plugin_residue_impl(&[PluginWiring {
            crate_name: "autumn-media-plugin".to_owned(),
            community: false,
            dependency: true,
            mount: true,
            mount_qualified: true,
            orphaned_migrations: vec!["20260720000000".to_owned()],
        }]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    /// The worst finding decides the status, and every finding is reported —
    /// a fail must not hide the warnings alongside it.
    #[test]
    fn plugin_residue_reports_every_finding_and_takes_the_worst_status() {
        let result = check_plugin_residue_impl(&[
            wiring("autumn-admin-plugin", true, false),
            wiring("autumn-search", false, true),
        ]);
        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("autumn-admin-plugin"), "{detail}");
        assert!(detail.contains("autumn-search"), "{detail}");
    }

    /// The residue check has to survive the `--json` contract like every other
    /// check: a name, a status, and a detail string.
    #[test]
    fn plugin_residue_serializes_under_the_json_contract() {
        let results = vec![check_plugin_residue_impl(&[wiring(
            "autumn-admin-plugin",
            true,
            false,
        )])];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let check = value["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|c| c["name"] == "plugin_residue")
            .expect("plugin_residue row");
        assert_eq!(check["status"], "warn");
        assert!(check["detail"].as_str().is_some_and(|d| !d.is_empty()));
    }

    /// `--strict` turns the dependency-without-mount warning into a failing
    /// run; that is the contract AC #7 asks the check to inherit.
    #[test]
    fn plugin_residue_warnings_are_strict_failures() {
        let results = vec![check_plugin_residue_impl(&[wiring(
            "autumn-admin-plugin",
            true,
            false,
        )])];
        let summary = compute_summary(&results);
        assert_eq!(summary.warned, 1);
        assert_eq!(summary.failed, 0);
        // The contract is the exit code, not the counts: `--strict` turns this
        // warning into a failing run, and a plain run leaves it advisory.
        assert_eq!(exit_code(&summary, true), 1);
        assert_eq!(exit_code(&summary, false), 0);
    }

    /// A community `autumn-plugin-*` dependency with no mount is residue too —
    /// but `plugin add` never writes a community mount, so the advice has to
    /// be different from a first-party plugin's.
    #[test]
    fn plugin_residue_gives_community_crates_their_own_advice() {
        let result = check_plugin_residue_impl(&[PluginWiring {
            crate_name: "autumn-plugin-live-feed".to_owned(),
            community: true,
            dependency: true,
            mount: false,
            mount_qualified: false,
            orphaned_migrations: Vec::new(),
        }]);
        assert_eq!(result.status, CheckStatus::Warn);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("README"), "{detail}");
        assert!(!detail.contains("to finish the install"), "{detail}");
    }

    /// The `Fail` says "this app does not compile", so it needs evidence that
    /// the app really reaches into THIS crate. A bare `SearchPlugin::new(` may
    /// be the app's own same-named type, and a hard failure on a substring
    /// collision is a verdict this check has not earned.
    #[test]
    fn plugin_residue_does_not_fail_on_an_unqualified_constructor_collision() {
        let result = check_plugin_residue_impl(&[PluginWiring {
            crate_name: "autumn-search".to_owned(),
            community: false,
            dependency: false,
            mount: true,
            mount_qualified: false,
            orphaned_migrations: Vec::new(),
        }]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    /// The disk glue, end to end: a dependency declared with no mount anywhere
    /// in `src/` is residue, and a mount that lives OUTSIDE `src/main.rs` is
    /// not — the shape `plugin add`'s manual fallback tells users to write.
    #[test]
    fn plugin_wirings_are_resolved_from_the_whole_src_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\nautumn-admin-plugin = \"0.7.0\"\nautumn-plugin-live-feed = \"0.3.1\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() { app::build(); }\n").unwrap();
        // The builder lives in a sibling module, not in `main.rs`.
        std::fs::write(
            root.join("src/app.rs"),
            "pub fn build() {\n    autumn_web::app().plugin(autumn_admin_plugin::AdminPlugin::new());\n}\n",
        )
        .unwrap();

        let wirings = resolve_plugin_wirings(root, Vec::new);
        let admin = wirings
            .iter()
            .find(|w| w.crate_name == "autumn-admin-plugin")
            .expect("admin wiring");
        assert!(admin.dependency && admin.mount, "{admin:?}");
        assert!(admin.mount_qualified, "{admin:?}");

        let feed = wirings
            .iter()
            .find(|w| w.crate_name == "autumn-plugin-live-feed")
            .expect("community wiring");
        assert!(feed.community && feed.dependency, "{feed:?}");
        assert!(!feed.mount, "{feed:?}");

        assert_eq!(
            check_plugin_residue_impl(&wirings).status,
            CheckStatus::Warn,
            "the unmounted community crate is the only finding"
        );
    }

    /// A declared migration still recorded as applied, for a plugin that is
    /// gone from the code — and the version key is matched after the
    /// `_name` suffix is stripped, the way `diesel migration list` prints it.
    #[test]
    fn plugin_wirings_match_declared_migrations_against_applied_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let wirings = resolve_plugin_wirings(root, || vec!["20260720000000".to_owned()]);
        let media = wirings
            .iter()
            .find(|w| w.crate_name == "autumn-media-plugin")
            .expect("media wiring");
        assert_eq!(media.orphaned_migrations, vec!["20260720000000".to_owned()]);
        assert_eq!(
            check_plugin_residue_impl(&wirings).status,
            CheckStatus::Warn
        );
    }

    /// The migration history costs a subprocess and a database round trip, so
    /// it must not be read when no departed plugin could possibly have left
    /// one — which is every ordinary project.
    #[test]
    fn plugin_wirings_do_not_read_the_migration_history_without_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\nautumn-media-plugin = \"0.7.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() { autumn_web::app().plugin(autumn_media_plugin::MediaPlugin::new()); }\n",
        )
        .unwrap();

        // The closure panics if called: the plugin is installed, so there is
        // no orphan to look for.
        let wirings = resolve_plugin_wirings(root, || panic!("must not query the database"));
        assert_eq!(
            check_plugin_residue_impl(&wirings).status,
            CheckStatus::Pass
        );
    }

    /// Every applied version is needed, not just the newest: an orphan can sit
    /// anywhere in the history.
    #[test]
    fn applied_migration_versions_are_parsed_from_the_diesel_listing() {
        let output = "Migrations:\n  [X] 20260101000000_create_users\n  [ ] 20260202000000_pending\n  [X] 20260720000000_media_rooms\n";
        let versions = parse_applied_migration_versions(output);
        assert_eq!(versions, vec!["20260101000000", "20260720000000"]);
    }

    // ── #1633: dependency advisories and policy in the dev loop ──────────────

    mod dependencies {
        use super::super::*;
        use crate::deps::{Evaluation, Finding, Severity};

        fn finding(
            id: &str,
            code: &str,
            severity: Severity,
            waived: bool,
            blocking: bool,
        ) -> Finding {
            Finding {
                id: id.to_owned(),
                code: code.to_owned(),
                package: "badcrate 1.2.3".to_owned(),
                title: "boom".to_owned(),
                severity,
                waived,
                blocking,
            }
        }

        fn audited(findings: Vec<Finding>, db_age_days: Option<u64>) -> Evaluation {
            Evaluation::Audited {
                findings,
                checks: vec!["advisories".to_owned()],
                db_age_days,
                auditor: "cargo-deny 0.20.2".to_owned(),
            }
        }

        #[test]
        fn a_clean_tree_is_exactly_one_passing_line() {
            // Noise discipline is falsifiable: a clean tree costs one line.
            let result = check_dependencies_impl(&audited(Vec::new(), Some(2)));
            assert_eq!(result.name, "dependencies");
            assert_eq!(result.status, CheckStatus::Pass);
            let line = format_check_line(&result);
            assert_eq!(line.lines().count(), 1, "{line}");
            assert!(line.contains("dependencies"), "{line}");
        }

        #[test]
        fn a_blocking_finding_fails_and_names_its_id_and_severity() {
            let result = check_dependencies_impl(&audited(
                vec![finding(
                    "RUSTSEC-2099-0001",
                    "vulnerability",
                    Severity::Critical,
                    false,
                    true,
                )],
                Some(1),
            ));
            assert_eq!(result.status, CheckStatus::Fail);
            let detail = result.detail.expect("detail");
            assert!(detail.contains("RUSTSEC-2099-0001"), "{detail}");
            assert!(detail.contains("critical"), "{detail}");
            assert!(detail.contains("badcrate 1.2.3"), "{detail}");
        }

        #[test]
        fn a_policy_violation_fails_and_names_its_violation_id() {
            let result = check_dependencies_impl(&audited(
                vec![finding("banned", "banned", Severity::High, false, true)],
                Some(1),
            ));
            assert_eq!(result.status, CheckStatus::Fail);
            let detail = result.detail.expect("detail");
            assert!(detail.contains("banned"), "{detail}");
            assert!(detail.contains("high"), "{detail}");
        }

        #[test]
        fn findings_the_policy_only_warns_about_warn() {
            let result = check_dependencies_impl(&audited(
                vec![finding("yanked", "yanked", Severity::Low, false, false)],
                Some(1),
            ));
            assert_eq!(result.status, CheckStatus::Warn);
        }

        #[test]
        fn a_waived_finding_displays_as_waived_and_never_fails() {
            let result = check_dependencies_impl(&audited(
                vec![finding(
                    "RUSTSEC-2023-0071",
                    "vulnerability",
                    Severity::Critical,
                    true,
                    false,
                )],
                Some(1),
            ));
            assert_eq!(
                result.status,
                CheckStatus::Pass,
                "a waived finding is not a failure"
            );
            let detail = result.detail.expect("detail");
            assert!(detail.contains("waived"), "{detail}");
            assert!(detail.contains("RUSTSEC-2023-0071"), "{detail}");
        }

        #[test]
        fn a_waived_finding_alongside_a_live_one_still_fails_on_the_live_one() {
            let result = check_dependencies_impl(&audited(
                vec![
                    finding(
                        "RUSTSEC-2023-0071",
                        "vulnerability",
                        Severity::Low,
                        true,
                        false,
                    ),
                    finding(
                        "RUSTSEC-2099-0001",
                        "vulnerability",
                        Severity::High,
                        false,
                        true,
                    ),
                ],
                Some(1),
            ));
            assert_eq!(result.status, CheckStatus::Fail);
            let detail = result.detail.expect("detail");
            // The live finding is listed first: it is the one to act on.
            let live = detail
                .find("RUSTSEC-2099-0001")
                .expect("live finding listed");
            let waived = detail
                .find("RUSTSEC-2023-0071")
                .expect("waived finding listed");
            assert!(live < waived, "{detail}");
        }

        #[test]
        fn a_missing_policy_file_warns_and_says_ci_needs_one() {
            let result = check_dependencies_impl(&Evaluation::NoPolicy);
            assert_eq!(result.status, CheckStatus::Warn);
            let detail = result.detail.expect("detail");
            assert!(detail.contains("deny.toml"), "{detail}");
        }

        #[test]
        fn a_machine_without_the_auditor_does_not_fail_doctor_strict() {
            // `exit_code` treats any warning as a failure under `--strict`, and
            // cargo-deny is not installed by any Autumn install path. Warning
            // here would make `autumn doctor --strict` — a Tier 1 command —
            // exit 1 on every machine that has not opted in.
            let result = check_dependencies_impl(&Evaluation::AuditorMissing {
                checks: vec!["advisories".to_owned()],
            });
            assert_eq!(result.status, CheckStatus::Pass);
            let summary = compute_summary(std::slice::from_ref(&result));
            assert_eq!(exit_code(&summary, true), 0);
            let detail = result.detail.expect("detail");
            assert!(
                detail.contains("not evaluated"),
                "a pass must not read as a verdict: {detail}"
            );
            assert!(
                detail.contains("cargo install"),
                "the detail must say how to enable it: {detail}"
            );
        }

        #[test]
        fn an_unfetched_database_does_not_fail_doctor_strict_either() {
            // Same reasoning: the database is only populated by an explicit
            // `cargo deny fetch db`.
            let result = check_dependencies_impl(&Evaluation::DatabaseMissing {
                checks: vec!["advisories".to_owned()],
            });
            assert_eq!(result.status, CheckStatus::Pass);
            let summary = compute_summary(std::slice::from_ref(&result));
            assert_eq!(exit_code(&summary, true), 0);
            let detail = result.detail.expect("detail");
            assert!(detail.contains("not evaluated"), "{detail}");
            assert!(
                detail.contains("cargo deny fetch db"),
                "the detail must say how to fetch it: {detail}"
            );
        }

        #[test]
        fn an_unevaluated_policy_still_names_the_checks_it_would_run() {
            // The derived check list is what a reader compares against CI, so
            // it must survive a state where nothing could be audited.
            let result = check_dependencies_impl(&Evaluation::AuditorMissing {
                checks: vec!["advisories".to_owned(), "licenses".to_owned()],
            });
            let detail = result.detail.expect("detail");
            assert!(detail.contains("advisories"), "{detail}");
            assert!(detail.contains("licenses"), "{detail}");
        }

        #[test]
        fn a_stale_database_warns_once_and_reports_the_data_age() {
            // Offline behavior: one warning with the data age, never a failure
            // and never a hang.
            let age = crate::deps::STALE_AFTER_DAYS + 5;
            let result = check_dependencies_impl(&audited(Vec::new(), Some(age)));
            assert_eq!(result.status, CheckStatus::Warn);
            let detail = result.detail.clone().expect("detail");
            assert!(
                detail.contains(&age.to_string()),
                "the age must appear: {detail}"
            );
            assert_eq!(
                format_check_line(&result).lines().count(),
                2,
                "one warning line plus its hint"
            );
        }

        #[test]
        fn fresh_data_reports_its_age_without_warning() {
            let result = check_dependencies_impl(&audited(Vec::new(), Some(3)));
            assert_eq!(result.status, CheckStatus::Pass);
            assert!(result.detail.expect("detail").contains('3'));
        }

        #[test]
        fn an_auditor_that_produced_no_verdict_warns_with_the_reason() {
            let result = check_dependencies_impl(&Evaluation::Unavailable {
                reason: "cargo metadata failed".to_owned(),
                checks: Vec::new(),
            });
            assert_eq!(result.status, CheckStatus::Warn);
            assert!(
                result
                    .detail
                    .expect("detail")
                    .contains("cargo metadata failed"),
                "the reason must be surfaced"
            );
        }

        #[test]
        fn a_long_finding_list_is_capped() {
            // 731 `source-not-allowed` findings is a real cargo-deny output.
            // Doctor names a bounded few and counts the rest.
            let findings: Vec<Finding> = (0..40)
                .map(|n| {
                    finding(
                        &format!("RUSTSEC-2099-{n:04}"),
                        "vulnerability",
                        Severity::High,
                        false,
                        true,
                    )
                })
                .collect();
            let result = check_dependencies_impl(&audited(findings, Some(1)));
            let detail = result.detail.expect("detail");
            assert!(
                detail.lines().count() <= 12,
                "detail must stay bounded, got {} lines",
                detail.lines().count()
            );
            assert!(
                detail.contains("40"),
                "the total must still be reported: {detail}"
            );
            assert!(detail.contains("more"), "{detail}");
        }

        #[test]
        fn a_multi_line_title_cannot_break_the_cap() {
            // Defence in depth: the parser normalizes titles, and the renderer
            // must not depend on that having happened.
            let findings: Vec<Finding> = (0..40)
                .map(|n| {
                    let mut f = finding(
                        &format!("RUSTSEC-2099-{n:04}"),
                        "duplicate",
                        Severity::Low,
                        false,
                        false,
                    );
                    f.title = "lock entries:\nfirst\nsecond\nthird".to_owned();
                    f
                })
                .collect();
            let result = check_dependencies_impl(&audited(findings, Some(1)));
            let detail = result.detail.expect("detail");
            assert!(
                detail.lines().count() <= 12,
                "one line per finding, got {} lines",
                detail.lines().count()
            );
        }

        #[test]
        fn a_finding_line_never_repeats_its_own_identifier() {
            // A policy violation's id *is* its code, so naming both reads
            // "duplicate low duplicate aes 0.8.4".
            let result = check_dependencies_impl(&audited(
                vec![finding(
                    "duplicate",
                    "duplicate",
                    Severity::Low,
                    false,
                    false,
                )],
                Some(1),
            ));
            let detail = result.detail.expect("detail");
            assert_eq!(
                detail.matches("duplicate").count(),
                1,
                "the identifier is named once: {detail}"
            );
        }

        #[test]
        fn finding_lines_are_indented_under_the_check() {
            let result = check_dependencies_impl(&audited(
                vec![finding(
                    "RUSTSEC-2099-0001",
                    "vulnerability",
                    Severity::High,
                    false,
                    true,
                )],
                Some(1),
            ));
            let detail = result.detail.expect("detail");
            let mut lines = detail.lines();
            assert!(
                !lines.next().expect("summary").starts_with(' '),
                "the summary shares the check's own line"
            );
            for line in lines {
                assert!(line.starts_with("   "), "unindented finding line: {line:?}");
            }
        }

        #[test]
        fn the_check_survives_the_json_contract() {
            let result = check_dependencies_impl(&audited(
                vec![finding(
                    "RUSTSEC-2099-0001",
                    "vulnerability",
                    Severity::Critical,
                    false,
                    true,
                )],
                Some(1),
            ));
            let summary = compute_summary(std::slice::from_ref(&result));
            let json = to_json_output(std::slice::from_ref(&result), &summary);
            let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            let check = &parsed["checks"][0];
            assert_eq!(check["name"], "dependencies");
            assert_eq!(check["status"], "fail");
            assert!(
                check["detail"]
                    .as_str()
                    .expect("detail")
                    .contains("RUSTSEC-2099-0001"),
                "--json must carry the advisory id"
            );
        }

        #[test]
        fn the_checks_the_policy_activates_are_reported() {
            // The reader has to be able to tell which CI gate this predicts.
            let result = check_dependencies_impl(&Evaluation::Audited {
                findings: Vec::new(),
                checks: vec!["advisories".to_owned(), "licenses".to_owned()],
                db_age_days: Some(1),
                auditor: "cargo-deny 0.20.2".to_owned(),
            });
            let detail = result.detail.expect("detail");
            assert!(detail.contains("advisories"), "{detail}");
            assert!(detail.contains("licenses"), "{detail}");
        }
    }
}
