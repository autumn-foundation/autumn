//! Run a lane's steps and report the outcome.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smop::sh::{self, CommandBuilder};

use crate::argv;
use crate::lanes::{Lane, Step, check_ids};

/// Which steps of a lane to run, and how.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    /// Run only these step ids. Empty means every step.
    pub only: Vec<String>,
    /// Never run these step ids.
    pub skip: Vec<String>,
    /// Start at this step id, skipping the ones before it.
    pub from: Option<String>,
    /// Override the lane's own keep-going setting.
    pub keep_going: Option<bool>,
    /// Print the commands instead of running them.
    pub dry_run: bool,
}

impl Selection {
    /// The steps of `lane` this selection runs, in lane order.
    ///
    /// # Errors
    ///
    /// Returns an error when any id given does not name a step of the lane.
    pub fn steps<'l>(&self, lane: &'l Lane) -> Result<Vec<&'l Step>> {
        check_ids(lane, self.only.iter().chain(&self.skip).chain(&self.from))?;
        let start = self
            .from
            .as_ref()
            .and_then(|f| lane.steps.iter().position(|s| s.id == f.as_str()))
            .unwrap_or(0);
        Ok(lane.steps[start..]
            .iter()
            .filter(|s| self.only.is_empty() || self.only.iter().any(|o| o == s.id))
            .filter(|s| !self.skip.iter().any(|k| k == s.id))
            .collect())
    }
}

/// How one step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exited 0.
    Passed,
    /// Exited non-zero, was killed, or could not be started.
    Failed(String),
    /// Not run: an earlier step failed and the lane stops at the first failure.
    NotRun,
}

/// The result of a lane run.
#[derive(Debug, Default)]
pub struct Report {
    /// One entry per selected step, in order.
    pub results: Vec<(&'static str, &'static str, Outcome, Duration)>,
}

impl Report {
    /// True when every selected step passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.results
            .iter()
            .all(|(_, _, outcome, _)| *outcome == Outcome::Passed)
    }

    /// The summary table, as GitHub-flavoured markdown.
    #[must_use]
    pub fn markdown(&self, lane: &str) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "### `cargo xtask` lane `{lane}`\n\n| Step | Result | Time |\n| --- | --- | --- |\n"
        );
        for (id, title, outcome, took) in &self.results {
            let result = match outcome {
                Outcome::Passed => "passed".to_owned(),
                Outcome::Failed(why) => format!("**failed**: {why}"),
                Outcome::NotRun => "not run".to_owned(),
            };
            let _ = writeln!(
                out,
                "| `{id}` {title} | {result} | {:.1}s |",
                took.as_secs_f64()
            );
        }
        out
    }
}

/// Where a step's log lines go and how they are framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `==> title` headers.
    Plain,
    /// GitHub Actions `::group::` folding and `::error::` annotations.
    GitHub,
}

impl Style {
    /// `GitHub` when running under GitHub Actions.
    #[must_use]
    pub fn detect() -> Self {
        if std::env::var_os("GITHUB_ACTIONS").is_some_and(|v| v == "true") {
            Self::GitHub
        } else {
            Self::Plain
        }
    }
}

/// Run the selected steps of `lane` from `root`, logging to `log`.
///
/// # Errors
///
/// Returns an error for an invalid selection or a failed write to `log`. A
/// step that fails is not an error here: it is recorded in the [`Report`].
pub fn run(
    root: &Path,
    lane: &Lane,
    selection: &Selection,
    style: Style,
    log: &mut impl Write,
) -> Result<Report> {
    let steps = selection.steps(lane)?;
    let keep_going = selection.keep_going.unwrap_or(lane.keep_going);
    let mut report = Report::default();

    if selection.dry_run {
        for step in &steps {
            writeln!(log, "# {} ({})", step.title, step.id)?;
            for (key, value) in &step.env {
                write!(log, "{key}={value} ")?;
            }
            writeln!(log, "{}", argv::join(&step.argv))?;
        }
        return Ok(report);
    }

    let mut stopped = false;
    for step in steps {
        if stopped {
            report
                .results
                .push((step.id, step.title, Outcome::NotRun, Duration::ZERO));
            continue;
        }
        match style {
            Style::Plain => writeln!(
                log,
                "\n==> [{}] {}\n    {}",
                step.id,
                step.title,
                argv::join(&step.argv)
            )?,
            Style::GitHub => writeln!(
                log,
                "::group::{} ({})\n{}",
                step.title,
                step.id,
                argv::join(&step.argv)
            )?,
        }
        log.flush()?;
        let started = Instant::now();
        let outcome = execute(root, step);
        let took = started.elapsed();
        if style == Style::GitHub {
            writeln!(log, "::endgroup::")?;
        }
        if let Outcome::Failed(why) = &outcome {
            match style {
                Style::Plain => writeln!(log, "FAILED [{}] {}: {why}", step.id, step.title)?,
                Style::GitHub => writeln!(log, "::error title={}::{why}", step.title)?,
            }
            stopped = !keep_going;
        }
        report.results.push((step.id, step.title, outcome, took));
    }

    writeln!(log, "\nlane `{}`:", lane.name)?;
    for (id, _, outcome, took) in &report.results {
        let mark = match outcome {
            Outcome::Passed => "ok     ",
            Outcome::Failed(_) => "FAILED ",
            Outcome::NotRun => "not run",
        };
        writeln!(log, "  {mark} {id} ({:.1}s)", took.as_secs_f64())?;
    }
    Ok(report)
}

