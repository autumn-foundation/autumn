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
    let before = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    run_autumn_ok(&project, &["console", "--scaffold-only"]);

    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("name = \"playground\"") && cargo.contains("src/bin/playground.rs"),
        "Cargo.toml must register the playground bin target:\n{cargo}"
    );
    assert!(
        cargo.contains("required-features = [\"playground\"]"),
        "the bin must be feature-gated so a bare `cargo build` — which is what \
         `autumn dev` runs — never compiles it:\n{cargo}"
    );
    assert!(
        cargo.contains("playground = [\"autumn-web/seed\"]"),
        "the gate feature must turn on the autumn-web feature the playground \
         bootstraps through:\n{cargo}"
    );
    assert!(
        !cargo.contains("default-run"),
        "the feature gate keeps `cargo run` unambiguous, so default-run — which \
         bricks every cargo command when it names a target that does not exist \
         — must not be written:\n{cargo}"
    );

    // The `autumn-web` dependency line must be byte-identical: we never
    // rewrite a line whose shape and decoration we do not control.
    let dep_line = |src: &str| {
        src.lines()
            .find(|l| l.trim_start().starts_with("autumn-web ="))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        dep_line(&cargo),
        dep_line(&before),
        "the autumn-web dependency must not be touched"
    );
}

/// The whole point of the `required-features` gate: scaffolding a playground
/// must not change what a bare `cargo build` compiles. This is the regression
/// test for "`autumn console` broke `autumn dev`".
#[test]
fn console_keeps_the_playground_out_of_the_default_build_set() {
    let (_tmp, project) = new_project("console-buildset-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);

    let out = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&project)
        .output()
        .expect("cargo metadata");
    assert!(
        out.status.success(),
        "the wired manifest must still parse:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta = String::from_utf8_lossy(&out.stdout);
    assert!(
        meta.contains("\"required-features\":[\"playground\"]")
            || meta.contains("\"required-features\": [\"playground\"]"),
        "cargo must see the playground target as feature-gated:\n{meta}"
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
        second.matches("playground = [").count(),
        1,
        "the gate feature must not be duplicated:\n{second}"
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

    let before = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();

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
    assert!(
        !playground_path(tmp.path()).exists(),
        "validation must run before any write"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
        before,
        "a rejected project's manifest must be left byte-identical"
    );
}

#[test]
fn console_fails_on_an_unparsable_manifest_without_writing_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let broken = "this is not = = toml\n";
    fs::write(tmp.path().join("Cargo.toml"), broken).unwrap();

    let out = run_autumn(tmp.path(), &["console", "--scaffold-only"]);
    assert!(
        !out.status.success(),
        "an unparsable manifest must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Cargo.toml"),
        "the error must name the file it could not parse:\n{stderr}"
    );
    assert!(
        !playground_path(tmp.path()).exists(),
        "validation must run before any write"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
        broken,
        "a manifest we cannot parse must never be rewritten"
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

/// Run `autumn console` (the real build-and-run path) in `project`, with the
/// higher-precedence database env vars scrubbed so an ambient
/// `AUTUMN_DATABASE__*` on the developer's box or the CI runner cannot decide
/// the outcome, and `extra` env applied on top.
fn run_console_for_real(project: &Path, extra: &[(&str, &str)]) -> (String, Option<i32>) {
    let out = Command::new(autumn_bin())
        .args(["console"])
        .current_dir(project)
        .env_remove("AUTUMN_DATABASE__PRIMARY_URL")
        .env_remove("AUTUMN_DATABASE__URL")
        .env_remove("DATABASE_URL")
        .env_remove("AUTUMN_ENV")
        .env_remove("AUTUMN_PROFILE")
        .envs(extra.iter().copied())
        .output()
        .expect("failed to run autumn console");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.code())
}

/// AC4: "a config or DB-connection failure exits non-zero and prints the
/// underlying error (no silent success)".
///
/// Builds and runs the real playground three ways against the same (already
/// compiled) project:
///
/// * no database configured at all -> the *config* stage fails;
/// * a database configured but unreachable -> the *connection* stage fails;
/// * a `[profile.<name>.database]` section -> `--profile` is forwarded and the
///   profile-aware `autumn.toml` lookup is the one that resolves the URL.
///
/// Each asserts a non-zero exit, the underlying error text, and — the real
/// "no silent success" oracle — that the playground's completion sentinel is
/// absent.
///
/// Ignored by default (it compiles a fresh project); run with:
/// `cargo test -p autumn-cli --test cli_tests -- --ignored console_run_exits`
#[test]
#[ignore = "slow: builds and runs a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn console_run_exits_non_zero_when_the_database_is_unreachable() {
    let (_tmp, project) = new_project("console-dbfail-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    patch_autumn_web_to_workspace(&project);

    // The scaffolded `autumn.toml` leaves the database URL unset, so with the
    // env scrubbed there is nothing to resolve: the *config* stage must fail.
    let (out, code) = run_console_for_real(&project, &[]);
    assert_eq!(
        code,
        Some(1),
        "an unresolvable database URL must exit 1:\n{out}"
    );
    assert!(
        out.contains("could not resolve configuration or build the database pool"),
        "the config-stage failure must be named:\n{out}"
    );
    assert!(
        out.contains("no primary database URL configured"),
        "the underlying error must be printed verbatim, not swallowed:\n{out}"
    );
    assert!(
        !out.contains("autumn console: done."),
        "a failed run must never print the completion sentinel:\n{out}"
    );

    // Port 1 is reserved and never listening: config resolves, the *connection*
    // stage fails. Proves the two stages are distinct and both are fatal.
    let (out, code) = run_console_for_real(
        &project,
        &[("DATABASE_URL", "postgres://nobody@127.0.0.1:1/nodb")],
    );
    assert_eq!(code, Some(1), "an unreachable database must exit 1:\n{out}");
    assert!(
        out.contains("could not connect to the database"),
        "the connection-stage failure must be named:\n{out}"
    );
    assert!(
        !out.contains("could not resolve configuration"),
        "a reachable config must not report a config failure:\n{out}"
    );
    assert!(
        !out.contains("autumn console: done."),
        "a failed run must never print the completion sentinel:\n{out}"
    );

    // AC2: the resolution is profile-aware and reads the project's
    // `autumn.toml` from the package root. Adding only a
    // `[profile.demo.database]` section must move the failure from the config
    // stage to the connection stage — but only under `--profile demo`.
    let autumn_toml = project.join("autumn.toml");
    let mut config = fs::read_to_string(&autumn_toml).unwrap();
    config.push_str("\n[profile.demo.database]\nurl = \"postgres://nobody@127.0.0.1:1/demo\"\n");
    fs::write(&autumn_toml, config).unwrap();

    let out = Command::new(autumn_bin())
        .args(["console", "--profile", "demo"])
        .current_dir(&project)
        .env_remove("AUTUMN_DATABASE__PRIMARY_URL")
        .env_remove("AUTUMN_DATABASE__URL")
        .env_remove("DATABASE_URL")
        .output()
        .expect("failed to run autumn console");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(1), "still unreachable:\n{combined}");
    assert!(
        combined.contains("autumn console: profile `demo`"),
        "--profile must reach the playground via AUTUMN_ENV:\n{combined}"
    );
    assert!(
        combined.contains("could not connect to the database"),
        "the profile-scoped autumn.toml URL must have resolved, leaving only \
         the connection to fail:\n{combined}"
    );
}

