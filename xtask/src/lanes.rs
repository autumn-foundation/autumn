//! Lanes: the ordered checks CI's `lint` job and the local pre-push gate run.
//!
//! This file is the one definition of those commands. `.github/workflows/ci.yml`
//! runs `cargo xtask ci lint`, `toolchain-drift.yml` runs single steps of the
//! same lane, and `cargo xtask pre-push` reuses the same constants, so a lane
//! and its local mirror cannot drift apart.
//!
//! Two shell gates read this file as text. `scripts/check-panic-gate.sh` and
//! `scripts/check-determinism-gate.sh` collect the features of every enforcing
//! `cargo clippy -p autumn-web … -D warnings` command to prove each gated module
//! is linted with its feature on. They trust this file only while ci.yml runs
//! `cargo xtask ci lint`. So:
//!
//! - write each CI clippy command as one string literal on one line;
//! - keep `-p autumn-web`, the quoted `--features "…"` list and `-D warnings`
//!   in that literal;
//! - never write such a command anywhere else in this file, tests included,
//!   or the gates will count a lane CI does not run. A test below enforces it.

use anyhow::{Result, bail};

use crate::argv;

/// `cargo fmt` in check mode.
pub const FMT_CHECK: &str = "cargo fmt --all -- --check";

/// Whole-workspace clippy, every target.
///
/// `-j 2` caps codegen parallelism. At cargo's default of one job per core
/// this command peaks above the 16 GB of an `ubuntu-latest` runner while
/// checking autumn-web, and the runner is reclaimed with `exit 143` and no
/// diagnostic. It reproduces locally on a 15 GB box at `-j 4` and completes at
/// `-j 2`.
pub const CLIPPY_WORKSPACE: &str = "cargo clippy -j 2 --workspace --all-targets -- -D warnings";

/// Clippy over autumn-web with the feature-gated request-path modules on.
///
/// The workspace run compiles none of these modules, so their
/// `#![cfg_attr(not(test), deny(clippy::…))]` panic-gate and determinism-gate
/// blocks never fire there. Every gated module's feature must be in this list
/// or another enforcing lane; the two gate scripts check that.
///
/// - `ws`, `mail`, `offline-sync`, `redis`, `storage`: the original gated
///   request-path modules (channels, mail, sync, `session_redis`, uploads).
/// - `inbound-mail`, `inbound-mailgun`, `inbound-ses`: `inbound_mail` parses
///   untrusted RFC 5322 / MIME bodies from an unauthenticated webhook.
/// - `constela`: parses and renders a document a language model wrote.
/// - `collab` (#1806): `collab/resolver.rs` needs `collab` and
///   `offline-sync`; no other lane has both.
/// - `tls`, `acme` (#1797, #1608): each owns a `now_unix` that reads wall time
///   on purpose; without the feature its allow and any read next to it are
///   never linted. `acme` also brings in the whole ACME tree.
/// - `sla` (#1826): the SLA engine carries both gates.
/// - `markdown`, `i18n`, `presence`: gated modules of their own.
///
/// `--all-features` is not usable: `managed-pg-bundled` downloads Postgres in
/// its build script.
pub const CLIPPY_GATED: &str = r#"cargo clippy -p autumn-web --features "ws,mail,offline-sync,collab,redis,markdown,constela,inbound-mail,inbound-mailgun,inbound-ses,storage,tls,acme,i18n,presence,sla" --all-targets -- -D warnings"#;

/// Clippy over autumn-web's plugin sandbox (#1609).
///
/// `host.rs` runs an interpreter over an artifact the operator did not audit,
/// so nothing there may panic. Kept apart from [`CLIPPY_GATED`] so that lane
/// does not have to link `wasmi`.
pub const CLIPPY_PLUGIN_SANDBOX: &str = r#"cargo clippy -p autumn-web --features "plugin-sandbox,test-support" --all-targets -- -D warnings"#;

/// Clippy over the off-by-default console REPL (#2148).
///
/// No other lane compiles `repl.rs` or the REPL glue `#[model]` and
/// `#[repository]` emit. Its own lane, so the Rhai build stays out of
/// [`CLIPPY_GATED`].
pub const CLIPPY_REPL: &str =
    "cargo clippy -p autumn-web --features repl --all-targets -- -D warnings";