fn execute(root: &Path, step: &Step) -> Outcome {
    let command = match command_for(root, &step.argv) {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let command = step
        .env
        .iter()
        .fold(command.dir(root), |c, (key, value)| c.env(key, value));
    // The error names the exit code and the command line, or why the
    // program could not start.
    match command.run() {
        Ok(()) => Outcome::Passed,
        Err(e) => Outcome::Failed(format!("{e:#}")),
    }
}

/// Build the process for `argv`, resolving the program:
///
/// - `cargo` is the cargo that launched xtask (`$CARGO`), so a nested run
///   uses the same toolchain;
/// - a `*.sh` or `*.py` script runs under the interpreter its shebang names
///   (`check-example-bin-names.sh` is Python), falling back to its extension,
///   so it runs the same way on Windows as `./scripts/x` does on Unix;
/// - anything else runs as written.
///
/// # Errors
///
/// Returns an error for an empty `argv`.
pub fn command_for(root: &Path, argv: &[String]) -> Result<CommandBuilder> {
    let (program, rest) = argv.split_first().context("empty command")?;
    let command = if program == "cargo" {
        sh::cargo()
    } else if let Some(fallback) = interpreter_for(program) {
        let script = root.join(program);
        let first = std::fs::read_to_string(&script)
            .ok()
            .and_then(|text| text.lines().next().map(str::to_owned))
            .unwrap_or_default();
        match shebang_interpreter(&first).unwrap_or(fallback) {
            "bash" => sh::cmd(bash()),
            other => sh::cmd(other),
        }
        .arg(script)
    } else {
        sh::cmd(program)
    };
    Ok(command.args(rest))
}

/// The interpreter a `#!` line names, for the interpreters scripts here use.
#[must_use]
pub fn shebang_interpreter(first_line: &str) -> Option<&'static str> {
    let line = first_line.strip_prefix("#!")?.trim();
    let program = line
        .split_whitespace()
        .find(|word| !word.ends_with("/env") && *word != "env")?;
    let name = program.rsplit('/').next().unwrap_or(program);
    match name {
        "bash" => Some("bash"),
        "sh" => Some("sh"),
        n if n.starts_with("python") => Some(python()),
        _ => None,
    }
}

/// The `bash` to run a script with.
///
/// On Windows a bare `bash` is the wrong program: `CreateProcess`, and so
/// Rust's `Command`, looks in the system directory before `PATH`, where
/// `System32\bash.exe` is the WSL launcher. With no Linux distribution
/// installed (GitHub's runners have none) it prints an install hint and
/// exits 1, so every script "failed". The first `bash.exe` on `PATH` outside
/// the Windows directory is Git for Windows' bash, which is what the scripts
/// are written for.
fn bash() -> OsString {
    if cfg!(windows)
        && let Some(found) = std::env::var_os("PATH").and_then(|path| {
            bash_on_path(
                &path,
                std::env::var_os("SystemRoot").as_deref(),
                "bash.exe",
                &|p: &Path| p.is_file(),
            )
        })
    {
        return found.into_os_string();
    }
    OsString::from("bash")
}

/// The first `name` in the directories of `path` that is a file and does not
/// sit under `system_root`, compared without regard to case as Windows paths
/// are.
fn bash_on_path(
    path: &OsStr,
    system_root: Option<&OsStr>,
    name: &str,
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let system_root = system_root.map(|r| r.to_string_lossy().to_lowercase());
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .filter(|candidate| is_file(candidate))
        .find(|candidate| {
            system_root.as_ref().is_none_or(|root| {
                !candidate
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with(root.as_str())
            })
        })
}

const fn python() -> &'static str {
    // Windows installs `python`; `python3` is a Store stub there.
    if cfg!(windows) { "python" } else { "python3" }
}

