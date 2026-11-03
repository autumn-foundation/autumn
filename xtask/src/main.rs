//! `cargo xtask`: the Autumn workspace's maintainer task runner.
//!
//! Run `cargo xtask list` for everything it can do. Lanes (CI's `lint` job,
//! the local pre-push gate) are defined in [`lanes`]; the scripts under
//! `scripts/` are found by listing the directory ([`scripts`]). See
//! `xtask/README.md` and `docs/adr/0033-xtask-for-repo-tooling.md`.

mod argv;
mod lanes;
mod runner;
mod scripts;

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::runner::{Selection, Style};

/// The workspace root: the directory above this crate.
///
/// Taken from the build, not the current directory, so `cargo xtask` works
/// from anywhere in the tree.
#[must_use]
pub fn repo_root() -> &'static Path {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest)
}

#[derive(Debug, Parser)]
#[command(
    name = "cargo xtask",
    bin_name = "cargo xtask",
    about = "Maintainer tasks for the Autumn workspace",
    long_about = "Maintainer tasks for the Autumn workspace.\n\nLanes are ordered lists of checks: `ci lint` is CI's lint job, `pre-push` is the local gate. Every script under scripts/ also runs from here."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the lanes and every script under scripts/.
    List,
    /// Run a CI lane (`lint`): the same commands, in the same order, as CI.
    Ci {
        /// Lane name.
        lane: String,
        #[command(flatten)]
        options: LaneOptions,
    },
    /// Compile everything CI compiles, without running trybuild.
    ///
    /// Run it before pushing: a narrow `cargo test -p <pkg>` never links the
    /// autumn-web consolidated `integration_tests` binary, so it misses
    /// cross-package compile breaks.
    PrePush {
        #[command(flatten)]
        options: LaneOptions,
    },
    /// Run a script under scripts/ by name, e.g. `check-msrv`.
    Script {
        /// Script name, with or without its extension.
        name: String,
        /// Arguments passed to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run `scripts/check-<name>`, e.g. `cargo xtask check docs-links`.
    Check {
        /// Gate name without the `check-` prefix.
        name: String,
        /// Arguments passed to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent command-line flag"
)]
struct LaneOptions {
    /// Print the lane's steps and exit.
    #[arg(long)]
    list: bool,
    /// Print the commands without running them.
    #[arg(long)]
    dry_run: bool,
    /// Run only this step (repeatable).
    #[arg(long, value_name = "STEP")]
    only: Vec<String>,
    /// Skip this step (repeatable).
    #[arg(long, value_name = "STEP")]
    skip: Vec<String>,
    /// Start at this step.
    #[arg(long, value_name = "STEP")]
    from: Option<String>,
    /// Run every step even after one fails.
    #[arg(long, conflicts_with = "fail_fast")]
    keep_going: bool,
    /// Stop at the first failing step.
    #[arg(long)]
    fail_fast: bool,
}

impl LaneOptions {
    fn selection(&self) -> Selection {
        Selection {
            only: self.only.clone(),
            skip: self.skip.clone(),
            from: self.from.clone(),
            keep_going: if self.keep_going {
                Some(true)
            } else if self.fail_fast {
                Some(false)
            } else {
                None
            },
            dry_run: self.dry_run,
        }
    }
}

