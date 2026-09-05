//! The architecture graph's completeness gate (issue #1747).
//!
//! The graph is only worth querying if nothing can quietly fall out of it. This
//! test censuses this application's *sources* for every macro that declares an
//! architectural element, runs the binary's own graph dump, and fails when the
//! two disagree.
//!
//! The census is deliberately an independent derivation. Checking the graph
//! against the same `inventory` registrations it is built from would be
//! tautological — it would pass for a build in which the graph module dropped
//! every node it was handed. Reading the attributes out of the source is the
//! only way the assertion can fail for the reason it exists.
//!
//! It also pins the recall claim: `impact Post` must return every route and job
//! that reaches the `posts` table, hand-verified against this app's sources.
//! Those handlers are listed by name, so a change that silently drops one from
//! the graph fails here rather than being discovered by someone trusting the
//! answer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use autumn_web::graph::manifest::{ArchitectureGraph, parse_manifest_dump};
use autumn_web::graph::{NodeKind, query};

/// What kind of element a source attribute declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DeclKind {
    Route,
    Model,
    Repository,
    Job,
}

/// One element found by the source census.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Declared {
    kind: DeclKind,
    /// Handler / struct / trait name.
    name: String,
    /// Module path, derived from the file's location under `src/`.
    module: String,
}

/// Attributes that declare a route handler.
///
/// `#[ws]` is deliberately absent: a WebSocket handler is not a `#[route]`, and
/// this slice models neither it nor the mechanism that mounts it. The graph
/// does not pretend otherwise — it names those mounts in
/// `completeness.unmodelled_mounted_routes`, which
/// [`ws_routes_are_named_as_unmodelled`] asserts.
const ROUTE_ATTRS: &[&str] = &[
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "head",
    "options",
    "static_get",
];

/// The crate root for the module paths the graph reports.
const CRATE: &str = "reddit_clone";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The module path a source file under `src/` contributes to.
fn module_path_of(file: &Path, src_root: &Path) -> String {
    let relative = file
        .strip_prefix(src_root)
        .expect("census only walks files under src/");
    let mut segments: Vec<String> = relative
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // `foo/mod.rs` is module `foo`; `lib.rs` and `main.rs` are the crate root.
    if segments.last().is_some_and(|s| s == "mod") {
        segments.pop();
    }
    if segments.len() == 1 && (segments[0] == "lib" || segments[0] == "main") {
        segments.clear();
    }
    std::iter::once(CRATE.to_owned())
        .chain(segments)
        .collect::<Vec<_>>()
        .join("::")
}

