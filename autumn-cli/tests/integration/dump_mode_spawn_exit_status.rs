//! Characterizes the spawn/exit-status handling shared by `autumn cache
//! audit`, `autumn graph show`, `autumn agents manifest` and `autumn
//! data-flow` before the Echo merge that unifies their byte-identical
//! "compile, spawn in dump mode, forward stderr, check exit status" block
//! into one `routes::run_dump_mode` (jscpd clone class: cache_audit.rs,
//! graph.rs, agents.rs, data_flow.rs).
//!
//! Each of the four commands reaches this same block regardless of what it
//! dumps, so one trivial fixture binary — which never prints a manifest —
//! exercises it identically for all four: the two outcomes that block can
//! produce (the binary fails to run cleanly, or it exits 0 having printed
//! nothing the command recognizes) are pinned here; the manifest-specific
//! "produced no X" message past that block is untouched by the merge and
//! only spot-checked for routing.

use std::path::PathBuf;
use std::process::{Command, Output};

const FIXTURE: &str = "dump_mode_fixture";

/// Always prints a fixed line to stderr first (so stderr-forwarding is
/// observable), then either exits with `FIXTURE_FAIL_CODE` or exits 0
/// printing nothing to stdout. It never looks at which `AUTUMN_DUMP_*` var
/// is set: all four commands under test reach the shared block before
/// anything manifest-specific, so the fixture doesn't need to tell them
/// apart.
const FIXTURE_MAIN: &str = r#"fn main() {
    eprintln!("fixture: started");
    if let Ok(code) = std::env::var("FIXTURE_FAIL_CODE") {
        eprintln!("fixture: deliberate failure");
        std::process::exit(code.parse().unwrap());
    }
}
"#;

struct Fixture {
    root: tempfile::TempDir,
    app: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let app = root.path().join("app");
        std::fs::create_dir_all(app.join("src")).expect("mkdir src");
        std::fs::write(
            app.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{FIXTURE}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n"
            ),
        )
        .expect("write Cargo.toml");
        std::fs::write(app.join("src/main.rs"), FIXTURE_MAIN).expect("write main.rs");
        Self { root, app }
    }

    /// Run `autumn <args>` in the fixture app. `fail_code`, when set, makes
    /// the fixture binary exit with that code instead of 0.
    fn autumn(&self, args: &[&str], fail_code: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_autumn"));
        cmd.args(args)
            .current_dir(&self.app)
            // An empty `CARGO_HOME` stops a user config from adding a triple
            // or a target directory the lookup wouldn't expect.
            .env("CARGO_HOME", self.root.path().join("cargo-home"))
            .env("CARGO_TARGET_DIR", self.app.join("target"))
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET");
        if let Some(code) = fail_code {
            cmd.env("FIXTURE_FAIL_CODE", code);
        } else {
            cmd.env_remove("FIXTURE_FAIL_CODE");
        }
        cmd.output().expect("run autumn")
    }
}

fn describe(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

/// (CLI args, the gerund phrase each command's "Binary exited with status…"
/// and "produced no …" messages name what is being dumped with).
const COMMANDS: &[(&[&str], &str, &str)] = &[
    (
        &["cache", "audit"],
        "the cache-coherence manifest",
        "cache-coherence manifest",
    ),
    (
        &["graph", "show"],
        "the architecture graph",
        "architecture graph",
    ),
    (
        &["agents", "manifest"],
        "the agent-authority manifest",
        "agent-authority manifest",
    ),
    (
        &["data-flow"],
        "the data-flow manifest",
        "data-flow manifest",
    ),
];

#[test]
fn nonzero_exit_is_reported_with_the_right_label_and_propagated_code() {
    let fixture = Fixture::new();
    for (args, exit_label, _) in COMMANDS {
        let output = fixture.autumn(args, Some("7"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("fixture: started"),
            "`autumn {}` did not forward the fixture's stderr: {}",
            args.join(" "),
            describe(&output)
        );
        assert!(
            stderr.contains("fixture: deliberate failure"),
            "`autumn {}` did not forward the fixture's stderr: {}",
            args.join(" "),
            describe(&output)
        );
        assert!(
            stderr.contains(&format!("Binary exited with status")) && stderr.contains(*exit_label),
            "`autumn {}` did not report the exit status against {exit_label}: {}",
            args.join(" "),
            describe(&output)
        );
        assert_eq!(
            output.status.code(),
            Some(7),
            "`autumn {}` did not propagate the child's exit code: {}",
            args.join(" "),
            describe(&output)
        );
    }
}

#[test]
fn clean_exit_with_no_manifest_is_reported_distinctly_from_a_crash() {
    let fixture = Fixture::new();
    for (args, _, no_manifest_label) in COMMANDS {
        let output = fixture.autumn(args, None);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("fixture: started"),
            "`autumn {}` did not forward the fixture's stderr: {}",
            args.join(" "),
            describe(&output)
        );
        assert!(
            !stderr.contains("Binary exited with status"),
            "`autumn {}` reported a crash for a clean exit: {}",
            args.join(" "),
            describe(&output)
        );
        assert!(
            stderr.contains(&format!("produced no {no_manifest_label}")),
            "`autumn {}` did not report the missing manifest distinctly: {}",
            args.join(" "),
            describe(&output)
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "`autumn {}` should exit 1 on a missing manifest: {}",
            args.join(" "),
            describe(&output)
        );
    }
}
