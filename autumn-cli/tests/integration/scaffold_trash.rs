//! Trash view with Restore and Purge for `--soft-delete` scaffolds (issue #1332).
//!
//! `autumn generate scaffold Post … --soft-delete` emits a `GET /posts/trash`
//! page listing rows through the repository's generated `only_deleted` scope
//! (`page_only_deleted`), a "Trash" link on the index, and per-row
//! CSRF-protected `POST /posts/{id}/restore` / `POST /posts/{id}/purge`
//! controls. A scaffold WITHOUT the flag emits none of it, and the gated-off
//! variants (`--live`, `--live-validation`, `--sharded`, an owner-scoped index,
//! `--api`) stay byte-identical to their pre-#1332 output.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// A slug column token. Held in a const so the `{from:title}` braces never sit
/// inside a macro or array literal, where clippy reads them as a format
/// argument (same reason `scaffold_csv_import.rs`/`scaffold_lock_version.rs`
/// each hold one).
const SLUG_COL: &str = "slug:slug{from:title}";

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn");
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `autumn new` + `generate scaffold Post <cols> [extra_flags]`.
fn scaffold_project(
    name: &str,
    extra_fields: &[&str],
    extra_flags: &[&str],
) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut args = vec![
        "generate",
        "scaffold",
        "Post",
        "title:String",
        "body:Text",
        "published:bool",
    ];
    args.extend_from_slice(extra_fields);
    args.extend_from_slice(extra_flags);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

/// `scaffold_project`, returning the generated `src/routes/posts.rs` source.
fn scaffold_routes(
    name: &str,
    extra_fields: &[&str],
    extra_flags: &[&str],
) -> (tempfile::TempDir, String) {
    let (tmp, project) = scaffold_project(name, extra_fields, extra_flags);
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    (tmp, routes)
}

/// Slice out a named `pub async fn` handler: its signature through the line
/// before the next top-level handler.
fn handler_slice<'a>(routes: &'a str, name: &str) -> &'a str {
    let needle = format!("pub async fn {name}(");
    let start = routes
        .find(&needle)
        .unwrap_or_else(|| panic!("routes file must emit `{needle}`:\n{routes}"));
    let rest = &routes[start + needle.len()..];
    rest.split("pub async fn ").next().unwrap_or(rest)
}

/// Every word the trash surface introduces. Deliberately whole WORDS rather
/// than the precise call sites: a gated-off variant must carry no trace of the
/// feature at all, and matching the narrow spellings would miss a stray link,
/// comment, or doc line that leaked through a gate.
const TRASH_WORDS: &[&str] = &[
    "trash",
    "Trash",
    "restore",
    "Restore",
    "purge",
    "Purge",
    "only_deleted",
];

#[test]
fn soft_delete_scaffold_emits_trash_route_and_index_link() {
    let (_tmp, routes) = scaffold_routes("trash-basic", &[], &["--soft-delete"]);
    assert!(
        routes.contains("#[get(\"/posts/trash\")]"),
        "a --soft-delete scaffold must emit GET /posts/trash:\n{routes}"
    );
    let trash = handler_slice(&routes, "trash");
    assert!(
        trash.contains("repo.page_only_deleted(&page_req)"),
        "the trash list must read through the generated only_deleted scope:\n{trash}"
    );
    assert!(
        !trash.contains("deleted_at.is_not_null()"),
        "the trash list must not hand-roll a deleted_at filter:\n{trash}"
    );
    let index = handler_slice(&routes, "index");
    assert!(
        index.contains("paths::trash()") && index.contains("\"Trash\""),
        "the index must link to the trash view:\n{index}"
    );
}

#[test]
fn trash_rows_carry_csrf_protected_restore_and_confirmed_purge_controls() {
    let (_tmp, routes) = scaffold_routes("trash-controls", &[], &["--soft-delete"]);
    let trash = handler_slice(&routes, "trash");
    assert!(
        trash.contains("paths::restore(row.id)"),
        "each trash row needs a Restore control:\n{trash}"
    );
    assert!(
        trash.contains("paths::purge(row.id)"),
        "each trash row needs a Purge control:\n{trash}"
    );
    assert!(
        trash.contains("csrf_input(csrf.as_ref(), csrf_field.as_ref())"),
        "the Restore control must carry the CSRF hidden input:\n{trash}"
    );
    // The Purge confirmation is the framework's server-rendered dialog, not an
    // inline `onclick` (blocked by the default `script-src 'self'` CSP).
    assert!(
        trash.contains("autumn_web::widgets::confirm_action("),
        "Purge must require an explicit, CSP-safe confirm step:\n{trash}"
    );
    assert!(
        trash.contains("purge_csrf"),
        "the Purge control must carry a CSRF token:\n{trash}"
    );
    assert!(
        !trash.contains("onclick="),
        "an inline onclick confirm is blocked by the default CSP:\n{trash}"
    );
}

