//! Integration tests for `autumn generate tauri-mobile` (issue #1507).
//!
//! The mobile generator scaffolds an **in-process** Tauri v2 shell: the Autumn
//! Axum server runs on a background thread inside the app process (mobile
//! sandboxes forbid spawning sidecar processes), connecting to a remote
//! Postgres database.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Scaffold a fresh Autumn app via `autumn new` in a temp dir; returns the
/// temp dir guard and the project directory path.
fn fresh_project(project_name: &str) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");

    let output = Command::new(autumn_bin)
        .args(["new", project_name])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run `autumn new`");
    assert!(
        output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let project = temp_dir.path().join(project_name);
    (temp_dir, project)
}

/// Run the autumn CLI in `project_dir`, returning the raw output.
fn run_autumn_raw(project_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(project_dir)
        .output()
        .expect("failed to run autumn")
}

/// Run the autumn CLI in `project_dir`, asserting success.
fn run_autumn(project_dir: &Path, args: &[&str]) -> Output {
    let output = run_autumn_raw(project_dir, args);
    assert!(
        output.status.success(),
        "autumn {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

#[test]
fn generate_tauri_mobile_scaffolds_expected_files() {
    let (_tmp, project) = fresh_project("mobile-scaffold-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    for file in &[
        "src-tauri/tauri.conf.json",
        "src-tauri/Cargo.toml",
        "src-tauri/build.rs",
        "src-tauri/src/main.rs",
        "src-tauri/src/lib.rs",
        "src-tauri/.gitignore",
        "src-tauri/icons/icon.svg",
        "src-tauri/icons/32x32.png",
        "src-tauri/icons/128x128.png",
        "src-tauri/icons/128x128@2x.png",
        "src-tauri/icons/icon.png",
        "src-tauri/icons/icon.ico",
        "src-tauri/icons/icon.icns",
    ] {
        assert!(
            project.join(file).is_file(),
            "{file} must be created by `autumn generate tauri-mobile`"
        );
    }

    // The mobile model has NO sidecar: no staging scripts may be emitted.
    assert!(
        !project.join("src-tauri/stage-sidecar.sh").exists(),
        "mobile scaffold must not emit stage-sidecar.sh (no sidecar on mobile)"
    );
    assert!(
        !project.join("src-tauri/stage-sidecar.ps1").exists(),
        "mobile scaffold must not emit stage-sidecar.ps1 (no sidecar on mobile)"
    );
}

#[test]
fn generate_tauri_mobile_lib_rs_spawns_server_in_process() {
    let (_tmp, project) = fresh_project("mobile-inprocess-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    let lib_rs = read(&project.join("src-tauri/src/lib.rs"));

    // In-process lifecycle: server thread spawned inside Builder::setup(...).
    // Assert the crate-qualified serve() call and the literal HTTP request
    // line — NOT comment-matchable substrings — so deleting the server-spawn
    // or health-poll code from the template fails this test even though the
    // surrounding comments survive.
    assert!(lib_rs.contains("tauri::Builder::default()"));
    assert!(lib_rs.contains(".setup("));
    assert!(
        lib_rs.contains("block_on(mobile_inprocess_app::serve())"),
        "lib.rs must block a dedicated thread on the app's serve() future"
    );
    let spawn_pos = lib_rs
        .find("std::thread::spawn")
        .expect("lib.rs must spawn the server thread");
    assert!(
        lib_rs[spawn_pos..].contains("block_on(mobile_inprocess_app::serve())"),
        "the serve() call must live inside a spawned thread"
    );
    assert!(
        lib_rs.contains("tauri::mobile_entry_point"),
        "lib.rs must declare the mobile entry point"
    );

    // Readiness: a real GET /health probe before loading the webview at
    // 127.0.0.1:<port>.
    assert!(
        lib_rs.contains(r"GET /health HTTP/1.1\r\n"),
        "lib.rs must send a literal GET /health request line"
    );
    assert!(lib_rs.contains(r#"format!("http://127.0.0.1:{port}")"#));

    // No sidecar machinery of any kind.
    assert!(
        !lib_rs.contains(".sidecar("),
        "mobile lib.rs must not spawn a sidecar process"
    );
    assert!(
        !lib_rs.contains("tauri_plugin_shell"),
        "mobile lib.rs must not use tauri-plugin-shell"
    );
}

#[test]
fn generate_tauri_mobile_cargo_toml_is_mobile_lib() {
    let (_tmp, project) = fresh_project("mobile-cargo-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    let cargo_toml = read(&project.join("src-tauri/Cargo.toml"));

    // Mobile targets link the app as a library.
    assert!(cargo_toml.contains("staticlib"));
    assert!(cargo_toml.contains("cdylib"));
    assert!(cargo_toml.contains("rlib"));

    // The shell depends on the parent app crate by path (in-process server),
    // built with embedded assets — a device has no static/ dir to serve from.
    assert!(
        cargo_toml.contains(r#"mobile-cargo-app = { path = "..", features = ["embed-assets"] }"#),
        "src-tauri/Cargo.toml must depend on the app crate with embed-assets enabled"
    );

    // No sidecar plugin.
    assert!(
        !cargo_toml.contains("tauri-plugin-shell"),
        "mobile shell must not depend on tauri-plugin-shell"
    );
}

#[test]
fn generate_tauri_mobile_conf_has_no_external_bin() {
    let (_tmp, project) = fresh_project("mobile-conf-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    let conf_raw = read(&project.join("src-tauri/tauri.conf.json"));
    let conf: serde_json::Value =
        serde_json::from_str(&conf_raw).expect("tauri.conf.json must be valid JSON");

    // Kebab-case package → hyphens stripped from the identifier segment:
    // Android forbids hyphens in application ids (`cargo tauri android init`
    // panics on them) and Apple forbids underscores, so only alphanumerics
    // survive (same normalization as the thin-client generator).
    assert_eq!(
        conf.get("identifier").and_then(|v| v.as_str()),
        Some("com.example.mobileconfapp"),
        "the identifier must be Android- and iOS-safe"
    );
    assert!(
        conf.get("productName").and_then(|v| v.as_str()).is_some(),
        "tauri.conf.json must set productName"
    );

    // Sidecars cannot ship on mobile: externalBin must be absent (or null).
    let external_bin = conf.get("bundle").and_then(|b| b.get("externalBin"));
    assert!(
        external_bin.is_none() || external_bin == Some(&serde_json::Value::Null),
        "mobile tauri.conf.json must not declare bundle.externalBin, got: {external_bin:?}"
    );
}

#[test]
fn generate_tauri_mobile_extracts_app_lib() {
    let (_tmp, project) = fresh_project("mobile-extract-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    // The generator extracts the stock main.rs into src/lib.rs::serve() so
    // the Tauri shell can run the server in-process.
    let lib_rs = read(&project.join("src/lib.rs"));
    assert!(
        lib_rs.contains("pub async fn serve()"),
        "app src/lib.rs must expose pub async fn serve()"
    );
    assert!(lib_rs.contains("autumn_web::app()"));
    assert!(lib_rs.contains("routes!["));

    // main.rs shrinks to a thin caller of the extracted lib entry point.
    let main_rs = read(&project.join("src/main.rs"));
    assert!(
        main_rs.contains("::serve()"),
        "app src/main.rs must call the extracted serve() entry point"
    );
    assert!(
        !main_rs.contains("autumn_web::app()"),
        "app src/main.rs must no longer build the app inline after extraction"
    );
}

#[test]
fn generate_tauri_mobile_skips_lib_extraction_when_main_customised() {
    let (_tmp, project) = fresh_project("mobile-custom-app");

    // Simulate a user-customised main.rs the anchored extraction can't match.
    let main_path = project.join("src/main.rs");
    let customised = read(&main_path).replace("#[autumn_web::main]", "#[tokio::main]");
    fs::write(&main_path, &customised).unwrap();

    let output = run_autumn(&project, &["generate", "tauri-mobile"]);

    // Command still succeeds; the extraction is skipped, not fatal.
    assert!(
        !project.join("src/lib.rs").exists(),
        "src/lib.rs must not be created when main.rs is customised"
    );
    assert_eq!(
        read(&main_path),
        customised,
        "customised main.rs must be left untouched"
    );

    // The user is pointed at the docs page for the manual extraction steps.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("tauri-mobile-in-process"),
        "output must point at docs/guide/tauri-mobile-in-process.md, got:\n{combined}"
    );
}

#[test]
fn generate_tauri_mobile_sets_flaky_network_pool_defaults() {
    let (_tmp, project) = fresh_project("mobile-pool-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    let lib_rs = read(&project.join("src-tauri/src/lib.rs"));

    // Conservative remote-Postgres pool defaults for mobile networks — pinned
    // by VALUE (the advertised defaults), not just by variable name.
    assert!(
        lib_rs.contains(r#"set_var("AUTUMN_DATABASE__POOL_SIZE", "2")"#),
        "mobile lib.rs must pin POOL_SIZE=2 for flaky mobile networks"
    );
    assert!(
        lib_rs.contains(r#"set_var("AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS", "5")"#),
        "mobile lib.rs must pin CONNECT_TIMEOUT_SECS=5 for flaky mobile networks"
    );
    assert!(
        lib_rs.contains("AUTUMN_DATABASE__URL"),
        "mobile lib.rs must reference AUTUMN_DATABASE__URL for the remote Postgres"
    );
    // Release builds must run the prod profile (serve() bypasses the macro
    // entry point that would otherwise mark release builds).
    assert!(
        lib_rs.contains(r#"set_var("AUTUMN_ENV", "prod")"#),
        "mobile lib.rs must pin AUTUMN_ENV=prod for release builds"
    );
}

#[test]
fn generate_tauri_mobile_prints_prerequisites() {
    let (_tmp, project) = fresh_project("mobile-prereq-app");
    let output = run_autumn(&project, &["generate", "tauri-mobile"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("tauri-cli"), "must mention the Tauri CLI");
    assert!(stdout.contains("ios"), "must mention iOS setup");
    assert!(stdout.contains("android"), "must mention Android setup");
    assert!(
        stdout.contains("tauri-mobile-in-process"),
        "must point at docs/guide/tauri-mobile-in-process.md"
    );
}

#[test]
fn generate_tauri_mobile_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("mobile-dryrun-app");
    let main_before = read(&project.join("src/main.rs"));

    run_autumn(&project, &["generate", "tauri-mobile", "--dry-run"]);

    assert!(
        !project.join("src-tauri").exists(),
        "--dry-run must not create src-tauri/"
    );
    assert!(
        !project.join("src/lib.rs").exists(),
        "--dry-run must not create src/lib.rs"
    );
    assert_eq!(
        read(&project.join("src/main.rs")),
        main_before,
        "--dry-run must not modify src/main.rs"
    );
}

#[test]
fn generate_tauri_mobile_collision_without_force_exits_nonzero() {
    let (_tmp, project) = fresh_project("mobile-collision-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    // Second run without --force must refuse to overwrite.
    let second = run_autumn_raw(&project, &["generate", "tauri-mobile"]);
    assert!(
        !second.status.success(),
        "second run without --force must exit non-zero"
    );

    // With --force it succeeds (idempotent overwrite) — and the already
    // extracted app stays extracted: main.rs remains the thin serve() caller
    // and no re-extraction warning is printed.
    let forced = run_autumn(&project, &["generate", "tauri-mobile", "--force"]);
    let main_rs = read(&project.join("src/main.rs"));
    assert!(
        main_rs.contains("::serve().await"),
        "after a --force re-run, src/main.rs must still be the thin serve() caller"
    );
    assert!(
        !main_rs.contains("autumn_web::app()"),
        "a --force re-run must not re-inline the app into main.rs"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&forced.stdout),
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(
        !combined.contains("skipping"),
        "a --force re-run over an extracted app must be silent, got:\n{combined}"
    );
}

#[test]
fn generate_tauri_mobile_honors_custom_lib_name() {
    let (_tmp, project) = fresh_project("mobile-libname-app");

    // Declare a custom library crate name: Cargo then exposes the lib to the
    // binary and to path dependents as `custom_lib`, so every generated
    // serve() call site must use it (issue #1585 review).
    let cargo_path = project.join("Cargo.toml");
    let cargo = read(&cargo_path);
    fs::write(
        &cargo_path,
        format!("{cargo}\n[lib]\nname = \"custom_lib\"\n"),
    )
    .unwrap();

    run_autumn(&project, &["generate", "tauri-mobile"]);

    let shell_lib = read(&project.join("src-tauri/src/lib.rs"));
    assert!(
        shell_lib.contains("block_on(custom_lib::serve())"),
        "shell lib.rs must call the custom [lib] name, got:\n{shell_lib}"
    );
    assert!(
        !shell_lib.contains("mobile_libname_app::serve()"),
        "shell lib.rs must not call the dash-to-underscore package name"
    );

    let main_rs = read(&project.join("src/main.rs"));
    assert!(
        main_rs.contains("custom_lib::serve().await;"),
        "thin src/main.rs must call the custom [lib] name, got:\n{main_rs}"
    );
}

#[test]
fn generate_tauri_mobile_honors_custom_lib_path() {
    let (_tmp, project) = fresh_project("mobile-libpath-app");

    // Declare a custom library target file: Cargo compiles THAT file as the
    // lib and ignores a stray src/lib.rs, so the serve() extraction must
    // land there (issue #1585 review).
    let cargo_path = project.join("Cargo.toml");
    let cargo = read(&cargo_path);
    fs::write(
        &cargo_path,
        format!("{cargo}\n[lib]\npath = \"src/app_lib.rs\"\n"),
    )
    .unwrap();

    run_autumn(&project, &["generate", "tauri-mobile"]);

    let app_lib = read(&project.join("src/app_lib.rs"));
    assert!(
        app_lib.contains("pub async fn serve()"),
        "serve() must be extracted into the [lib].path file, got:\n{app_lib}"
    );
    assert!(
        !project.join("src/lib.rs").exists(),
        "no stray src/lib.rs may be created when [lib].path points elsewhere"
    );
    let main_rs = read(&project.join("src/main.rs"));
    assert!(
        main_rs.contains("::serve().await"),
        "src/main.rs must be the thin serve() caller, got:\n{main_rs}"
    );
}

#[test]
fn generate_tauri_mobile_custom_lib_path_with_existing_file_falls_back_with_warning() {
    let (_tmp, project) = fresh_project("mobile-libpath-existing-app");
    let cargo_path = project.join("Cargo.toml");
    let cargo = read(&cargo_path);
    fs::write(
        &cargo_path,
        format!("{cargo}\n[lib]\npath = \"src/app_lib.rs\"\n"),
    )
    .unwrap();
    let custom = "pub fn helper() {}\n";
    fs::write(project.join("src/app_lib.rs"), custom).unwrap();
    let main_before = read(&project.join("src/main.rs"));

    let output = run_autumn(&project, &["generate", "tauri-mobile"]);

    // Graceful fallback: the existing lib target and main.rs are untouched,
    // and the warning names the actual [lib].path file.
    assert_eq!(read(&project.join("src/app_lib.rs")), custom);
    assert_eq!(read(&project.join("src/main.rs")), main_before);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("src/app_lib.rs"),
        "the fallback warning must name the [lib].path file, got:\n{combined}"
    );
    assert!(
        combined.contains("tauri-mobile-in-process"),
        "the fallback warning must point at the docs page, got:\n{combined}"
    );
}

#[test]
fn generate_tauri_mobile_refuses_desktop_leftovers_even_with_force() {
    let (_tmp, project) = fresh_project("mobile-over-desktop-app");
    run_autumn(&project, &["generate", "tauri"]);

    for args in [
        &["generate", "tauri-mobile"][..],
        &["generate", "tauri-mobile", "--force"][..],
    ] {
        let refused = run_autumn_raw(&project, args);
        assert!(
            !refused.status.success(),
            "autumn {args:?} must refuse over a desktop scaffold"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains("desktop (sidecar)"),
            "error must name the conflicting mode, got:\n{stderr}"
        );
        assert!(
            stderr.contains("autumn destroy tauri"),
            "error must point at the destroy remedy, got:\n{stderr}"
        );
    }

    // The desktop scaffold survives intact — nothing was half-overwritten.
    assert!(project.join("src-tauri/stage-sidecar.sh").is_file());
    assert!(project.join("src-tauri/tauri.linux.conf.json").is_file());

    // The documented remedy keeps working: destroy desktop, then generate.
    run_autumn(&project, &["destroy", "tauri"]);
    run_autumn(&project, &["generate", "tauri-mobile"]);
    assert!(!project.join("src-tauri/stage-sidecar.sh").exists());
}

#[test]
fn generate_tauri_mobile_refuses_thin_client_leftovers_even_with_force() {
    let (_tmp, project) = fresh_project("mobile-over-thin-app");
    run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ],
    );

    let refused = run_autumn_raw(&project, &["generate", "tauri-mobile", "--force"]);
    assert!(
        !refused.status.success(),
        "generate tauri-mobile --force must refuse over a thin-client scaffold"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("thin-client"),
        "error must name the conflicting mode, got:\n{stderr}"
    );
    assert!(
        stderr.contains("destroy tauri --remote-url"),
        "error must point at the thin-client destroy remedy, got:\n{stderr}"
    );

    // The thin-client capability grants survive intact.
    assert!(
        project
            .join("src-tauri/capabilities/remote-app.json")
            .is_file()
    );
}

#[test]
fn generate_tauri_mobile_same_mode_force_regenerate_still_works() {
    let (_tmp, project) = fresh_project("mobile-samemode-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    // A same-mode --force regenerate is NOT a mixed-mode conflict: the guard
    // only fires on the other modes' marker files.
    let forced = run_autumn(&project, &["generate", "tauri-mobile", "--force"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&forced.stdout),
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(
        !combined.contains("already contains"),
        "same-mode --force must not trip the mixed-mode guard, got:\n{combined}"
    );
    assert!(project.join("src-tauri/src/lib.rs").is_file());
}

#[test]
fn destroy_tauri_mobile_removes_shell_keeps_extracted_app_lib() {
    let (_tmp, project) = fresh_project("mobile-destroy-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);
    assert!(project.join("src/lib.rs").is_file());

    // Dry-run first: reports the plan, removes nothing.
    run_autumn(&project, &["destroy", "tauri-mobile", "--dry-run"]);
    assert!(
        project.join("src-tauri").exists(),
        "destroy --dry-run must not remove src-tauri/"
    );

    // Real destroy (unedited scaffold — no --force needed).
    run_autumn(&project, &["destroy", "tauri-mobile"]);
    assert!(
        !project.join("src-tauri").exists(),
        "destroy must remove the generated src-tauri/ shell"
    );
    // The extracted app lib survives: main.rs depends on it.
    let lib_rs = read(&project.join("src/lib.rs"));
    assert!(
        lib_rs.contains("pub async fn serve()"),
        "destroy must keep the extracted src/lib.rs"
    );
    let main_rs = read(&project.join("src/main.rs"));
    assert!(
        main_rs.contains("::serve()"),
        "destroy must keep the thin src/main.rs serve() caller"
    );
}

#[test]
fn destroy_tauri_mobile_requires_force_after_editing_the_shell() {
    let (_tmp, project) = fresh_project("mobile-destroy-edited-app");
    run_autumn(&project, &["generate", "tauri-mobile"]);

    // The documented setup flow edits src-tauri/src/lib.rs (DB URL), so every
    // subsequent destroy hits the divergence check.
    let shell_lib = project.join("src-tauri/src/lib.rs");
    let edited = read(&shell_lib) + "\n// edited: AUTUMN_DATABASE__URL configured here\n";
    fs::write(&shell_lib, edited).unwrap();

    let refused = run_autumn_raw(&project, &["destroy", "tauri-mobile"]);
    assert!(
        !refused.status.success(),
        "destroy over an edited shell must refuse without --force"
    );
    assert!(project.join("src-tauri").exists());

    run_autumn(&project, &["destroy", "tauri-mobile", "--force"]);
    assert!(
        !project.join("src-tauri").exists(),
        "destroy --force must remove the edited shell"
    );
    assert!(
        project.join("src/lib.rs").is_file(),
        "destroy --force must still keep the extracted app src/lib.rs"
    );
}

#[test]
fn docs_page_covers_sandboxing_pool_and_app_store() {
    let docs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/guide");
    let page = read(&docs_dir.join("tauri-mobile-in-process.md"));

    // AC1: mobile sandboxing restrictions — no process spawning / sidecars.
    assert!(
        page.contains("sandbox"),
        "docs must cover mobile sandboxing"
    );
    assert!(
        page.contains("sidecar"),
        "docs must explain why the desktop sidecar model cannot ship on mobile"
    );

    // AC2: in-process background-thread architecture inside setup(...).
    assert!(page.contains(".setup("), "docs must show the setup() hook");
    assert!(
        page.contains("std::thread::spawn"),
        "docs must show the background server thread"
    );

    // AC3: remote Postgres pool behavior under flaky mobile networks.
    assert!(
        page.contains("AUTUMN_DATABASE__POOL_SIZE"),
        "docs must cover pool sizing"
    );
    assert!(
        page.contains("AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS"),
        "docs must cover connect timeouts"
    );

    // Security model: loopback trust boundary, DB TLS, failure modes.
    assert!(
        page.contains("## 5. Security"),
        "docs must have a clearly-titled security section"
    );
    assert!(
        page.contains("Any app on the device"),
        "docs must state the loopback trust boundary"
    );
    assert!(
        page.contains("verify-full"),
        "docs must cover database TLS modes"
    );

    // AC4: App Store Guideline compliance for hybrid native-web apps.
    assert!(page.contains("4.2"), "docs must cover Apple guideline 4.2");
    assert!(
        page.contains("2.5.2"),
        "docs must cover Apple guideline 2.5.2"
    );
    assert!(
        page.contains("Google Play"),
        "docs must cover the Google Play equivalent"
    );

    // The desktop guide links to the mobile page.
    let desktop = read(&docs_dir.join("tauri.md"));
    assert!(
        desktop.contains("tauri-mobile-in-process.md"),
        "docs/guide/tauri.md must link to the mobile in-process guide"
    );
}