/// AC4: a user's broken edit surfaces cargo's diagnostics and a non-zero exit,
/// rather than a confusing success. Reuses the already-built target directory
/// of a scaffolded project so this costs an incremental rebuild, not a full one.
///
/// Ignored by default; run with:
/// `cargo test -p autumn-cli --test cli_tests -- --ignored console_run_surfaces`
#[test]
#[ignore = "slow: builds a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn console_run_surfaces_a_compile_error_in_the_playground() {
    let (_tmp, project) = new_project("console-buildfail-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);
    patch_autumn_web_to_workspace(&project);

    let src = fs::read_to_string(playground_path(&project)).unwrap();
    fs::write(
        playground_path(&project),
        src.replace("// ── your code here", "this is not valid rust;"),
    )
    .unwrap();

    let (out, code) = run_console_for_real(
        &project,
        &[("DATABASE_URL", "postgres://nobody@127.0.0.1:1/nodb")],
    );
    assert_ne!(code, Some(0), "a playground that fails to build must not report success:\n{out}");
    assert!(
        out.contains("error"),
        "cargo's diagnostics must reach the user:\n{out}"
    );
    assert!(
        !out.contains("autumn console: done."),
        "a build failure must never print the completion sentinel:\n{out}"
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
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
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
    // Touch every module the playground declares — models, repositories, and
    // policies — so a wrong `#[path]` on any of them fails the build rather
    // than passing unnoticed.
    let round_trip = "\
use repositories::post::{PgPostRepository, PostRepository};\n    \
let _policy = policies::post::PostPolicy::default();\n    \
let repo = PgPostRepository::with_pool_untracked(ctx.pool().clone());\n    \
let _rows: Vec<models::post::Post> = repo.find_all().await.unwrap();\n    \
let _by_id: Option<models::post::Post> = repo.find_by_id(1).await.unwrap();\n    \
// ── your code here";
    fs::write(playground_path(&project), src.replace(marker, round_trip)).unwrap();

    cargo_check_playground(&project);
}

/// AC2/AC7: the *first-invocation* shape — a bare `autumn new` project with no
/// models, so `{{app_modules}}` renders empty — must compile untouched. This is
/// the exact file a user sees on their very first `autumn console`, and it is
/// the one shape the model round-trip test never exercises.
///
/// Ignored by default; run with:
/// `cargo test -p autumn-cli --test cli_tests -- --ignored console_bare`
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn console_bare_playground_target_compiles_untouched() {
    let (_tmp, project) = new_project("console-bare-app");
    run_autumn_ok(&project, &["console", "--scaffold-only"]);

    let src = fs::read_to_string(playground_path(&project)).unwrap();
    assert!(
        !src.contains("#[path"),
        "a project with no data modules must render no #[path] declarations:\n{src}"
    );

    patch_autumn_web_to_workspace(&project);
    cargo_check_playground(&project);
}

fn cargo_check_playground(project: &Path) {
    let check = Command::new("cargo")
        .args(["check", "--bin", "playground", "--features", "playground"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the scaffolded playground failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
    // The playground ships with `#![allow(...)]` for the lints an
    // intentionally-empty editable region would otherwise trip; a warning here
    // means the scaffold is nagging the user on a file they have not edited.
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(
        !stderr.contains("warning: unused")
            && !stderr.contains("warning: variable does not need to be mutable"),
        "the scaffolded playground must compile without warnings:\n{stderr}"
    );
}

// ── AC2/AC6: no drift from `autumn seed`, and the docs exist ───────────────

/// AC2: the console's bootstrap must stay the same one `autumn seed` uses.
/// A future edit that gives the playground its own config/pool wiring — the
/// exact drift the issue calls out — fails here.
#[test]
fn console_and_seed_share_one_bootstrap() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let seed_tmpl = fs::read_to_string(root.join("src/templates/seed.rs.tmpl")).unwrap();
    let playground_tmpl =
        fs::read_to_string(root.join("src/templates/playground.rs.tmpl")).unwrap();

    for (name, tmpl) in [("seed", &seed_tmpl), ("playground", &playground_tmpl)] {
        assert!(
            tmpl.contains("use autumn_web::seed::SeedContext;")
                && tmpl.contains("SeedContext::build()"),
            "the {name} template must bootstrap through the shared SeedContext, \
             so the console and the app never resolve different databases:\n{tmpl}"
        );
    }
}

/// AC6: README and the CLI docs carry a one-line usage example.
#[test]
fn console_usage_is_documented() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");

    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    assert!(
        readme.contains("autumn console"),
        "README must show `autumn console`"
    );
    assert!(
        readme.contains("docs/guide/console.md"),
        "README must link the data-playground guide"
    );

    let guide = fs::read_to_string(root.join("docs/guide/console.md")).unwrap();
    assert!(
        guide.contains("```bash\nautumn console\n```"),
        "the guide must show the one-line usage example:\n{guide}"
    );
    for flag in ["--force", "--profile", "--scaffold-only"] {
        assert!(
            guide.contains(flag),
            "the guide must document `{flag}`:\n{guide}"
        );
    }
}
