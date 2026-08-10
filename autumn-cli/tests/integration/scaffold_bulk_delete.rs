//! Bulk-select + delete-selected wiring in scaffolded list views (issue #1312).
//!
//! The generated non-live, non-sharded HTML scaffold wraps its index list in an
//! `autumn_web::widgets::bulk_actions_form(...)` posting to a new
//! `POST /{plural}/bulk_delete` handler, prepends a per-row
//! `bulk_select_checkbox(...)` column, and routes the delete through the
//! repository's `delete_many`. Empty selections are a 303 no-op, never a 400.
//! The `--live`, `--live-validation`, `--sharded` and `--api` variants are
//! deliberately gated off and stay byte-identical to their pre-#1312 output.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

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

/// `autumn new` + `generate scaffold Post <cols> [extra_flags]`, returning the
/// tempdir guard and the generated project root.
fn scaffold_project(name: &str, extra_flags: &[&str]) -> (tempfile::TempDir, PathBuf) {
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
    args.extend_from_slice(extra_flags);
    run_autumn_ok(&project, &args);
    (tmp, project)
}

/// `scaffold_project`, returning the generated `src/routes/posts.rs` source.
fn scaffold_routes(name: &str, extra_flags: &[&str]) -> (tempfile::TempDir, String) {
    let (tmp, project) = scaffold_project(name, extra_flags);
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

#[test]
fn plain_scaffold_emits_bulk_form_checkbox_and_handler() {
    let (_tmp, routes) = scaffold_routes("bulk-plain", &[]);
    assert!(
        routes.contains("#[post(\"/posts/bulk_delete\")]"),
        "a bulk delete route must be emitted:\n{routes}"
    );
    assert!(
        routes.contains("pub async fn bulk_delete("),
        "a bulk delete handler must be emitted:\n{routes}"
    );
    assert!(
        routes.contains("autumn_web::widgets::bulk_actions_form("),
        "the index list must be wrapped in the bulk actions form:\n{routes}"
    );
    assert!(
        routes.contains("bulk_select_checkbox("),
        "each row must render a bulk-select checkbox:\n{routes}"
    );
    assert!(
        routes.contains("paths::bulk_delete()"),
        "the form action must resolve through paths::bulk_delete():\n{routes}"
    );
    // The "New Post" link stays outside the bulk form.
    let index = handler_slice(&routes, "index");
    let new_link_at = index
        .find("New Post")
        .unwrap_or_else(|| panic!("index must keep the New Post link:\n{index}"));
    let form_at = index
        .find("bulk_actions_form(")
        .unwrap_or_else(|| panic!("index must render the bulk form:\n{index}"));
    assert!(
        new_link_at < form_at,
        "the New Post link must stay outside the bulk form:\n{index}"
    );
}

/// `SubmitTokenLayer` passes a request carrying no token straight through, so
/// a bulk form without the hidden field gives up double-submit protection
/// entirely: a double-clicked "Delete selected" would run the whole destructive
/// path — hooks and dependent deletes included — a second time instead of
/// replaying the first response, as generated create/update forms do.
#[test]
fn bulk_form_carries_a_one_time_submit_token() {
    // The plain scaffold has only `index`; the searchable one also renders the
    // bulk form from its `search` fragment, so both must carry the token.
    let (_plain_tmp, plain) = scaffold_routes("bulk-submit-token", &[]);
    let (_search_tmp, searchable) =
        scaffold_routes("bulk-submit-token-search", &["--searchable", "title,body"]);
    for (routes, handler) in [(&plain, "index"), (&searchable, "search")] {
        let slice = handler_slice(routes, handler);
        assert!(
            slice.contains("submit_token: Option<SubmitToken>")
                && slice.contains("submit_field: Option<SubmitFormField>"),
            "`{handler}` must extract the submit-token pair:\n{slice}"
        );
        assert!(
            slice.contains("submit_token.as_ref().map(|t| t.token())")
                && slice.contains("submit_field.as_ref().map(|f| f.0.as_str())"),
            "`{handler}` must thread the submit token into the bulk form:\n{slice}"
        );
    }
}

/// Non-bulk variants keep their byte-identical signatures — the submit-token
/// pair rides in only alongside the bulk form.
#[test]
fn scaffolds_without_bulk_delete_keep_plain_index_signatures() {
    for (name, flags) in [
        ("bulk-none-live", &["--live"][..]),
        ("bulk-none-sharded", &["--sharded"][..]),
    ] {
        let (_tmp, routes) = scaffold_routes(name, flags);
        let index = handler_slice(&routes, "index");
        assert!(
            !index.contains("submit_token: Option<SubmitToken>"),
            "`{name}` has no bulk form, so `index` must not take a submit token:\n{index}"
        );
    }
}

/// `eq_any` binds one parameter per id, and the handler parses a raw body
/// rather than a page-sized selection — so a crafted POST can carry more ids
/// than the backend's placeholder ceiling (`MAX_BIND_PARAMS`, 32766 on
/// SQLite). A single unbounded `eq_any` would fail the request with "too many
/// SQL variables" before reaching the already-chunked `delete_many`.
#[test]
fn bulk_delete_preflight_chunks_its_id_query() {
    for (name, flags) in [
        ("bulk-chunk-plain", &[][..]),
        ("bulk-chunk-soft", &["--soft-delete"][..]),
    ] {
        let (_tmp, routes) = scaffold_routes(name, flags);
        let bulk = handler_slice(&routes, "bulk_delete");
        assert!(
            bulk.contains("for chunk in ids.chunks(1000)"),
            "`{name}` must chunk the pre-flight id query:\n{bulk}"
        );
        assert!(
            bulk.contains(".eq_any(chunk)") && !bulk.contains(".eq_any(&ids)"),
            "`{name}` must bind one chunk at a time, not the whole selection:\n{bulk}"
        );
    }
}

#[test]
fn soft_delete_scaffold_routes_bulk_through_delete_many() {
    let (_tmp, routes) = scaffold_routes("bulk-soft", &["--soft-delete"]);
    let bulk = handler_slice(&routes, "bulk_delete");
    assert!(
        bulk.contains(".delete_many(&"),
        "the soft-delete bulk path must go through the repository:\n{bulk}"
    );
    assert!(
        bulk.contains("deleted_at.is_null()"),
        "the SELECT must skip already soft-deleted rows:\n{bulk}"
    );
    assert!(
        !bulk.contains("deleted_at.eq("),
        "the handler must not hand-roll the soft-delete update:\n{bulk}"
    );
    assert!(
        !bulk.contains("diesel::delete"),
        "the soft-delete variant must never physically delete:\n{bulk}"
    );
}

/// `Db` acquires its pooled connection at extraction and holds it until it is
/// dropped — not merely for the duration of a `&mut *db` borrow — while
/// `delete_many` checks out a second connection of its own. A handler holding
/// both at once stalls every bulk delete on a single-connection pool and
/// deadlocks a larger one once enough requests reach the handler together, so
/// the generated code must release `db` between the load and the delete.
#[test]
fn bulk_delete_releases_its_db_before_delete_many() {
    for (name, flags) in [
        ("bulk-drop-plain", &[][..]),
        ("bulk-drop-soft", &["--soft-delete"][..]),
    ] {
        let (_tmp, routes) = scaffold_routes(name, flags);
        let bulk = handler_slice(&routes, "bulk_delete");
        let load_at = bulk
            .find(".load(&mut *db)")
            .unwrap_or_else(|| panic!("bulk_delete must load the selected rows:\n{bulk}"));
        let drop_at = bulk
            .find("drop(db);")
            .unwrap_or_else(|| panic!("bulk_delete must release its Db:\n{bulk}"));
        let delete_at = bulk
            .find(".delete_many(&")
            .unwrap_or_else(|| panic!("bulk_delete must call delete_many:\n{bulk}"));
        assert!(
            load_at < drop_at && drop_at < delete_at,
            "the Db must be dropped after the load and before delete_many:\n{bulk}"
        );
    }
}

#[test]
fn owner_scoped_scaffold_authorizes_each_row_before_bulk_delete() {
    // A `user_id` column turns on the record-level policy + authorize wiring.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bulk-owner"]);
    let project = tmp.path().join("bulk-owner");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "user_id:i64",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).unwrap();
    let bulk = handler_slice(&routes, "bulk_delete");
    assert!(
        bulk.contains("autumn_web::authorization::authorize::<Post>("),
        "each selected row must be authorized:\n{bulk}"
    );
    assert!(bulk.contains("\"delete\""), "{bulk}");
    assert!(
        bulk.contains("for "),
        "authorization must run per row in a loop:\n{bulk}"
    );
    // Unauthorized rows are dropped from the selection, not 403'd.
    assert!(
        !bulk.contains("forbidden") && !bulk.contains("StatusCode::FORBIDDEN"),
        "an unauthorized row must be skipped, not fail the whole request:\n{bulk}"
    );
}