/// Every `.rs` file under `src/`.
fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("src/ must be readable") {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The attribute name a line opens with, when the line *is* an attribute.
///
/// Only lines whose trimmed form starts with `#[` are considered, so the
/// `"#[model]"` that `routes/about.rs` renders inside a maud template — a
/// string literal in the middle of a line — is not mistaken for a declaration.
fn attribute_name(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("#[")?;
    let rest = rest.strip_prefix("autumn_web::").unwrap_or(rest);
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The item name a declaration line introduces, for the item kinds we census.
fn item_name(line: &str, kind: DeclKind) -> Option<String> {
    let trimmed = line.trim_start();
    let keyword = match kind {
        DeclKind::Route | DeclKind::Job => "fn ",
        DeclKind::Model => "struct ",
        DeclKind::Repository => "trait ",
    };
    let after = trimmed.split_once(keyword)?.1;
    let end = after
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(after.len());
    (end > 0).then(|| after[..end].to_owned())
}

/// Census the application's sources for macro-declared elements.
fn census() -> BTreeSet<Declared> {
    let src = manifest_dir().join("src");
    let mut files = Vec::new();
    source_files(&src, &mut files);
    files.sort();

    let mut found = BTreeSet::new();
    for file in files {
        let module = module_path_of(&file, &src);
        let text = std::fs::read_to_string(&file).expect("source must be readable");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(attr) = attribute_name(line) else {
                continue;
            };
            let kind = if ROUTE_ATTRS.contains(&attr) {
                DeclKind::Route
            } else if attr == "model" {
                DeclKind::Model
            } else if attr == "repository" {
                DeclKind::Repository
            } else if matches!(attr, "job" | "scheduled" | "task") {
                DeclKind::Job
            } else {
                continue;
            };
            // Walk forward to the item the attribute stack sits on: a route
            // attribute can be followed by more attributes, a multi-line
            // argument list, and doc comments before the `fn` itself.
            let name = lines[i + 1..]
                .iter()
                .take(40)
                .find_map(|candidate| item_name(candidate, kind));
            let Some(name) = name else {
                panic!(
                    "`#[{attr}]` at {}:{} declares no item this census could name; the \
                        census has to be taught the new shape rather than silently skipping it",
                    file.display(),
                    i + 1
                );
            };
            found.insert(Declared {
                kind,
                name,
                module: module.clone(),
            });
        }
    }
    found
}

/// Build this application's architecture graph by running its own dump mode.
fn graph() -> ArchitectureGraph {
    let output = Command::new(env!("CARGO_BIN_EXE_reddit-clone"))
        .current_dir(manifest_dir())
        .env(autumn_web::graph::manifest::DUMP_ENV, "1")
        // Every one of these is dispatched BEFORE the graph dump in
        // `AppBuilder::run`, so an inherited value would silently win and hand
        // this test a marker-less stdout.
        .env_remove("AUTUMN_BUILD_STATIC")
        .env_remove("AUTUMN_DUMP_ROUTES")
        .env_remove("AUTUMN_DUMP_CACHE_COHERENCE")
        .env_remove("AUTUMN_DUMP_DATA_FLOW")
        .env_remove("AUTUMN_DUMP_AGENT_AUTHORITY")
        .output()
        .expect("the reference app binary must run");
    assert!(
        output.status.success(),
        "the graph dump exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_manifest_dump(&stdout).unwrap_or_else(|| {
        panic!("the app emitted no architecture graph. stdout was:\n{stdout}");
    })
}

/// Whether the graph holds a node for a censused declaration.
fn graph_has(graph: &ArchitectureGraph, decl: &Declared) -> bool {
    graph.nodes.iter().any(|node| {
        let kind_matches = match decl.kind {
            DeclKind::Route => matches!(node.kind, NodeKind::Route | NodeKind::StaticRoute),
            DeclKind::Model => node.kind == NodeKind::Model,
            DeclKind::Repository => node.kind == NodeKind::Repository,
            DeclKind::Job => matches!(
                node.kind,
                NodeKind::Job | NodeKind::ScheduledTask | NodeKind::OneOffTask
            ),
        };
        // Job and task names are renamed by `name = "..."`, so a job node's
        // `name` is its *registered* name, not the handler's. The node id is
        // built from the module path and the handler, which is what the census
        // sees.
        kind_matches
            && node.module == decl.module
            && (node.name == decl.name || node.id.ends_with(&format!("::{}", decl.name)))
    })
}

#[test]
fn the_census_finds_the_elements_this_app_is_known_to_declare() {
    // A guard on the guard: a census that silently stopped matching anything
    // would make every completeness assertion below vacuous.
    let found = census();
    let count = |kind: DeclKind| found.iter().filter(|d| d.kind == kind).count();
    assert_eq!(count(DeclKind::Model), 4, "{found:#?}");
    assert_eq!(count(DeclKind::Repository), 3, "{found:#?}");
    assert_eq!(count(DeclKind::Job), 4, "{found:#?}");
    assert!(
        count(DeclKind::Route) >= 39,
        "the app declares at least 39 routes: {found:#?}"
    );
}

#[test]
fn every_macro_declared_element_is_in_the_graph() {
    let graph = graph();
    let declared = census();
    let missing: Vec<&Declared> = declared
        .iter()
        .filter(|decl| !graph_has(&graph, decl))
        .collect();
    assert!(
        missing.is_empty(),
        "these macro-declared elements are absent from the architecture graph:\n{missing:#?}\n\n\
         The graph is only worth querying if nothing can fall out of it silently."
    );
}

#[test]
fn the_graph_accounts_for_every_route_it_declares() {
    let graph = graph();
    assert!(
        graph.completeness.unmounted_routes.is_empty(),
        "this app mounts every route it declares; a declared route the app stopped mounting \
         is drift, not a detail: {:?}",
        graph.completeness.unmounted_routes
    );
    assert_eq!(
        graph.completeness.declared_routes,
        graph.completeness.mounted_routes
    );
}

