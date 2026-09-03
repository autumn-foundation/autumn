//! End-to-end test: scaffold a project, build it, run it, and verify HTTP responses.
//!
//! This test is `#[ignore]` because it compiles a fresh Rust project from scratch,
//! which takes a while. Run explicitly with:
//!
//! ```sh
//! cargo test -p autumn-cli -- --ignored
//! ```

use std::fmt::Write as _;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::Duration;

/// RAII guard that kills the child process on drop (even on test failure / panic).
struct ServerGuard(std::process::Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn patch_generated_cargo_toml(project_dir: &std::path::Path) {
    let cargo_toml_path = project_dir.join("Cargo.toml");
    let mut content =
        std::fs::read_to_string(&cargo_toml_path).expect("failed to read generated Cargo.toml");

    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root not found");
    let autumn_web_crate = workspace_root.join("autumn");

    write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web_crate.display().to_string().replace('\\', "/")
    )
    .expect("write to String is infallible");

    std::fs::write(&cargo_toml_path, content).expect("failed to patch Cargo.toml");
}

#[test]
#[ignore = "slow: compiles a fresh Rust project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_project_compiles_runs_and_serves() {
    // ── 1. Create temp directory ────────────────────────────────────
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");

    // ── 2. Scaffold project via the real CLI binary ─────────────────
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");

    let new_output = Command::new(autumn_bin)
        .args(["new", "test-app"])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run `autumn new`");

    assert!(
        new_output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&new_output.stdout),
        String::from_utf8_lossy(&new_output.stderr),
    );

    let project_dir = temp_dir.path().join("test-app");
    assert!(project_dir.join("Cargo.toml").is_file());
    assert!(project_dir.join("src/main.rs").is_file());

    // ── 3. Patch Cargo.toml to use local autumn crate ───────────────
    patch_generated_cargo_toml(&project_dir);

    // ── 4. Build the scaffolded project ─────────────────────────────
    let build_output = Command::new("cargo")
        .args(["build"])
        .current_dir(&project_dir)
        .output()
        .expect("failed to run cargo build");

    assert!(
        build_output.status.success(),
        "cargo build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&build_output.stdout),
        String::from_utf8_lossy(&build_output.stderr),
    );

    // ── 6. Pick a free port and launch the server ───────────────────
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
        listener.local_addr().unwrap().port()
    };

    let child = Command::new("cargo")
        .args(["run"])
        .current_dir(&project_dir)
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn cargo run");

    let _guard = ServerGuard(child);

    // ── 7. Wait for the server to be ready (up to 30 s) ────────────
    let client = reqwest::blocking::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut ready = false;

    for _ in 0..60 {
        if client.get(format!("{base}/health")).send().is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(ready, "Server failed to become ready within 30 seconds");

    // ── 8. HTTP assertions ──────────────────────────────────────────

    // GET / -> 200 with welcome text
    let resp = client.get(format!("{base}/")).send().expect("GET / failed");
    assert_eq!(resp.status(), 200, "GET / status");
    let body = resp.text().unwrap();
    assert!(
        body.contains("Welcome to test-app!"),
        "GET / body missing welcome text, got: {body}",
    );

    // GET /hello/world -> 200 with greeting
    let resp = client
        .get(format!("{base}/hello/world"))
        .send()
        .expect("GET /hello/world failed");
    assert_eq!(resp.status(), 200, "GET /hello/world status");
    let body = resp.text().unwrap();
    assert!(
        body.contains("Hello, world!"),
        "GET /hello/world body missing greeting, got: {body}",
    );

    // GET /health -> 200 with JSON content-type
    let resp = client
        .get(format!("{base}/health"))
        .send()
        .expect("GET /health failed");
    assert_eq!(resp.status(), 200, "GET /health status");
    let ct = resp
        .headers()
        .get("content-type")
        .expect("missing content-type on /health")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        ct.contains("application/json"),
        "GET /health content-type expected application/json, got: {ct}",
    );

    // ── 9. Cleanup: _guard drops here and kills the server process ──
}

