//! Optimistic locking wired into scaffolded edit forms (issue #1318).
//!
//! Autumn ships the concurrency primitive — `#[lock_version]` plus the
//! `RepositoryError::Conflict` a `#[repository]` update raises on a stale write
//! (#575) — but `autumn generate scaffold` used to route around it: the update
//! handler hand-wrote an unconditional `diesel::update(table.find(id)).set(...)`
//! and the edit form carried no version, so two people editing the same
//! `lock_version`-bearing record silently clobbered each other.
//!
//! Declaring the column is now the whole opt-in. This suite drives the real
//! binary end to end (`autumn new` + `autumn generate scaffold`) and asserts the
//! emitted output, which the in-module unit tests cannot: they render templates,
//! while these run the CLI a user actually types.
//!
//! Deliberately *not* covered here: whether the emitted SQL behaves. That needs
//! a database and lives in `generate_lock_version_postgres.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str]) -> (String, bool) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (combined, output.status.success())
}

fn run_autumn_ok(dir: &Path, args: &[&str]) -> String {
    let (out, ok) = run_autumn(dir, args);
    assert!(ok, "autumn {args:?} failed:\n{out}");
    out
}

/// `autumn new` + `generate scaffold Post <cols> [flags]`, returning the tempdir
/// guard, the project root, and the generator's own stdout/stderr.
fn scaffold(name: &str, cols: &[&str], flags: &[&str]) -> (tempfile::TempDir, PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(cols);
    args.extend_from_slice(flags);
    let out = run_autumn_ok(&project, &args);
    (tmp, project, out)
}

/// The generation attempt's combined output, for the refusal cases.
fn scaffold_err(name: &str, cols: &[&str], flags: &[&str]) -> String {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec!["generate", "scaffold", "Post"];
    args.extend_from_slice(cols);
    args.extend_from_slice(flags);
    let (out, ok) = run_autumn(&project, &args);
    assert!(!ok, "expected refusal, but generation succeeded:\n{out}");
    out
}

/// A slug column token. Held in a const so the `{from:title}` braces never sit
/// inside a macro invocation, where clippy reads them as a format argument.
const SLUG_COL: &str = "slug:slug{from:title}";

fn handler_slice<'a>(routes: &'a str, name: &str) -> &'a str {
    let needle = format!("pub async fn {name}(");
    let start = routes
        .find(&needle)
        .unwrap_or_else(|| panic!("routes file must emit `{needle}`:\n{routes}"));
    let rest = &routes[start + needle.len()..];
    rest.split("pub async fn ").next().unwrap_or(rest)
}

#[test]
fn lock_version_scaffold_wires_the_model_migration_form_and_guarded_update() {
    let (_tmp, project, stdout) = scaffold(
        "lockapp",
        &["title:String", "body:Text", "lock_version:i32"],
        &[],
    );

    // The opt-in is never silent: declaring the column changes what it *is*, so
    // someone who wanted a plain counter is told, and told how to back out.
    assert!(
        stdout.contains("optimistic locking") && stdout.contains("Rename"),
        "generation must warn that lock_version is a magic column:\n{stdout}"
    );

    // Model: the framework's own primitive, not an inert integer.
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(
        model.contains("#[lock_version]\n    pub lock_version: i32,"),
        "model:\n{model}"
    );

    // Migration: `#[lock_version]` keeps the column out of `NewPost`, so the
    // INSERT never names it and the column MUST carry a default.
    let migrations = fs::read_dir(project.join("migrations")).unwrap();
    let up = migrations
        .filter_map(Result::ok)
        .map(|e| e.path().join("up.sql"))
        .find(|p| p.exists())
        .map(|p| fs::read_to_string(p).unwrap())
        .expect("a migration up.sql");
    assert!(
        up.contains("lock_version INTEGER NOT NULL DEFAULT 0"),
        "up.sql:\n{up}"
    );

    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();

    // AC1: the edit form carries the row's current version; the new form does
    // not, and the version is never an editable control.
    assert!(
        handler_slice(&routes, "edit_form").contains("let lock_version_value = row.lock_version;"),
        "edit_form must seed the hidden field from the row:\n{routes}"
    );
    assert!(
        routes.contains("input type=\"hidden\" name=\"lock_version\" value=(version);"),
        "routes:\n{routes}"
    );
    assert!(
        !handler_slice(&routes, "new_form").contains("lock_version"),
        "a row that does not exist yet has no version to guard against:\n{routes}"
    );
    assert!(
        !routes.contains("pub lock_version: i32,\n}"),
        "lock_version must not be a PostForm column:\n{routes}"
    );

    // AC2 + AC4: compare-and-swap — guard and bump in one statement.
    let update = handler_slice(&routes, "update");
    assert!(
        update.contains("posts::lock_version.eq(expected_lock_version)")
            && update.contains("posts::lock_version.eq(posts::lock_version + 1)"),
        "update:\n{update}"
    );
    // AC4: the success path still redirects to the show page, unchanged.
    assert!(
        update.contains("Redirect::to(&paths::show(*id))"),
        "update:\n{update}"
    );

    // AC3: a stale submit re-renders at 409 with a banner, and a missing row is
    // still a 404 — the two are told apart by a re-read, not conflated.
    assert!(
        update.contains("StatusCode::CONFLICT")
            && update.contains("let lock_version_value = current.lock_version;")
            && update.contains("not_found_msg"),
        "update:\n{update}"
    );
    assert!(
        routes.contains("changed by someone else") && routes.contains("role=\"alert\""),
        "routes:\n{routes}"
    );

    // AC6: the generated suite carries the stale-write test.
    let test_file = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        test_file.contains("async fn posts_optimistic_lock_conflict()")
            && test_file.contains("assert_status(409)"),
        "tests/post.rs:\n{test_file}"
    );
}

