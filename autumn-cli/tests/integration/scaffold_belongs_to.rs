//! `belongs_to` `references` fields render a populated `<select>` labeled by a
//! display column, and index/show views render the parent's display value
//! instead of the raw foreign-key id (issue #1146).

// DSL tokens like `"post:references{label:slug}"` are literal scaffold inputs,
// not format strings — the `{label:col}` is the modifier syntax under test.
#![allow(clippy::literal_string_with_formatting_args)]

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

/// `autumn new` + `generate model Post <post_cols>` + `generate scaffold
/// Comment body:Text <comment_ref>`, returning the generated `comments.rs`.
fn belongs_to_routes(
    name: &str,
    post_cols: &[&str],
    comment_ref: &str,
) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    let mut post_args = vec!["generate", "model", "Post"];
    post_args.extend_from_slice(post_cols);
    run_autumn_ok(&project, &post_args);
    run_autumn_ok(
        &project,
        &["generate", "scaffold", "Comment", "body:Text", comment_ref],
    );
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    (tmp, routes)
}

#[test]
fn select_loader_uses_title_display_column_heuristic() {
    let (_tmp, routes) = belongs_to_routes("bt-title-app", &["title:String"], "post:references");
    assert!(
        routes.contains(".select((posts::id, posts::title))"),
        "the loader must select the id + the `title` display column:\n{routes}"
    );
    assert!(
        routes.contains(".map(|(id, label)| (id.to_string(), label))"),
        "options must be labeled by the display value, not the id:\n{routes}"
    );
}

#[test]
fn select_loader_prefers_name_over_other_string_columns() {
    // `name` beats a later `headline` string column per the heuristic.
    let (_tmp, routes) = belongs_to_routes(
        "bt-name-app",
        &["headline:String", "name:String"],
        "post:references",
    );
    assert!(
        routes.contains(".select((posts::id, posts::name))"),
        "the loader must prefer the `name` column:\n{routes}"
    );
}

#[test]
fn label_override_selects_the_named_column() {
    let (_tmp, routes) = belongs_to_routes(
        "bt-override-app",
        &["title:String", "slug:String"],
        "post:references{label:slug}",
    );
    assert!(
        routes.contains(".select((posts::id, posts::slug))"),
        "an explicit {{label:slug}} must select the `slug` column:\n{routes}"
    );
}

#[test]
fn invalid_label_override_falls_back_to_heuristic_and_warns() {
    // `Post` exposes `title` (heuristic display) but no `slug`; an explicit
    // `{label:slug}` names a column the model doesn't have. Trusting it would
    // emit `select posts::slug` (loaded as `String`) and fail to compile the
    // generated app, so the resolver falls back to the `title` heuristic and
    // the generator warns instead of hard-failing.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bt-bad-label-app"]);
    let project = tmp.path().join("bt-bad-label-app");
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    let output = Command::new(autumn_bin())
        .args([
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references{label:slug}",
        ])
        .current_dir(&project)
        .output()
        .expect("failed to run autumn");
    assert!(
        output.status.success(),
        "scaffold with an invalid label must still succeed (graceful fallback)\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    assert!(
        routes.contains(".select((posts::id, posts::title))"),
        "an invalid label must fall back to the `title` heuristic column:\n{routes}"
    );
    assert!(
        !routes.contains("posts::slug"),
        "the generated routes must not reference the nonexistent `slug` column:\n{routes}"
    );
    assert!(
        stderr.contains("slug") && stderr.contains("falling back"),
        "the generator must warn that the invalid label was ignored:\n{stderr}"
    );
}

