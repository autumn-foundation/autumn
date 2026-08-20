//! The `comments:commentable` scaffold DSL token (issue #1367).
//!
//! One token on a scaffolded model has to produce everything a threaded,
//! polymorphic comment system needs:
//!
//! 1. the shared `comments` table migration — `(commentable_type,
//!    commentable_id)` plus the `parent_id` self-FK and `deleted_at`;
//! 2. `comment_count BIGINT NOT NULL DEFAULT 0` on *this* model's own table
//!    (the counter-cache source), excluded from the form the way `position`
//!    is, since the framework maintains it;
//! 3. `#[commentable(...)]` on the generated `#[model]`.
//!
//! And, for AC5, scaffolding a **second** commentable model must add the token
//! to that model and nothing else — no second comments table, no routes.

use std::fs;
use std::path::Path;
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

fn run_autumn(dir: &Path, args: &[&str]) -> (bool, String) {
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
    (output.status.success(), combined)
}

/// The single migration directory whose name ends in `suffix`.
fn migration_ending_in(project: &Path, suffix: &str) -> Option<std::path::PathBuf> {
    fs::read_dir(project.join("migrations"))
        .expect("migrations dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
}

fn scaffolded(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", name]);
    let project = tmp.path().join(name);
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "comments:commentable",
        ],
    );
    (tmp, project)
}

/// AC1 + AC2: the token emits the polymorphic table, keyed on the
/// discriminator pair, with the `parent_id` self-FK that threads it.
#[test]
fn commentable_emits_the_shared_polymorphic_comments_migration() {
    let (_tmp, project) = scaffolded("cmt-table-app");
    let dir = migration_ending_in(&project, "_create_comments").expect("comments migration");
    let up = fs::read_to_string(dir.join("up.sql")).expect("up.sql");

    assert!(up.contains("CREATE TABLE IF NOT EXISTS comments"), "{up}");
    assert!(up.contains("commentable_type TEXT NOT NULL"), "{up}");
    assert!(up.contains("commentable_id BIGINT NOT NULL"), "{up}");
    assert!(
        up.contains("parent_id BIGINT REFERENCES comments(id) ON DELETE CASCADE"),
        "the self-FK is what threads replies:\n{up}"
    );
    assert!(
        up.contains("deleted_at TIMESTAMP"),
        "the thread query is soft-delete aware, so the column has to exist:\n{up}"
    );
    // The discriminator pair is the read path's whole `WHERE`, so it needs an
    // index or every thread render is a sequential scan.
    assert!(
        up.contains("commentable_type, commentable_id"),
        "the (type, id) lookup needs a composite index:\n{up}"
    );

    let down = fs::read_to_string(dir.join("down.sql")).expect("down.sql");
    assert!(down.contains("DROP TABLE IF EXISTS comments"), "{down}");
}

/// AC2 + AC6: `comment_count` lands on the scaffolded model's own table, in
/// its `schema.rs` block and its `#[model]` — it is the counter-cache source.
#[test]
fn commentable_adds_the_counter_column_to_the_model() {
    let (_tmp, project) = scaffolded("cmt-counter-app");

    let dir = migration_ending_in(&project, "_create_posts").expect("posts migration");
    let up = fs::read_to_string(dir.join("up.sql")).expect("up.sql");
    assert!(
        up.contains("comment_count BIGINT NOT NULL DEFAULT 0"),
        "`NOT NULL DEFAULT 0` is load-bearing: the maintenance is `c = c + 1`:\n{up}"
    );

    let schema = fs::read_to_string(project.join("src/schema.rs")).expect("schema.rs");
    assert!(schema.contains("comment_count -> Int8"), "{schema}");

    let model = fs::read_to_string(project.join("src/models/post.rs")).expect("model");
    assert!(model.contains("pub comment_count: i64"), "{model}");
    assert!(
        model.contains("#[default]"),
        "the counter is server-maintained, so it must not be a required insert field:\n{model}"
    );
}