fn main() -> ExitCode {
    match dispatch(Cli::parse(), &mut std::io::stdout().lock()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli, out: &mut impl Write) -> Result<ExitCode> {
    let root = repo_root();
    match cli.command {
        Command::List => {
            list(root, out)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Ci { lane, options } => {
            let lane = lanes::by_name(&lane)?;
            run_lane(root, &lane, &options, out)
        }
        Command::PrePush { options } => run_lane(root, &lanes::pre_push(), &options, out),
        Command::Script { name, args } => run_script(root, &name, &args),
        Command::Check { name, args } => {
            let name = name.trim_start_matches("check-");
            run_script(root, &format!("check-{name}"), &args)
        }
    }
}

fn run_lane(
    root: &Path,
    lane: &lanes::Lane,
    options: &LaneOptions,
    out: &mut impl Write,
) -> Result<ExitCode> {
    if options.list {
        writeln!(out, "lane `{}`: {}", lane.name, lane.about)?;
        for step in &lane.steps {
            writeln!(out, "  {:<28} {}", step.id, step.title)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let report = runner::run(root, lane, &options.selection(), Style::detect(), out)?;
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY")
        && !options.dry_run
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary)
            .with_context(|| format!("opening {}", Path::new(&summary).display()))?;
        file.write_all(report.markdown(lane.name).as_bytes())?;
    }
    Ok(if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_script(root: &Path, name: &str, args: &[String]) -> Result<ExitCode> {
    let script = scripts::resolve(root, name)?;
    let status = scripts::run(root, &script, args)?;
    Ok(status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

fn list(root: &Path, out: &mut impl Write) -> Result<()> {
    writeln!(out, "Lanes")?;
    for lane in lanes::all() {
        let how = if lane.name == "pre-push" {
            "cargo xtask pre-push".to_owned()
        } else {
            format!("cargo xtask ci {}", lane.name)
        };
        writeln!(
            out,
            "  {how:<36} {} ({} steps)",
            lane.about,
            lane.steps.len()
        )?;
    }
    let scripts = scripts::discover(root)?;
    let mut groups: Vec<&str> = scripts.iter().map(scripts::Script::group).collect();
    groups.dedup();
    groups.sort_unstable();
    groups.dedup();
    for group in groups {
        writeln!(out, "\nScripts: {group}  (cargo xtask script <name>)")?;
        for s in scripts.iter().filter(|s| s.group() == group) {
            writeln!(out, "  {:<36} {}", s.name(), s.about)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("cargo xtask").chain(args.iter().copied())).unwrap()
    }

    fn output(args: &[&str]) -> (ExitCode, String) {
        let mut out = Vec::new();
        let code = dispatch(parse(args), &mut out).unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_repo_root_holds_the_workspace_manifest() {
        assert!(repo_root().join("Cargo.toml").is_file());
        assert!(repo_root().join("xtask/Cargo.toml").is_file());
    }

    #[test]
    fn list_shows_lanes_and_grouped_scripts() {
        let (code, out) = output(&["list"]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.contains("cargo xtask ci lint"), "{out}");
        assert!(out.contains("cargo xtask pre-push"), "{out}");
        assert!(out.contains("Scripts: docs gates"), "{out}");
        assert!(out.contains("check-docs-links"), "{out}");
        assert_eq!(out.matches("Scripts: gates ").count(), 1, "{out}");
    }

    #[test]
    fn lane_list_prints_step_ids() {
        let (code, out) = output(&["ci", "lint", "--list"]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(out.contains("clippy-gated"), "{out}");
        let (_, out) = output(&["pre-push", "--list"]);
        assert!(out.contains("test-compile-gated"), "{out}");
    }

    #[test]
    fn dry_run_prints_the_selected_commands() {
        let (code, out) = output(&["ci", "lint", "--dry-run", "--only", "clippy-gated"]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(
            out.contains("cargo clippy -p autumn-web --features"),
            "{out}"
        );
        assert!(!out.contains("cargo fmt"), "{out}");
    }

    #[test]
    fn an_unknown_lane_or_step_is_an_error() {
        assert!(dispatch(parse(&["ci", "nope"]), &mut Vec::new()).is_err());
        assert!(dispatch(parse(&["ci", "lint", "--only", "nope"]), &mut Vec::new()).is_err());
    }

    #[test]
    fn keep_going_and_fail_fast_conflict() {
        assert!(
            Cli::try_parse_from(["cargo xtask", "pre-push", "--keep-going", "--fail-fast"])
                .is_err()
        );
        let Command::PrePush { options } = parse(&["pre-push", "--fail-fast"]).command else {
            panic!("expected pre-push");
        };
        assert_eq!(options.selection().keep_going, Some(false));
        let Command::PrePush { options } = parse(&["pre-push", "--keep-going"]).command else {
            panic!("expected pre-push");
        };
        assert_eq!(options.selection().keep_going, Some(true));
    }

    #[test]
    fn script_arguments_pass_through_including_flags() {
        let Command::Check { name, args } = parse(&["check", "panic-gate", "--self-test"]).command
        else {
            panic!("expected check");
        };
        assert_eq!(name, "panic-gate");
        assert_eq!(args, ["--self-test"]);
    }

    #[test]
    fn an_unknown_script_is_an_error() {
        assert!(dispatch(parse(&["script", "no-such-script"]), &mut Vec::new()).is_err());
        assert!(dispatch(parse(&["check", "no-such-gate"]), &mut Vec::new()).is_err());
    }
}
