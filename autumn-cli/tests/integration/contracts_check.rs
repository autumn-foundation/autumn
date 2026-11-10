//! Golden tests for `autumn contracts check`.
//!
//! Each case under `tests/golden/contracts/<case>/` holds `current.json`, a
//! release-2 bundle. The deployed side is `release-1.json`, an export of
//! `examples/mesh-catalog` and `examples/mesh-storefront`. The test runs the
//! binary and compares its report with `report.txt` and its exit code with the
//! table below. Set `UPDATE_GOLDEN=1` to write the reports again, then review
//! the diff.
//!
//! A case with `deployed/` and `current/` directories holds raw descriptors,
//! as the macros write them. The test joins each with `autumn contracts
//! export --from`, so the case also covers how the export resolves types and
//! routes.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/contracts")
}

fn check(
    current: &Path,
    against: impl AsRef<std::ffi::OsStr>,
    extra: &[&str],
) -> (i32, String, String) {
    check_in(Path::new("."), current, against, extra)
}

fn check_in(
    dir: &Path,
    current: &Path,
    against: impl AsRef<std::ffi::OsStr>,
    extra: &[&str],
) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .current_dir(dir)
        .args(["contracts", "check", "--against"])
        .arg(against)
        .arg("--current")
        .arg(current)
        .args(extra)
        // A local test server must not go through a proxy.
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .output()
        .expect("run autumn");
    (
        out.status.code().expect("exit code"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run one case and compare it with its golden report.
fn golden(case: &str, want_exit: i32) -> String {
    let dir = golden_dir();
    let (code, stdout, stderr) = check(
        &dir.join(case).join("current.json"),
        dir.join("release-1.json"),
        &[],
    );
    let report = dir.join(case).join("report.txt");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&report, &stdout).expect("write golden");
    }
    let expected = std::fs::read_to_string(&report)
        .unwrap_or_else(|e| panic!("missing golden {}: {e}", report.display()));
    assert_eq!(
        stdout, expected,
        "{case}: the report differs from its golden file. If the change is intended, run \
         `UPDATE_GOLDEN=1 cargo test -p autumn-cli --test cli_tests contracts_check` and \
         review the diff."
    );
    assert_eq!(code, want_exit, "{case}: exit code\n{stdout}\n{stderr}");
    stdout
}

/// Join the descriptors in `dir` into a bundle file in `out`.
fn export_from(dir: &Path, out: &Path) {
    let run = Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(["contracts", "export", "--from"])
        .arg(dir)
        .arg("--out")
        .arg(out)
        .output()
        .expect("run autumn");
    assert!(
        run.status.success(),
        "export --from {}: {}",
        dir.display(),
        String::from_utf8_lossy(&run.stderr)
    );
}

/// Run one case of raw descriptors and compare it with its golden report.
fn golden_descriptors(case: &str, want_exit: i32) -> String {
    let dir = golden_dir().join(case);
    let scratch = tempfile::tempdir().expect("tempdir");
    let deployed = scratch.path().join("deployed.json");
    let current = scratch.path().join("current.json");
    export_from(&dir.join("deployed"), &deployed);
    export_from(&dir.join("current"), &current);
    let (code, stdout, stderr) = check(&current, &deployed, &[]);
    let report = dir.join("report.txt");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&report, &stdout).expect("write golden");
    }
    let expected = std::fs::read_to_string(&report)
        .unwrap_or_else(|e| panic!("missing golden {}: {e}", report.display()));
    assert_eq!(
        stdout, expected,
        "{case}: the report differs from its golden file. If the change is intended, run \
         `UPDATE_GOLDEN=1 cargo test -p autumn-cli --test cli_tests contracts_check` and \
         review the diff."
    );
    assert_eq!(code, want_exit, "{case}: exit code\n{stdout}\n{stderr}");
    stdout
}

#[test]
fn a_change_to_a_crate_qualified_type_is_reported_beside_a_local_type_of_one_name() {
    // `Json<shared::Item>`, and the catalog crate has its own `Item`. A
    // name-only lookup takes the local `Item`, which does not change, and
    // passes.
    let report = golden_descriptors("shared_type_identity", 3);
    assert!(
        report.contains("reads response field `price_cents`"),
        "{report}"
    );
}

#[test]
fn a_new_json_form_with_the_same_rust_type_is_a_break() {
    // `created_at: DateTime<Utc>` gains `#[serde(with = "ts_seconds")]` and
    // `#[wire(type = "integer", format = "unix-seconds")]`. `ty` does not
    // change, so only the wire type shows the break.
    let report = golden_descriptors("wire_type_representation_changed", 1);
    assert!(
        report.contains("`DateTime<Utc> as integer(i64, unix-seconds)`"),
        "{report}"
    );
    assert!(
        report.contains("`DateTime<Utc> as string(date-time)`"),
        "{report}"
    );
}

#[test]
fn a_required_field_removed_from_a_nested_type_is_a_break() {
    let report = golden_descriptors("nested_required_field_removed", 3);
    assert!(report.contains("needs field `details.color`"), "{report}");
}

#[test]
fn a_unit_struct_that_becomes_an_empty_object_breaks_both_windows() {
    let report = golden_descriptors("unit_to_empty_object", 1);
    assert!(
        report.contains("If the callees ship first (new callees, deployed callers): 1 break(s)"),
        "{report}"
    );
    assert!(
        report.contains("If the callers ship first (new callers, deployed callees): 1 break(s)"),
        "{report}"
    );
    assert!(report.contains("JSON `null`"), "{report}");
}