#[test]
fn ws_routes_are_named_as_unmodelled_rather_than_dropped() {
    // `#[ws]` handlers are served but are not `#[route]`s, so they are not
    // nodes. The document must say so rather than under-report the surface.
    let graph = graph();
    assert!(
        graph
            .completeness
            .unmodelled_mounted_routes
            .iter()
            .any(|r| r.starts_with("WS ")),
        "the WebSocket mounts must be named: {:?}",
        graph.completeness.unmodelled_mounted_routes
    );
}

/// Hand-verified ground truth: every handler in this application whose own
/// tokens reach the `posts` table, or that holds `PgPostRepository`.
///
/// Derived by reading the sources, not by reading the graph — that is the whole
/// point. `routes/posts.rs` contributes seven handlers (six holding the
/// repository plus `show_by_id`, which queries `posts::table` directly),
/// `routes/subreddits.rs::show` counts posts for a community, and both vote
/// routes hold `PgPostRepository`.
const POSTS_ROUTE_GROUND_TRUTH: &[&str] = &[
    "delete_post",
    "downvote",
    "front_page",
    "manage_tags",
    "show", // routes::posts::show
    "show", // routes::subreddits::show
    "show_by_id",
    "submit",
    "update",
    "upvote",
];

/// Hand-verified ground truth: the background work that touches `posts`.
///
/// `post_publication` reads `posts::table`; `hot-rank-calculator` reaches the
/// table only through a raw `sql_query("UPDATE posts …")` literal.
const POSTS_JOB_GROUND_TRUTH: &[&str] = &["hot-rank-calculator", "post_publication"];

#[test]
fn impact_of_the_post_model_has_total_recall() {
    let graph = graph();
    let answer = query::impact(&graph, "Post").expect("the Post model must resolve");

    let mut handlers: Vec<String> = answer
        .routes
        .iter()
        .filter(|n| n.route.as_ref().is_some_and(|r| r.generated_by.is_none()))
        .map(|n| n.name.clone())
        .collect();
    handlers.sort();
    assert_eq!(
        handlers, POSTS_ROUTE_GROUND_TRUTH,
        "impact analysis must return every hand-verified route with zero false negatives"
    );

    let mut jobs: Vec<String> = answer.jobs.iter().map(|n| n.name.clone()).collect();
    jobs.sort();
    assert_eq!(jobs, POSTS_JOB_GROUND_TRUTH);

    assert!(
        answer
            .routes
            .iter()
            .any(|n| n.route.as_ref().is_some_and(|r| r.generated_by.is_some())),
        "the `#[repository(api = ...)]` auto-API routes read and write the table too, and \
         must not be missing from an impact answer: {:?}",
        answer.routes
    );
}

#[test]
fn a_route_reaching_the_votes_table_only_through_a_generated_relation_is_reported() {
    // `#[votable(by = User)]` on `Post` puts `react` on `PgPostRepository`.
    // `routes::votes::upvote` never names `votes` or `Vote`, and reaches the
    // edge table entirely through that generated method.
    let graph = graph();
    let answer = query::touches(&graph, "votes").expect("the votes table must resolve");
    let handlers: Vec<&str> = answer.routes.iter().map(|n| n.name.as_str()).collect();
    assert!(handlers.contains(&"upvote"), "{handlers:?}");
    assert!(handlers.contains(&"downvote"), "{handlers:?}");
}

#[test]
fn every_route_node_carries_its_declared_auth_requirement() {
    let graph = graph();
    for node in graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Route | NodeKind::StaticRoute))
    {
        let facts = node.route.as_ref().expect("a route node has route facts");
        assert!(
            facts.auth.is_some(),
            "every mounted route states its auth requirement, even when it declares none: {node:?}"
        );
    }
    assert!(
        graph.nodes.iter().any(|n| n
            .route
            .as_ref()
            .and_then(|r| r.auth.as_ref())
            .is_some_and(|a| a.secured)),
        "this app has `#[secured]` routes; if none is reported, the posture is not being read"
    );
}