/// One command in a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Stable name, used by `--only`, `--skip` and `--from`.
    pub id: &'static str,
    /// What the step checks, shown in the log.
    pub title: &'static str,
    /// Program and arguments. `cargo`, a `scripts/*.sh` or a `scripts/*.py`
    /// path is resolved by the runner; anything else runs as written.
    pub argv: Vec<String>,
    /// Extra environment for this step only.
    pub env: Vec<(&'static str, &'static str)>,
}

impl Step {
    /// A step from a command line written as one string.
    ///
    /// # Panics
    ///
    /// Panics if `line` is not a valid command. Every call site passes a
    /// constant from this file, and a test builds every lane.
    fn cmd(id: &'static str, title: &'static str, line: &str) -> Self {
        let argv = argv::split(line)
            .unwrap_or_else(|e| panic!("lane step `{id}` has a malformed command: {e}"));
        Self {
            id,
            title,
            argv,
            env: Vec::new(),
        }
    }

    /// A step that runs a script under `scripts/`.
    fn script(id: &'static str, title: &'static str, script: &str, args: &[&str]) -> Self {
        let mut words = vec![format!("scripts/{script}")];
        words.extend(args.iter().map(|a| (*a).to_owned()));
        Self {
            id,
            title,
            argv: words,
            env: Vec::new(),
        }
    }

    fn with_env(mut self, key: &'static str, value: &'static str) -> Self {
        self.env.push((key, value));
        self
    }
}

/// An ordered list of steps.
#[derive(Debug, Clone)]
pub struct Lane {
    /// The lane's name, as given on the command line.
    pub name: &'static str,
    /// One line on what the lane is for.
    pub about: &'static str,
    /// The steps, in run order.
    pub steps: Vec<Step>,
    /// Run every step and report all failures, rather than stopping at the
    /// first. CI wants the full list; locally the first break is enough.
    pub keep_going: bool,
}

/// Every lane, for `cargo xtask list` and name lookup.
#[must_use]
pub fn all() -> Vec<Lane> {
    vec![lint(), pre_push()]
}

/// Look a lane up by name.
///
/// # Errors
///
/// Returns an error naming the known lanes when `name` is not one of them.
pub fn by_name(name: &str) -> Result<Lane> {
    let lanes = all();
    let known: Vec<_> = lanes.iter().map(|l| l.name).collect();
    lanes
        .into_iter()
        .find(|l| l.name == name)
        .ok_or_else(|| anyhow::anyhow!("no lane `{name}`; known lanes: {}", known.join(", ")))
}

/// CI's `lint` job.
///
/// Toolchain-free gates run first: they take seconds, and with `keep_going`
/// their result is in the log before the multi-minute clippy runs start.
#[must_use]
pub fn lint() -> Lane {
    Lane {
        name: "lint",
        about: "CI's `lint` job: formatting, clippy lanes, and the repo gates",
        keep_going: true,
        steps: vec![
            Step::cmd("fmt", "Check formatting", FMT_CHECK),
            // The Rust tests only search the widget runtime for strings, so a
            // syntax error there passes them (#3406). Node parses the file.
            Step::cmd(
                "widget-runtime-parses",
                "Check the widget runtime parses",
                "node --check autumn/vendor/autumn-widgets.js",
            ),
            panic_gate(),
            determinism_gate(),
            // The resolver layer asks `cargo metadata` what cargo resolves, so
            // CI requires it rather than letting the gate skip it.
            sqlite_unification().with_env("SQLITE_GATE_REQUIRE_RESOLVE", "1"),
            bin_names(),
            // Every `[[bench]]` needs an entry in docs/perf/bolt/workloads.toml
            // and a committed callgrind + dhat profile. No build. See
            // docs/perf/README.md.
            Step::script(
                "bolt-profiles",
                "Bolt profile gate (every workload has a profile)",
                "check-perf-profiles.py",
                &["bolt"],
            ),
            Step::cmd("clippy-workspace", "Clippy", CLIPPY_WORKSPACE),
            Step::cmd(
                "clippy-gated",
                "Clippy (autumn-web, gated request-path features — panic gate)",
                CLIPPY_GATED,
            ),
            Step::cmd(
                "clippy-plugin-sandbox",
                "Clippy (autumn-web, plugin-sandbox — panic gate)",
                CLIPPY_PLUGIN_SANDBOX,
            ),
            Step::cmd("clippy-repl", "Clippy (autumn-web, repl)", CLIPPY_REPL),
            // No framework diesel declaration over 32 columns, and no wide
            // diesel feature: cargo unifies it onto every app. Apps get the
            // same findings as a warning from `autumn build`. See
            // docs/guide/table-width.md.
            Step::cmd(
                "table-width",
                "Diesel table-width gate (no framework declaration over 32 columns)",
                "cargo run -p autumn-cli --bin autumn -- schema width .",
            ),
            // Raw `html!` fields that bypass the typed a11y primitives and have
            // no accessible name, over the whole repo (#1934). The scanner
            // skips `target/`, dot-dirs and `tests/`.
            Step::cmd(
                "a11y",
                "Accessibility gate (raw html! escape hatches across the repo)",
                "cargo run -p autumn-cli --bin autumn -- a11y verify .",
            ),
        ],
    }
}

