//! Drift guards for the resilience hygiene fixes (issue #3066).
//!
//! Each test reads a doc or a script and compares it with the code it
//! describes, so a later edit cannot make them disagree again.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli should live under the workspace root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
        .replace("\r\n", "\n")
}

/// The `///` doc lines directly above `pub struct <name>`, without the
/// `/// ` prefix. Attribute lines between the doc and the item are skipped.
fn struct_doc(source: &str, name: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let item = format!("pub struct {name} ");
    let at = lines
        .iter()
        .position(|line| line.starts_with(&item))
        .unwrap_or_else(|| panic!("`pub struct {name}` not found"));
    let mut doc = Vec::new();
    for line in lines[..at].iter().rev() {
        let line = line.trim_start();
        if let Some(text) = line.strip_prefix("///") {
            doc.push(text.strip_prefix(' ').unwrap_or(text));
        } else if !line.starts_with("#[") {
            break;
        }
    }
    doc.reverse();
    doc.join("\n")
}

/// The body of the `## <heading>` section of a markdown file.
fn markdown_section<'a>(markdown: &'a str, heading: &str) -> &'a str {
    let start = markdown
        .find(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("section `## {heading}` not found"));
    let body = &markdown[start + 1..];
    let end = body[3..].find("\n## ").map_or(body.len(), |at| at + 3);
    &body[..end]
}

// ── 1. request timeout status ───────────────────────────────────────────────

#[test]
fn request_timeout_docs_name_503_not_408() {
    let config = read("autumn/src/config.rs");
    let doc = struct_doc(&config, "RequestTimeoutsConfig");
    assert!(
        doc.starts_with("Per-request timeout configuration."),
        "RequestTimeoutsConfig must carry its own doc, not a neighbour's:\n{doc}"
    );
    // 408 means the client was too slow to send the request. A server-side
    // deadline is a 503.
    assert!(
        !doc.contains("408"),
        "RequestTimeoutsConfig must not claim a 408 response:\n{doc}"
    );
    assert!(
        doc.contains("503 Service Unavailable"),
        "RequestTimeoutsConfig must name the 503 that RequestTimeoutLayer returns:\n{doc}"
    );
}

#[test]
fn server_config_keeps_its_own_doc() {
    let config = read("autumn/src/config.rs");
    let doc = struct_doc(&config, "ServerConfig");
    assert!(
        doc.starts_with("HTTP server configuration."),
        "ServerConfig must carry its own doc:\n{doc}"
    );
}

// ── 2. Postgres job runtime wake-ups ────────────────────────────────────────

