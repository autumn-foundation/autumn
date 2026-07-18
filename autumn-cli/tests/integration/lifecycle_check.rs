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

#[test]
fn alias_in_one_module_does_not_leak_to_sibling() {
    // Regression: a `use autumn_web::lifecycle as lc;` inside `mod a` must be
    // scoped to `mod a` only. In `mod b`, `lc` is a *different* macro (never
    // aliased to autumn's lifecycle here), and its arguments are not a valid
    // lifecycle grammar. If the alias leaked file-wide, the scanner would misparse
    // `mod b`'s `#[lc(...)]` as a lifecycle and emit a malformed-lifecycle error,
    // failing the check. With per-module scoping it is skipped, so `Alpha` (sound)
    // is the only lifecycle scanned and the check passes cleanly.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/lib.rs",
        r#"
mod a {
    use autumn_web::lifecycle as lc;

    #[lc(
        initial = Draft,
        terminal(Done),
        transitions(
            Draft -> Done,
        )
    )]
    pub enum Alpha {
        Draft,
        Done,
    }
}

mod b {
    // `lc` here is NOT autumn's lifecycle: no `use ... as lc` in this module. Its
    // arguments would be a malformed lifecycle if the sibling alias leaked in.
    #[lc(role = "admin", audit)]
    pub enum Beta {
        Active,
        Inactive,
    }
}
"#,
    );
    let output = run_lifecycle(dir.path(), &["check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a sibling module's unrelated `#[lc]` must not be misread as a lifecycle\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Alpha"),
        "the genuinely-aliased lifecycle in module a should be scanned\n{stdout}"
    );
    assert!(
        !stdout.contains("Beta"),
        "module b's `#[lc]` is not an autumn lifecycle and must be skipped\n{stdout}"
    );
    assert!(
        !stdout.contains("malformed"),
        "no malformed-lifecycle error should be attributed to the leaked alias\n{stdout}"
    );
    assert!(stdout.contains("PASS"), "stdout:\n{stdout}");
}

#[test]
fn dot_diagram_with_multiple_lifecycles_is_single_digraph() {
    // With more than one lifecycle, `--format dot` must emit ONE `digraph`
    // containing a `subgraph cluster_<name>` per lifecycle — not several
    // back-to-back `digraph {}` blocks (invalid DOT: one graph per document).
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/order.rs",
        r"
#[lifecycle(
    initial = Cart,
    terminal(Delivered),
    transitions(
        Cart -> Delivered,
    )
)]
pub enum OrderState {
    Cart,
    Delivered,
}
",
    );
    write(
        dir.path(),
        "src/post.rs",
        r"
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
}
",
    );
    let output = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "dot render of multiple lifecycles should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.matches("digraph").count(),
        1,
        "multiple lifecycles must be one single digraph document\n{stdout}"
    );
    assert_eq!(
        stdout.matches("subgraph cluster_").count(),
        2,
        "each lifecycle should be its own cluster subgraph\n{stdout}"
    );
    assert!(
        stdout.contains("cluster_OrderState") && stdout.contains("cluster_PostState"),
        "both lifecycles should be present as named clusters\n{stdout}"
    );
    assert_eq!(
        stdout.matches('{').count(),
        stdout.matches('}').count(),
        "the DOT document must have balanced braces\n{stdout}"
    );
}

#[test]
fn dot_diagram_same_named_enums_in_separate_modules_do_not_collide() {
    // Two lifecycle enums with the SAME identifier (`State`) declared in two
    // separate modules must be namespaced by their module-qualified id, so their
    // clusters and nodes stay distinct instead of merging under a shared
    // `cluster_State`.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/domain.rs",
        r"
mod orders {
    #[lifecycle(
        initial = Open,
        terminal(Closed),
        transitions(
            Open -> Closed,
        )
    )]
    pub enum State {
        Open,
        Closed,
    }
}

