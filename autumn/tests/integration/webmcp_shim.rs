//! Run the `WebMCP` shim behaviour cases (issue #2619) under Node.
//!
//! The cases are in `tests/fixtures/webmcp/shim_harness.js`. They run the
//! script the framework serves, not a copy. With no Node on `PATH` the test
//! skips, unless `AUTUMN_REQUIRE_NODE` is set: then it fails. The `webmcp`
//! CI lane sets it.

#![cfg(feature = "webmcp")]

use std::path::PathBuf;
use std::process::Command;

#[test]
fn shim_behaviour_cases_pass_under_node() {
    let node = std::env::var_os("NODE").unwrap_or_else(|| "node".into());
    if Command::new(&node).arg("--version").output().is_err() {
        assert!(
            std::env::var_os("AUTUMN_REQUIRE_NODE").is_none(),
            "AUTUMN_REQUIRE_NODE is set, but Node is not on PATH"
        );
        eprintln!("skipping the WebMCP shim cases: Node is not on PATH");
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let shim = dir.path().join("autumn-webmcp.js");
    std::fs::write(&shim, autumn_web::webmcp::SHIM_JS).expect("write the shim");
    let harness =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/webmcp/shim_harness.js");

    let out = Command::new(&node)
        .arg(&harness)
        .arg(&shim)
        .output()
        .expect("run node");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "WebMCP shim cases failed:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("all passed"), "{stdout}\n{stderr}");
}
