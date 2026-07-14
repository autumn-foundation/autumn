//! Integration tests for `autumn workflow check` / `autumn workflow diagram`
//! (issue #1675).
//!
//! Each test scaffolds a throwaway project with a `src/*.rs` file containing a
//! `#[workflow]` enum in a `TempDir`, runs the real `autumn` binary against it,
//! and asserts on the exit code and output. The fixture `.rs` files are only
//! ever *parsed* by the scanner, so they need to be syntactically valid Rust but
//! need not compile or link.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

/// Write a file, creating parent directories as needed.
fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Run `autumn workflow <sub> [args...]` inside `root`.
fn run_workflow(root: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("workflow")
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run autumn workflow")
}

/// A sound order-lifecycle workflow: every state reachable from `Cart`, and
/// every non-terminal state can reach a terminal (`Delivered`/`Cancelled`).
const GOOD_ORDER: &str = r#"
use autumn_web::workflow;

#[workflow(
    initial = Cart,
    terminal(Delivered, Cancelled),
    transitions(
        Cart -> Placed,
        Placed -> Paid,
        Paid -> Shipped,
        Shipped -> Delivered,
        Placed -> Cancelled,
        Paid -> Cancelled,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Cart,
    Placed,
    Paid,
    Shipped,
    Delivered,
    Cancelled,
}
"#;

fn good_project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/order.rs", GOOD_ORDER);
    dir
}

#[test]
fn sound_workflow_exits_zero() {
    let dir = good_project();
    let output = run_workflow(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a sound workflow should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("PASS"), "stdout:\n{stdout}");
    assert!(stdout.contains("OrderState"), "stdout:\n{stdout}");
    assert!(stdout.contains("sound"), "stdout:\n{stdout}");
}

#[test]
fn unreachable_state_fails_and_is_named() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r#"
#[workflow(
    initial = Cart,
    terminal(Delivered),
    transitions(
        Cart -> Placed,
        Placed -> Delivered,
    )
)]
pub enum OrderState {
    Cart,
    Placed,
    Delivered,
    Refunded,
}
"#,
    );
    let output = run_workflow(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "an unreachable state must exit non-zero\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Refunded"),
        "should name the unreachable state\n{stdout}"
    );
    assert!(
        stdout.contains("unreachable"),
        "should describe the reachability violation\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn non_terminal_dead_end_fails_and_is_named() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/post.rs",
        r#"
#[workflow(
    initial = Draft,
    terminal(Published),
    transitions(
        Draft -> Published,
        Draft -> Limbo,
    )
)]
pub enum PostState {
    Draft,
    Published,
    Limbo,
}
"#,
    );
    let output = run_workflow(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a non-terminal dead-end must exit non-zero\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Limbo"),
        "should name the dead-end state\n{stdout}"
    );
    assert!(
        stdout.contains("dead-end"),
        "should describe the dead-end violation\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn unknown_state_reference_fails_and_is_named() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/ticket.rs",
        r#"
#[workflow(
    initial = Open,
    terminal(Closed),
    transitions(
        Open -> Closed,
        Open -> Ghost,
    )
)]
pub enum TicketState {
    Open,
    Closed,
}
"#,
    );
    let output = run_workflow(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "an unknown state reference must exit non-zero\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Ghost"),
        "should name the unknown state\n{stdout}"
    );
    assert!(
        stdout.contains("unknown state"),
        "should describe the existence violation\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn json_format_on_good_workflow_is_valid_and_exits_zero() {
    let dir = good_project();
    let output = run_workflow(dir.path(), &["check", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "sound workflow exits 0 in json mode\nstdout:\n{stdout}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));
    let workflows = json["workflows"].as_array().expect("workflows array");
    assert_eq!(workflows.len(), 1, "one workflow expected\n{stdout}");
    let wf = &workflows[0];
    assert_eq!(wf["name"], "OrderState");
    assert_eq!(wf["initial"], "Cart");
    let states = wf["states"].as_array().expect("states array");
    assert!(
        states.iter().any(|s| s == "Delivered"),
        "states should include Delivered\n{stdout}"
    );
    let transitions = wf["transitions"].as_array().expect("transitions array");
    assert!(
        transitions
            .iter()
            .any(|t| t[0] == "Cart" && t[1] == "Placed"),
        "transitions should include Cart -> Placed\n{stdout}"
    );
    assert!(
        wf["violations"]
            .as_array()
            .expect("violations array")
            .is_empty(),
        "sound workflow has no violations\n{stdout}"
    );
}

#[test]
fn diagram_mermaid_emits_state_diagram() {
    let dir = good_project();
    let output = run_workflow(dir.path(), &["diagram", "--format", "mermaid"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "diagram render should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("stateDiagram-v2"),
        "mermaid output should contain a stateDiagram-v2 block\n{stdout}"
    );
    assert!(
        stdout.contains("[*] --> Cart"),
        "initial state should be entered from [*]\n{stdout}"
    );
    assert!(
        stdout.contains("Delivered --> [*]"),
        "terminal state should exit to [*]\n{stdout}"
    );
}

#[test]
fn diagram_dot_emits_digraph() {
    let dir = good_project();
    let output = run_workflow(dir.path(), &["diagram", "--format", "dot"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "dot render should exit 0\n{stdout}"
    );
    assert!(
        stdout.contains("digraph OrderState"),
        "dot output should contain a digraph\n{stdout}"
    );
    assert!(
        stdout.contains("__start ->"),
        "dot output should mark the initial state\n{stdout}"
    );
}