mod invoices {
    #[lifecycle(
        initial = Draft,
        terminal(Paid),
        transitions(
            Draft -> Paid,
        )
    )]
    pub enum State {
        Draft,
        Paid,
    }
}
",
    );
    let output = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "dot render of same-named lifecycles should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.matches("digraph").count(),
        1,
        "multiple lifecycles must be one single digraph document\n{stdout}"
    );
    assert_eq!(
        stdout.matches("subgraph cluster_").count(),
        2,
        "each lifecycle should be its own cluster subgraph\n{stdout}"
    );
    // The module-qualified id `orders::State` sanitizes to `orders__State`
    // (`::` -> `__`), so the two clusters get DISTINCT ids and never collapse
    // into a shared `cluster_State`.
    assert!(
        stdout.contains("cluster_orders__State"),
        "the orders lifecycle should be namespaced by its module\n{stdout}"
    );
    assert!(
        stdout.contains("cluster_invoices__State"),
        "the invoices lifecycle should be namespaced by its module\n{stdout}"
    );
    // Their state nodes are namespaced apart, so no id is shared between them.
    assert!(
        stdout.contains("\"orders__State.Open\"") && stdout.contains("\"invoices__State.Draft\""),
        "state nodes must be namespaced by module-qualified id\n{stdout}"
    );
}

/// A `#[lifecycle]` enum whose diagram edges carry transition effects declared
/// on a `#[state_machine(lifecycle = OrderState, effects(...))]` binding on a
/// separate `#[model]` struct field (issue #1973). The fixture only needs to
/// parse — it is scanned, not compiled.
const EFFECTS_FIXTURE: &str = r#"
use autumn_web::lifecycle;

#[lifecycle(
    initial = Draft,
    terminal(Archived),
    transitions(
        Draft -> Published,
        Published -> Archived,
    )
)]
pub enum OrderState {
    Draft,
    Published,
    Archived,
}

#[model]
pub struct Article {
    #[state_machine(
        lifecycle = OrderState,
        effects(
            Draft -> Published: on_commit = AnnouncePublishJob,
            Published -> Archived: on = "write_archive_audit",
        )
    )]
    pub status: String,
}
"#;

#[test]
fn diagram_mermaid_shows_effect_labels() {
    // The Mermaid diagram annotates each edge that has a correlated
    // `#[state_machine(... effects(...))]` binding with its effect label
    // (`on: ...` / `on_commit: ...`), leaving effect-free edges unchanged.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/article.rs", EFFECTS_FIXTURE);
    let output = run_lifecycle(dir.path(), &["diagram", "--format", "mermaid"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "diagram render should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Draft --> Published"),
        "the effect edge should still render its transition\n{stdout}"
    );
    assert!(
        stdout.contains("on_commit: AnnouncePublishJob"),
        "the on_commit effect should be annotated on its edge\n{stdout}"
    );
    assert!(
        stdout.contains("on: write_archive_audit"),
        "the on effect should be annotated on its edge\n{stdout}"
    );
}

#[test]
fn diagram_dot_shows_effect_labels() {
    // The DOT diagram annotates effect edges with a `[label="..."]` carrying the
    // same effect text.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/article.rs", EFFECTS_FIXTURE);
    let output = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "dot render should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("[label=\"on_commit: AnnouncePublishJob\"]"),
        "the on_commit effect should be a DOT edge label\n{stdout}"
    );
    assert!(
        stdout.contains("[label=\"on: write_archive_audit\"]"),
        "the on effect should be a DOT edge label\n{stdout}"
    );
}

/// Two `#[lifecycle]` enums with the SAME identifier (`State`) and identical
/// edges declared in two separate modules, where ONLY the `orders` module also
/// carries a `#[state_machine(lifecycle = State, effects(...))]` binding. The
/// effect label must be correlated by the module-qualified `LifecycleDef::id`
/// (`orders::State`), so it annotates the `orders` diagram only and never bleeds
/// onto the unrelated same-named `invoices::State` (issue #2029 Codex P2).
const SAME_NAMED_EFFECTS_FIXTURE: &str = r"
mod orders {
    #[lifecycle(
        initial = Draft,
        terminal(Published),
        transitions(
            Draft -> Published,
        )
    )]
    pub enum State {
        Draft,
        Published,
    }

    #[model]
    pub struct Order {
        #[state_machine(
            lifecycle = State,
            effects(
                Draft -> Published: on_commit = SomeJob,
            )
        )]
        pub status: String,
    }
}

