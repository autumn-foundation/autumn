//! Integration tests for `autumn lifecycle check` / `autumn lifecycle diagram`
//! (issue #1675).
//!
//! Each test scaffolds a throwaway project with a `src/*.rs` file containing a
//! `#[lifecycle]` enum in a `TempDir`, runs the real `autumn` binary against it,
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

/// Run `autumn lifecycle <sub> [args...]` inside `root`.
fn run_lifecycle(root: &Path, args: &[&str]) -> Output {
    Command::new(autumn_bin())
        .arg("lifecycle")
        .args(args)
        .current_dir(root)
        .output()
        .expect("failed to run autumn lifecycle")
}

/// A sound order-lifecycle lifecycle: every state reachable from `Cart`, and
/// every non-terminal state can reach a terminal (`Delivered`/`Cancelled`).
const GOOD_ORDER: &str = r"
use autumn_web::lifecycle;

#[lifecycle(
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
";

fn good_project() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/order.rs", GOOD_ORDER);
    dir
}

#[test]
fn sound_lifecycle_exits_zero() {
    let dir = good_project();
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a sound lifecycle should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
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
        r"
#[lifecycle(
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
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
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
        r"
#[lifecycle(
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
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
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
        r"
#[lifecycle(
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
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
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
fn terminal_source_transition_fails_and_is_named() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r"
#[lifecycle(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> Shipped,
        Shipped -> Delivered,
        Delivered -> Reopened,
    )
)]
pub enum OrderState {
    Pending,
    Shipped,
    Delivered,
    Reopened,
}
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a transition out of a terminal state must exit non-zero\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("terminal state 'Delivered'"),
        "should name the offending terminal state\n{stdout}"
    );
    assert!(
        stdout.contains("terminal-source"),
        "should tag the terminal-source violation\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn qualified_lifecycle_attribute_is_detected() {
    // A qualified `#[autumn_web::lifecycle(...)]` invocation (valid Rust) must be
    // recognized by the scanner; the enum below is unsound (Reopened is an
    // unreachable dead-end), so detection means a non-zero exit that names it.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r"
#[autumn_web::lifecycle(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> Shipped,
        Shipped -> Delivered,
    )
)]
pub enum OrderState {
    Pending,
    Shipped,
    Delivered,
    Reopened,
}
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a qualified #[autumn_web::lifecycle] must be detected and flagged\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("OrderState"),
        "should scan the qualified-attribute lifecycle\n{stdout}"
    );
    assert!(
        stdout.contains("Reopened"),
        "should name the unsound state in the qualified lifecycle\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn aliased_lifecycle_attribute_is_detected() {
    // An aliased `use autumn_web::lifecycle as lc; #[lc(...)]` invocation (valid
    // Rust) must be recognized by the scanner via same-file alias tracking; the
    // enum below is unsound (Reopened is an unreachable dead-end), so detection
    // means a non-zero exit that names it. Skipping the aliased enum would be a
    // silent false PASS.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r"
use autumn_web::lifecycle as lc;

#[lc(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> Shipped,
        Shipped -> Delivered,
    )
)]
pub enum OrderState {
    Pending,
    Shipped,
    Delivered,
    Reopened,
}
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "an aliased #[lc] lifecycle must be detected and flagged\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("OrderState"),
        "should scan the aliased-attribute lifecycle\n{stdout}"
    );
    assert!(
        stdout.contains("Reopened"),
        "should name the unsound state in the aliased lifecycle\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn aliased_sound_lifecycle_exits_zero() {
    // The alias path must not spuriously flag a sound lifecycle: an aliased but
    // sound enum passes cleanly, proving detection is precise, not a blanket fail.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r"
use autumn_web::lifecycle as lc;

#[lc(
    initial = Pending,
    terminal(Delivered),
    transitions(
        Pending -> Shipped,
        Shipped -> Delivered,
    )
)]
pub enum OrderState {
    Pending,
    Shipped,
    Delivered,
}
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "an aliased sound lifecycle should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("PASS"), "stdout:\n{stdout}");
    assert!(stdout.contains("OrderState"), "stdout:\n{stdout}");
    assert!(stdout.contains("sound"), "stdout:\n{stdout}");
}

#[test]
fn lifecycle_nested_in_inline_module_is_detected() {
    // A `#[lifecycle]` enum nested inside `mod orders { ... }` (and one nested a
    // level deeper) must be walked recursively; both are unsound, so detection
    // means a non-zero exit naming the offending states.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r"
mod orders {
    #[lifecycle(
        initial = Pending,
        terminal(Delivered),
        transitions(
            Pending -> Shipped,
            Shipped -> Delivered,
        )
    )]
    pub enum OrderState {
        Pending,
        Shipped,
        Delivered,
        Orphan,
    }

    mod inner {
        #[lifecycle(
            initial = Draft,
            terminal(Published),
            transitions(
                Draft -> Published,
            )
        )]
        pub enum PostState {
            Draft,
            Published,
            Ghost,
        }
    }
}
",
    );
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "a lifecycle nested in an inline module must be detected\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("OrderState"),
        "should scan the module-nested lifecycle\n{stdout}"
    );
    assert!(
        stdout.contains("Orphan"),
        "should name the unsound state in the module-nested lifecycle\n{stdout}"
    );
    assert!(
        stdout.contains("PostState") && stdout.contains("Ghost"),
        "should also scan the lifecycle nested one module deeper\n{stdout}"
    );
    assert!(stdout.contains("FAIL"), "stdout:\n{stdout}");
}

#[test]
fn json_format_on_good_lifecycle_is_valid_and_exits_zero() {
    let dir = good_project();
    let output = run_lifecycle(dir.path(), &["check", "--format", "json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "sound lifecycle exits 0 in json mode\nstdout:\n{stdout}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"));
    let lifecycles = json["lifecycles"].as_array().expect("lifecycles array");
    assert_eq!(lifecycles.len(), 1, "one lifecycle expected\n{stdout}");
    let wf = &lifecycles[0];
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
        "sound lifecycle has no violations\n{stdout}"
    );
}

#[test]
fn diagram_mermaid_emits_state_diagram() {
    let dir = good_project();
    let output = run_lifecycle(dir.path(), &["diagram", "--format", "mermaid"]);
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
    let output = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
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