#[test]
fn an_endpoint_removed_from_its_router_is_a_removed_route() {
    let report = golden_descriptors("endpoint_removed_from_router", 1);
    assert!(report.contains("does not serve it"), "{report}");
    assert!(report.contains("no `routes![…]`"), "{report}");
}

#[test]
fn a_user_no_body_dto_that_loses_a_read_field_is_a_break() {
    let report = golden_descriptors("user_no_body_response", 3);
    assert!(report.contains("reads response field `token`"), "{report}");
}

#[test]
fn a_removed_response_field_that_a_caller_reads_fails() {
    let report = golden("removed_read_response_field", 3);
    assert!(
        report.contains("reads response field `price_cents`"),
        "{report}"
    );
    assert!(
        report.contains("Deploy mesh-storefront before catalog"),
        "{report}"
    );
}

#[test]
fn a_removed_response_field_that_no_caller_reads_passes() {
    golden("removed_unread_response_field", 0);
}

#[test]
fn a_new_required_request_field_fails() {
    let report = golden("new_required_request_field", 3);
    assert!(report.contains("requires request field `sku`"), "{report}");
}

#[test]
fn a_new_optional_request_field_passes() {
    golden("new_optional_request_field", 0);
}

#[test]
fn a_path_change_fails() {
    let report = golden("path_changed", 1);
    assert!(report.contains("not safe in any deploy order"), "{report}");
}

#[test]
fn a_method_change_fails() {
    golden("method_changed", 1);
}

#[test]
fn a_path_change_that_keeps_the_old_route_names_the_deploy_order() {
    let report = golden("path_changed_old_route_kept", 3);
    assert!(
        report.contains("Deploy catalog before mesh-storefront"),
        "{report}"
    );
}

#[test]
fn a_new_response_field_a_new_caller_reads_names_the_deploy_order() {
    let report = golden("new_response_field_read_by_new_caller", 3);
    assert!(
        report.contains("safe only if the callees ship first"),
        "{report}"
    );
}

#[test]
fn an_unchanged_release_passes() {
    golden("unchanged", 0);
}

#[test]
fn a_declared_safe_order_passes_and_the_other_order_fails() {
    let dir = golden_dir();
    let current = dir.join("path_changed_old_route_kept/current.json");
    let against = dir.join("release-1.json");
    let (code, stdout, _) = check(&current, &against, &["--order", "callee-first"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("Declared order: callee-first. Safe."),
        "{stdout}"
    );
    let (code, stdout, _) = check(&current, &against, &["--order", "caller-first"]);
    assert_eq!(code, 1, "{stdout}");
}

#[test]
fn a_file_that_is_not_a_bundle_fails_with_a_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, b"{\"not\": \"a bundle\"}").expect("write");
    let (code, _, stderr) = check(&bad, golden_dir().join("release-1.json"), &[]);
    assert_eq!(code, 4, "a tool error is not a verdict");
    assert!(stderr.contains("not a contract bundle"), "{stderr}");
}

#[test]
fn the_golden_release_matches_the_example_release() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/mesh-storefront/contracts/release-1.json");
    assert_eq!(
        std::fs::read(golden_dir().join("release-1.json")).expect("golden"),
        std::fs::read(example).expect("example"),
        "copy examples/mesh-storefront/contracts/release-1.json to the golden dir"
    );
}

#[test]
fn the_removed_and_required_field_cases_fail_if_the_callees_ship_first() {
    let dir = golden_dir();
    for case in ["removed_read_response_field", "new_required_request_field"] {
        let current = dir.join(case).join("current.json");
        let (code, stdout, _) = check(
            &current,
            dir.join("release-1.json"),
            &["--order", "callee-first"],
        );
        assert_eq!(code, 1, "{case}: {stdout}");
    }
}

/// Serve `body` once over plain HTTP. Returns the URL.
fn serve_once(body: Vec<u8>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    format!("http://{addr}/contracts.json")
}

#[test]
fn a_bundle_url_is_fetched_when_plain_http_is_allowed() {
    let dir = golden_dir();
    let url = serve_once(std::fs::read(dir.join("release-1.json")).expect("bundle"));
    let current = dir.join("unchanged/current.json");
    let (code, stdout, stderr) = check(&current, &url, &["--allow-http"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

#[test]
fn a_plain_http_url_is_refused_by_default() {
    let current = golden_dir().join("unchanged/current.json");
    let (code, _, stderr) = check(&current, "http://127.0.0.1:9/contracts.json", &[]);
    assert_eq!(code, 4);
    assert!(stderr.contains("--allow-http"), "{stderr}");
}

#[test]
fn a_git_ref_is_exported_in_a_worktree_that_is_removed_afterwards() {
    let repo = tempfile::tempdir().expect("tempdir");
    let root = repo.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"plain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("Cargo.toml");
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::write(root.join("src/lib.rs"), "pub fn plain() {}\n").expect("lib.rs");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "release 1"]);

    let current = golden_dir().join("unchanged/current.json");
    let (code, _, stderr) = check_in(root, &current, "HEAD", &[]);
    // The crate has no `#[endpoint]` and no `#[event]`, so the export at HEAD
    // refuses an empty contract. The worktree and its pass ran to get there.
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("export at `HEAD`"), "{stderr}");
    assert!(
        stderr.contains("no endpoint or event descriptor"),
        "{stderr}"
    );
    assert_eq!(
        git(&["worktree", "list"]).lines().count(),
        1,
        "worktree removed"
    );

    let (code, _, stderr) = check_in(root, &current, "no-such-ref", &[]);
    assert_eq!(code, 4);
    assert!(stderr.contains("not a bundle file"), "{stderr}");
}