#[test]
fn live_scaffold_omits_bulk_delete() {
    let (_tmp, routes) = scaffold_routes("bulk-live", &["--live"]);
    assert!(!routes.contains("bulk_delete"), "{routes}");
    assert!(!routes.contains("bulk_actions_form"), "{routes}");
    assert!(!routes.contains("bulk_select_checkbox"), "{routes}");
}

#[test]
fn sharded_scaffold_omits_bulk_delete() {
    let (_tmp, routes) = scaffold_routes("bulk-sharded", &["--sharded", "--shard-key", "title"]);
    assert!(!routes.contains("bulk_delete"), "{routes}");
    assert!(!routes.contains("bulk_actions_form"), "{routes}");
    assert!(!routes.contains("bulk_select_checkbox"), "{routes}");
}

#[test]
fn api_scaffold_omits_bulk_delete() {
    // `--api` scaffolds have no HTML routes file at all.
    let (_tmp, project) = scaffold_project("bulk-api", &["--api"]);
    assert!(
        !project.join("src/routes/posts.rs").exists(),
        "an --api scaffold must not emit an HTML routes file"
    );
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(!main.contains("bulk_delete"), "{main}");
}

#[test]
fn main_rs_mounts_bulk_delete_route() {
    let (_tmp, project) = scaffold_project("bulk-main", &[]);
    let main = fs::read_to_string(project.join("src/main.rs")).unwrap();
    assert!(
        main.contains("routes::posts::bulk_delete"),
        "main.rs must mount the bulk delete route:\n{main}"
    );
    let destroy_at = main
        .find("routes::posts::destroy")
        .unwrap_or_else(|| panic!("main.rs must mount destroy:\n{main}"));
    let bulk_at = main
        .find("routes::posts::bulk_delete")
        .unwrap_or_else(|| panic!("main.rs must mount bulk_delete:\n{main}"));
    assert!(
        destroy_at < bulk_at,
        "bulk_delete must be registered immediately after destroy:\n{main}"
    );
}