#[test]
fn lock_version_survives_the_live_validation_variant() {
    // `--live-validation` renders its controls as raw htmx markup instead of one
    // `form_for` call, but writes through the same guarded statement — so it is
    // supported, and its hidden field is spliced into the form body directly.
    let (_tmp, project, _) = scaffold(
        "lvlockapp",
        &["title:String", "lock_version:i32"],
        &["--live-validation"],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        routes.contains("input type=\"hidden\" name=\"lock_version\" value=(lock_version_value);"),
        "routes:\n{routes}"
    );
    assert!(
        handler_slice(&routes, "update").contains("posts::lock_version.eq(expected_lock_version)"),
        "routes:\n{routes}"
    );
}

#[test]
fn state_transitions_bump_the_version_so_open_edit_forms_learn_about_them() {
    let (_tmp, project, _) = scaffold(
        "smlockapp",
        &[
            "title:String",
            "status:String:states(draft->published)",
            "lock_version:i32",
        ],
        &[],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    assert!(
        handler_slice(&routes, "transition_status")
            .contains("posts::lock_version.eq(posts::lock_version + 1)"),
        "a transition mutates the row, so it must move the counter:\n{routes}"
    );
}

#[test]
fn api_scaffold_gets_the_model_attribute_and_therefore_a_conflict_checked_json_update() {
    // `--api` emits no HTML form to wire, so it is not gated — but the model
    // attribute still lands, which is what makes the repository's own JSON
    // update conflict-check. That also makes `lock_version` a REQUIRED field on
    // `UpdatePost`: a deliberate contract change, documented in the guide.
    let (_tmp, project, _) = scaffold(
        "apilockapp",
        &["title:String", "lock_version:i32"],
        &["--api"],
    );
    let model = fs::read_to_string(project.join("src/models/post.rs")).unwrap();
    assert!(model.contains("#[lock_version]"), "model:\n{model}");
    assert!(
        !project.join("src/routes/posts.rs").exists(),
        "--api emits no HTML routes file, so there is no form to wire"
    );
}

#[test]
fn a_scaffold_without_the_column_carries_no_locking_artifacts() {
    // AC5: zero behaviour change for the overwhelming common case.
    let (_tmp, project, stdout) = scaffold("plainapp", &["title:String", "body:Text"], &[]);
    assert!(
        !stdout.contains("optimistic locking"),
        "no column, no warning:\n{stdout}"
    );
    for file in ["src/routes/posts.rs", "src/models/post.rs", "tests/post.rs"] {
        let src = fs::read_to_string(project.join(file)).unwrap();
        for needle in ["lock_version", "lock_conflict", "CONFLICT"] {
            assert!(
                !src.contains(needle),
                "{file} must carry no `{needle}`:\n{src}"
            );
        }
    }
}

#[test]
fn unsupported_variants_are_refused_up_front_with_an_actionable_message() {
    // Each of these writes through a path that does not route via the guarded
    // statement. Emitting them would ship an edit form that only *looks*
    // concurrency-safe, so the generator refuses instead — and every message
    // names both the offending combination and the way out.
    let cases: [(&str, &[&str], &[&str], &str); 5] = [
        (
            "liverefuse",
            &["title:String", "lock_version:i32"],
            &["--live"],
            "--live",
        ),
        (
            "shardrefuse",
            &["title:String", "lock_version:i32"],
            &["--sharded", "--shard-key", "title"],
            "--sharded",
        ),
        (
            "attachrefuse",
            &["title:String", "cover:Attachment", "lock_version:i32"],
            &[],
            "attachment",
        ),
        (
            "slugrefuse",
            &["title:String", SLUG_COL, "lock_version:i32"],
            &[],
            "slug",
        ),
        ("onlyrefuse", &["lock_version:i32"], &[], "only column"),
    ];
    for (app, cols, flags, needle) in cases {
        let out = scaffold_err(app, cols, flags);
        assert!(
            out.contains("lock_version") && out.contains(needle),
            "the {needle} refusal must name the combination:\n{out}"
        );
    }
}

#[test]
fn a_lock_version_column_the_primitive_cannot_use_is_rejected_not_ignored() {
    // Silently ignoring these would leave the author believing they had opted
    // into optimistic locking when they had not.
    for cols in [
        vec!["title:String", "lock_version:String"],
        vec!["title:String", "lock_version:Option<i32>"],
        vec!["title:String", "lock_version:i32:unique"],
    ] {
        let out = scaffold_err("badlock", &cols, &[]);
        assert!(
            out.contains("lock_version"),
            "the refusal must name the column ({cols:?}):\n{out}"
        );
    }
}