#[test]
fn restore_and_purge_handlers_route_through_the_repository() {
    let (_tmp, routes) = scaffold_routes("trash-handlers", &[], &["--soft-delete"]);
    assert!(
        routes.contains("#[secured]\n#[post(\"/posts/{id}/restore\")]"),
        "restore must be a secured POST route:\n{routes}"
    );
    assert!(
        routes.contains("#[secured]\n#[post(\"/posts/{id}/purge\")]"),
        "purge must be a secured POST route:\n{routes}"
    );

    let restore = handler_slice(&routes, "restore");
    assert!(restore.contains("repo.restore(row.id).await?"), "{restore}");
    assert!(restore.contains("flash.success("), "{restore}");
    assert!(
        restore.contains("Redirect::to(&paths::trash())"),
        "{restore}"
    );

    let purge = handler_slice(&routes, "purge");
    assert!(purge.contains("repo.purge(row.id).await?"), "{purge}");
    assert!(
        purge.contains("posts::deleted_at.is_not_null()"),
        "purge must never hard-delete a live row:\n{purge}"
    );
    assert!(
        !purge.contains("diesel::delete("),
        "purge is the repository's job, not a hand-rolled delete:\n{purge}"
    );
    assert!(purge.contains("Redirect::to(&paths::trash())"), "{purge}");
}

#[test]
fn soft_delete_scaffold_mounts_the_trash_surface_and_tests_the_lifecycle() {
    let (_tmp, project) = scaffold_project("trash-wiring", &[], &["--soft-delete"]);
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    for entry in [
        "routes::posts::trash",
        "routes::posts::restore",
        "routes::posts::purge",
    ] {
        assert!(
            main.contains(entry),
            "main.rs must mount `{entry}`:\n{main}"
        );
    }
    let test_src = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        test_src.contains("async fn posts_trash_restore_purge_lifecycle()"),
        "the generated smoke test must cover the trash lifecycle:\n{test_src}"
    );
    for step in [
        "/posts/1/delete",
        "/posts/trash",
        "/posts/1/restore",
        "/posts/1/purge",
    ] {
        assert!(
            test_src.contains(step),
            "the lifecycle test must exercise `{step}`:\n{test_src}"
        );
    }
}

/// Assert a generated project carries no trace of the trash surface: not in the
/// routes module (when it has one), not in the mounted route list, not in the
/// generated test.
fn assert_no_trash_surface(project: &Path, label: &str) {
    let routes_path = project.join("src/routes/posts.rs");
    if routes_path.exists() {
        let routes = fs::read_to_string(&routes_path).unwrap();
        for word in TRASH_WORDS {
            assert!(
                !routes.contains(word),
                "{label} must not emit `{word}` anywhere:\n{routes}"
            );
        }
    }
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    for entry in ["::trash", "::restore", "::purge"] {
        assert!(
            !main.contains(entry),
            "{label} must not mount `{entry}`:\n{main}"
        );
    }
    let test_src = fs::read_to_string(project.join("tests/post.rs")).unwrap();
    assert!(
        !test_src.contains("trash"),
        "{label} must generate no trash lifecycle test:\n{test_src}"
    );
}

#[test]
fn scaffold_without_soft_delete_emits_no_trash_surface() {
    let (_tmp, project) = scaffold_project("trash-absent", &[], &[]);
    assert_no_trash_surface(&project, "a scaffold without --soft-delete");
}

#[test]
fn gated_off_soft_delete_variants_emit_no_trash_surface() {
    for (name, extra_fields, flags) in [
        ("trash-live", &[][..], &["--soft-delete", "--live"][..]),
        (
            "trash-live-validation",
            &[][..],
            &["--soft-delete", "--live-validation"][..],
        ),
        (
            "trash-sharded",
            &[][..],
            &["--soft-delete", "--sharded"][..],
        ),
        // An owner-scoped index has no owner-filtered deleted-rows scope to list
        // through; a trash page there would show every user's deleted rows.
        (
            "trash-owner",
            &["user:references"][..],
            &["--soft-delete"][..],
        ),
        // `--api` emits no HTML routes module at all, so there is nothing to
        // hang a trash view on — and nothing may be mounted for one.
        ("trash-api", &[][..], &["--soft-delete", "--api"][..]),
    ] {
        let (_tmp, project) = scaffold_project(name, extra_fields, flags);
        assert_no_trash_surface(&project, name);
    }
}

/// Point a generated project at this workspace's `autumn-web` rather than a
/// published version, so a compile of the generated code exercises the code
/// under test.
fn patch_autumn_web_path(project: &Path) {
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

fn assert_project_cargo_checks(project: &Path) {
    patch_autumn_web_path(project);

    let check = Command::new("cargo")
        .args(["check", "--tests"])
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on generated scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}

/// The emitted trash view, its Restore/Purge handlers, and the generated
/// lifecycle test must actually compile — and so must the same scaffold's
/// `--slug` variant, whose routes key off the slug rather than the id.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn trash_generated_project_cargo_checks() {
    let (_tmp, project) = scaffold_project("trash-check", &[], &["--soft-delete"]);
    assert_project_cargo_checks(&project);

    let (_tmp_slug, project_slug) =
        scaffold_project("trash-check-slug", &[SLUG_COL], &["--soft-delete"]);
    assert_project_cargo_checks(&project_slug);
}
