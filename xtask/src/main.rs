//! `cargo xtask`: the Autumn workspace's maintainer task runner.
//!
//! Run `cargo xtask list` for everything it can do. Lanes (CI's `lint` job,
//! the local pre-push gate) are defined in [`lanes`]; the scripts under
//! `scripts/` are found by listing the directory ([`scripts`]); docs gates
//! ported from those scripts live in [`docs`]. See `xtask/README.md` and
//! `docs/adr/0033-xtask-for-repo-tooling.md`.

mod argv;
mod docs;
mod lanes;
mod runner;
mod scripts;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, bail};
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
    /// List the lanes, the docs gates and every script under scripts/.
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
    /// Run a gate by name: a ported docs gate (`docs-links`), else
    /// `scripts/check-<name>`, e.g. `cargo xtask check msrv`.
    Check {
        /// Gate name without the `check-` prefix.
        name: String,
        /// Arguments passed to the script.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a ported docs gate, e.g. `cargo xtask docs links`.
    Docs {
        /// Gate name, with or without `docs-` (`links`, `docs-links`).
        gate: String,
        /// Check this checkout instead of the one xtask was built from.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// Print the pages the gate reads, one per line, instead of checking.
        #[arg(long, conflicts_with = "list")]
        corpus: bool,
        /// Print the truth set the gate checks against, one per line.
        #[arg(long)]
        list: bool,
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
    /// Run only the steps of this shard, as one CI job does.
    #[arg(long, value_name = "SHARD")]
    shard: Option<String>,
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
            shard: self.shard.clone(),
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
            if let Some(gate) = docs::gate(name) {
                if !args.is_empty() {
                    bail!("`{}` takes no arguments; it is not a script", gate.name);
                }
                return run_gate(gate, root, out);
            }
            run_script(root, &format!("check-{name}"), &args)
        }
        Command::Docs {
            gate,
            root: dir,
            corpus,
            list,
        } => {
            let name = if gate.starts_with("docs-") {
                gate.clone()
            } else {
                format!("docs-{gate}")
            };
            let Some(found) = docs::gate(&name) else {
                let known: Vec<_> = docs::GATES.iter().map(|g| g.name).collect();
                bail!("no docs gate `{gate}`; known: {}", known.join(", "));
            };
            let root = dir.as_deref().unwrap_or(root);
            let listing = if corpus {
                Some((found.corpus(root), "--corpus"))
            } else if list {
                Some((found.list(root), "--list"))
            } else {
                None
            };
            match listing {
                None => run_gate(found, root, out),
                Some((None, flag)) => bail!("`{}` has no `{flag}`", found.name),
                Some((Some(lines), _)) => {
                    for line in lines? {
                        writeln!(out, "{line}")?;
                    }
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
}

fn run_gate(gate: &docs::Gate, root: &Path, out: &mut impl Write) -> Result<ExitCode> {
    Ok(if gate.run(root, out)? {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
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
            match step.shard {
                Some(shard) => writeln!(out, "  {:<28} {:<22} {}", step.id, shard, step.title)?,
                None => writeln!(out, "  {:<28} {}", step.id, step.title)?,
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let report = runner::run(root, lane, &options.selection(), Style::detect(), out)?;
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY")
        && !options.dry_run
    {
        smop::fs::append(&summary, report.markdown(lane.name))?;
    }
    Ok(if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `cargo xtask` exits with the script's own code: a usage error stays 2.
/// A script killed by a signal has no code and exits 1.
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
    writeln!(out, "\nDocs gates  (cargo xtask check <name>)")?;
    for gate in docs::GATES {
        writeln!(out, "  {:<36} {}", gate.name, gate.about)?;
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
        assert!(out.contains("check-docs-orphans"), "{out}");
        assert!(out.contains("Docs gates"), "{out}");
        assert!(out.contains("  docs-links "), "{out}");
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
    fn check_and_docs_run_a_ported_gate() {
        for args in [
            &["check", "docs-links"][..],
            &["docs", "links"],
            &["docs", "docs-links"],
        ] {
            let (code, out) = output(args);
            assert_eq!(code, ExitCode::SUCCESS, "{args:?}: {out}");
            assert!(out.contains("Markdown link gate OK."), "{args:?}: {out}");
        }
        assert!(
            dispatch(
                parse(&["check", "docs-links", "--self-test"]),
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(dispatch(parse(&["docs", "nope"]), &mut Vec::new()).is_err());
    }

    #[test]
    fn a_gate_answers_corpus_and_list_when_it_has_them() {
        let (code, out) = output(&["docs", "routes", "--corpus"]);
        assert_eq!(code, ExitCode::SUCCESS, "{out}");
        assert!(out.lines().any(|l| l == "README.md"), "{out}");
        let (code, out) = output(&["docs", "routes", "--list"]);
        assert_eq!(code, ExitCode::SUCCESS, "{out}");
        assert!(out.lines().any(|l| l == "/actuator/health"), "{out}");
        assert!(dispatch(parse(&["docs", "links", "--corpus"]), &mut Vec::new()).is_err());
    }

    /// Every `cargo xtask check <name>` a workflow runs must name a docs
    /// gate or a script, or the step fails only when CI reaches it.
    #[test]
    fn every_workflow_check_names_a_gate_or_a_script() {
        let dir = repo_root().join(".github/workflows");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let Some(at) = line.find("cargo xtask check ") else {
                    continue;
                };
                if line.trim_start().starts_with('#') {
                    continue;
                }
                let name = line[at + "cargo xtask check ".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap();
                seen += 1;
                let script = format!("check-{}", name.trim_start_matches("check-"));
                assert!(
                    docs::gate(name).is_some() || scripts::resolve(repo_root(), &script).is_ok(),
                    "{}: `cargo xtask check {name}` names no gate or script",
                    path.display()
                );
            }
        }
        assert!(seen >= 1, "no `cargo xtask check` in any workflow");
    }

    /// A script's exit code is passed through, not folded into 1, so a
    /// caller can still tell a usage error (2) from a failed check (1).
    #[test]
    fn a_script_exit_code_passes_through() {
        let root = smop::fs::temp_dir().unwrap();
        let scripts = root.path().join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(
            scripts.join("exits.sh"),
            "#!/usr/bin/env bash\n# Exit with the code given.\nexit \"$1\"\n",
        )
        .unwrap();
        for (arg, want) in [("0", 0u8), ("1", 1), ("3", 3)] {
            let code = run_script(root.path(), "exits", &[arg.to_owned()]).unwrap();
            assert_eq!(code, ExitCode::from(want), "exit {arg}");
        }
    }

    #[test]
    fn an_unknown_script_is_an_error() {
        assert!(dispatch(parse(&["script", "no-such-script"]), &mut Vec::new()).is_err());
        assert!(dispatch(parse(&["check", "no-such-gate"]), &mut Vec::new()).is_err());
    }
}
