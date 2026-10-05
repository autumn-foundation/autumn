//! `AUTUMN_DATA_CAPSULE=verify` on an app with migrations (#1811).
//!
//! Verify needs only the signer. It must not open the database or run a
//! migration, so it must work when the database is down.

const REPORT: &str = "AUTUMN_DATA_CAPSULE_REPORT=";

#[test]
fn verify_does_not_open_the_database() {
    // A port with no listener: a connection attempt fails at once.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let missing = std::env::temp_dir().join(format!("no-capsule-{}", std::process::id()));

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_todo-app"))
        .env("AUTUMN_DATA_CAPSULE", "verify")
        .env("AUTUMN_DATA_CAPSULE_PATH", &missing)
        .env(
            "AUTUMN_SECURITY__SIGNING_SECRET",
            "todo-capsule-secret-0123456789abcdef",
        )
        .env(
            "AUTUMN_DATABASE__URL",
            format!("postgres://todo:none@127.0.0.1:{port}/todo"),
        )
        .env_remove("DATABASE_URL")
        .output()
        .expect("the app binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(REPORT))
        .unwrap_or_else(|| {
            panic!(
                "no report line; stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    let report: serde_json::Value = serde_json::from_str(line).expect("report is JSON");
    assert_eq!(report["mode"], "verify");
    // The capsule is missing, so verify fails, but on the capsule, not on the
    // database.
    assert_eq!(report["ok"], false);
    let error = report["error"].as_str().unwrap_or_default();
    assert!(error.contains("file is missing"), "{error}");
}