mod invoices {
    #[lifecycle(
        initial = Draft,
        terminal(Published),
        transitions(
            Draft -> Published,
        )
    )]
    pub enum State {
        Draft,
        Published,
    }
}
";

#[test]
fn effect_labels_do_not_bleed_across_same_named_enums_in_separate_modules() {
    // The `orders` binding must label only the `orders::State` diagram; the
    // same-named `invoices::State` (no binding) must render its `Draft ->
    // Published` edge with no effect label. Step 1 (qualified match) pins the
    // binding to its co-located `orders::State`; the bare name `State` is
    // ambiguous (two same-named enums), so the step-2 fallback never fires and
    // cannot bleed the label onto `invoices::State`.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/domain.rs", SAME_NAMED_EFFECTS_FIXTURE);

    // DOT: node ids are namespaced by module-qualified id, so the two clusters'
    // edges are distinguishable and the label is pinned to exactly one edge.
    let dot = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
    let dstdout = String::from_utf8_lossy(&dot.stdout);
    assert!(
        dot.status.success(),
        "dot render should exit 0\nstdout:\n{dstdout}\nstderr:\n{}",
        String::from_utf8_lossy(&dot.stderr)
    );
    assert_eq!(
        dstdout.matches("on_commit: SomeJob").count(),
        1,
        "the effect label must appear exactly once (only on the enum with the binding)\n{dstdout}"
    );
    assert!(
        dstdout.contains(
            "\"orders__State.Draft\" -> \"orders__State.Published\" [label=\"on_commit: SomeJob\"];"
        ),
        "the effect label must annotate the orders::State edge that has the binding\n{dstdout}"
    );
    assert!(
        dstdout.contains("\"invoices__State.Draft\" -> \"invoices__State.Published\";"),
        "the unrelated invoices::State edge must render with no effect label\n{dstdout}"
    );

    // Mermaid: the composite document must carry the label exactly once, too.
    let mermaid = run_lifecycle(dir.path(), &["diagram", "--format", "mermaid"]);
    let mstdout = String::from_utf8_lossy(&mermaid.stdout);
    assert!(
        mermaid.status.success(),
        "mermaid render should exit 0\nstdout:\n{mstdout}\nstderr:\n{}",
        String::from_utf8_lossy(&mermaid.stderr)
    );
    assert_eq!(
        mstdout.matches("on_commit: SomeJob").count(),
        1,
        "the effect label must appear exactly once in the mermaid document\n{mstdout}"
    );
}

/// A `#[lifecycle]` enum declared in one module (`states`) and *imported* into
/// another (`models`) that binds it with a bare `#[state_machine(lifecycle =
/// OrderState, effects(...))]`. Rust resolves the bare `OrderState` to
/// `states::OrderState` via the `use`, but the binding's own module prefix would
/// mis-qualify it to `models::OrderState` (no such lifecycle). The unique-bare-name
/// fallback must correlate the effect to the single scanned `OrderState` diagram
/// (issue #2029 Codex P2 — the imported-lifecycle case).
const IMPORTED_LIFECYCLE_EFFECTS_FIXTURE: &str = r"
mod states {
    #[lifecycle(
        initial = Draft,
        terminal(Published),
        transitions(
            Draft -> Published,
        )
    )]
    pub enum OrderState {
        Draft,
        Published,
    }
}

mod models {
    use crate::states::OrderState;

    #[model]
    pub struct Order {
        #[state_machine(
            lifecycle = OrderState,
            effects(
                Draft -> Published: on_commit = SomeJob,
            )
        )]
        pub status: String,
    }
}
";

#[test]
fn effect_labels_apply_to_imported_lifecycle_via_unique_bare_name() {
    // The binding's qualified id (`models::OrderState`) is not a scanned
    // lifecycle, so correlation falls back to the globally-unique bare enum name
    // `OrderState` and labels the `states::OrderState` diagram.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "src/domain.rs",
        IMPORTED_LIFECYCLE_EFFECTS_FIXTURE,
    );

    let mermaid = run_lifecycle(dir.path(), &["diagram", "--format", "mermaid"]);
    let mstdout = String::from_utf8_lossy(&mermaid.stdout);
    assert!(
        mermaid.status.success(),
        "mermaid render should exit 0\nstdout:\n{mstdout}\nstderr:\n{}",
        String::from_utf8_lossy(&mermaid.stderr)
    );
    assert_eq!(
        mstdout.matches("on_commit: SomeJob").count(),
        1,
        "the imported-lifecycle effect must be correlated exactly once\n{mstdout}"
    );

    let dot = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
    let dstdout = String::from_utf8_lossy(&dot.stdout);
    assert!(
        dot.status.success(),
        "dot render should exit 0\nstdout:\n{dstdout}\nstderr:\n{}",
        String::from_utf8_lossy(&dot.stderr)
    );
    assert!(
        dstdout.contains("[label=\"on_commit: SomeJob\"]"),
        "the imported-lifecycle effect should be a DOT edge label\n{dstdout}"
    );
}

