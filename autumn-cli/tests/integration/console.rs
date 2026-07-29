//! `autumn console` scaffolds and runs a pre-wired data playground (issue #1039).
//!
//! These tests exercise the CLI end-to-end against a project produced by
//! `autumn new`, covering the issue's acceptance criteria:
//!
//! * the subcommand exists and appears in `--help`;
//! * the first invocation scaffolds `src/bin/playground.rs` pre-wired with the
//!   shared config + database-URL resolution and a constructed pool;
//! * re-running never overwrites a user-edited playground, and `--force` does;
//! * the generated target compiles (the `#[ignore]`d `cargo check` test);
//! * a missing project exits non-zero with an actionable error.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

fn run_autumn_ok(dir: &Path, args: &[&str]) -> std::process::Output {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// `autumn new <name>`, returning the tempdir guard and the project directory.
fn new_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    assert!(project.join("Cargo.toml").is_file());
    (tmp, project)
}

fn playground_path(project: &Path) -> PathBuf {
    project.join("src/bin/playground.rs")
}

// ── AC1: the subcommand exists and is discoverable ─────────────────────────

#[test]
fn console_appears_in_top_level_help() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_autumn_ok(tmp.path(), &["--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("console"),
        "`autumn --help` must list the console subcommand:\n{help}"
    );
}

#[test]
fn console_has_its_own_help_describing_the_playground() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_autumn_ok(tmp.path(), &["console", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("playground"),
        "`autumn console --help` must describe the playground:\n{help}"
    );
    assert!(
        help.contains("--force"),
        "`autumn console --help` must document --force:\n{help}"
    );
}

// ── AC2: first invocation scaffolds a pre-wired playground ─────────────────

#[test]
fn console_scaffolds_a_prewired_playground() {
    let (_tmp, project) = new_project("console-scaffold-app");
    assert!(!playground_path(&project).exists());

    let out = run_autumn_ok(&project, &["console", "--scaffold-only"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let src = fs::read_to_string(playground_path(&project)).expect("playground must be scaffolded");
    assert!(
        src.contains("your code here"),
        "playground must carry a clearly-marked editable region:\n{src}"
    );
    assert!(
        src.contains("SeedContext::build()"),
        "playground must reuse the shared config + DB-URL resolution:\n{src}"
    );
    assert!(
        src.contains("ctx.conn()"),
        "playground must check out a real pooled connection:\n{src}"
    );
    assert!(
        src.contains("console-scaffold-app"),
        "playground must name the project it belongs to:\n{src}"
    );
    assert!(
        stderr.contains("src/bin/playground.rs"),
        "the CLI must report the file it created:\n{stderr}"
    );
}

#[test]
fn console_wires_the_manifest_for_the_playground_target() {
    let (_tmp, project) = new_project("console-manifest-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("name = \"playground\"") && cargo.contains("src/bin/playground.rs"),
        "Cargo.toml must register the playground bin target:\n{cargo}"
    );
    assert!(
        cargo.contains("\"seed\""),
        "Cargo.toml must enable the autumn-web `seed` feature the playground uses:\n{cargo}"
    );
    assert!(
        cargo.contains("default-run = \"console-manifest-app\""),
        "a second bin needs default-run so bare `cargo run` stays unambiguous:\n{cargo}"
    );
}

// ── AC3: model/repository APIs are reachable from the playground ───────────

#[test]
fn console_wires_generated_models_into_the_playground_crate() {
    let (_tmp, project) = new_project("console-models-app");
    run_autumn_ok(
        &project,
        &["generate", "model", "Post", "title:String", "body:Text"],
    );
    run_autumn_ok(&project, &["console", "--scaffold-only"]);

    let src = fs::read_to_string(playground_path(&project)).unwrap();
    assert!(
        src.contains("mod models;"),
        "a bin target cannot see src/models/ without an explicit module \
         declaration -- the playground must supply one:\n{src}"
    );
    assert!(
        src.contains("mod schema;"),
        "models reference `crate::schema`, so schema must be declared too:\n{src}"
    );
}

// ── AC5: idempotent; --force regenerates ───────────────────────────────────

#[test]
fn console_never_overwrites_an_edited_playground() {
    let (_tmp, project) = new_project("console-idempotent-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);

    let edited = "// MY PRECIOUS QUERY\nfn main() {}\n";
    fs::write(playground_path(&project), edited).unwrap();

    let out = run_autumn_ok(&project, &["console", "--scaffold-only"]);
    assert_eq!(
        fs::read_to_string(playground_path(&project)).unwrap(),
        edited,
        "re-running `autumn console` must never clobber a user-edited playground"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--force"),
        "the CLI must point at --force when it keeps an existing file:\n{stderr}"
    );
}

#[test]
fn console_force_regenerates_from_the_template() {
    let (_tmp, project) = new_project("console-force-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    fs::write(playground_path(&project), "// stale\n").unwrap();

    run_autumn_ok(&project, &["console", "--scaffold-only", "--force"]);
    let src = fs::read_to_string(playground_path(&project)).unwrap();
    assert!(
        src.contains("your code here") && !src.contains("// stale"),
        "--force must regenerate the playground from the template:\n{src}"
    );
}

#[test]
fn console_manifest_wiring_is_idempotent() {
    let (_tmp, project) = new_project("console-manifest-idem-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    let first = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    let second = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    assert_eq!(
        first, second,
        "a second `autumn console` must leave Cargo.toml byte-identical"
    );
    assert_eq!(
        second.matches("name = \"playground\"").count(),
        1,
        "the bin entry must not be duplicated:\n{second}"
    );
    assert_eq!(
        second.matches("\"seed\"").count(),
        1,
        "the seed feature must not be duplicated:\n{second}"
    );
}

// ── AC4: failures are loud and non-zero ────────────────────────────────────

#[test]
fn console_fails_outside_a_cargo_project() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_autumn(tmp.path(), &["console", "--scaffold-only"]);
    assert!(
        !out.status.success(),
        "`autumn console` outside a project must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Cargo.toml"),
        "the error must name what is missing:\n{stderr}"
    );
    assert!(
        !playground_path(tmp.path()).exists(),
        "no file may be written when the project check fails"
    );
}

#[test]
fn console_fails_when_the_project_has_no_autumn_web_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"plain\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n",
    )
    .unwrap();

    let out = run_autumn(tmp.path(), &["console", "--scaffold-only"]);
    assert!(
        !out.status.success(),
        "a non-Autumn Cargo project must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("autumn-web"),
        "the error must name the missing dependency:\n{stderr}"
    );
}

