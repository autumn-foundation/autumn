//! `autumn new --api` emits a lean, JSON-first project skeleton with zero
//! HTML/CSS/Tailwind artifacts (issue #1390).

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

fn run_autumn_ok(dir: &Path, args: &[&str]) {
    let output = run_autumn(dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed (exit={:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `autumn new <name> --api [extra flags]`, returning the tempdir and the
/// generated project directory.
fn api_project(name: &str, extra: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut args = vec!["new", name, "--api"];
    args.extend_from_slice(extra);
    run_autumn_ok(tmp.path(), &args);
    let project = tmp.path().join(name);
    (tmp, project)
}

/// Recursively collect every file path under `root`.
fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// AC #2: the whole generated `--api` tree must contain no `tailwind`,
/// `input.css`, or `maud::html` — the marker strings for the HTML/CSS view
/// stack the fullstack scaffold ships.
#[test]
fn api_scaffold_has_no_html_css_template_artifacts() {
    let (_tmp, project) = api_project("api-artifacts-app", &[]);

    // No CSS/Tailwind files exist.
    for missing in [
        "tailwind.config.js",
        "static/css/input.css",
        "static/css/app.css",
    ] {
        assert!(
            !project.join(missing).exists(),
            "--api scaffold must not create {missing}"
        );
    }
    // No `static/` tree at all (no vendored htmx/JS, no CSS).
    assert!(
        !project.join("static").exists(),
        "--api scaffold must not create a static/ directory"
    );

    // A content grep across every generated file returns zero matches for the
    // HTML/CSS/Tailwind markers.
    for path in all_files(&project) {
        let Ok(content) = fs::read_to_string(&path) else {
            continue; // skip binary files (e.g. credentials ciphertext)
        };
        for marker in ["tailwind", "input.css", "maud::html"] {
            assert!(
                !content.contains(marker),
                "--api generated file {} contains forbidden marker {marker:?}:\n{content}",
                path.display(),
            );
        }
    }
}

/// AC #2 (build.rs) + AC #5 (Dockerfile): neither the build script nor the
/// Dockerfile carries a CSS/asset build step.
#[test]
fn api_scaffold_build_and_docker_have_no_css_step() {
    let (_tmp, project) = api_project("api-build-app", &[]);

    let build_rs = fs::read_to_string(project.join("build.rs")).unwrap();
    assert!(
        !build_rs.contains("tailwind") && !build_rs.contains("input.css"),
        "--api build.rs must not compile CSS:\n{build_rs}"
    );
    // Provenance baking is retained.
    assert!(
        build_rs.contains("emit_build_provenance"),
        "--api build.rs must keep build/git provenance emission:\n{build_rs}"
    );

    let dockerfile = fs::read_to_string(project.join("Dockerfile")).unwrap();
    assert!(
        !dockerfile.to_lowercase().contains("tailwind"),
        "--api Dockerfile must not download/run Tailwind:\n{dockerfile}"
    );
    assert!(
        !dockerfile.contains("COPY static"),
        "--api Dockerfile must not copy a static/ tree:\n{dockerfile}"
    );
}

/// AC #3: the `--api` `main.rs` defines a `Json<...>` handler and registers it
/// via `routes![]`, with no maud/layout/HTML.
#[test]
fn api_scaffold_main_rs_has_json_handler_and_routes() {
    let (_tmp, project) = api_project("api-main-app", &[]);
    let main_rs = fs::read_to_string(project.join("src/main.rs")).unwrap();

    assert!(
        main_rs.contains("-> Json<"),
        "--api main.rs must define at least one Json<...> handler:\n{main_rs}"
    );
    assert!(
        main_rs.contains("routes![index, hello_name]"),
        "--api main.rs must register handlers via routes![]:\n{main_rs}"
    );
    assert!(
        main_rs.contains("use autumn_web::prelude::*;"),
        "--api main.rs must import the prelude (re-exports Json):\n{main_rs}"
    );
    // No HTML/view layer.
    for forbidden in ["maud", "layout(", "<link", "nav_bar("] {
        assert!(
            !main_rs.contains(forbidden),
            "--api main.rs must not contain {forbidden:?}:\n{main_rs}"
        );
    }
}

/// AC #4: the `--api` `Cargo.toml` drops the `maud` dependency and disables the
/// view features (`default-features = false`), keeping `db` so migrations work.
#[test]
fn api_scaffold_cargo_toml_drops_view_stack() {
    let (_tmp, project) = api_project("api-cargo-app", &[]);
    let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();

    assert!(
        !cargo.contains("maud ="),
        "--api Cargo.toml must not depend on maud:\n{cargo}"
    );
    assert!(
        cargo.contains("default-features = false"),
        "--api Cargo.toml must disable autumn-web default (view) features:\n{cargo}"
    );
    assert!(
        cargo.contains("\"db\""),
        "--api Cargo.toml must keep the db feature so migrations compile:\n{cargo}"
    );
}

/// AC #7: `--with-i18n` and `--with-seed` compose with `--api`.
#[test]
fn api_scaffold_composes_with_i18n_and_seed() {
    // i18n: locale bundle + i18n feature + `.i18n_auto()` wiring, still JSON-first.
    let (_tmp_i18n, i18n) = api_project("api-i18n-app", &["--with-i18n"]);
    assert!(
        i18n.join("i18n/en.ftl").is_file(),
        "--api --with-i18n must scaffold i18n/en.ftl"
    );
    let i18n_main = fs::read_to_string(i18n.join("src/main.rs")).unwrap();
    assert!(
        i18n_main.contains(".i18n_auto()") && i18n_main.contains("-> Json<"),
        "--api --with-i18n main.rs must wire i18n and stay JSON-first:\n{i18n_main}"
    );
    assert!(
        fs::read_to_string(i18n.join("Cargo.toml"))
            .unwrap()
            .contains("\"i18n\""),
        "--api --with-i18n must enable the i18n feature"
    );

    // seed: seed binary + seed feature/bin entry.
    let (_tmp_seed, seed) = api_project("api-seed-app", &["--with-seed"]);
    assert!(
        seed.join("src/bin/seed.rs").is_file(),
        "--api --with-seed must scaffold src/bin/seed.rs"
    );
    let seed_cargo = fs::read_to_string(seed.join("Cargo.toml")).unwrap();
    assert!(
        seed_cargo.contains("\"seed\"") && seed_cargo.contains("name = \"seed\""),
        "--api --with-seed must enable the seed feature and [[bin]] entry:\n{seed_cargo}"
    );
}

/// `--api` cannot be combined with the daemon flavors (conflicting feature
/// sets); the CLI rejects the combination.
#[test]
fn api_scaffold_conflicts_with_daemon_flavors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for bad in [
        ["new", "api-daemon-bad", "--api", "--daemon"],
        ["new", "api-bundled-bad", "--api", "--bundled-pg"],
    ] {
        let output = run_autumn(tmp.path(), &bad);
        assert!(
            !output.status.success(),
            "autumn {bad:?} should be rejected"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--api") && stderr.contains("incompatible"),
            "rejection message should explain the --api conflict, got:\n{stderr}"
        );
    }
}

/// Slow end-to-end check: the generated `--api` project type-checks against this
/// workspace's `autumn-web` (proving AC #1's compile guarantee and the lean
/// feature set actually builds).
///
/// Ignored by default; run with
/// `cargo test -p autumn-cli --test cli_tests -- --ignored api_scaffold_cargo_checks`.
#[test]
#[ignore = "slow: cargo-checks a fresh project — run with `cargo test -p autumn-cli -- --ignored`"]
fn api_scaffold_cargo_checks() {
    use std::fmt::Write as _;

    let (_tmp, project) = api_project("api-check-app", &[]);
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
        .args(["check", "--tests"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "cargo check on the --api scaffold failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
}
