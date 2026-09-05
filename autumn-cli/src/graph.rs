//! `autumn graph` — query the application's architecture graph (issue #1747).
//!
//! Runs the app's own binary in architecture-graph dump mode
//! (`AUTUMN_DUMP_GRAPH=1`), reads back the graph the framework assembles from
//! every `#[route]`, `#[static_get]`, `#[model]`, `#[repository]`, `#[job]`,
//! `#[scheduled]` and `#[task]` in the binary, and answers structural questions
//! against it.
//!
//! Why run the binary rather than parse the sources: what an application
//! *contains* is a whole-binary, feature-resolved fact. A model declared in one
//! crate can be written by a route declared in another, or in a plugin the app
//! merely depends on, and link-time `inventory` collection is the only place
//! all of those registrations exist together. Same shape as `autumn data-flow`
//! (#1654), `autumn agents manifest` (#1691) and `autumn routes audit` (#1604).
//!
//! Three verbs:
//!
//! * `autumn graph show` — the whole graph, as a report or as JSON;
//! * `autumn graph touches <NAME>` — which routes and jobs reach a model, table
//!   or repository;
//! * `autumn graph impact <NAME>` — the transitive set a change to it would
//!   affect, repositories included.
//!
//! `--check` compares against a committed copy and fails on drift, which is
//! what makes a route quietly losing its access to a table — or a declared
//! element vanishing from the graph — something a reviewer must approve.
//!
//! See `docs/guide/architecture-graph.md`.

use std::process::Command;

use autumn_web::graph::manifest::{ArchitectureGraph, Completeness, parse_manifest_dump};
use autumn_web::graph::query;

use crate::routes;

/// The env var selecting the app binary's architecture-graph dump mode.
///
/// Re-exported from the framework rather than spelled again here: the CLI sets
/// this string and `AppBuilder::run` reads it, so two independent literals
/// would be a protocol that could silently drift apart at a typo.
pub const DUMP_ENV: &str = autumn_web::graph::manifest::DUMP_ENV;

/// Which question `autumn graph` is being asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// The whole graph.
    Show,
    /// Which routes and jobs reach the named element.
    Touches(String),
    /// The transitive set a change to the named element would affect.
    Impact(String),
}

/// Options controlling `autumn graph`.
pub struct GraphOptions<'a> {
    /// The question being asked.
    pub query: Query,
    /// Cargo package to build and run.
    pub package: Option<&'a str>,
    /// Binary target name for packages that expose multiple bin targets.
    pub bin: Option<&'a str>,
    /// Write the JSON graph to this path (in addition to any stdout).
    pub manifest: Option<&'a str>,
    /// Emit JSON to stdout instead of the human report, for every verb.
    pub json: bool,
    /// Compare against a committed graph and fail on drift.
    pub check: Option<&'a str>,
    /// Cargo feature selection the inspected binary is built under.
    ///
    /// The graph describes the binary that produced it. A model, route or job
    /// behind a non-default feature is simply not compiled in, so it cannot
    /// appear in the graph.
    pub features: routes::CargoFeatures,
    /// Build and inspect the release binary rather than the debug one.
    pub release: bool,
}

/// Render the human report for a graph.
///
/// The graph's own summary: the CLI has no second opinion about what the
/// document says, and two renderings that could disagree is exactly the drift
/// these commands exist to catch.
#[must_use]
pub fn format_report(graph: &ArchitectureGraph) -> String {
    graph.summary()
}

/// The identity of a node, for the drift report.
fn node_key(node: &autumn_web::graph::manifest::GraphNode) -> &str {
    &node.id
}