#[test]
fn label_override_naming_a_non_string_column_falls_back_to_id_and_warns() {
    // `Post` has only numeric columns, so an explicit `{label:count}` names a
    // column that isn't (and can't be) a string display column. Trusting it
    // would emit `select posts::count` loaded as `String` and fail to compile
    // the generated app, so the resolver must fall back to id-only and warn.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bt-nonstring-label-app"]);
    let project = tmp.path().join("bt-nonstring-label-app");
    run_autumn_ok(&project, &["generate", "model", "Post", "count:i64"]);
    let output = Command::new(autumn_bin())
        .args([
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references{label:count}",
        ])
        .current_dir(&project)
        .output()
        .expect("failed to run autumn");
    assert!(
        output.status.success(),
        "scaffold with a non-string label must still succeed (graceful fallback)\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    assert!(
        !routes.contains("posts::count"),
        "must not select the non-string column as a label:\n{routes}"
    );
    assert!(
        routes.contains(".select(posts::id)"),
        "with no string column the loader must fall back to id-only:\n{routes}"
    );
    assert!(
        stderr.contains("count") && stderr.contains("falling back"),
        "the generator must warn that the non-string label was ignored:\n{stderr}"
    );
}

#[test]
fn falls_back_to_id_when_target_has_no_string_column() {
    // A `Post` with only numeric columns has no display column, so the loader
    // keeps the id as both value and label.
    let (_tmp, routes) = belongs_to_routes("bt-noid-app", &["views:i64"], "post:references");
    assert!(
        routes.contains(".select(posts::id)"),
        "with no string column the loader must fall back to id-only:\n{routes}"
    );
    assert!(
        routes.contains("(id.to_string(), id.to_string())"),
        "the id-fallback loader labels each option by its id:\n{routes}"
    );
}

#[test]
fn show_view_renders_parent_label_not_raw_fk() {
    let (_tmp, routes) = belongs_to_routes("bt-show-app", &["title:String"], "post:references");
    // A per-view load of the parent's title, rendered in the property list.
    assert!(
        routes.contains("let post_id_label: String")
            && routes.contains("posts::table.find(fk).select(posts::title)"),
        "show must load the parent's display label:\n{routes}"
    );
    assert!(
        routes.contains("(post_id_label)"),
        "show must render the loaded label:\n{routes}"
    );
    // The show property list must NOT render the raw FK integer.
    assert!(
        !routes.contains("maud::html! { (row.post_id) }"),
        "show must not render the raw post_id integer:\n{routes}"
    );
}

#[test]
fn index_view_renders_parent_label_via_loaded_map() {
    let (_tmp, routes) = belongs_to_routes("bt-index-app", &["title:String"], "post:references");
    assert!(
        routes.contains("let post_id_labels: std::collections::HashMap<String, String>"),
        "index must load a parent-label map:\n{routes}"
    );
    assert!(
        routes.contains("post_id_labels.get(&row.post_id.to_string())"),
        "the index column must look the label up from the map:\n{routes}"
    );
}

/// `autumn new` + `generate model Post title:String` + `generate scaffold
/// Comment body:Text post:references --sharded`, returning `comments.rs`.
fn belongs_to_sharded_routes(name: &str) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--sharded",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();
    (tmp, routes)
}

#[test]
fn sharded_index_renders_parent_label_via_loaded_map() {
    // Issue #1146's index parent-display (a `<select>`-free VIEW concern) must
    // survive `--sharded` too: the sharded index handler already threads a
    // `ShardedDb` (for `from_shard`), and the page-scoped label query (issue
    // #835) derefs it the same way (`&mut *db`) as the non-sharded `Db` path,
    // so the index reuses the label map rather than falling back to the raw
    // FK id.
    let (_tmp, routes) = belongs_to_sharded_routes("bt-sharded-index-app");

    // Sharded handler stays sharded: ShardedDb extractor (promoted to `mut`
    // because it must run the loader) + from_shard, never a bare `Db`.
    assert!(
        routes.contains("mut db: ShardedDb"),
        "sharded index must take a mutable ShardedDb extractor:\n{routes}"
    );
    assert!(
        routes.contains("PgCommentRepository::from_shard(&db)"),
        "sharded index must still build its repo via from_shard(&db):\n{routes}"
    );
    assert!(
        !routes.contains("mut db: Db"),
        "sharded index must not fall back to a bare Db extractor:\n{routes}"
    );
    // The parent-label map is loaded and the index column looks the label up —
    // identical to the non-sharded path.
    assert!(
        routes.contains("let post_id_labels: std::collections::HashMap<String, String>"),
        "sharded index must load a parent-label map:\n{routes}"
    );
    assert!(
        routes.contains("posts::table")
            && routes.contains(".filter(posts::id.eq_any(post_id_ids))"),
        "the sharded label map must be built from a page-scoped query, not a full-table load:\n{routes}"
    );
    assert!(
        routes.contains("post_id_labels.get(&row.post_id.to_string())"),
        "the sharded index column must look the label up from the map:\n{routes}"
    );
}

