//! E2E tests for `autumn generate tauri --remote-url <URL>` — the mobile
//! thin-client mode (issue #1506).
//!
//! All tests are pure codegen assertions: they run the real `autumn` binary in
//! a tempdir and inspect files on disk. Nothing invokes the Tauri CLI, no
//! network, no mobile toolchain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Scaffold a fresh autumn project with `autumn new` and return
/// (tempdir guard, project dir).
fn fresh_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let output = Command::new(autumn_bin())
        .args(["new", name])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run `autumn new`");
    assert!(
        output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let project = temp_dir.path().join(name);
    (temp_dir, project)
}

/// Run the autumn binary with `args` in `dir` and return the raw output.
fn run_autumn(dir: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run autumn")
}

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn generate_tauri_remote_url_scaffolds_thin_client() {
    let (_tmp, project) = fresh_project("thin-client-app");
    let output = run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ],
    );
    assert_success(&output, "autumn generate tauri --remote-url");

    let capability_path = project.join("src-tauri/capabilities/remote-app.json");
    let capability = fs::read_to_string(&capability_path).unwrap_or_else(|e| {
        panic!(
            "thin-client scaffold must write {}: {e}",
            capability_path.display()
        )
    });
    assert!(
        capability.contains("https://app.example.com"),
        "capability file must grant the remote URL:\n{capability}"
    );

    let lib = fs::read_to_string(project.join("src-tauri/src/lib.rs"))
        .expect("thin-client scaffold must write src-tauri/src/lib.rs");
    assert!(
        lib.contains("https://app.example.com"),
        "generated lib.rs must point the webview at the remote URL:\n{lib}"
    );

    assert!(
        !project.join("src-tauri/stage-sidecar.sh").exists(),
        "thin-client scaffold must not emit sidecar staging scripts"
    );
}

#[test]
fn generate_tauri_remote_url_rejects_http() {
    let (_tmp, project) = fresh_project("thin-client-http-app");
    let output = run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--remote-url",
            "http://app.example.com",
        ],
    );
    assert!(
        !output.status.success(),
        "a plain-http --remote-url must be rejected with a non-zero exit:\nstdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("https"),
        "the error must point at the https requirement:\n{stderr}"
    );
    assert!(
        !project.join("src-tauri").exists(),
        "a rejected --remote-url must not leave a partial src-tauri/ behind"
    );
}

#[test]
fn generate_tauri_without_flag_still_scaffolds_desktop_sidecar() {
    let (_tmp, project) = fresh_project("desktop-sidecar-app");
    let output = run_autumn(&project, &["generate", "tauri"]);
    assert_success(&output, "autumn generate tauri");

    assert!(
        project.join("src-tauri/stage-sidecar.sh").is_file(),
        "no-flag desktop scaffold must still emit stage-sidecar.sh"
    );
    let conf = fs::read_to_string(project.join("src-tauri/tauri.conf.json"))
        .expect("desktop scaffold must write tauri.conf.json");
    assert!(
        conf.contains("externalBin"),
        "desktop tauri.conf.json must keep the sidecar externalBin:\n{conf}"
    );
    assert!(
        !project.join("src-tauri/capabilities").exists(),
        "desktop scaffold must not grow a capabilities/ dir from the thin-client feature"
    );
}

#[test]
fn generate_tauri_remote_url_dry_run_writes_nothing() {
    let (_tmp, project) = fresh_project("thin-client-dry-app");
    let output = run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--dry-run",
            "--remote-url",
            "https://app.example.com",
        ],
    );
    assert_success(&output, "autumn generate tauri --dry-run --remote-url");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("capabilities/remote-app.json"),
        "dry-run must print the planned capability file path:\n{stdout}"
    );
    assert!(
        !project.join("src-tauri").exists(),
        "dry-run must not write anything to disk"
    );
}

#[test]
fn destroy_tauri_remote_url_reverts_thin_client_scaffold() {
    let (_tmp, project) = fresh_project("thin-client-destroy-app");
    let generate = run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ],
    );
    assert_success(&generate, "autumn generate tauri --remote-url");
    assert!(
        project
            .join("src-tauri/capabilities/remote-app.json")
            .is_file()
    );

    let destroy = run_autumn(
        &project,
        &[
            "destroy",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ],
    );
    assert_success(&destroy, "autumn destroy tauri --remote-url");

    assert!(
        !project
            .join("src-tauri/capabilities/remote-app.json")
            .exists(),
        "destroy must remove the generated capability file"
    );
    assert!(
        !project.join("src-tauri/src/lib.rs").exists(),
        "destroy must remove the generated shell sources"
    );
}

#[test]
fn thin_client_docs_page_covers_required_topics() {
    // The docs page is a co-equal deliverable of issue #1506; this pins it to
    // the generator so they cannot drift apart silently.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli should live under the workspace root")
        .to_path_buf();
    let doc_path = workspace_root.join("docs/guide/tauri-mobile-thin-client.md");
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", doc_path.display()));

    for anchor in [
        "remote",
        "urls",
        "notification:default",
        "biometric:default",
        "store:default",
        "SameSite=None",
        "Authorization",
        "Guideline 4.2",
    ] {
        assert!(
            doc.contains(anchor),
            "docs/guide/tauri-mobile-thin-client.md must cover '{anchor}'"
        );
    }
}