/// A copy of the graph with every `file:line` blanked.
///
/// Locations are worth having in the document — they point a reader at the
/// declaration — but they are not worth *gating* on: adding a blank line above
/// a handler would fail `--check` with a diff the report cannot explain, and a
/// gate that cries wolf on every cosmetic line shift gets regenerated
/// reflexively, which is exactly how a `#[secured]` → `#[public]` flip rides
/// along unreviewed.
fn without_locations(graph: &ArchitectureGraph) -> ArchitectureGraph {
    let mut copy = graph.clone();
    for node in &mut copy.nodes {
        node.location = String::new();
    }
    copy
}

/// Name each completeness field that moved.
///
/// Per-field, because a new opaque or unmodelled served surface is *exactly*
/// how this section changes, and reporting only the declared/mounted counts
/// printed an apparently no-op transition (`39 / 39 -> 39 / 39`) for the one
/// case a reviewer most needs to see (Codex round 4).
fn completeness_changes(committed: &Completeness, current: &Completeness) -> Vec<String> {
    let mut lines = Vec::new();
    let mut count = |label: &str, before: usize, after: usize| {
        if before != after {
            lines.push(format!("  completeness {label} {before} -> {after}"));
        }
    };
    count(
        "declared routes",
        committed.declared_routes,
        current.declared_routes,
    );
    count(
        "mounted routes",
        committed.mounted_routes,
        current.mounted_routes,
    );
    count("models", committed.models, current.models);
    count("repositories", committed.repositories, current.repositories);
    count("jobs", committed.jobs, current.jobs);
    count(
        "repository auto-API routes",
        committed.generated_routes,
        current.generated_routes,
    );
    count(
        "unenumerable mounted routers",
        committed.opaque_mounted_routers,
        current.opaque_mounted_routers,
    );
    let mut list = |label: &str, before: &[String], after: &[String]| {
        for entry in after {
            if !before.contains(entry) {
                lines.push(format!("  + {label} {entry}"));
            }
        }
        for entry in before {
            if !after.contains(entry) {
                lines.push(format!("  - {label} {entry}"));
            }
        }
    };
    list(
        "route declared but not mounted",
        &committed.unmounted_routes,
        &current.unmounted_routes,
    );
    list(
        "mounted with no macro declaration",
        &committed.unmodelled_mounted_routes,
        &current.unmodelled_mounted_routes,
    );
    lines
}

/// The symbol that resolved an edge, or `declared` for a declaration edge.
fn symbol_or_declared(symbol: &str) -> &str {
    if symbol.is_empty() {
        "declared"
    } else {
        symbol
    }
}

