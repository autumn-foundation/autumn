//! `generate model`/`scaffold` must link scaffolded models into the
//! standalone `src/bin/seed.rs` binary (issue #1718), so
//! `autumn seed --count N --model M` can resolve the model through the
//! `#[model]` inventory registry — completing AC4 of #1343 for a real
//! scaffolded app (not just the `examples/bookmarks` seed that declared its
//! model inline in `seed.rs`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_autumn(dir: &Path, args: &[&str]) {
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
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

/// `autumn new <name> --with-seed` then `generate scaffold Post title:String`.
fn seed_project(project_name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", project_name, "--with-seed"]);
    let project = tmp.path().join(project_name);
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);
    (tmp, project)
}

#[test]
fn scaffold_links_models_and_schema_into_seed_binary() {
    let (_tmp, project) = seed_project("seed-linked-app");
    let seed = fs::read_to_string(project.join("src/bin/seed.rs")).unwrap();

    assert!(
        seed.contains("#[path = \"../schema.rs\"]\nmod schema;"),
        "seed.rs must declare the schema module via #[path]:\n{seed}"
    );
    assert!(
        seed.contains("#[path = \"../models/mod.rs\"]\nmod models;"),
        "seed.rs must declare the models module via #[path]:\n{seed}"
    );
    // The declarations are items, so they must land after the crate-level
    // `//!` doc block (which the template emits) — otherwise the file won't
    // parse.
    let mods_at = seed.find("mod schema;").unwrap();
    let doc_at = seed.find("//!").unwrap();
    assert!(doc_at < mods_at, "mods must follow the doc block:\n{seed}");
}

#[test]
fn second_generate_does_not_duplicate_seed_links() {
    let (_tmp, project) = seed_project("seed-idempotent-app");
    // A second model must not re-inject the shared declarations.
    run_autumn(&project, &["generate", "model", "Comment", "body:Text"]);
    let seed = fs::read_to_string(project.join("src/bin/seed.rs")).unwrap();
    assert_eq!(
        seed.matches("mod schema;").count(),
        1,
        "schema module must be declared exactly once:\n{seed}"
    );
    assert_eq!(
        seed.matches("mod models;").count(),
        1,
        "models module must be declared exactly once:\n{seed}"
    );
}

#[test]
fn project_without_seed_binary_leaves_no_seed_file() {
    // A plain (non-`--with-seed`) project has no seed binary to link into;
    // the generator must not create one.
    let tmp = tempfile::tempdir().expect("tempdir");
    run_autumn(tmp.path(), &["new", "no-seed-app"]);
    let project = tmp.path().join("no-seed-app");
    run_autumn(&project, &["generate", "scaffold", "Post", "title:String"]);
    assert!(
        !project.join("src/bin/seed.rs").exists(),
        "no seed binary must be created for a project generated without --with-seed"
    );
}

#[test]
fn destroying_the_last_model_removes_the_seed_links() {
    let (_tmp, project) = seed_project("seed-destroy-last-app");
    let seed = fs::read_to_string(project.join("src/bin/seed.rs")).unwrap();
    assert!(
        seed.contains("mod schema;") && seed.contains("mod models;"),
        "precondition: generate must have linked the modules:\n{seed}"
    );

    // Destroying the only model empties and DELETES src/schema.rs and
    // src/models/mod.rs, so the seed binary's `#[path]` links to them must be
    // removed too — otherwise `cargo check --bins` fails on missing files.
    run_autumn(&project, &["destroy", "scaffold", "Post", "title:String"]);

    assert!(
        !project.join("src/schema.rs").exists(),
        "destroying the last model must delete src/schema.rs"
    );
    assert!(
        !project.join("src/models/mod.rs").exists(),
        "destroying the last model must delete src/models/mod.rs"
    );
    let seed = fs::read_to_string(project.join("src/bin/seed.rs")).unwrap();
    assert!(
        !seed.contains("mod schema;") && !seed.contains("mod models;"),
        "seed.rs must not keep links to the deleted modules:\n{seed}"
    );
    assert!(
        !seed.contains("#[path = \"../schema.rs\"]")
            && !seed.contains("#[path = \"../models/mod.rs\"]"),
        "seed.rs must not keep dangling #[path] attributes:\n{seed}"
    );
}

#[test]
fn destroying_one_of_several_models_keeps_the_seed_links() {
    let (_tmp, project) = seed_project("seed-destroy-one-app");
    // A second model gives src/models/ a survivor.
    run_autumn(&project, &["generate", "model", "Comment", "body:Text"]);
    // Destroy only Comment: Post's model file remains, so src/models/mod.rs and
    // src/schema.rs survive — the seed links must be KEPT in lockstep.
    run_autumn(&project, &["destroy", "model", "Comment", "body:Text"]);

    assert!(
        project.join("src/models/mod.rs").exists(),
        "src/models/mod.rs must survive while Post remains"
    );
    assert!(
        project.join("src/schema.rs").exists(),
        "src/schema.rs must survive while Post remains"
    );
    let seed = fs::read_to_string(project.join("src/bin/seed.rs")).unwrap();
    assert!(
        seed.contains("#[path = \"../schema.rs\"]\nmod schema;")
            && seed.contains("#[path = \"../models/mod.rs\"]\nmod models;"),
        "seed links must be kept while another model still exists:\n{seed}"
    );
}

/// Slow end-to-end check: the linked seed binary (plus the scaffolded model
/// and schema it now pulls in) actually type-checks against this workspace's
/// `autumn-web`. This is the compile-side proof that
/// `autumn seed --count/--model` can resolve the model — the `#[model]`
/// inventory `submit!` is only reachable if `src/bin/seed.rs` compiles the
/// model in, which is exactly what the `#[path]` links achieve.
///
/// Ignored by default; run with `cargo test -p autumn-cli -- --ignored`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn linked_seed_binary_cargo_checks() {
    use std::fmt::Write as _;

    let (_tmp, project) = seed_project("seed-check-app");

    // Point the generated project at this workspace's autumn-web.
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

    // `--bins` builds the `seed` binary too — the target that used to link
    // neither models nor schema.
    let check = Command::new("cargo")
        .args(["check", "--bins"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check --bins on the linked seed project failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