/// Two `#[lifecycle]` enums that SHARE the bare name `OrderState` in different
/// modules, plus an imported-style bare `#[state_machine(lifecycle = OrderState,
/// effects(...))]` binding in a third module. The binding's qualified id does not
/// name a scanned lifecycle *and* the bare name is ambiguous (two candidates), so
/// the unique-bare-name fallback must NOT fire — the effect is dropped rather than
/// guessed onto an arbitrary same-named enum (issue #2029 Codex P2 — safe failure).
const AMBIGUOUS_IMPORTED_FIXTURE: &str = r"
mod states {
    #[lifecycle(
        initial = Draft,
        terminal(Published),
        transitions(
            Draft -> Published,
        )
    )]
    pub enum OrderState {
        Draft,
        Published,
    }
}

mod other {
    #[lifecycle(
        initial = Draft,
        terminal(Published),
        transitions(
            Draft -> Published,
        )
    )]
    pub enum OrderState {
        Draft,
        Published,
    }
}

mod models {
    #[model]
    pub struct Order {
        #[state_machine(
            lifecycle = OrderState,
            effects(
                Draft -> Published: on_commit = SomeJob,
            )
        )]
        pub status: String,
    }
}
";

#[test]
fn effect_labels_dropped_when_bare_name_is_ambiguous_across_modules() {
    // An ambiguous bare name (two same-named enums) means the fallback cannot
    // pick a lifecycle, so the effect must be dropped — no diagram gets the label.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/domain.rs", AMBIGUOUS_IMPORTED_FIXTURE);

    for format in ["mermaid", "dot"] {
        let output = run_lifecycle(dir.path(), &["diagram", "--format", format]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{format} render should exit 0\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            stdout.matches("on_commit: SomeJob").count(),
            0,
            "an ambiguous bare-name binding must never label a same-named enum ({format})\n{stdout}"
        );
    }
}

/// A plain `#[lifecycle]` enum with no `#[state_machine]` effects binding — its
/// diagram edges must be byte-identical to the pre-#1973 output (no labels).
const PLAIN_LIFECYCLE: &str = r"
#[lifecycle(
    initial = Draft,
    terminal(Archived),
    transitions(
        Draft -> Published,
        Published -> Archived,
    )
)]
pub enum OrderState {
    Draft,
    Published,
    Archived,
}
";

#[test]
fn diagram_without_effects_has_no_edge_labels() {
    // With no effects binding, an edge renders with no ` : ` label (Mermaid) and
    // no `[label=` (DOT) — the byte-identical guarantee.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/order.rs", PLAIN_LIFECYCLE);

    let mermaid = run_lifecycle(dir.path(), &["diagram", "--format", "mermaid"]);
    let mstdout = String::from_utf8_lossy(&mermaid.stdout);
    assert!(
        mermaid.status.success(),
        "mermaid render should exit 0\n{mstdout}"
    );
    assert!(
        mstdout.contains("Draft --> Published"),
        "the transition edge should render\n{mstdout}"
    );
    assert!(
        !mstdout.contains(" : "),
        "an effect-free lifecycle must render no edge labels\n{mstdout}"
    );

    let dot = run_lifecycle(dir.path(), &["diagram", "--format", "dot"]);
    let dstdout = String::from_utf8_lossy(&dot.stdout);
    assert!(dot.status.success(), "dot render should exit 0\n{dstdout}");
    assert!(
        !dstdout.contains("[label="),
        "an effect-free single-lifecycle DOT diagram must render no edge labels\n{dstdout}"
    );
}