/// The local pre-push gate: compile what CI compiles, without running it.
///
/// CI's blocking gate is `cargo test --workspace`, which links the autumn-web
/// consolidated `integration_tests` binary. A narrow `cargo test -p <pkg>`
/// never does, so a cross-package compile break passes locally and fails on
/// the PR. This lane builds what CI's `lint` and `test` jobs build.
///
/// It compiles test targets with `--no-run`. Running them would start the
/// trybuild suite, whose cases each spawn a nested `cargo build` and expand
/// scratch by about 17 GB. Doctests have no compile-only mode, so they run,
/// but they are almost all `no_run` or `ignore` and need no services.
///
/// Not covered (CI runs them in their own jobs): the Docker sweep, the
/// `sqlite` backend-flip lane (reproduce with
/// `cargo test -p autumn-web --features sqlite --lib` when touching a fixture
/// that names a database), and the `system-tests` browser suite.
#[must_use]
pub fn pre_push() -> Lane {
    let gated = features_of(CLIPPY_GATED);
    let sandbox = features_of(CLIPPY_PLUGIN_SANDBOX);
    Lane {
        name: "pre-push",
        about: "compile everything CI compiles, without running trybuild",
        keep_going: false,
        steps: vec![
            // Toolchain-free and self-testing, so they go first: a dropped
            // gate header is reported in seconds, before the compile legs.
            panic_gate(),
            determinism_gate(),
            // The docs table drifting from `PLUGIN_SURFACES`, or a surface
            // change with no "Plugin authors" section in the migration guide.
            Step::script(
                "plugin-surface",
                "Plugin API surface gate",
                "check-plugin-surface.sh",
                &[],
            ),
            // None of the legs below enable `sqlite`, so a dependency edge that
            // turns it on for the whole graph would compile here and break the
            // Postgres lanes in CI.
            sqlite_unification(),
            bin_names(),
            // A note written into CHANGELOG.md instead of `changelog.d/` is
            // cheapest to fix before the push.
            Step::script(
                "changelog-self-test",
                "Changelog fragment gate (self-test)",
                "check-changelog-fragments.sh",
                &["--self-test"],
            ),
            Step::script(
                "changelog",
                "Changelog fragment gate",
                "check-changelog-fragments.sh",
                &[],
            ),
            Step::cmd("fmt", "Check formatting", FMT_CHECK),
            // Without CI's `-j 2`: a developer machine sets its own limit.
            Step::cmd(
                "clippy-workspace",
                "Clippy",
                "cargo clippy --workspace --all-targets -- -D warnings",
            ),
            // The gates are `cfg_attr(not(test), …)`, so the lib target is
            // where they apply. `--lib` keeps these legs minutes shorter than
            // CI's `--all-targets`; the feature lists are CI's own.
            Step {
                argv: lib_only(CLIPPY_GATED),
                ..Step::cmd(
                    "clippy-gated",
                    "Clippy (autumn-web, gated request-path features, --lib)",
                    CLIPPY_GATED,
                )
            },
            Step {
                argv: lib_only(CLIPPY_PLUGIN_SANDBOX),
                ..Step::cmd(
                    "clippy-plugin-sandbox",
                    "Clippy (autumn-web, plugin-sandbox, --lib)",
                    CLIPPY_PLUGIN_SANDBOX,
                )
            },
            // `--all-targets` kept: it compiles the REPL glue every `#[model]`
            // and `#[repository]` in the test binaries emits.
            Step::cmd("clippy-repl", "Clippy (autumn-web, repl)", CLIPPY_REPL),
            Step::cmd(
                "test-compile",
                "Compile every workspace test target (no run)",
                "cargo test --workspace --no-run",
            ),
            // The default-feature compile above and the `--lib` clippy legs
            // compile none of the `#![cfg(feature = "…")]` integration modules
            // (#2395). One link of the consolidated binary per gated set does.
            integration_compile(
                "test-compile-gated",
                "Compile integration_tests with the gated features (no run)",
                &gated,
            ),
            // Compiles the sandbox containment corpus (`plugin_sandbox.rs`).
            integration_compile(
                "test-compile-plugin-sandbox",
                "Compile integration_tests with plugin-sandbox (no run)",
                &sandbox,
            ),
            // Compiles `repl_registration.rs`, which no other leg builds.
            integration_compile(
                "test-compile-repl",
                "Compile integration_tests with repl (no run)",
                "repl,test-support",
            ),
            Step::cmd(
                "doctests",
                "Doctests (workspace, doc target only)",
                "cargo test --workspace --doc",
            ),
        ],
    }
}