#[test]
fn searchable_scaffold_keeps_checkboxes_in_search_fragment() {
    let (_tmp, routes) = scaffold_routes("bulk-search", &["--searchable", "title,body"]);
    let search = handler_slice(&routes, "search");
    assert!(
        search.contains("bulk_select_checkbox("),
        "the search results fragment must keep the row checkboxes:\n{search}"
    );
    assert!(
        search.contains("bulk_actions_form("),
        "the search results fragment must stay inside the bulk form:\n{search}"
    );
    // The search input itself stays outside the bulk form.
    let index = handler_slice(&routes, "index");
    let input_at = index
        .find("active_search_input(")
        .unwrap_or_else(|| panic!("index must render the search box:\n{index}"));
    let form_at = index
        .find("bulk_actions_form(")
        .unwrap_or_else(|| panic!("index must render the bulk form:\n{index}"));
    assert!(
        input_at < form_at,
        "the search input must stay outside the bulk form:\n{index}"
    );
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

/// The emitted bulk-delete handler, parser helper, and index/search views must
/// actually compile — both on the physical-delete and soft-delete paths.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn bulk_delete_generated_project_cargo_checks() {
    let (_tmp, project) = scaffold_project("bulk-check-plain", &[]);
    assert_project_cargo_checks(&project);

    let (_tmp_soft, project_soft) = scaffold_project("bulk-check-soft", &["--soft-delete"]);
    assert_project_cargo_checks(&project_soft);
}