/// Proves the *other* half of the "local development" install path that
/// README.md and `docs/guide/getting-started.md` document alongside the
/// published-CLI quickstart: `cargo install --path autumn-cli` from a
/// source checkout, then `autumn new`.
///
/// [`generated_project_compiles_runs_and_serves`] above calls
/// [`patch_generated_cargo_toml`] so its `cargo build` always compiles
/// against *this* checkout's `autumn-web` — it proves the CLI and the
/// framework stay internally consistent with each other, which is the
/// question `generator-conformance.yml` exists to answer. It does not, and
/// by construction cannot, prove that a source-built CLI's scaffold still
/// compiles against the **published** `autumn-web` it actually depends on:
/// `autumn new` pins `autumn-web = "{CARGO_PKG_VERSION}"` (`new.rs`), and
/// that version string is frozen at the last release tag until a maintainer
/// explicitly cuts the next one (see CLAUDE.md, "Never bump the workspace
/// version"). So for every commit between a release and the next one, a
/// source-built CLI reports the *same* version as the last published
/// `autumn-web` while its own scaffold templates may already require an
/// API only the in-tree crate has — and nothing else in this repo's CI
/// builds that exact pairing: `quickstart-gate.yml` always installs the
/// *published* CLI (so CLI and framework are release-locked together by
/// definition), and the test above always re-patches back to the in-tree
/// crate.
///
/// This is not hypothetical: commit 76c56b1 (#2320/#2341) widened
/// `inject_consent_banner`'s `csrf_cookie_name` from `&str` to
/// `Option<&str>` and updated the `autumn new` scaffold template to match
/// (correctly — the in-tree call site and the in-tree signature agree).
/// Three days later, with no release cut since, `autumn doctor`'s
/// `version_compat` check still reports the two versions as matching
/// (`0.7.0` / `0.7.0` — see `doctor.rs`; the check compares version
/// *strings*, and `autumn new` wrote that string from the CLI's own
/// `CARGO_PKG_VERSION` in the first place, so it is close to tautological
/// right after scaffolding) — yet `autumn new my-app && cargo build`
/// against the real, published `autumn-web = "0.7.0"` fails outright with
/// `error[E0308]: mismatched types` on the freshly generated `src/main.rs`,
/// before a single line of application code is written. That is the
/// literal first build of the literal `autumn new` output — not a
/// generator, not an edge case.
///
/// A red run here means exactly that has recurred: some in-tree commit has
/// changed a public `autumn-web` API that the **base `autumn new` template**
/// calls, without a release to carry the change to crates.io yet. This test
/// only builds the bare `autumn new` output — it does not run `autumn
/// generate`, so drift confined to a generator-emitted call site (a
/// `scaffold`/`auth`/etc. template) is not covered here and would need a
/// separate, generator-specific version of this same gap. Until the next
/// release ships, the workaround `autumn doctor`'s docs already describe is
/// to point the generated `Cargo.toml`
/// at the checkout with a `[patch.crates-io]` override (exactly what
/// [`patch_generated_cargo_toml`] does above) instead of building against
/// the stale-pinned published crate.
#[test]
#[ignore = "slow: compiles a fresh Rust project — run with `cargo test -p autumn-cli -- --ignored`"]
fn generated_project_compiles_against_published_autumn_web() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");

    let new_output = Command::new(autumn_bin)
        .args(["new", "test-app"])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run `autumn new`");

    assert!(
        new_output.status.success(),
        "autumn new failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&new_output.stdout),
        String::from_utf8_lossy(&new_output.stderr),
    );

    let project_dir = temp_dir.path().join("test-app");
    assert!(project_dir.join("Cargo.toml").is_file());

    // No [patch.crates-io] here, deliberately: this project must build
    // exactly as `autumn new` left it, against the real crates.io
    // `autumn-web`, the way a local-development user's first `cargo build`
    // actually resolves it.
    let build_output = Command::new("cargo")
        .args(["build"])
        .current_dir(&project_dir)
        .output()
        .expect("failed to run cargo build");

    assert!(
        build_output.status.success(),
        "A source-built `autumn` CLI's scaffold no longer compiles against the \
         published `autumn-web` it pins (\"local development\" path in README.md \
         / docs/guide/getting-started.md). This means an in-tree commit changed a \
         public autumn-web API that an autumn new/autumn generate template calls, \
         ahead of a release — see this test's doc comment for the mechanism and \
         the [patch.crates-io] workaround.\ncargo build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&build_output.stdout),
        String::from_utf8_lossy(&build_output.stderr),
    );
}