/// Panic-free request-path gate (#1611). Runs its own self-test first.
fn panic_gate() -> Step {
    Step::script(
        "panic-gate",
        "Panic-free request-path gate",
        "check-panic-gate.sh",
        &[],
    )
}

/// Determinism seam gate (#1797): keeps the clippy lanes able to see an
/// off-seam time read. Manifest integrity, header shape, config completeness,
/// no module-wide `allow` spoof, no crate-local `clippy.toml`.
fn determinism_gate() -> Step {
    Step::script(
        "determinism-gate",
        "Determinism seam gate",
        "check-determinism-gate.sh",
        &[],
    )
}

/// The `sqlite` backend flip (#1614) is safe only while no manifest enables it
/// through a dependency edge: cargo unifies features across the graph.
fn sqlite_unification() -> Step {
    Step::script(
        "sqlite-unification",
        "SQLite feature-unification gate",
        "check-sqlite-unification.sh",
        &[],
    )
}

/// Two members producing the same binary file name break the Windows linker
/// intermittently (LNK1104, #2639) and nothing else. Covers explicit `[[bin]]`
/// and auto-discovered `src/bin` targets (#2690, #2691).
fn bin_names() -> Step {
    Step::script(
        "bin-names",
        "Example binary-name collision gate",
        "check-example-bin-names.sh",
        &[],
    )
}

fn integration_compile(id: &'static str, title: &'static str, features: &str) -> Step {
    let features = if features.split(',').any(|f| f == "test-support") {
        features.to_owned()
    } else {
        format!("{features},test-support")
    };
    Step {
        id,
        title,
        argv: [
            "cargo",
            "test",
            "-p",
            "autumn-web",
            "--features",
            &features,
            "--test",
            "integration_tests",
            "--no-run",
        ]
        .map(str::to_owned)
        .to_vec(),
        env: Vec::new(),
    }
}

/// The value of `--features` in a command line, or an empty string.
#[must_use]
pub fn features_of(line: &str) -> String {
    let argv = argv::split(line).unwrap_or_default();
    argv.iter()
        .position(|a| a == "--features")
        .and_then(|i| argv.get(i + 1))
        .cloned()
        .unwrap_or_default()
}

/// `line` with `--all-targets` replaced by `--lib`.
fn lib_only(line: &str) -> Vec<String> {
    argv::split(line)
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            if a == "--all-targets" {
                "--lib".to_owned()
            } else {
                a
            }
        })
        .collect()
}