/// AC2: the generated model carries the attribute that wires the whole
/// feature — that is what makes the repository helpers exist.
#[test]
fn commentable_writes_the_attribute_onto_the_model() {
    let (_tmp, project) = scaffolded("cmt-attr-app");
    let model = fs::read_to_string(project.join("src/models/post.rs")).expect("model");
    assert!(
        model.contains("#[commentable("),
        "the model must declare the association:\n{model}"
    );
    assert!(
        model.contains("counter_cache = comment_count"),
        "the attribute must point at the column the migration just added:\n{model}"
    );
}

/// The counter is maintained by the framework, so — exactly like `position`
/// (#1358) — it must not appear in the create/edit form or the `New*`/`Update*`
/// structs. It *is* still rendered (a comment count is worth showing), so this
/// asserts on the submitted surface, not on every mention.
#[test]
fn commentable_counter_is_excluded_from_the_generated_form() {
    let (_tmp, project) = scaffolded("cmt-form-app");
    let routes = fs::read_to_string(project.join("src/routes/posts.rs")).expect("routes");
    assert!(
        !routes.contains(r#"name="comment_count""#),
        "the server maintains the counter; a form input would let a client set it:\n{routes}"
    );
    let form = routes
        .split_once("pub struct PostForm {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(body, _)| body.to_owned())
        .expect("generated form struct");
    assert!(
        !form.contains("comment_count"),
        "the submitted form struct must not carry the counter:\n{form}"
    );
    let model = fs::read_to_string(project.join("src/models/post.rs")).expect("model");
    // The struct field exists (it is read for display), and `#[default]` is
    // what keeps it out of `NewPost`/`UpdatePost`.
    assert!(model.contains("pub comment_count: i64"), "{model}");
}

/// AC5: a **second** commentable model reuses the one shared table. It gets the
/// attribute and its own counter column; it must not emit a second `comments`
/// migration, and it must not generate a single comment route.
#[test]
fn a_second_commentable_model_adds_no_second_comments_table() {
    let (_tmp, project) = scaffolded("cmt-second-app");
    run_autumn_ok(
        &project,
        &[
            "generate",
            "scaffold",
            "Photo",
            "caption:String",
            "comments:commentable",
        ],
    );

    let comment_migrations = fs::read_dir(project.join("migrations"))
        .expect("migrations dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with("_create_comments"))
        })
        .count();
    assert_eq!(
        comment_migrations, 1,
        "the comments table is shared — a second commentable model must not recreate it"
    );

    let photo = fs::read_to_string(project.join("src/models/photo.rs")).expect("photo model");
    assert!(photo.contains("#[commentable("), "{photo}");

    assert!(
        !project.join("src/routes/comments.rs").exists(),
        "comment routes come from the framework router, not from the scaffold"
    );
}

/// The token is not a column and cannot take column modifiers; saying so is
/// cheaper than letting the mistake reach the migration.
#[test]
fn commentable_rejects_nullable_and_unique_modifiers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "cmt-reject-app"]);
    let project = tmp.path().join("cmt-reject-app");

    let (ok, out) = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "comments:Option<commentable>",
        ],
    );
    assert!(!ok, "a nullable commentable must be rejected:\n{out}");

    let (ok, out) = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Note",
            "title:String",
            "comments:commentable:unique",
        ],
    );
    assert!(!ok, "a unique commentable must be rejected:\n{out}");
}

/// Two `commentable` tokens on one model would mean two counter columns and
/// two attributes; the macro allows only one, so the DSL must refuse first.
#[test]
fn only_one_commentable_token_per_model() {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn_ok(tmp.path(), &["new", "cmt-dup-app"]);
    let project = tmp.path().join("cmt-dup-app");
    let (ok, out) = run_autumn(
        &project,
        &[
            "generate",
            "scaffold",
            "Post",
            "title:String",
            "comments:commentable",
            "notes:commentable",
        ],
    );
    assert!(!ok, "a second commentable token must be rejected:\n{out}");
    assert!(
        out.contains("commentable"),
        "the error must name the offending token:\n{out}"
    );
}