/// The interpreter for a script path, by extension.
#[must_use]
pub fn interpreter_for(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?;
    match ext {
        "sh" => Some("bash"),
        "py" => Some(python()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &'static str, line: &str) -> Step {
        Step {
            id,
            title: id,
            argv: argv::split(line).unwrap(),
            env: Vec::new(),
        }
    }

    fn lane(keep_going: bool, steps: Vec<Step>) -> Lane {
        Lane {
            name: "t",
            about: "test lane",
            steps,
            keep_going,
        }
    }

    fn ids(steps: &[&Step]) -> Vec<&'static str> {
        steps.iter().map(|s| s.id).collect()
    }

    fn root() -> &'static Path {
        crate::repo_root()
    }

    fn sample() -> Lane {
        lane(
            false,
            vec![
                step("a", "cargo --version"),
                step("b", "cargo --version"),
                step("c", "cargo --version"),
            ],
        )
    }

    #[test]
    fn selection_defaults_to_every_step() {
        let l = sample();
        assert_eq!(
            ids(&Selection::default().steps(&l).unwrap()),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn only_skip_and_from_combine_in_lane_order() {
        let l = sample();
        let s = Selection {
            only: vec!["c".into(), "a".into()],
            ..Selection::default()
        };
        assert_eq!(ids(&s.steps(&l).unwrap()), ["a", "c"]);

        let s = Selection {
            skip: vec!["b".into()],
            ..Selection::default()
        };
        assert_eq!(ids(&s.steps(&l).unwrap()), ["a", "c"]);

        let s = Selection {
            from: Some("b".into()),
            ..Selection::default()
        };
        assert_eq!(ids(&s.steps(&l).unwrap()), ["b", "c"]);
    }

    #[test]
    fn an_unknown_id_in_any_filter_is_an_error() {
        let l = sample();
        for s in [
            Selection {
                only: vec!["x".into()],
                ..Selection::default()
            },
            Selection {
                skip: vec!["x".into()],
                ..Selection::default()
            },
            Selection {
                from: Some("x".into()),
                ..Selection::default()
            },
        ] {
            assert!(s.steps(&l).is_err(), "{s:?}");
        }
    }

    #[test]
    fn dry_run_prints_commands_and_runs_nothing() {
        let l = lane(
            false,
            vec![Step {
                env: vec![("K", "v")],
                ..step("a", r#"definitely-not-a-program --features "x,y""#)
            }],
        );
        let mut log = Vec::new();
        let sel = Selection {
            dry_run: true,
            ..Selection::default()
        };
        let report = run(root(), &l, &sel, Style::Plain, &mut log).unwrap();
        assert!(report.results.is_empty());
        let log = String::from_utf8(log).unwrap();
        assert!(
            log.contains(r#"K=v definitely-not-a-program --features "x,y""#),
            "{log}"
        );
    }

    fn failing() -> Step {
        step("bad", "cargo --no-such-flag-for-xtask-tests")
    }

    #[test]
    fn fail_fast_stops_at_the_first_failure() {
        let l = lane(
            false,
            vec![
                step("ok", "cargo --version"),
                failing(),
                step("after", "cargo --version"),
            ],
        );
        let mut log = Vec::new();
        let report = run(root(), &l, &Selection::default(), Style::Plain, &mut log).unwrap();
        assert!(!report.passed());
        let outcomes: Vec<_> = report.results.iter().map(|r| r.2.clone()).collect();
        assert_eq!(outcomes[0], Outcome::Passed);
        assert!(matches!(outcomes[1], Outcome::Failed(_)));
        assert_eq!(outcomes[2], Outcome::NotRun);
        let log = String::from_utf8(log).unwrap();
        assert!(log.contains("FAILED [bad]"), "{log}");
    }

    #[test]
    fn keep_going_runs_every_step_and_reports_each_failure() {
        let l = lane(true, vec![failing(), step("after", "cargo --version")]);
        let mut log = Vec::new();
        let report = run(root(), &l, &Selection::default(), Style::GitHub, &mut log).unwrap();
        assert!(!report.passed());
        assert_eq!(report.results[1].2, Outcome::Passed);
        let log = String::from_utf8(log).unwrap();
        assert!(log.contains("::group::bad (bad)"), "{log}");
        assert!(log.contains("::endgroup::"), "{log}");
        assert!(
            log.contains("::error title=bad::Command failed with exit code"),
            "{log}"
        );
        let md = report.markdown("t");
        assert!(
            md.contains("| `bad` bad | **failed**: Command failed with exit code"),
            "{md}"
        );
        assert!(md.contains("| `after` after | passed |"), "{md}");
    }

    #[test]
    fn the_selection_can_override_keep_going() {
        let l = lane(true, vec![failing(), step("after", "cargo --version")]);
        let sel = Selection {
            keep_going: Some(false),
            ..Selection::default()
        };
        let report = run(root(), &l, &sel, Style::Plain, &mut Vec::new()).unwrap();
        assert_eq!(report.results[1].2, Outcome::NotRun);
    }

    #[test]
    fn a_missing_program_is_a_failure_not_an_error() {
        let l = lane(
            false,
            vec![step("gone", "xtask-test-program-that-does-not-exist")],
        );
        let report = run(
            root(),
            &l,
            &Selection::default(),
            Style::Plain,
            &mut Vec::new(),
        )
        .unwrap();
        match &report.results[0].2 {
            Outcome::Failed(why) => assert!(why.contains("Failed to execute command"), "{why}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_step_gets_its_own_environment() {
        // `cargo xtaskprobe` exists only when the environment defines the
        // alias, so the step passes exactly when its env reached the child.
        let probe = || step("env", "cargo xtaskprobe");
        let with_env = lane(
            false,
            vec![Step {
                env: vec![("CARGO_ALIAS_XTASKPROBE", "version")],
                ..probe()
            }],
        );
        let report = run(
            root(),
            &with_env,
            &Selection::default(),
            Style::Plain,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(report.passed(), "{:?}", report.results);

        let without = lane(false, vec![probe()]);
        let report = run(
            root(),
            &without,
            &Selection::default(),
            Style::Plain,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(!report.passed());
        assert!(std::env::var_os("CARGO_ALIAS_XTASKPROBE").is_none());
    }

    #[test]
    fn interpreters_follow_the_extension() {
        assert_eq!(interpreter_for("scripts/check-msrv.sh"), Some("bash"));
        assert!(interpreter_for("scripts/check-perf-profiles.py").is_some());
        assert_eq!(interpreter_for("node"), None);
        assert_eq!(interpreter_for("scripts/data.txt"), None);
    }

    #[test]
    fn the_shebang_names_the_interpreter() {
        assert_eq!(shebang_interpreter("#!/usr/bin/env bash"), Some("bash"));
        assert_eq!(shebang_interpreter("#!/bin/bash -e"), Some("bash"));
        assert_eq!(shebang_interpreter("#!/bin/sh"), Some("sh"));
        assert_eq!(
            shebang_interpreter("#!/usr/bin/env python3"),
            Some(python())
        );
        assert_eq!(shebang_interpreter("#!/usr/bin/env -S python3 -I"), None);
        assert_eq!(shebang_interpreter("#!/usr/bin/env node"), None);
        assert_eq!(shebang_interpreter("set -e"), None);
    }

    /// `check-example-bin-names.sh` is a Python script. Running it under
    /// bash because of its name was a real bug.
    #[test]
    fn a_python_script_named_sh_runs_under_python() {
        let c = command_for(root(), &["scripts/check-example-bin-names.sh".into()]).unwrap();
        assert!(c.to_string().starts_with(&format!("{} ", python())), "{c}");
    }

    #[test]
    fn scripts_run_from_the_repo_root_under_their_interpreter() {
        let c = command_for(root(), &["scripts/check-msrv.sh".into(), "--x".into()]).unwrap();
        let script = root().join("scripts/check-msrv.sh");
        let line = c.to_string();
        assert!(line.contains("bash"), "{line}");
        assert!(
            line.ends_with(&format!("{} --x", script.display())),
            "{line}"
        );
    }

    /// `System32\bash.exe` is the WSL launcher, not a bash: skipping it, the
    /// search finds Git for Windows' bash later on `PATH`.
    #[test]
    fn bash_search_skips_the_windows_directory() {
        let path = std::env::join_paths(["/WINDOWS/system32", "/git/bin", "/other/bin"]).unwrap();
        let exists = |p: &Path| p.starts_with("/WINDOWS/system32") || p.starts_with("/git/bin");
        let found = bash_on_path(&path, Some(OsStr::new("/windows")), "bash.exe", &exists);
        assert_eq!(found, Some(PathBuf::from("/git/bin/bash.exe")));
    }

    #[test]
    fn bash_search_finds_nothing_without_a_bash_outside_the_windows_directory() {
        let path = std::env::join_paths(["/WINDOWS/system32", "/other/bin"]).unwrap();
        let exists = |p: &Path| p.starts_with("/WINDOWS/system32");
        assert_eq!(
            bash_on_path(&path, Some(OsStr::new("/Windows")), "bash.exe", &exists),
            None
        );
        // Without a system root nothing is skipped.
        assert_eq!(
            bash_on_path(&path, None, "bash.exe", &exists),
            Some(PathBuf::from("/WINDOWS/system32/bash.exe"))
        );
    }

    #[test]
    fn bash_is_plain_bash_off_windows() {
        if !cfg!(windows) {
            assert_eq!(bash(), OsString::from("bash"));
        }
    }

    #[test]
    fn an_empty_command_is_an_error() {
        assert!(command_for(root(), &[]).is_err());
    }
}