/// Check that every id in `ids` names a step of `lane`.
///
/// A filter that matches nothing still exits 0, so a typo in `--only` would
/// otherwise turn a gate into a no-op.
///
/// # Errors
///
/// Returns an error naming the unknown ids and the lane's real ones.
pub fn check_ids<'a>(lane: &Lane, ids: impl IntoIterator<Item = &'a String>) -> Result<()> {
    let unknown: Vec<&str> = ids
        .into_iter()
        .filter(|id| !lane.steps.iter().any(|s| s.id == id.as_str()))
        .map(String::as_str)
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let known: Vec<_> = lane.steps.iter().map(|s| s.id).collect();
    bail!(
        "lane `{}` has no step {}; its steps are: {}",
        lane.name,
        unknown
            .iter()
            .map(|u| format!("`{u}`"))
            .collect::<Vec<_>>()
            .join(", "),
        known.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use super::*;

    fn repo_root() -> &'static Path {
        crate::repo_root()
    }

    #[test]
    fn every_lane_builds_and_its_ids_are_unique() {
        for lane in all() {
            assert!(!lane.steps.is_empty(), "lane `{}` is empty", lane.name);
            let mut seen = HashSet::new();
            for step in &lane.steps {
                assert!(
                    seen.insert(step.id),
                    "lane `{}` has two steps named `{}`",
                    lane.name,
                    step.id
                );
                assert!(!step.argv.is_empty());
            }
        }
    }

    #[test]
    fn every_script_a_lane_names_exists() {
        for lane in all() {
            for step in &lane.steps {
                let program = &step.argv[0];
                if program.starts_with("scripts/") {
                    assert!(
                        repo_root().join(program).is_file(),
                        "lane `{}` step `{}` runs {program}, which does not exist",
                        lane.name,
                        step.id
                    );
                }
            }
        }
    }

    #[test]
    fn pre_push_mirrors_the_ci_gated_feature_lists() {
        let lane = pre_push();
        let step = |id: &str| lane.steps.iter().find(|s| s.id == id).unwrap().clone();

        let gated = step("clippy-gated");
        assert_eq!(
            features_of(&argv::join(&gated.argv)),
            features_of(CLIPPY_GATED)
        );
        assert!(gated.argv.contains(&"--lib".to_owned()));
        assert!(!gated.argv.contains(&"--all-targets".to_owned()));

        let compile = step("test-compile-gated");
        assert_eq!(
            features_of(&argv::join(&compile.argv)),
            format!("{},test-support", features_of(CLIPPY_GATED))
        );

        let sandbox = step("test-compile-plugin-sandbox");
        assert_eq!(
            features_of(&argv::join(&sandbox.argv)),
            features_of(CLIPPY_PLUGIN_SANDBOX),
            "test-support is already in the sandbox list and must not be doubled"
        );
    }

    /// `docs/perf/README.md` promises a new bench fails CI until it has a
    /// profile; that holds while the lint lane runs the Bolt gate.
    #[test]
    fn the_lint_lane_runs_the_bolt_profile_gate() {
        assert!(
            lint()
                .steps
                .iter()
                .any(|s| s.argv == ["scripts/check-perf-profiles.py", "bolt"])
        );
    }

    #[test]
    fn features_of_reads_the_quoted_list() {
        assert_eq!(features_of(r#"cargo x --features "a,b" --lib"#), "a,b");
        assert_eq!(features_of("cargo x --features repl --lib"), "repl");
        assert_eq!(features_of("cargo x --lib"), "");
        assert_eq!(features_of("cargo x --features"), "");
    }

    #[test]
    fn the_lint_lane_runs_every_ci_clippy_constant() {
        let lane = lint();
        for constant in [
            CLIPPY_WORKSPACE,
            CLIPPY_GATED,
            CLIPPY_PLUGIN_SANDBOX,
            CLIPPY_REPL,
        ] {
            let argv = argv::split(constant).unwrap();
            assert!(
                lane.steps.iter().any(|s| s.argv == argv),
                "the lint lane does not run `{constant}`"
            );
        }
    }

    /// The shell gates count every uncommented line here that holds
    /// `cargo clippy`, `-p autumn-web` and `-D warnings`. Each such line must
    /// be a command the lint lane really runs, or a gate would accept a
    /// feature no CI lane compiles.
    #[test]
    fn every_line_the_shell_gates_read_is_a_lint_lane_command() {
        let source = std::fs::read_to_string(repo_root().join("xtask/src/lanes.rs")).unwrap();
        let lint = lint();
        let mut seen = 0;
        for line in source.lines() {
            let trimmed = line.trim_start();
            let visible = !trimmed.starts_with("//")
                && line.contains("cargo clippy")
                && line.contains("-p autumn-web")
                && line.contains("-D warnings");
            if !visible {
                continue;
            }
            seen += 1;
            let start = line.find("cargo clippy").unwrap();
            let end = line.rfind("-D warnings").unwrap() + "-D warnings".len();
            let argv = argv::split(&line[start..end]).unwrap();
            assert!(
                lint.steps.iter().any(|s| s.argv == argv),
                "lanes.rs line is read by the panic and determinism gates as an \
                 enforcing CI clippy lane, but the lint lane does not run it:\n{line}"
            );
        }
        assert!(
            seen >= 3,
            "expected the CI clippy constants, found {seen} lines"
        );
    }

    /// Every `cargo xtask ci <lane>` a workflow runs names a real lane, and
    /// every `--only`/`--skip`/`--from` names a real step of it. The weekly
    /// `toolchain-drift.yml` would otherwise find a typo only when it fires.
    #[test]
    fn every_workflow_invocation_names_real_lanes_and_steps() {
        let dir = repo_root().join(".github/workflows");
        let mut invocations = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            for line in text.lines() {
                let Some(at) = line.find("cargo xtask ci ") else {
                    continue;
                };
                // A command: a `run:` value or a line of a `run: |` block.
                // Not a step name or a comment that mentions the lane.
                if !matches!(line[..at].trim(), "" | "run:" | "- run:") {
                    continue;
                }
                invocations += 1;
                let words = argv::split(&line[at..]).unwrap();
                let lane = by_name(&words[3]).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let ids: Vec<String> = words
                    .windows(2)
                    .filter(|w| matches!(w[0].as_str(), "--only" | "--skip" | "--from"))
                    .map(|w| w[1].clone())
                    .collect();
                check_ids(&lane, &ids).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            }
        }
        assert!(
            invocations >= 4,
            "found {invocations} `cargo xtask ci` lines in workflows"
        );
    }

    /// The `lint` job runs the whole lane. The panic and determinism gates
    /// trust the clippy commands here only on this exact line.
    #[test]
    fn ci_lint_job_runs_the_whole_lint_lane() {
        let ci = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
        assert!(
            ci.lines().any(|l| l.trim() == "run: cargo xtask ci lint"),
            "ci.yml must run `cargo xtask ci lint`"
        );
    }

    #[test]
    fn the_old_pre_push_entry_point_runs_this_lane() {
        let shim = std::fs::read_to_string(repo_root().join("scripts/pre-push-check.sh")).unwrap();
        assert!(shim.contains("exec cargo xtask pre-push \"$@\""), "{shim}");
        let config = std::fs::read_to_string(repo_root().join(".cargo/config.toml")).unwrap();
        assert!(
            config.contains("xtask = \"run --package xtask --\""),
            ".cargo/config.toml must define the `cargo xtask` alias"
        );
    }

    #[test]
    fn check_ids_names_the_unknown_ids() {
        let lane = lint();
        assert!(check_ids(&lane, &["fmt".to_owned()]).is_ok());
        let err = check_ids(&lane, &["fmt".to_owned(), "fmtt".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`fmtt`"), "{err}");
        assert!(!err.contains("`fmt`,"), "{err}");
        assert!(err.contains("clippy-gated"), "{err}");
    }

    #[test]
    fn by_name_finds_lanes_and_lists_them_on_a_miss() {
        assert_eq!(by_name("lint").unwrap().name, "lint");
        assert_eq!(by_name("pre-push").unwrap().name, "pre-push");
        let err = by_name("nope").unwrap_err().to_string();
        assert!(err.contains("lint") && err.contains("pre-push"), "{err}");
    }
}