/// Describe the difference between a committed graph and a fresh one.
///
/// Returns `None` when they agree. The report names *which* elements and edges
/// moved, because "the graph changed" is not reviewable but "`front_page` lost
/// its read of `posts`" is the one line a reviewer needs.
#[must_use]
pub fn format_drift(committed: &ArchitectureGraph, current: &ArchitectureGraph) -> Option<String> {
    let (committed, current) = (&without_locations(committed), &without_locations(current));
    if committed == current {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();

    if committed.schema_version != current.schema_version {
        lines.push(format!(
            "  graph schema version {} -> {}",
            committed.schema_version, current.schema_version
        ));
    }

    for node in &current.nodes {
        match committed
            .nodes
            .iter()
            .find(|c| node_key(c) == node_key(node))
        {
            None => lines.push(format!("  + {} {}", node.kind, node.label())),
            Some(before) if before != node => {
                lines.push(format!("  ~ {} {}", node.kind, node.label()));
                if before.label() != node.label() {
                    lines.push(format!("      {} -> {}", before.label(), node.label()));
                }
                let (before_auth, after_auth) = (
                    before.route.as_ref().and_then(|r| r.auth.as_ref()),
                    node.route.as_ref().and_then(|r| r.auth.as_ref()),
                );
                if before_auth != after_auth {
                    lines.push(format!(
                        "      auth {} -> {}",
                        before_auth.map_or_else(|| "unmounted".to_owned(), |a| a.label()),
                        after_auth.map_or_else(|| "unmounted".to_owned(), |a| a.label()),
                    ));
                }
            }
            Some(_) => {}
        }
    }
    for node in &committed.nodes {
        if !current.nodes.iter().any(|c| node_key(c) == node_key(node)) {
            lines.push(format!("  - {} {}", node.kind, node.label()));
        }
    }

    for edge in &current.edges {
        if !committed
            .edges
            .iter()
            .any(|c| c.from == edge.from && c.to == edge.to)
        {
            lines.push(format!("  + {} {} -> {}", edge.access, edge.from, edge.to));
        }
    }
    for edge in &committed.edges {
        match current
            .edges
            .iter()
            .find(|c| c.from == edge.from && c.to == edge.to)
        {
            None => lines.push(format!("  - {} {} -> {}", edge.access, edge.from, edge.to)),
            Some(after) if after.access != edge.access => lines.push(format!(
                "  ~ {} -> {}: {} -> {}",
                edge.from, edge.to, edge.access, after.access
            )),
            // A route that stopped taking the repository as an extractor and
            // now only names it in its body keeps the edge but weakens the
            // evidence for it; one that reaches the same target through a
            // different name has swapped its resolving symbol. Both are changes
            // the report has in hand, and neither should hide behind "the
            // documents differ in a field this report does not name".
            Some(after) if after.provenance != edge.provenance => lines.push(format!(
                "  ~ {} -> {}: evidence {} ({}) -> {} ({})",
                edge.from,
                edge.to,
                edge.provenance,
                symbol_or_declared(&edge.symbol),
                after.provenance,
                symbol_or_declared(&after.symbol),
            )),
            Some(after) if after.symbol != edge.symbol => lines.push(format!(
                "  ~ {} -> {}: resolved by {} -> {}",
                edge.from,
                edge.to,
                symbol_or_declared(&edge.symbol),
                symbol_or_declared(&after.symbol),
            )),
            Some(_) => {}
        }
    }

    lines.extend(completeness_changes(
        &committed.completeness,
        &current.completeness,
    ));

    if lines.is_empty() {
        lines.push("  (the documents differ in a field this report does not name)".to_string());
    }
    Some(format!(
        "\u{2717} The architecture graph has drifted from the committed copy:\n{}",
        lines.join("\n")
    ))
}

/// Answer the query against the graph, or `None` when the name resolves to
/// nothing.
#[must_use]
pub fn answer(graph: &ArchitectureGraph, query: &Query) -> Option<String> {
    match query {
        Query::Show => Some(format_report(graph)),
        Query::Touches(name) => query::touches(graph, name)
            .as_ref()
            .map(query::format_touches),
        Query::Impact(name) => query::impact(graph, name)
            .as_ref()
            .map(query::format_impact),
    }
}

/// Answer the query as JSON, so `--json` means the same thing for every verb.
///
/// A flag the command accepts and silently ignores is worse than one it
/// rejects: a script piping `autumn graph impact Post --json` into `jq` would
/// otherwise get a human report and no error.
#[must_use]
pub fn answer_json(graph: &ArchitectureGraph, query: &Query) -> Option<String> {
    let ids = |nodes: &[&autumn_web::graph::manifest::GraphNode]| -> Vec<String> {
        nodes.iter().map(|n| n.id.clone()).collect()
    };
    let value = match query {
        Query::Show => serde_json::to_value(graph).ok()?,
        Query::Touches(name) => {
            let a = query::touches(graph, name)?;
            serde_json::json!({
                "target": a.target,
                "routes": ids(&a.routes),
                "jobs": ids(&a.jobs),
            })
        }
        Query::Impact(name) => {
            let a = query::impact(graph, name)?;
            serde_json::json!({
                "target": a.target,
                "repositories": ids(&a.repositories),
                "routes": ids(&a.routes),
                "jobs": ids(&a.jobs),
            })
        }
    };
    serde_json::to_string_pretty(&value).ok()
}

/// Explain a name nothing answers to, and exit non-zero.
fn report_unresolved(graph: &ArchitectureGraph, query: &Query) -> ! {
    let name = match query {
        Query::Show => unreachable!("`show` always resolves"),
        Query::Touches(name) | Query::Impact(name) => name,
    };
    eprintln!("\u{2717} Nothing in this application answers to {name:?}.");
    let names = queryable_names(graph);
    if names.is_empty() {
        eprintln!(
            "This binary declares no `#[model]` or `#[repository]`, so there is nothing to \
             query yet."
        );
    } else {
        eprintln!(
            "Known models, tables and repositories: {}",
            names.join(", ")
        );
    }
    std::process::exit(1);
}

/// The names a failed query could have meant, for the "did you mean" line.
#[must_use]
pub fn queryable_names(graph: &ArchitectureGraph) -> Vec<String> {
    let mut names: Vec<String> = graph
        .nodes
        .iter()
        .filter(|n| n.model.is_some() || n.repository.is_some())
        .flat_map(|n| {
            let mut out = vec![n.name.clone()];
            if let Some(model) = &n.model {
                out.push(model.table.clone());
            }
            out
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Write the graph to a file, creating parent directories as needed.
fn write_manifest(graph: &ArchitectureGraph, path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", graph.to_json()))
}

/// Run `autumn graph`.
pub fn run(opts: &GraphOptions<'_>) {
    eprintln!("\u{1F342} autumn graph\n");
    if !opts.features.is_default() {
        eprintln!("Building with {}\n", opts.features.to_args().join(" "));
    }
    if opts.release {
        eprintln!("Building the release profile\n");
    }
    routes::compile_binary_with_profile(opts.package, opts.bin, &opts.features, opts.release);
    let binary = routes::find_binary_in_profile(opts.package, opts.bin, opts.release);

    let output = Command::new(&binary)
        .env(DUMP_ENV, "1")
        // Every one of these is checked BEFORE the graph dump in
        // `AppBuilder::run`, so an exported one in the ambient environment
        // would silently win and hand us a marker-less stdout.
        .env_remove("AUTUMN_BUILD_STATIC")
        .env_remove("AUTUMN_DUMP_ROUTES")
        .env_remove("AUTUMN_DUMP_CACHE_COHERENCE")
        .env_remove("AUTUMN_DUMP_DATA_FLOW")
        .env_remove("AUTUMN_DUMP_AGENT_AUTHORITY")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        eprintln!(
            "\u{2717} Binary exited with status {} while dumping the architecture graph",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(graph) = parse_manifest_dump(&stdout) else {
        eprintln!(
            "\u{2717} The app produced no architecture graph. Either it was built against an \
             autumn-web without `autumn graph` support, or it took a different startup path \
             first \u{2014} `AUTUMN_BUILD_STATIC`, `AUTUMN_DUMP_ROUTES`, \
             `AUTUMN_DUMP_CACHE_COHERENCE`, `AUTUMN_DUMP_DATA_FLOW` and \
             `AUTUMN_DUMP_AGENT_AUTHORITY` are all handled before the graph dump and are \
             cleared for this run, so an app that exits earlier for its own reasons will land \
             here too."
        );
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    };

    // Read the committed copy BEFORE writing anything. `--manifest P --check P`
    // would otherwise compare the fresh graph against the copy this very run
    // just wrote and always pass -- a gate that certifies itself.
    let committed = opts.check.map(|path| match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<ArchitectureGraph>(&text) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("\u{2717} {path} is not an architecture graph: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("\u{2717} Failed to read {path}: {e}");
            std::process::exit(1);
        }
    });

    if let Some(path) = opts.manifest {
        if let Err(e) = write_manifest(&graph, std::path::Path::new(path)) {
            eprintln!("\u{2717} Failed to write the graph to {path}: {e}");
            std::process::exit(1);
        }
        eprintln!("\u{2713} Wrote architecture graph \u{2192} {path}");
    }

    if opts.json {
        let Some(json) = answer_json(&graph, &opts.query) else {
            report_unresolved(&graph, &opts.query);
        };
        println!("{json}");
    } else {
        let Some(report) = answer(&graph, &opts.query) else {
            report_unresolved(&graph, &opts.query);
        };
        print!("{report}");
    }

    if let (Some(path), Some(committed)) = (opts.check, committed) {
        if let Some(drift) = format_drift(&committed, &graph) {
            eprintln!("{drift}");
            eprintln!(
                "\nIf the change is intended, re-run with `--manifest {path}` and commit the result."
            );
            std::process::exit(1);
        }
        eprintln!("\u{2713} The architecture graph matches {path}.");
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::graph::manifest::build;
    use autumn_web::graph::{
        JobGraphDescriptor, JobKind, ModelGraphDescriptor, RepositoryGraphDescriptor,
        RouteGraphDescriptor,
    };

    use super::*;

    const MODELS: &[ModelGraphDescriptor] = &[ModelGraphDescriptor {
        model: "Post",
        model_path: "app::models::Post",
        table: "posts",
        relations: &[],
        module_path: "app::models",
        file: "src/models.rs",
        line: 1,
    }];

    const REPOSITORIES: &[RepositoryGraphDescriptor] = &[RepositoryGraphDescriptor {
        repository: "PostRepository",
        implementation: "PgPostRepository",
        model: "Post",
        table: "posts",
        api: "",
        module_path: "app::repositories",
        file: "src/repositories.rs",
        line: 1,
    }];

    const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
        handler: "index",
        module_path: "app::routes::posts",
        method: "GET",
        path: "/posts",
        static_route: false,
        file: "src/routes/posts.rs",
        line: 1,
        signature_symbols: &["PgPostRepository"],
        body_symbols: &[],
    }];

    const JOBS: &[JobGraphDescriptor] = &[JobGraphDescriptor {
        name: "digest",
        kind: JobKind::Job,
        handler: "digest",
        module_path: "app::jobs",
        schedule: "",
        mutating: true,
        file: "src/jobs.rs",
        line: 1,
        signature_symbols: &[],
        body_symbols: &["posts"],
    }];

    fn graph() -> ArchitectureGraph {
        build(&[], 0, ROUTES, MODELS, REPOSITORIES, JOBS)
    }

    #[test]
    fn show_renders_the_graphs_own_summary() {
        let g = graph();
        assert_eq!(answer(&g, &Query::Show).expect("show"), g.summary());
    }

    #[test]
    fn touches_answers_for_a_table_name() {
        let report = answer(&graph(), &Query::Touches("posts".to_owned())).expect("posts");
        assert!(report.contains("GET /posts"), "{report}");
        assert!(report.contains("digest"), "{report}");
    }

    #[test]
    fn impact_answers_for_a_model_name() {
        let report = answer(&graph(), &Query::Impact("Post".to_owned())).expect("Post");
        assert!(report.contains("Changing model Post"), "{report}");
        assert!(report.contains("PostRepository"), "{report}");
    }

    #[test]
    fn an_unknown_name_has_no_answer() {
        assert!(answer(&graph(), &Query::Impact("Nope".to_owned())).is_none());
    }

    #[test]
    fn the_suggestion_list_names_models_tables_and_repositories() {
        assert_eq!(
            queryable_names(&graph()),
            vec!["Post", "PostRepository", "posts"]
        );
    }

    #[test]
    fn an_identical_graph_has_no_drift() {
        assert!(format_drift(&graph(), &graph()).is_none());
    }

    #[test]
    fn a_removed_node_is_named_in_the_drift_report() {
        let before = graph();
        let after = build(&[], 0, &[], MODELS, REPOSITORIES, JOBS);
        let drift = format_drift(&before, &after).expect("removing a route is drift");
        assert!(drift.contains("- route"), "{drift}");
        assert!(drift.contains("GET /posts"), "{drift}");
    }

    #[test]
    fn a_lost_edge_is_named_in_the_drift_report() {
        let before = graph();
        const STRIPPED: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            signature_symbols: &[],
            ..ROUTES[0]
        }];
        let after = build(&[], 0, STRIPPED, MODELS, REPOSITORIES, JOBS);
        let drift = format_drift(&before, &after).expect("losing an edge is drift");
        assert!(
            drift.contains("- read route:app::routes::posts::index"),
            "{drift}"
        );
    }

    #[test]
    fn a_weakened_edge_evidence_is_named_in_the_drift_report() {
        // The route stops taking the repository as an extractor and reaches it
        // only by naming it in the body. Same endpoints, same access — but the
        // evidence changed, and "the documents differ in a field this report
        // does not name" is not something a reviewer can approve.
        let before = graph();
        const BODY_ONLY: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            signature_symbols: &[],
            body_symbols: &["PgPostRepository"],
            ..ROUTES[0]
        }];
        let after = build(&[], 0, BODY_ONLY, MODELS, REPOSITORIES, JOBS);
        let drift = format_drift(&before, &after).expect("weakened evidence is drift");
        assert!(drift.contains("evidence signature"), "{drift}");
        assert!(drift.contains("-> body"), "{drift}");
    }

    #[test]
    fn a_new_unenumerable_router_is_named_in_the_drift_report() {
        // The route and model counts are identical; only the opaque-router
        // count moved. Printing "39 / 39 -> 39 / 39" for that would withhold
        // exactly what a reviewer needs to approve.
        let before = graph();
        let after = build(&[], 2, ROUTES, MODELS, REPOSITORIES, JOBS);
        let drift = format_drift(&before, &after).expect("a new opaque router is drift");
        assert!(
            drift.contains("unenumerable mounted routers 0 -> 2"),
            "{drift}"
        );
    }

    #[test]
    fn a_new_unmodelled_mount_is_named_in_the_drift_report() {
        let before = graph();
        let mounted = autumn_web::graph::MountedRoute {
            method: "WS".to_owned(),
            path: "/ws/feed".to_owned(),
            handler: "feed".to_owned(),
            module_path: "app::live".to_owned(),
            auth: autumn_web::graph::RouteAuth::default(),
        };
        let after = build(&[mounted], 0, ROUTES, MODELS, REPOSITORIES, JOBS);
        let drift = format_drift(&before, &after).expect("a new opaque mount is drift");
        assert!(
            drift.contains("+ mounted with no macro declaration WS /ws/feed"),
            "{drift}"
        );
    }

    #[test]
    fn a_cosmetic_line_move_is_not_drift() {
        // `location` is worth having in the document and not worth gating on: a
        // gate that fails on every blank line added above a handler gets
        // regenerated reflexively, and an auth change rides along in the same
        // regeneration.
        let before = graph();
        let mut after = graph();
        for node in &mut after.nodes {
            node.location = format!("{}:999", node.location);
        }
        assert!(format_drift(&before, &after).is_none());
    }

    #[test]
    fn a_changed_auth_posture_is_named_in_the_drift_report() {
        let mounted = |secured: bool| autumn_web::graph::MountedRoute {
            method: "GET".to_owned(),
            path: "/posts".to_owned(),
            handler: "index".to_owned(),
            module_path: "app::routes::posts".to_owned(),
            auth: autumn_web::graph::RouteAuth {
                secured,
                ..autumn_web::graph::RouteAuth::default()
            },
        };
        let before = build(&[mounted(true)], 0, ROUTES, MODELS, REPOSITORIES, JOBS);
        let after = build(&[mounted(false)], 0, ROUTES, MODELS, REPOSITORIES, JOBS);
        let drift = format_drift(&before, &after).expect("dropping #[secured] is drift");
        assert!(drift.contains("auth secured -> none"), "{drift}");
    }
}