#[test]
fn live_validation_keeps_parent_display_on_index_and_show() {
    // Issue #1146's index/show parent-display rendering is generator-emitted
    // VIEW markup (independent of the form control), so it must still appear in
    // `--live-validation` mode — where the form itself uses the per-field path
    // and the belongs_to `<select>` is deferred (#1750), the raw FK id must NOT
    // leak into the index/show pages.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bt-live-app"]);
    let project = tmp.path().join("bt-live-app");
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--live-validation",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();

    // Show: loads + renders the parent's `title`, not the raw FK.
    assert!(
        routes.contains("let post_id_label: String")
            && routes.contains("posts::table.find(fk).select(posts::title)")
            && routes.contains("(post_id_label)"),
        "live-validation show must still load + render the parent display label:\n{routes}"
    );
    // Index: loads + looks up the parent-label map.
    assert!(
        routes.contains("let post_id_labels: std::collections::HashMap<String, String>")
            && routes.contains("post_id_labels.get(&row.post_id.to_string())"),
        "live-validation index must still render the parent display via the label map:\n{routes}"
    );
    // The referenced table's schema is imported so those loads compile.
    assert!(
        routes.contains("use crate::schema::{comments, posts}")
            || routes.contains("use crate::schema::posts"),
        "the referenced `posts` schema must be imported for the label loads:\n{routes}"
    );
    // The option loader IS emitted — it populates the in-FORM `<select>`
    // below, so it's a live dependency of the form (not dead code). The index
    // parent-label map (asserted above) is built by its own page-scoped
    // query (#835), not by this loader.
    assert!(
        routes.contains("async fn post_id_select_options"),
        "the option loader must be emitted (the form's select depends on it):\n{routes}"
    );
    // Issue #1750: the in-FORM belongs_to `<select>` is now rendered in
    // live-validation too — the FK no longer leaks out as a raw per-field text
    // input. The per-field path emits the `<select>` (not a `form_for`
    // `.override_field` override, which live-validation never uses) populated
    // from the runtime option loader. Issue #1951 routes it through the typed
    // `a11y::Select` primitive.
    assert!(
        routes.contains("let post_id_options = post_id_select_options(&mut db).await?;"),
        "live-validation must load the belongs_to select options in the form handler:\n{routes}"
    );
    assert!(
        routes.contains("autumn_web::a11y::Select::new(\"post_id\")"),
        "the FK form control must be a populated a11y::Select in live-validation:\n{routes}"
    );
    assert!(
        !routes.contains("required_text_input(&changeset, \"post_id\", \"Post Id\")"),
        "the FK must no longer render as a per-field text input:\n{routes}"
    );
    assert!(
        !routes.contains(".override_field(\"post_id\""),
        "no `form_for` select override is emitted in live-validation:\n{routes}"
    );
}

