//! E2E tests for `autumn generate tauri --remote-url <URL>` — the mobile
//! thin-client mode (issue #1506).
//!
//! All tests are pure codegen assertions: they run the real `autumn` binary in
//! a tempdir and inspect files on disk. Nothing invokes the Tauri CLI, no
//! network, no mobile toolchain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const fn autumn_bin() -> &'static str {
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
    assert!(
        !capability.contains("biometric:default"),
        "base capability must not grant biometric — the plugin is not compiled \
         on desktop, so tauri-build would reject the permission there:\n{capability}"
    );

    let mobile_capability_path = project.join("src-tauri/capabilities/remote-app-mobile.json");
    let mobile_capability = fs::read_to_string(&mobile_capability_path).unwrap_or_else(|e| {
        panic!(
            "thin-client scaffold must write {}: {e}",
            mobile_capability_path.display()
        )
    });
    assert!(
        mobile_capability.contains("https://app.example.com"),
        "mobile capability file must grant the remote URL:\n{mobile_capability}"
    );
    assert!(
        mobile_capability.contains("biometric:default"),
        "mobile capability file must grant biometric:\n{mobile_capability}"
    );
    assert!(
        mobile_capability.contains(r#""platforms": ["android", "iOS"]"#),
        "mobile capability must be restricted to Android/iOS:\n{mobile_capability}"
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
        !project
            .join("src-tauri/capabilities/remote-app-mobile.json")
            .exists(),
        "destroy must remove the generated mobile capability file"
    );
    assert!(
        !project.join("src-tauri/src/lib.rs").exists(),
        "destroy must remove the generated shell sources"
    );
}

#[test]
fn generate_tauri_desktop_over_thin_client_is_rejected_even_with_force() {
    let (_tmp, project) = fresh_project("switch-to-desktop-app");
    let thin = run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ],
    );
    assert_success(&thin, "autumn generate tauri --remote-url");

    // Both a plain run and a --force run must refuse: --force means
    // "overwrite within the same mode", never "silently mix modes" —
    // stale capability files would break the desktop shell build
    // (tauri-build loads every file under src-tauri/capabilities/).
    for args in [
        &["generate", "tauri"][..],
        &["generate", "tauri", "--force"][..],
    ] {
        let output = run_autumn(&project, args);
        assert!(
            !output.status.success(),
            "`autumn {}` over a thin-client scaffold must fail:\nstdout: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("destroy tauri --remote-url"),
            "the error must point at the matching destroy command:\n{stderr}"
        );
    }

    // The rejection must leave the existing thin-client scaffold untouched
    // and write no desktop files.
    assert!(
        project
            .join("src-tauri/capabilities/remote-app.json")
            .is_file(),
        "rejection must not delete the existing thin-client scaffold"
    );
    assert!(
        !project.join("src-tauri/stage-sidecar.sh").exists(),
        "rejection must not write any desktop files"
    );

    // The prescribed remedy works: destroy the thin client, then desktop
    // generates cleanly with no stale capability files left over.
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
    let desktop = run_autumn(&project, &["generate", "tauri"]);
    assert_success(&desktop, "autumn generate tauri after destroy");
    assert!(
        !project.join("src-tauri/capabilities").exists(),
        "mode switch via destroy must leave no stale capability files"
    );
    assert!(project.join("src-tauri/stage-sidecar.sh").is_file());
}

#[test]
fn generate_tauri_thin_client_over_desktop_is_rejected_even_with_force() {
    let (_tmp, project) = fresh_project("switch-to-thin-app");
    let desktop = run_autumn(&project, &["generate", "tauri"]);
    assert_success(&desktop, "autumn generate tauri");

    for args in [
        &[
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ][..],
        &[
            "generate",
            "tauri",
            "--force",
            "--remote-url",
            "https://app.example.com",
        ][..],
    ] {
        let output = run_autumn(&project, args);
        assert!(
            !output.status.success(),
            "`autumn {}` over a desktop scaffold must fail:\nstdout: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("`autumn destroy tauri`"),
            "the error must point at the matching destroy command:\n{stderr}"
        );
    }

    // The rejection must leave the desktop scaffold untouched and write no
    // thin-client files.
    assert!(
        project.join("src-tauri/stage-sidecar.sh").is_file(),
        "rejection must not delete the existing desktop scaffold"
    );
    assert!(
        !project.join("src-tauri/capabilities").exists(),
        "rejection must not write any thin-client files"
    );

    // The prescribed remedy works: destroy the desktop scaffold, then the
    // thin client generates cleanly with no stale sidecar files left over.
    let destroy = run_autumn(&project, &["destroy", "tauri"]);
    assert_success(&destroy, "autumn destroy tauri");
    let thin = run_autumn(
        &project,
        &[
            "generate",
            "tauri",
            "--remote-url",
            "https://app.example.com",
        ],
    );
    assert_success(&thin, "autumn generate tauri --remote-url after destroy");
    for stale in [
        "src-tauri/stage-sidecar.sh",
        "src-tauri/stage-sidecar.ps1",
        "src-tauri/tauri.linux.conf.json",
        "src-tauri/tauri.macos.conf.json",
        "src-tauri/tauri.windows.conf.json",
    ] {
        assert!(
            !project.join(stale).exists(),
            "mode switch via destroy must leave no stale desktop file {stale}"
        );
    }
    assert!(
        project
            .join("src-tauri/capabilities/remote-app.json")
            .is_file()
    );
}

#[test]
fn destroy_tauri_still_works_on_a_mixed_tree() {
    // Users who mixed modes with an older autumn (before the guard) need the
    // documented remedy to actually run — destroy must never be blocked by
    // the other mode's leftovers.
    let (_tmp, project) = fresh_project("mixed-tree-destroy-app");
    let desktop = run_autumn(&project, &["generate", "tauri"]);
    assert_success(&desktop, "autumn generate tauri");
    // Simulate the old bug: thin-client capability files alongside the
    // desktop scaffold.
    fs::create_dir_all(project.join("src-tauri/capabilities")).unwrap();
    fs::write(project.join("src-tauri/capabilities/remote-app.json"), "{}").unwrap();

    let destroy = run_autumn(&project, &["destroy", "tauri"]);
    assert_success(&destroy, "autumn destroy tauri on a mixed tree");
    assert!(
        !project.join("src-tauri/stage-sidecar.sh").exists(),
        "destroy must remove the desktop scaffold despite thin-client leftovers"
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
        "remote-app-mobile.json",
        "platforms",
        "SameSite=None",
        "Authorization",
        "Guideline 4.2",
        "Switching between desktop and thin-client scaffolds",
    ] {
        assert!(
            doc.contains(anchor),
            "docs/guide/tauri-mobile-thin-client.md must cover '{anchor}'"
        );
    }
}
