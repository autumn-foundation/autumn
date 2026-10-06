//! `autumn data capsule verify`, end to end through this app's binary (#1811).
//!
//! The CLI does not link the app. It runs the app with
//! `AUTUMN_DATA_CAPSULE=verify` and reads one report line. Verify needs only
//! the signing secret, so this test runs with no database.

use std::path::Path;
use std::process::Output;

use autumn_web::gdpr::portability::{
    CapsuleModel, CapsuleSigner, FieldSpec, MemoryCapsuleStore, export_subject,
};

const SECRET: &str = "hello-capsule-secret-0123456789abcdef";
const REPORT: &str = "AUTUMN_DATA_CAPSULE_REPORT=";

fn run_verify(path: &Path) -> (Output, serde_json::Value) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hello"))
        .env("AUTUMN_DATA_CAPSULE", "verify")
        .env("AUTUMN_DATA_CAPSULE_PATH", path)
        .env("AUTUMN_SECURITY__SIGNING_SECRET", SECRET)
        .env_remove("DATABASE_URL")
        .output()
        .expect("the app binary must run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(REPORT))
        .unwrap_or_else(|| panic!("no report line; stdout:\n{stdout}"));
    let report = serde_json::from_str(line).expect("report is JSON");
    (output, report)
}

#[tokio::test]
async fn the_binary_verifies_a_capsule_and_detects_a_change() {
    let store = MemoryCapsuleStore::new().table(
        "users",
        vec![
            FieldSpec::new("id", "bigint"),
            FieldSpec::new("name", "text"),
        ],
    );
    store.insert("users", serde_json::json!({"id": 1, "name": "Ada"}));
    let capsule = export_subject(&[CapsuleModel::new("users", "id")], &store, "1")
        .await
        .expect("export");
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path().join("capsule");
    capsule
        .write_dir(&root, &CapsuleSigner::new(SECRET))
        .expect("write");

    let (output, report) = run_verify(&root);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["ok"], true);
    assert_eq!(report["report"]["subject"], "1");

    std::fs::write(root.join("records/users.json"), "[]").expect("tamper");
    let (output, report) = run_verify(&root);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["ok"], false);
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|e| e.contains("integrity")),
        "{report}"
    );
}

#[test]
fn the_prod_profile_refuses_a_weak_signing_secret() {
    let dir = tempfile::tempdir().expect("tmp");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hello"))
        .env("AUTUMN_DATA_CAPSULE", "verify")
        .env("AUTUMN_DATA_CAPSULE_PATH", dir.path())
        .env("AUTUMN_PROFILE", "prod")
        .env("AUTUMN_SECURITY__SIGNING_SECRET", "short")
        .env_remove("AUTUMN_ENV")
        .env_remove("DATABASE_URL")
        .output()
        .expect("the app binary must run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("Invalid signing secret configuration"),
        "stderr:\n{stderr}"
    );
    assert!(!stdout.contains(REPORT), "stdout:\n{stdout}");
}