#[test]
fn live_validation_renders_populated_belongs_to_select() {
    // Issue #1750 (belongs_to half): the in-form parent `<select>` must render
    // in `--live-validation` mode, populated from the same runtime option loader
    // the standard path uses, with a blank placeholder and changeset-driven
    // `selected` state so a 422 re-render keeps the chosen parent.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bt-live-select-app"]);
    let project = tmp.path().join("bt-live-select-app");
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
            "--live-validation",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/comments.rs")).unwrap();

    // The new-form handler gains `db` and loads the options before rendering.
    assert!(
        routes.contains("let post_id_options = post_id_select_options(&mut db).await?;"),
        "the options must be loaded in the form handler:\n{routes}"
    );
    // Issue #1951: the `<select>` is routed through the typed `a11y::Select`
    // primitive, populated from those options with changeset-driven selection.
    assert!(
        routes.contains("autumn_web::a11y::Select::new(\"post_id\")"),
        "an a11y::Select must be rendered for the belongs_to parent:\n{routes}"
    );
    assert!(
        routes.contains(
            "post_id_options.iter().map(|(opt_value, opt_label)| \
             autumn_web::a11y::SelectOption::new(opt_value.as_str(), opt_label.as_str()))"
        ),
        "the <select> options must come from the runtime loader (mapped to SelectOption):\n{routes}"
    );
    assert!(
        routes.contains(".selected_value(changeset.field_value(\"post_id\").unwrap_or_default())"),
        "the selected option must be driven by the changeset value (422 re-render):\n{routes}"
    );
    // The placeholder is the first option on the primitive.
    assert!(
        routes.contains(".option(\"\", \"— Select —\")"),
        "the belongs_to select must carry a blank placeholder option:\n{routes}"
    );
}

#[test]
fn self_reference_resolves_display_from_in_flight_fields() {
    // A self-reference (`node:references` on Node → the `nodes` table being
    // generated right now) has no `src/models/node.rs` on disk yet, so the #1146
    // display column must resolve from the IN-FLIGHT fields — `nodes::name`, not
    // the raw id. Nullable self-ref (`references?`). (A regular-plural model is
    // used so the generated app also compiles — an irregular plural like
    // `Category` hits a separate, pre-existing `#[model]`-macro pluralization
    // bug, `category` -> `categorys`, unrelated to this display resolution.)
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "self-ref-app"]);
    let project = tmp.path().join("self-ref-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Node",
            "name:String",
            "node:references?",
        ],
    );
    let routes = fs::read_to_string(project.join("src/routes/nodes.rs")).unwrap();

    // Option loader selects the in-flight `name` display column (not id-only).
    assert!(
        routes.contains(".select((nodes::id, nodes::name))"),
        "self-ref option loader must select the in-flight `name` column:\n{routes}"
    );
    // Show loads + renders the parent's `name`.
    assert!(
        routes.contains("nodes::table.find(fk).select(nodes::name)"),
        "self-ref show must load the parent `name` display value:\n{routes}"
    );
    // Index builds the parent-label map.
    assert!(
        routes.contains("let node_id_labels: std::collections::HashMap<String, String>"),
        "self-ref index must build the parent-label map:\n{routes}"
    );
    // Must NOT fall back to raw-id labeling.
    assert!(
        !routes.contains("(id.to_string(), id.to_string())"),
        "self-ref must not fall back to raw-id labeling:\n{routes}"
    );
    // Nullable self-ref: the unset FK renders a dash on show/index.
    assert!(
        routes.contains("None => \"—\".to_string()"),
        "nullable self-ref must handle the None case:\n{routes}"
    );
}

#[test]
fn nullable_reference_renders_dash_and_blank_option() {
    let (_tmp, routes) =
        belongs_to_routes("bt-nullable-app", &["title:String"], "post:references?");
    // The nullable select gets a blank first option.
    assert!(
        routes.contains("\"— Unset —\".to_string()"),
        "a nullable reference must offer a blank first option:\n{routes}"
    );
    // The nullable FK renders a dash for the None case in show and index.
    assert!(
        routes.contains("None => \"—\".to_string()"),
        "show must render a dash for an unset nullable reference:\n{routes}"
    );
    assert!(
        routes.contains("match &row.post_id { Some(v) =>"),
        "index must handle the nullable FK's None case:\n{routes}"
    );
}

/// Slow end-to-end check: the `belongs_to` scaffold (display-column select
/// loader, index label map, show label load) type-checks against this
/// workspace's `autumn-web`.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn belongs_to_scaffold_cargo_checks() {
    use std::fmt::Write as _;

    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "bt-check-app"]);
    let project: PathBuf = tmp.path().join("bt-check-app");
    run_autumn_ok(&project, &["generate", "model", "Post", "title:String"]);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Comment",
            "body:Text",
            "post:references",
        ],
    );

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

    let check = Command::new("cargo")
        .args(["check", "--bins"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the belongs_to scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