/// The `PG_WORKER_IDLE_SLEEP` value in milliseconds.
fn pg_worker_idle_sleep_ms(job_rs: &str) -> u64 {
    let line = job_rs
        .lines()
        .find(|line| line.contains("const PG_WORKER_IDLE_SLEEP"))
        .expect("job.rs must declare PG_WORKER_IDLE_SLEEP");
    let digits = line
        .split("from_millis(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("PG_WORKER_IDLE_SLEEP must use from_millis: {line}"));
    digits
        .parse()
        .unwrap_or_else(|err| panic!("bad PG_WORKER_IDLE_SLEEP value {digits:?}: {err}"))
}

#[test]
fn postgres_job_runtime_is_not_described_as_listen_notify() {
    // The Postgres workers poll. Nothing in the job runtime uses
    // LISTEN/NOTIFY, so a line that names it may only deny it.
    for file in [
        "autumn/src/job.rs",
        "autumn/src/job/sqlite.rs",
        "autumn/src/sim/substrate.rs",
    ] {
        let source = read(file);
        for (index, line) in source.lines().enumerate() {
            assert!(
                !line.contains("LISTEN") || line.contains("no `LISTEN`"),
                "{file}:{}: the job runtime does not use LISTEN/NOTIFY: {line}",
                index + 1
            );
        }
    }
}

#[test]
fn jobs_guide_documents_the_postgres_poll_interval() {
    let poll_ms = pg_worker_idle_sleep_ms(&read("autumn/src/job.rs"));
    let guide = read("docs/guide/jobs.md");
    let section = markdown_section(&guide, "Postgres delivery semantics");
    assert!(
        section.contains(&format!("{poll_ms}ms")),
        "the Postgres section must state the {poll_ms}ms idle poll interval:\n{section}"
    );
    assert!(
        section.contains("no `LISTEN`/`NOTIFY`"),
        "the Postgres section must say that workers do not use LISTEN/NOTIFY:\n{section}"
    );
}

// ── 3. probe paths outside the release templates ────────────────────────────

#[test]
fn app_runner_cutover_health_check_uses_ready() {
    let guide = read("docs/guide/deployment.md");
    let lines: Vec<&str> = guide
        .lines()
        .filter(|line| line.contains("--health-check-configuration"))
        .collect();
    assert!(
        !lines.is_empty(),
        "deployment.md must show the App Runner cutover health check"
    );
    for line in lines {
        assert!(
            line.contains(r#"\"Path\": \"/ready\""#),
            "the App Runner cutover must check /ready, the drain-aware probe: {line}"
        );
    }
}

#[test]
fn healthcheck_url_overrides_use_live() {
    // The image HEALTHCHECK is a liveness probe. An override must keep the
    // same path and change only the scheme. Each file shows one or more
    // overrides, so a reworded example cannot make this check empty.
    for file in [
        "docs/guide/tls.md",
        "scripts/check-release-image-boot.sh",
        "skills/autumn-web/references/api-reference.md",
    ] {
        let content = read(file);
        let overrides: Vec<&str> = content
            .lines()
            .filter(|line| line.contains("AUTUMN_HEALTHCHECK_URL="))
            .collect();
        assert!(
            !overrides.is_empty(),
            "{file} must show an AUTUMN_HEALTHCHECK_URL override"
        );
        for line in overrides {
            assert!(
                line.contains("/live") && !line.contains("/health"),
                "{file}: an AUTUMN_HEALTHCHECK_URL override must point at /live: {line}"
            );
        }
    }
}

// ── 4. Verus proofs in CI ───────────────────────────────────────────────────

const VERUS_WORKFLOW: &str = ".github/workflows/verus.yml";
const VERUS_SCRIPT: &str = "scripts/verify-verus.sh";

fn verus_specs() -> Vec<String> {
    let dir = workspace_root().join("verification");
    let mut specs: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        .map(|entry| entry.expect("dir entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        // Case-sensitive, as the script's `*.rs` glob is.
        .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "rs"))
        .collect();
    specs.sort();
    assert!(
        !specs.is_empty(),
        "verification/ must hold at least one spec"
    );
    specs
}

#[test]
fn verus_workflow_runs_the_verification_script() {
    let workflow = read(VERUS_WORKFLOW);
    assert!(
        workflow.contains(&format!("./{VERUS_SCRIPT}")),
        "{VERUS_WORKFLOW} must run ./{VERUS_SCRIPT}:\n{workflow}"
    );
    for trigger in ["pull_request:", "schedule:", "workflow_dispatch:"] {
        assert!(
            workflow.contains(trigger),
            "{VERUS_WORKFLOW} must trigger on `{trigger}`:\n{workflow}"
        );
    }
    for path in ["verification/**", VERUS_SCRIPT, VERUS_WORKFLOW] {
        assert!(
            workflow.contains(&format!("\"{path}\"")),
            "{VERUS_WORKFLOW} must run when `{path}` changes:\n{workflow}"
        );
    }
    // Non-blocking at first: no CI job depends on it.
    let ci = read(".github/workflows/ci.yml");
    for line in ci.lines().filter(|line| line.contains("needs:")) {
        assert!(
            !line.to_ascii_lowercase().contains("verus"),
            "ci.yml must not depend on the Verus job while it is non-blocking: {line}"
        );
    }
}

#[test]
fn verification_readme_names_the_ci_workflow() {
    let readme = read("verification/README.md");
    assert!(
        readme.contains(VERUS_WORKFLOW) && readme.contains(VERUS_SCRIPT),
        "verification/README.md must name {VERUS_WORKFLOW} and {VERUS_SCRIPT}:\n{readme}"
    );
}

/// Run the Verus script with a stub `verus` that logs each spec and exits
/// with `stub_exit`. Return the script's exit success and the logged specs.
#[cfg(unix)]
fn run_verus_script_with_stub(stub_exit: i32) -> (bool, Vec<String>, String) {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let log = tmp.path().join("calls.log");
    let stub = tmp.path().join("verus");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho \"$1\" >> '{}'\nexit {stub_exit}\n",
            log.display()
        ),
    )
    .expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");

    let output = Command::new("bash")
        .arg(workspace_root().join(VERUS_SCRIPT))
        .env("VERUS_BIN", &stub)
        .output()
        .expect("run the Verus script");
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), calls, stderr)
}

#[cfg(unix)]
#[test]
fn verus_script_checks_every_spec() {
    let (ok, mut calls, stderr) = run_verus_script_with_stub(0);
    assert!(ok, "the script must pass when every proof passes: {stderr}");
    // The shell glob order can differ from a byte sort, so compare sorted.
    calls.sort();
    let expected: Vec<String> = verus_specs()
        .into_iter()
        .map(|spec| format!("verification/{spec}"))
        .collect();
    assert_eq!(calls, expected, "the script must run Verus once per spec");
}

#[cfg(unix)]
#[test]
fn verus_script_fails_when_a_proof_fails() {
    let (ok, calls, stderr) = run_verus_script_with_stub(1);
    assert!(!ok, "the script must fail when Verus rejects a proof");
    assert_eq!(
        calls.len(),
        verus_specs().len(),
        "one failed proof must not hide the result of the others"
    );
    assert!(
        stderr.contains("Verus rejected"),
        "the script must name the failed spec: {stderr}"
    );
}