#[test]
fn console_scaffolding_does_not_require_a_database() {
    // Scaffolding is a pure filesystem operation: it must succeed with no
    // database reachable at all. The run path (which does need one) is proved
    // separately by `console_run_exits_non_zero_when_the_database_is_unreachable`.
    let (_tmp, project) = new_project("console-db-split-app");
    let out = run_autumn(&project, &["console", "--scaffold-only"]);
    assert!(
        out.status.success(),
        "scaffolding must not require a database (exit={:?})\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// AC4: "a config or DB-connection failure exits non-zero and prints the
/// underlying error (no silent success)".
///
/// Builds and runs the real playground against a database URL that cannot be
/// reached, and asserts `autumn console` propagates the failure.
///
/// Ignored by default (it compiles a fresh project); run with:
/// `cargo test -p autumn-cli --test cli_tests -- --ignored console_run_exits`
#[test]
#[ignore = "slow: builds and runs a fresh project -- run with `cargo test -p autumn-cli -- --ignored`"]
fn console_run_exits_non_zero_when_the_database_is_unreachable() {
    let (_tmp, project) = new_project("console-dbfail-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    patch_autumn_web_to_workspace(&project);

    let out = Command::new(autumn_bin())
        .args(["console"])
        .current_dir(&project)
        // Port 1 is reserved and never listening.
        .env("DATABASE_URL", "postgres://nobody@127.0.0.1:1/nodb")
        .output()
        .expect("failed to run autumn console");

    assert!(
        !out.status.success(),
        "an unreachable database must not report success\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("autumn console:"),
        "the underlying error must be printed, not swallowed:\n{combined}"
    );
}

// ── AC3/AC7: the generated target actually compiles ────────────────────────

/// Repoint `autumn-web` at the workspace copy so builds in these tests use this
/// tree rather than a published crate.
fn patch_autumn_web_to_workspace(project: &Path) {
    use std::fmt::Write as _;

    let cargo_toml_path = project.join("Cargo.toml");
    let mut content = fs::read_to_string(&cargo_toml_path).unwrap();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web = workspace_root.join("autumn");
    let _ = write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web.display().to_string().replace('\\', "/")
    );
    fs::write(&cargo_toml_path, content).unwrap();
}

/// AC3/AC7: the scaffolded target compiles, and a real repository round-trip
/// (`find_all()`) written into the "your code here" region compiles with it —
/// no further wiring by hand.
///
/// Ignored by default (it builds a fresh project); run with:
/// `cargo test -p autumn-cli --test cli_tests -- --ignored console_playground`
#[test]
#[ignore = "slow: cargo-checks a fresh project -- run with `cargo test -p autumn-cli -- --ignored`"]
fn console_playground_target_compiles_with_a_repository_round_trip() {
    let (_tmp, project) = new_project("console-check-app");
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Post", "title:String", "body:Text"],
    );
    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    patch_autumn_web_to_workspace(&project);

    // Drop a real repository round-trip into the editable region.
    let src = fs::read_to_string(playground_path(&project)).unwrap();
    let marker = "// ── your code here";
    assert!(
        src.contains(marker),
        "expected the editable-region marker in:\n{src}"
    );
    let round_trip = "\
use repositories::post::{PgPostRepository, PostRepository};\n    \
let repo = PgPostRepository::with_pool_untracked(ctx.pool().clone());\n    \
let _rows: Vec<models::post::Post> = repo.find_all().await.unwrap();\n    \
// ── your code here";
    fs::write(playground_path(&project), src.replace(marker, round_trip)).unwrap();

    let check = Command::new("cargo")
        .args(["check", "--bin", "playground"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the scaffolded playground failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
