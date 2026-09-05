//! The application architecture graph document (issue #1747).
//!
//! One node per macro-declared element, one edge per declared or derived
//! relationship, plus the completeness accounting and the derivation's own
//! limits. See [the module docs](super) for what the derivation can and cannot
//! see.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{
    Access, JobGraphDescriptor, JobKind, ModelGraphDescriptor, MountedRoute, NodeKind, Provenance,
    RepositoryGraphDescriptor, RouteAuth, RouteGraphDescriptor, is_safe_method,
};

/// Schema version of the emitted architecture graph. Bumped only on breaking
/// changes to the document shape.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Machine-readable stdout marker preceding the graph JSON emitted by the
/// `AUTUMN_DUMP_GRAPH=1` dump mode.
///
/// A process-boundary protocol: `autumn graph` runs the built binary as a child
/// and scans its stdout for this marker, so an app that prints anything else
/// during startup cannot corrupt the parse.
pub const ARCHITECTURE_GRAPH_MARKER: &str = "[autumn:graph] ";

/// The env var selecting the app binary's architecture-graph dump mode.
pub const DUMP_ENV: &str = "AUTUMN_DUMP_GRAPH";

/// The derivation limits carried in the document itself.
///
/// They live here, not only in the guide, for the same reason the
/// agent-authority manifest carries its `excluded` dimensions: a document read
/// without its caveats is read as more than it is.
pub const LIMITS: &[&str] = &[
    "Edges from a route or job are derived from that item's own tokens: a call into a helper \
     function in another module is not followed (static derivation only — dynamic call-graph \
     tracing is out of scope for this slice).",
    "Symbol resolution is name-based: a type alias or a `use ... as ...` rename is not resolved, \
     and a model whose name matches a common type is linked wherever that name appears.",
    "Raw SQL is matched by identifier, and only inside string literals that contain a SQL \
     keyword; SQL assembled at runtime from fragments is invisible.",
    "A router mounted with `merge`/`nest` is opaque: its endpoints cannot be enumerated at all, \
     so `completeness.opaque_mounted_routers` counts them rather than naming them. The \
     `AUTUMN_DUMP_GRAPH` count covers the routers the *builder* declares; a router the \
     configuration adds during startup (a blob store, the SEO endpoints, an inbound-mail \
     webhook) is mounted after that dump exits, so the running binary's `/actuator/graph` can \
     report a higher count than the committed document.",
    "A model's declared relations (`#[votable]`, `#[commentable]`) are attributed to its \
     repository as a whole, because that is where the generated methods live — so a route \
     holding the repository is reported as reaching the edge table whether or not it calls \
     them. The edge exists only when some `#[model]` maps that table; `#[commentable]`'s \
     shared comments table in an app with no comment model is a relation with nothing to \
     point at.",
    "Only the item's own tokens are read, so a module-level `use crate::schema::posts::dsl::*` \
     followed by a bare `posts.filter(…)` in the body names no candidate and produces no edge. \
     Write the table module (`posts::table`) or the model in the handler to be linked.",
    "A route's access is its declared HTTP method (safe methods read, everything else writes); a \
     job's is whether its tokens carry mutation evidence. Either way it is the declared intent, \
     not an executed statement.",
];

// ── Node facts ───────────────────────────────────────────────────────

/// Facts carried by a route node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteFacts {
    /// Uppercase HTTP method.
    pub method: String,
    /// The mounted path when the route is in the route table, otherwise the
    /// path as declared on the macro.
    pub path: String,
    /// Whether the route was found in the application's mounted route table.
    ///
    /// A declared handler that no `routes![]` list mounts is still a node: it
    /// is macro-declared, and a route that silently stopped being served is
    /// exactly the drift the completeness gate exists to catch.
    pub mounted: bool,
    /// The route's declared authorization requirement, when it is mounted.
    /// A route the app never mounted has no resolved posture to state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RouteAuth>,
    /// Further mounted paths for the same handler, sorted.
    ///
    /// A handler passed to more than one `routes![]` list — a top-level one and
    /// a scoped group, say — is served at every one of them. Recording only the
    /// first would report the rest as mounts with no declaration, sending a
    /// reader hunting for a handler that is right there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_mounted_at: Vec<String>,
    /// The repository that generated this route, for a `#[repository(api =
    /// "...")]` auto-API mount that no `#[route]` declares.
    ///
    /// `None` for a hand-written handler. These routes are real served
    /// endpoints that read and write a table, so omitting them would mean
    /// `autumn graph touches <table>` missed the whole REST surface of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<String>,
}

/// Facts carried by a model node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFacts {
    /// The database table the model maps to.
    pub table: String,
    /// Further tables the model's declared relations touch (`#[votable]`'s
    /// edge table, `#[commentable]`'s comments table), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<String>,
}

/// Facts carried by a repository node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryFacts {
    /// The generated implementation type, e.g. `PgPostRepository`.
    pub implementation: String,
    /// The model the repository is declared over.
    pub model: String,
    /// The table that model maps to.
    pub table: String,
    /// Mount prefix of the generated REST auto-API, when it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// Facts carried by a job, scheduled-task or one-off-task node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFacts {
    /// Which macro declared it.
    pub kind: JobKind,
    /// Schedule expression, for `#[scheduled]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
}

/// One element of the application's architecture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Stable identity, `"{kind}:{module_path}::{name}"` for every kind except
    /// models, which use their module-qualified type path.
    ///
    /// Deliberately independent of the mounted path: a route that moves under a
    /// renamed scope must read as one node whose path changed, not as one node
    /// removed and another added.
    pub id: String,
    /// What the element is.
    pub kind: NodeKind,
    /// Display name — the handler, model, repository trait or job name.
    pub name: String,
    /// Module the element was declared in.
    pub module: String,
    /// `file:line` of the declaration.
    pub location: String,
    /// Route facts, for route and static-route nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteFacts>,
    /// Model facts, for model nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelFacts>,
    /// Repository facts, for repository nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryFacts>,
    /// Job facts, for job, scheduled-task and one-off-task nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobFacts>,
}

impl GraphNode {
    /// A one-line rendering of the node, for the human report.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.route, &self.model, &self.repository, &self.job) {
            (Some(r), ..) => format!("{} {}", r.method, r.path),
            (_, Some(m), ..) => format!("{} (table {})", self.name, m.table),
            (_, _, Some(r), _) => format!("{} over {}", self.name, r.model),
            (.., Some(j)) => j.schedule.as_ref().map_or_else(
                || self.name.clone(),
                |s| format!("{} (every {s})", self.name),
            ),
            _ => self.name.clone(),
        }
    }
}

/// One relationship between two elements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Node id of the acting element — a route, job or repository.
    pub from: String,
    /// Node id of the element acted upon — a repository or model.
    pub to: String,
    /// Whether the source reads or writes the target.
    pub access: Access,
    /// Why the edge exists.
    pub provenance: Provenance,
    /// The symbol that resolved the edge; empty for declaration edges.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub symbol: String,
}

/// The completeness accounting for the graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completeness {
    /// Macro-declared routes (mounted or not).
    pub declared_routes: usize,
    /// Declared routes found in the application's mounted route table.
    pub mounted_routes: usize,
    /// Declared models.
    pub models: usize,
    /// Declared repositories.
    pub repositories: usize,
    /// Declared jobs, scheduled tasks and one-off tasks.
    pub jobs: usize,
    /// Mounted routes attributed to a `#[repository]` auto-API rather than to
    /// a `#[route]` declaration.
    #[serde(default)]
    pub generated_routes: usize,
    /// Raw routers mounted with `merge`/`nest` whose endpoints this graph
    /// cannot enumerate at all.
    ///
    /// An opaque router exposes no API to list its routes, so — unlike
    /// [`Self::unmodelled_mounted_routes`] — the paths behind it cannot be
    /// *named*, only counted. Carrying the count is what stops the document
    /// from reading as a complete account of the served surface when it is
    /// not: it is the same count `autumn routes audit` hard-fails its
    /// coverage gate on.
    #[serde(default)]
    pub opaque_mounted_routers: usize,
    /// Node ids of declared routes that no `routes![]` list mounts, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmounted_routes: Vec<String>,
    /// Mounted routes with no macro-declared node — a route registered by a
    /// mechanism this graph cannot see (a raw `merge`/`nest` router, or a
    /// framework endpoint), sorted. Named rather than dropped so the document
    /// cannot quietly under-report the served surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmodelled_mounted_routes: Vec<String>,
}

/// The application's architecture, as its macros declare it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchitectureGraph {
    /// Schema version of this document.
    pub schema_version: u32,
    /// Every macro-declared element, sorted by [`GraphNode::id`].
    pub nodes: Vec<GraphNode>,
    /// Every declared or derived relationship, sorted by `(from, to)`.
    pub edges: Vec<GraphEdge>,
    /// What the graph accounts for.
    pub completeness: Completeness,
    /// What the derivation cannot see, carried with the document.
    pub limits: Vec<String>,
}

impl ArchitectureGraph {
    /// Serialize the graph as pretty JSON.
    ///
    /// # Panics
    ///
    /// Never in practice: every field is a plain serializable value.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| {
            panic!("architecture graph is not serializable: {e}");
        })
    }

    /// Look a node up by id.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The human report.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Application architecture graph (schema v{})",
            self.schema_version
        );
        let _ = writeln!(
            out,
            "  {} nodes, {} edges",
            self.nodes.len(),
            self.edges.len()
        );
        let c = &self.completeness;
        let _ = writeln!(
            out,
            "  {} routes ({} mounted), {} models, {} repositories, {} jobs",
            c.declared_routes, c.mounted_routes, c.models, c.repositories, c.jobs
        );
        if c.generated_routes > 0 {
            let _ = writeln!(out, "  {} repository auto-API route(s)", c.generated_routes);
        }
        if c.opaque_mounted_routers > 0 {
            let _ = writeln!(
                out,
                "  {} mounted router(s) this graph cannot enumerate (raw merge/nest)",
                c.opaque_mounted_routers
            );
        }

        for kind in [
            NodeKind::Model,
            NodeKind::Repository,
            NodeKind::Route,
            NodeKind::StaticRoute,
            NodeKind::Job,
            NodeKind::ScheduledTask,
            NodeKind::OneOffTask,
        ] {
            let nodes: Vec<&GraphNode> = self.nodes.iter().filter(|n| n.kind == kind).collect();
            if nodes.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n{kind}s ({}):", nodes.len());
            for node in nodes {
                let _ = writeln!(out, "  {}", node.label());
                for edge in self.edges.iter().filter(|e| e.from == node.id) {
                    let target = self
                        .node(&edge.to)
                        .map_or(edge.to.as_str(), |n| n.name.as_str());
                    let _ = writeln!(
                        out,
                        "      {} {} ({})",
                        edge.access, target, edge.provenance
                    );
                }
            }
        }

        if !c.unmounted_routes.is_empty() {
            let _ = writeln!(
                out,
                "\nDeclared but not mounted ({}):",
                c.unmounted_routes.len()
            );
            for id in &c.unmounted_routes {
                let _ = writeln!(out, "  {id}");
            }
        }
        if !c.unmodelled_mounted_routes.is_empty() {
            let _ = writeln!(
                out,
                "\nMounted with no macro declaration ({}):",
                c.unmodelled_mounted_routes.len()
            );
            for id in &c.unmodelled_mounted_routes {
                let _ = writeln!(out, "  {id}");
            }
        }

        let _ = writeln!(out, "\nLimits of this derivation:");
        for limit in &self.limits {
            let _ = writeln!(out, "  - {limit}");
        }
        out
    }
}

// ── Building ─────────────────────────────────────────────────────────

/// The id of a route node.
fn route_id(module_path: &str, handler: &str) -> String {
    format!("route:{module_path}::{handler}")
}

/// The id of a model node.
fn model_id(model_path: &str) -> String {
    format!("model:{model_path}")
}

/// The id of a repository node.
fn repository_id(module_path: &str, repository: &str) -> String {
    format!("repository:{module_path}::{repository}")
}

/// The id of a job node.
fn job_id(module_path: &str, handler: &str) -> String {
    format!("job:{module_path}::{handler}")
}

/// A `file:line` location string, with a dependency's registry path shortened.
///
/// `file!()` for an element declared in a non-path dependency is the absolute
/// checkout path — `/home/<user>/.cargo/registry/src/index.crates.io-<hash>/
/// plugin-0.1.0/src/routes.rs`. Left whole it puts the build machine's username
/// and layout into a document served over HTTP, and makes the committed graph
/// differ between two developers for reasons that have nothing to do with the
/// app. The crate-relative tail is the part that identifies anything.
fn location(file: &str, line: u32) -> String {
    // Separators first: on Windows `file!()` yields backslashes, so a graph
    // committed from Linux would otherwise read as drift on every node, and the
    // registry shortening below would not match at all.
    let file = file.replace('\\', "/");
    let normalized = file
        .split_once("/registry/src/")
        .and_then(|(_, tail)| tail.split_once('/'))
        .map_or(file.as_str(), |(_, crate_relative)| crate_relative);
    format!("{normalized}:{line}")
}

/// The access a declared HTTP method states.
fn method_access(method: &str) -> Access {
    if is_safe_method(method) {
        Access::Read
    } else {
        Access::Write
    }
}

/// The symbol index: which node ids a candidate name can resolve to.
///
/// A name maps to *every* node that answers to it, never to a best guess: two
/// crates that each declare a `Post` produce two edges, which is the honest
/// answer to a name-based derivation and keeps recall total.
fn symbol_index(
    models: &[ModelGraphDescriptor],
    repositories: &[RepositoryGraphDescriptor],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut index: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for model in models {
        let id = model_id(model.model_path);
        index
            .entry(model.model.to_owned())
            .or_default()
            .insert(id.clone());
        index.entry(model.table.to_owned()).or_default().insert(id);
    }
    for repo in repositories {
        let id = repository_id(repo.module_path, repo.repository);
        index
            .entry(repo.repository.to_owned())
            .or_default()
            .insert(id.clone());
        index
            .entry(repo.implementation.to_owned())
            .or_default()
            .insert(id);
    }
    index
}

/// Table names, lowercased, for matching an unquoted SQL identifier.
///
/// A separate map from [`symbol_index`] on purpose. An unquoted SQL identifier
/// is case-insensitive, so `sql_query("SELECT * FROM POSTS")` names the `posts`
/// table — but a Rust type name is not, and folding everything made a DTO
/// called `Posts` resolve to that table (Codex round 5). Only candidates that
/// came from a SQL literal are looked up here.
fn sql_table_index(models: &[ModelGraphDescriptor]) -> BTreeMap<String, BTreeSet<String>> {
    let mut index: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for model in models {
        index
            .entry(model.table.to_ascii_lowercase())
            .or_default()
            .insert(model_id(model.model_path));
    }
    index
}

/// The candidate names one item published, by where they were read from.
///
/// Grouped rather than passed as three parallel slices: they always travel
/// together, and which list a name came from decides how it may be matched.
#[derive(Debug, Clone, Copy)]
struct Candidates<'a> {
    /// Names from the item's signature — the extractors it declares.
    signature: &'a [&'a str],
    /// Names from the item's body.
    body: &'a [&'a str],
    /// The subset of `body` that came from a SQL literal, and so may be
    /// matched against a table name case-insensitively.
    sql: &'a [&'a str],
}

/// Resolve one item's symbols into edges, strongest provenance winning.
fn resolve_edges(
    from: &str,
    candidates: Candidates<'_>,
    access: Access,
    index: &BTreeMap<String, BTreeSet<String>>,
    sql_tables: &BTreeMap<String, BTreeSet<String>>,
    edges: &mut BTreeMap<(String, String), GraphEdge>,
) {
    let sql_symbols = candidates.sql;
    for (symbols, provenance) in [
        (candidates.signature, Provenance::Signature),
        (candidates.body, Provenance::Body),
    ] {
        for symbol in symbols {
            // A SQL-derived candidate also matches a table name case-folded,
            // because an unquoted SQL identifier is case-insensitive. Every
            // other candidate is matched verbatim: `Post` and `post` are
            // different Rust items.
            let folded = sql_symbols
                .contains(symbol)
                .then(|| sql_tables.get(&symbol.to_ascii_lowercase()))
                .flatten();
            let targets = index.get(*symbol).or(folded);
            let Some(targets) = targets else {
                continue;
            };
            for target in targets {
                let key = (from.to_owned(), target.clone());
                let candidate = GraphEdge {
                    from: from.to_owned(),
                    to: target.clone(),
                    access,
                    provenance,
                    symbol: (*symbol).to_owned(),
                };
                match edges.get(&key) {
                    // A signature edge outranks a body edge for the same pair:
                    // an extractor is a stronger statement than a name in the
                    // body, and the report should say the stronger one.
                    Some(existing) if existing.provenance <= candidate.provenance => {}
                    _ => {
                        edges.insert(key, candidate);
                    }
                }
            }
        }
    }
}

/// Assemble the graph from macro-declared elements and the mounted route table.
///
/// `mounted` supplies the served path and resolved auth posture for routes the
/// application actually mounts; everything else is a macro declaration, so a
/// declared element is in the graph whether or not the app wired it up.
#[must_use]
pub fn build(
    mounted: &[MountedRoute],
    opaque_mounted_routers: usize,
    routes: &[RouteGraphDescriptor],
    models: &[ModelGraphDescriptor],
    repositories: &[RepositoryGraphDescriptor],
    jobs: &[JobGraphDescriptor],
) -> ArchitectureGraph {
    let index = symbol_index(models, repositories);
    let sql_tables = sql_table_index(models);
    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    let mut edges: BTreeMap<(String, String), GraphEdge> = BTreeMap::new();

    add_model_nodes(models, &mut nodes);
    add_repository_nodes(repositories, models, &index, &mut nodes, &mut edges);
    let route_totals =
        add_route_nodes(routes, mounted, &index, &sql_tables, &mut nodes, &mut edges);
    add_job_nodes(jobs, &index, &sql_tables, &mut nodes, &mut edges);
    let surface = add_auto_api_nodes(
        mounted,
        repositories,
        &route_totals.matched,
        &mut nodes,
        &mut edges,
    );

    let mut unmounted = route_totals.unmounted;
    unmounted.sort();
    unmounted.dedup();

    ArchitectureGraph {
        schema_version: MANIFEST_SCHEMA_VERSION,
        completeness: Completeness {
            declared_routes: routes.len(),
            mounted_routes: route_totals.mounted,
            models: models.len(),
            repositories: repositories.len(),
            jobs: jobs.len(),
            generated_routes: surface.generated,
            opaque_mounted_routers,
            unmounted_routes: unmounted,
            unmodelled_mounted_routes: surface.unmodelled,
        },
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
        limits: LIMITS.iter().map(|l| (*l).to_owned()).collect(),
    }
}

/// One model node per `#[model]`.
fn add_model_nodes(models: &[ModelGraphDescriptor], nodes: &mut BTreeMap<String, GraphNode>) {
    for model in models {
        let id = model_id(model.model_path);
        nodes.insert(
            id.clone(),
            GraphNode {
                id,
                kind: NodeKind::Model,
                name: model.model.to_owned(),
                module: model.module_path.to_owned(),
                location: location(model.file, model.line),
                route: None,
                model: Some(ModelFacts {
                    table: model.table.to_owned(),
                    relations: model.relations.iter().map(|t| (*t).to_owned()).collect(),
                }),
                repository: None,
                job: None,
            },
        );
    }
}

/// One repository node per `#[repository]`, with its declared model edges.
fn add_repository_nodes(
    repositories: &[RepositoryGraphDescriptor],
    models: &[ModelGraphDescriptor],
    index: &BTreeMap<String, BTreeSet<String>>,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut BTreeMap<(String, String), GraphEdge>,
) {
    for repo in repositories {
        let id = repository_id(repo.module_path, repo.repository);
        nodes.insert(
            id.clone(),
            GraphNode {
                id: id.clone(),
                kind: NodeKind::Repository,
                name: repo.repository.to_owned(),
                module: repo.module_path.to_owned(),
                location: location(repo.file, repo.line),
                route: None,
                model: None,
                repository: Some(RepositoryFacts {
                    implementation: repo.implementation.to_owned(),
                    model: repo.model.to_owned(),
                    table: repo.table.to_owned(),
                    api: (!repo.api.is_empty()).then(|| repo.api.to_owned()),
                }),
                job: None,
            },
        );
        // `#[repository(Post)]` states its model outright, so this edge is a
        // declaration rather than a name match. It is resolved through the
        // symbol index all the same, so a repository over a model no `#[model]`
        // declared produces no dangling edge.
        //
        // The model's declared *relations* are folded in for the same reason
        // and at the same strength: `#[votable]` and `#[commentable]` put their
        // generated methods on this repository, so a route holding it reaches
        // those edge tables too — without ever naming them.
        let relation_tables: Vec<&str> = models
            .iter()
            .filter(|m| m.model == repo.model)
            .flat_map(|m| m.relations.iter().copied())
            .collect();
        for name in std::iter::once(repo.model).chain(relation_tables) {
            for target in index
                .get(name)
                .into_iter()
                .flatten()
                .filter(|t| t.starts_with("model:"))
            {
                edges.insert(
                    (id.clone(), target.clone()),
                    GraphEdge {
                        from: id.clone(),
                        to: target.clone(),
                        access: Access::ReadWrite,
                        provenance: Provenance::Declaration,
                        symbol: String::new(),
                    },
                );
            }
        }
    }
}

/// What [`add_route_nodes`] accounts for.
struct RouteTotals {
    /// Declared routes found in the mounted route table.
    mounted: usize,
    /// Node ids of declared routes nothing mounts.
    unmounted: Vec<String>,
    /// `(method, path)` pairs a declared route claimed.
    matched: BTreeSet<(String, String)>,
}

/// One route node per `#[route]`/`#[static_get]`, with its resolved edges.
fn add_route_nodes(
    routes: &[RouteGraphDescriptor],
    mounted: &[MountedRoute],
    index: &BTreeMap<String, BTreeSet<String>>,
    sql_tables: &BTreeMap<String, BTreeSet<String>>,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut BTreeMap<(String, String), GraphEdge>,
) -> RouteTotals {
    let mut mounted_count = 0usize;
    let mut unmounted: Vec<String> = Vec::new();
    let mut matched_mounted: BTreeSet<(String, String)> = BTreeSet::new();
    for route in routes {
        let id = route_id(route.module_path, route.handler);
        // Every mount of this handler, not just the first: a handler passed to
        // both a top-level `routes![]` and a scoped group is served at both,
        // and claiming only one leaves the other looking undeclared.
        let hits: Vec<&MountedRoute> = mounted
            .iter()
            .filter(|m| {
                m.handler == route.handler
                    && m.module_path == route.module_path
                    && m.method.eq_ignore_ascii_case(route.method)
            })
            .collect();
        if hits.is_empty() {
            unmounted.push(id.clone());
        } else {
            mounted_count += 1;
            for m in &hits {
                matched_mounted.insert((m.method.clone(), m.path.clone()));
            }
        }
        let mut paths: Vec<String> = hits.iter().map(|m| m.path.clone()).collect();
        paths.sort();
        paths.dedup();
        let hit = hits.first().copied();
        // A route's access is read off its *declared method*, and not off any
        // mutation evidence in its tokens. A page that renders `hx-delete=(…)`
        // on a link names `delete` in its body while being a read-only GET, and
        // reporting it as a writer would make the write set useless. The method
        // is the declaration: a GET that mutates is a bug in the handler, not a
        // fact for this document to launder.
        let access = method_access(route.method);
        nodes.insert(
            id.clone(),
            GraphNode {
                id: id.clone(),
                kind: if route.static_route {
                    NodeKind::StaticRoute
                } else {
                    NodeKind::Route
                },
                name: route.handler.to_owned(),
                module: route.module_path.to_owned(),
                location: location(route.file, route.line),
                route: Some(RouteFacts {
                    method: route.method.to_owned(),
                    // Sorted, so which mount is "the" path does not depend on
                    // registration order; the rest are carried alongside.
                    path: paths
                        .first()
                        .cloned()
                        .unwrap_or_else(|| route.path.to_owned()),
                    also_mounted_at: paths.iter().skip(1).cloned().collect(),
                    mounted: hit.is_some(),
                    auth: hit.map(|m| m.auth.clone()),
                    generated_by: None,
                }),
                model: None,
                repository: None,
                job: None,
            },
        );
        resolve_edges(
            &id,
            Candidates {
                signature: route.signature_symbols,
                body: route.body_symbols,
                sql: route.sql_symbols,
            },
            access,
            index,
            sql_tables,
            edges,
        );
    }
    RouteTotals {
        mounted: mounted_count,
        unmounted,
        matched: matched_mounted,
    }
}

/// One node per `#[job]`/`#[scheduled]`/`#[task]`, with its resolved edges.
fn add_job_nodes(
    jobs: &[JobGraphDescriptor],
    index: &BTreeMap<String, BTreeSet<String>>,
    sql_tables: &BTreeMap<String, BTreeSet<String>>,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut BTreeMap<(String, String), GraphEdge>,
) {
    for job in jobs {
        let id = job_id(job.module_path, job.handler);
        nodes.insert(
            id.clone(),
            GraphNode {
                id: id.clone(),
                kind: match job.kind {
                    JobKind::Job => NodeKind::Job,
                    JobKind::Scheduled => NodeKind::ScheduledTask,
                    JobKind::Task => NodeKind::OneOffTask,
                },
                name: job.name.to_owned(),
                module: job.module_path.to_owned(),
                location: location(job.file, job.line),
                route: None,
                model: None,
                repository: None,
                job: Some(JobFacts {
                    kind: job.kind,
                    schedule: (!job.schedule.is_empty()).then(|| job.schedule.to_owned()),
                }),
            },
        );
        let access = if job.mutating {
            Access::Write
        } else {
            Access::Read
        };
        resolve_edges(
            &id,
            Candidates {
                signature: job.signature_symbols,
                body: job.body_symbols,
                sql: job.sql_symbols,
            },
            access,
            index,
            sql_tables,
            edges,
        );
    }
}

/// What [`add_auto_api_nodes`] accounts for.
struct MountedSurface {
    /// Mounted routes attributed to a repository auto-API.
    generated: usize,
    /// Mounted routes no declaration and no auto-API accounts for.
    unmodelled: Vec<String>,
}

/// One node per mounted route a `#[repository(api = "...")]` generated.
fn add_auto_api_nodes(
    mounted: &[MountedRoute],
    repositories: &[RepositoryGraphDescriptor],
    matched_mounted: &BTreeSet<(String, String)>,
    nodes: &mut BTreeMap<String, GraphNode>,
    edges: &mut BTreeMap<(String, String), GraphEdge>,
) -> MountedSurface {
    // `#[repository(api = "/api/posts")]` mounts a CRUD surface no `#[route]`
    // declares. The mount prefix is declared, so attributing these routes to
    // their repository is a declaration too — and without them a query for the
    // routes touching a table would miss its entire REST surface.
    let mut generated_routes = 0usize;
    let mut unmodelled: BTreeSet<String> = BTreeSet::new();
    for route in mounted {
        if matched_mounted.contains(&(route.method.clone(), route.path.clone())) {
            continue;
        }
        // Ownership is *declared*, not inferred: the route carries the
        // `api = "..."` prefix of the repository that generated it. Matching on
        // the served path instead got two cases wrong — a CRUD surface mounted
        // inside `.scoped("/v1", …)` no longer starts with the declared prefix
        // and was dropped, and a hand-written route under an API prefix was
        // claimed as generated (Codex round 5).
        let owner = route.repository_api.as_ref().and_then(|api| {
            repositories
                .iter()
                .filter(|repo| repo.api == api)
                // Two repositories cannot declare the same prefix (the router
                // would refuse the mount), but ties break on node id anyway so
                // the answer never depends on link order.
                .min_by_key(|repo| repository_id(repo.module_path, repo.repository))
        });
        let Some(repo) = owner else {
            unmodelled.insert(format!("{} {}", route.method, route.path));
            continue;
        };
        generated_routes += 1;
        let repo_node = repository_id(repo.module_path, repo.repository);
        let id = format!("route:auto-api:{} {}", route.method, route.path);
        let access = method_access(&route.method);
        nodes.insert(
            id.clone(),
            GraphNode {
                id: id.clone(),
                kind: NodeKind::Route,
                name: format!("{}::auto_api", repo.repository),
                module: repo.module_path.to_owned(),
                location: location(repo.file, repo.line),
                route: Some(RouteFacts {
                    method: route.method.clone(),
                    path: route.path.clone(),
                    also_mounted_at: Vec::new(),
                    mounted: true,
                    auth: Some(route.auth.clone()),
                    generated_by: Some(repo.repository.to_owned()),
                }),
                model: None,
                repository: None,
                job: None,
            },
        );
        edges.insert(
            (id.clone(), repo_node.clone()),
            GraphEdge {
                from: id,
                to: repo_node,
                access,
                provenance: Provenance::Declaration,
                symbol: String::new(),
            },
        );
    }

    MountedSurface {
        generated: generated_routes,
        unmodelled: unmodelled.into_iter().collect(),
    }
}

/// Assemble the graph from this binary's link-time registrations.
#[must_use]
pub fn audit(mounted: &[MountedRoute], opaque_mounted_routers: usize) -> ArchitectureGraph {
    let routes: Vec<RouteGraphDescriptor> = inventory::iter::<RouteGraphDescriptor>
        .into_iter()
        .copied()
        .collect();
    let models: Vec<ModelGraphDescriptor> = inventory::iter::<ModelGraphDescriptor>
        .into_iter()
        .copied()
        .collect();
    let repositories: Vec<RepositoryGraphDescriptor> = inventory::iter::<RepositoryGraphDescriptor>
        .into_iter()
        .copied()
        .collect();
    let jobs: Vec<JobGraphDescriptor> = inventory::iter::<JobGraphDescriptor>
        .into_iter()
        .copied()
        .collect();
    build(
        mounted,
        opaque_mounted_routers,
        &routes,
        &models,
        &repositories,
        &jobs,
    )
}

// ── Dump-mode protocol ───────────────────────────────────────────────

/// Whether the process was started in architecture-graph dump mode.
#[must_use]
pub fn is_dump_mode() -> bool {
    std::env::var(DUMP_ENV).as_deref() == Ok("1")
}

/// Print the graph to stdout behind [`ARCHITECTURE_GRAPH_MARKER`].
///
/// # Panics
///
/// Never in practice: every field is a plain serializable value, and a failure
/// here would mean the dump protocol is broken rather than that this app is.
pub fn print_manifest_dump(graph: &ArchitectureGraph) {
    let json = serde_json::to_string(graph).unwrap_or_else(|e| {
        panic!("architecture graph is not serializable: {e}");
    });
    println!("{ARCHITECTURE_GRAPH_MARKER}{json}");
}

/// Recover a graph from a child process's stdout.
///
/// Scans for [`ARCHITECTURE_GRAPH_MARKER`] rather than parsing the whole stream,
/// so an app that prints anything else during startup cannot corrupt the parse.
#[must_use]
pub fn parse_manifest_dump(stdout: &str) -> Option<ArchitectureGraph> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(ARCHITECTURE_GRAPH_MARKER))
        .and_then(|json| serde_json::from_str(json).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &'static str, path: &'static str, table: &'static str) -> ModelGraphDescriptor {
        ModelGraphDescriptor {
            model: name,
            model_path: path,
            table,
            relations: &[],
            module_path: "app::models",
            file: "src/models.rs",
            line: 10,
        }
    }

    fn repository(
        name: &'static str,
        implementation: &'static str,
        model: &'static str,
        table: &'static str,
    ) -> RepositoryGraphDescriptor {
        RepositoryGraphDescriptor {
            repository: name,
            implementation,
            model,
            table,
            api: "",
            module_path: "app::repositories",
            file: "src/repositories.rs",
            line: 20,
        }
    }

    fn repository_with_api(
        name: &'static str,
        implementation: &'static str,
        model: &'static str,
        table: &'static str,
        api: &'static str,
    ) -> RepositoryGraphDescriptor {
        RepositoryGraphDescriptor {
            api,
            ..repository(name, implementation, model, table)
        }
    }

    fn route(
        handler: &'static str,
        method: &'static str,
        path: &'static str,
        signature_symbols: &'static [&'static str],
        body_symbols: &'static [&'static str],
    ) -> RouteGraphDescriptor {
        RouteGraphDescriptor {
            handler,
            module_path: "app::routes::posts",
            method,
            path,
            static_route: false,
            file: "src/routes/posts.rs",
            line: 30,
            signature_symbols,
            body_symbols,
            sql_symbols: &[],
        }
    }

    fn job(
        name: &'static str,
        kind: JobKind,
        body_symbols: &'static [&'static str],
    ) -> JobGraphDescriptor {
        JobGraphDescriptor {
            name,
            kind,
            handler: name,
            module_path: "app::jobs",
            schedule: "",
            mutating: false,
            file: "src/jobs.rs",
            line: 40,
            signature_symbols: &[],
            body_symbols,
            sql_symbols: &[],
        }
    }

    fn mounted(method: &str, path: &str, handler: &str) -> MountedRoute {
        MountedRoute {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: handler.to_owned(),
            module_path: "app::routes::posts".to_owned(),
            auth: RouteAuth {
                secured: true,
                roles: vec!["admin".to_owned()],
                ..RouteAuth::default()
            },
            repository_api: None,
        }
    }

    /// A mounted route a `#[repository(api = ...)]` generated, carrying the
    /// declared ownership the real `Route` does.
    fn mounted_by_repository(method: &str, path: &str, api: &str) -> MountedRoute {
        MountedRoute {
            repository_api: Some(api.to_owned()),
            ..mounted(method, path, "auto")
        }
    }

    #[test]
    fn every_declared_element_becomes_a_node() {
        let graph = build(
            &[],
            0,
            &[route("index", "GET", "/posts", &[], &[])],
            &[model("Post", "app::models::Post", "posts")],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[job("send_digest", JobKind::Job, &[])],
        );
        let kinds: Vec<NodeKind> = graph.nodes.iter().map(|n| n.kind).collect();
        assert!(kinds.contains(&NodeKind::Route), "{kinds:?}");
        assert!(kinds.contains(&NodeKind::Model), "{kinds:?}");
        assert!(kinds.contains(&NodeKind::Repository), "{kinds:?}");
        assert!(kinds.contains(&NodeKind::Job), "{kinds:?}");
        assert_eq!(graph.completeness.declared_routes, 1);
        assert_eq!(graph.completeness.models, 1);
        assert_eq!(graph.completeness.repositories, 1);
        assert_eq!(graph.completeness.jobs, 1);
    }

    #[test]
    fn a_repository_declares_its_model_edge() {
        let graph = build(
            &[],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[],
        );
        let edge = graph
            .edges
            .iter()
            .find(|e| e.to == "model:app::models::Post")
            .expect("repository must declare an edge to its model");
        assert_eq!(edge.provenance, Provenance::Declaration);
        assert_eq!(edge.access, Access::ReadWrite);
    }

    #[test]
    fn a_repository_extractor_links_the_route_to_the_repository() {
        let graph = build(
            &[],
            0,
            &[route(
                "show",
                "GET",
                "/posts/{id}",
                &["PgPostRepository"],
                &[],
            )],
            &[model("Post", "app::models::Post", "posts")],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[],
        );
        let edge = graph
            .edges
            .iter()
            .find(|e| e.from.starts_with("route:") && e.to.starts_with("repository:"))
            .expect("a repository extractor must link the route to the repository");
        assert_eq!(edge.provenance, Provenance::Signature);
        assert_eq!(edge.symbol, "PgPostRepository");
    }

    #[test]
    fn a_table_module_named_in_the_body_links_the_route_to_the_model() {
        let graph = build(
            &[],
            0,
            &[route("show", "GET", "/posts/{id}", &[], &["posts"])],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[],
        );
        let edge = graph
            .edges
            .iter()
            .find(|e| e.to == "model:app::models::Post")
            .expect("a table module named in the body must link the route to the model");
        assert_eq!(edge.provenance, Provenance::Body);
        assert_eq!(edge.access, Access::Read);
    }

    #[test]
    fn a_non_safe_method_makes_the_edge_a_write() {
        let graph = build(
            &[],
            0,
            &[route("create", "POST", "/posts", &[], &["posts"])],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[],
        );
        assert_eq!(graph.edges[0].access, Access::Write);
    }

    #[test]
    fn a_signature_edge_outranks_a_body_edge_for_the_same_pair() {
        let graph = build(
            &[],
            0,
            &[route(
                "show",
                "GET",
                "/posts/{id}",
                &["PgPostRepository"],
                &["PgPostRepository"],
            )],
            &[],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[],
        );
        assert_eq!(graph.edges.len(), 1, "{:?}", graph.edges);
        assert_eq!(graph.edges[0].provenance, Provenance::Signature);
    }

    #[test]
    fn a_job_that_names_a_table_gets_a_model_edge() {
        let graph = build(
            &[],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[job("hot_rank", JobKind::Scheduled, &["posts"])],
        );
        let edge = graph
            .edges
            .iter()
            .find(|e| e.from.starts_with("job:"))
            .expect("a job that names a table must link to the model");
        assert_eq!(edge.to, "model:app::models::Post");
    }

    #[test]
    fn a_mounted_route_carries_its_served_path_and_auth() {
        let graph = build(
            &[mounted("GET", "/api/v1/posts", "index")],
            0,
            &[route("index", "GET", "/posts", &[], &[])],
            &[],
            &[],
            &[],
        );
        let facts = graph.nodes[0].route.as_ref().expect("route facts");
        assert!(facts.mounted);
        assert_eq!(facts.path, "/api/v1/posts", "the mounted path must win");
        assert_eq!(facts.auth.as_ref().expect("auth").roles, vec!["admin"]);
        assert_eq!(graph.completeness.mounted_routes, 1);
        assert!(graph.completeness.unmounted_routes.is_empty());
    }

    #[test]
    fn a_declared_route_the_app_never_mounts_is_still_a_node() {
        let graph = build(
            &[],
            0,
            &[route("orphan", "GET", "/orphan", &[], &[])],
            &[],
            &[],
            &[],
        );
        assert_eq!(graph.nodes.len(), 1);
        assert!(!graph.nodes[0].route.as_ref().expect("route facts").mounted);
        assert_eq!(
            graph.completeness.unmounted_routes,
            vec!["route:app::routes::posts::orphan"]
        );
    }

    #[test]
    fn a_mounted_route_with_no_declaration_is_named_not_dropped() {
        let graph = build(
            &[mounted("GET", "/actuator/health", "actuator")],
            0,
            &[],
            &[],
            &[],
            &[],
        );
        assert_eq!(
            graph.completeness.unmodelled_mounted_routes,
            vec!["GET /actuator/health"]
        );
    }

    #[test]
    fn an_unresolvable_symbol_produces_no_edge() {
        let graph = build(
            &[],
            0,
            &[route(
                "show",
                "GET",
                "/posts",
                &["Db"],
                &["Ok", "Vec", "String"],
            )],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[],
        );
        assert!(graph.edges.is_empty(), "{:?}", graph.edges);
    }

    #[test]
    fn a_repository_also_declares_its_models_relation_tables() {
        // `#[votable(by = User)]` on `Post` puts `react`/`reaction_of` on
        // `PgPostRepository`, and those write the `votes` edge table. A route
        // holding the repository can reach `votes` without ever naming it, so
        // without this edge `autumn graph touches votes` misses the upvote and
        // downvote routes entirely.
        let mut post = model("Post", "app::models::Post", "posts");
        post.relations = &["votes"];
        let graph = build(
            &[],
            0,
            &[route(
                "upvote",
                "POST",
                "/upvote",
                &["PgPostRepository"],
                &[],
            )],
            &[post, model("Vote", "app::models::Vote", "votes")],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[],
        );
        let answer = crate::graph::query::touches(&graph, "votes").expect("votes must resolve");
        assert_eq!(
            answer
                .routes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["upvote"],
        );
        let edge = graph
            .edges
            .iter()
            .find(|e| e.from.starts_with("repository:") && e.to.ends_with("::Vote"))
            .expect("the repository must declare the relation edge");
        assert_eq!(edge.provenance, Provenance::Declaration);
        assert_eq!(edge.access, Access::ReadWrite);
    }

    #[test]
    fn a_relation_table_no_model_maps_produces_no_edge() {
        // `#[commentable]` defaults to a shared `comments` table. An app with
        // no `Comment` model has nothing for that to point at, and a dangling
        // edge would be worse than none.
        let mut post = model("Post", "app::models::Post", "posts");
        post.relations = &["comments"];
        let graph = build(
            &[],
            0,
            &[],
            &[post],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[],
        );
        assert_eq!(
            graph.edges.len(),
            1,
            "only the repository's own model edge: {:?}",
            graph.edges
        );
    }

    #[test]
    fn a_models_relation_tables_are_visible_in_the_document() {
        let mut post = model("Post", "app::models::Post", "posts");
        post.relations = &["votes"];
        let graph = build(&[], 0, &[], &[post], &[], &[]);
        assert_eq!(
            graph.nodes[0]
                .model
                .as_ref()
                .expect("model facts")
                .relations,
            vec!["votes"]
        );
    }

    #[test]
    fn markup_that_merely_names_a_mutation_does_not_make_a_get_a_write() {
        // A maud template writes `hx-delete=(...)` on a link. The route is a
        // GET: its declared method is the contract, and reading `delete` out of
        // an attribute name would report a read-only page as a writer.
        let graph = build(
            &[],
            0,
            &[route("show", "GET", "/posts/{id}", &[], &["posts"])],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[],
        );
        assert_eq!(graph.edges[0].access, Access::Read);
    }

    #[test]
    fn a_jobs_access_comes_from_its_mutation_evidence() {
        // A job declares no HTTP method, so the tokens are all there is.
        let mut writer = job("hot_rank", JobKind::Scheduled, &["posts"]);
        writer.mutating = true;
        let graph = build(
            &[],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[writer],
        );
        assert_eq!(graph.edges[0].access, Access::Write);
    }

    #[test]
    fn a_repository_auto_api_route_is_attributed_to_its_repository() {
        // `#[repository(api = "/api/posts")]` mounts CRUD routes that no
        // `#[route]` declares. Leaving them out would mean `autumn graph
        // touches posts` missed the entire REST surface of the table.
        let graph = build(
            &[
                mounted_by_repository("GET", "/api/posts", "/api/posts"),
                mounted_by_repository("POST", "/api/posts", "/api/posts"),
            ],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[repository_with_api(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
                "/api/posts",
            )],
            &[],
        );
        let generated = graph
            .nodes
            .iter()
            .filter(|n| n.route.as_ref().is_some_and(|r| r.generated_by.is_some()))
            .count();
        assert_eq!(generated, 2, "{:?}", graph.nodes);
        assert!(
            graph.completeness.unmodelled_mounted_routes.is_empty(),
            "an attributed auto-API route is no longer unaccounted for: {:?}",
            graph.completeness.unmodelled_mounted_routes
        );
        assert_eq!(graph.completeness.generated_routes, 2);
        assert_eq!(
            graph.completeness.declared_routes, 0,
            "a generated route is not a macro-declared handler"
        );
        let post_edge = graph
            .edges
            .iter()
            .find(|e| e.from.contains("POST") && e.to.starts_with("repository:"))
            .expect("the mutating auto-API route must link to its repository");
        assert_eq!(post_edge.access, Access::Write);
        assert_eq!(post_edge.provenance, Provenance::Declaration);
    }

    #[test]
    fn an_auto_api_route_reaches_the_model_through_its_repository() {
        let graph = build(
            &[mounted_by_repository(
                "GET",
                "/api/posts/{id}",
                "/api/posts",
            )],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[repository_with_api(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
                "/api/posts",
            )],
            &[],
        );
        let answer = crate::graph::query::touches(&graph, "posts").expect("posts must resolve");
        assert_eq!(
            answer.routes.len(),
            1,
            "the auto-API route must reach the model through its repository: {:?}",
            answer.routes
        );
    }

    #[test]
    fn a_ws_mount_under_an_api_prefix_is_not_claimed_as_an_auto_api_route() {
        // A `#[ws("/api/posts/live")]` handler is in the mounted table and has
        // no route descriptor, but it is not a CRUD route the repository
        // generated. Claiming it would report a read-only socket as a REST
        // endpoint that writes `posts`.
        let graph = build(
            &[mounted("WS", "/api/posts/live", "live")],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[repository_with_api(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
                "/api/posts",
            )],
            &[],
        );
        assert_eq!(graph.completeness.generated_routes, 0, "{:?}", graph.nodes);
        assert_eq!(
            graph.completeness.unmodelled_mounted_routes,
            vec!["WS /api/posts/live"]
        );
    }

    #[test]
    fn a_scoped_auto_api_route_is_still_attributed_to_its_repository() {
        // Mounted inside `.scoped("/v1", …)` the CRUD surface is served at
        // `/v1/api/posts`, which does not start with the declared prefix.
        // Inferring ownership from the path dropped these routes entirely
        // (Codex round 5); the route carries its own repository, so nothing
        // has to be guessed.
        let graph = build(
            &[mounted_by_repository("GET", "/v1/api/posts", "/api/posts")],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[repository_with_api(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
                "/api/posts",
            )],
            &[],
        );
        assert_eq!(graph.completeness.generated_routes, 1, "{:?}", graph.nodes);
        assert!(graph.completeness.unmodelled_mounted_routes.is_empty());
    }

    #[test]
    fn a_hand_written_route_under_an_api_prefix_is_not_claimed_as_generated() {
        // The inverse: a plugin GET beneath `/api/posts` that no repository
        // generated must not be attributed to one.
        let graph = build(
            &[mounted("GET", "/api/posts/search", "search")],
            0,
            &[],
            &[model("Post", "app::models::Post", "posts")],
            &[repository_with_api(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
                "/api/posts",
            )],
            &[],
        );
        assert_eq!(graph.completeness.generated_routes, 0);
        assert_eq!(
            graph.completeness.unmodelled_mounted_routes,
            vec!["GET /api/posts/search"]
        );
    }

    #[test]
    fn a_handler_mounted_twice_accounts_for_both_mounts() {
        // The same handler in a top-level `routes![]` and in a scoped group.
        // Reporting the second mount as "no macro declaration" would send a
        // reader hunting for a handler that is right there.
        let graph = build(
            &[
                mounted("GET", "/posts", "index"),
                mounted("GET", "/admin/posts", "index"),
            ],
            0,
            &[route("index", "GET", "/posts", &[], &[])],
            &[],
            &[],
            &[],
        );
        assert!(
            graph.completeness.unmodelled_mounted_routes.is_empty(),
            "{:?}",
            graph.completeness.unmodelled_mounted_routes
        );
        let facts = graph.nodes[0].route.as_ref().expect("route facts");
        assert_eq!(facts.path, "/admin/posts");
        assert_eq!(facts.also_mounted_at, vec!["/posts"]);
    }

    #[test]
    fn the_document_counts_routers_it_cannot_enumerate() {
        let graph = build(&[], 3, &[], &[], &[], &[]);
        assert_eq!(graph.completeness.opaque_mounted_routers, 3);
        assert!(
            graph.summary().contains("cannot enumerate"),
            "{}",
            graph.summary()
        );
    }

    #[test]
    fn a_mounted_route_no_repository_generated_stays_unmodelled() {
        let graph = build(
            &[mounted("GET", "/api/postscript", "other")],
            0,
            &[],
            &[],
            &[repository_with_api(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
                "/api/posts",
            )],
            &[],
        );
        assert_eq!(
            graph.completeness.unmodelled_mounted_routes,
            vec!["GET /api/postscript"],
            "a prefix match must respect segment boundaries"
        );
    }

    #[test]
    fn a_dependencys_registry_path_is_shortened_to_its_crate_relative_tail() {
        assert_eq!(
            location(
                "/home/someone/.cargo/registry/src/index.crates.io-6f17d22b/plug-0.1.0/src/r.rs",
                7
            ),
            "plug-0.1.0/src/r.rs:7"
        );
        assert_eq!(location("src/models.rs", 7), "src/models.rs:7");
    }

    #[test]
    fn an_uppercase_raw_sql_table_still_resolves() {
        // PostgreSQL folds unquoted identifiers, so `FROM POSTS` is the `posts`
        // table. Dropping the edge would be a false negative in an impact
        // answer — the one failure this feature cannot afford.
        let graph = build(
            &[],
            0,
            &[RouteGraphDescriptor {
                sql_symbols: &["FROM", "POSTS", "SELECT"],
                ..route(
                    "report",
                    "GET",
                    "/report",
                    &[],
                    &["POSTS", "SELECT", "FROM"],
                )
            }],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[],
        );
        assert_eq!(
            graph.edges.len(),
            1,
            "an uppercase table name must resolve: {:?}",
            graph.edges
        );
        assert_eq!(graph.edges[0].to, "model:app::models::Post");
    }

    #[test]
    fn a_rust_type_named_like_a_table_does_not_resolve_to_it() {
        // A DTO called `Posts` is a type-shaped candidate, not a SQL word.
        // Folding every candidate made it resolve to the `posts` table — a
        // false dependency the graph would then report forever.
        let graph = build(
            &[],
            0,
            &[route("show", "GET", "/show", &[], &["Posts"])],
            &[model("Post", "app::models::Post", "posts")],
            &[],
            &[],
        );
        assert!(graph.edges.is_empty(), "{:?}", graph.edges);
    }

    #[test]
    fn a_type_name_is_not_matched_case_insensitively() {
        // `Post` and `post` are different Rust items; only SQL folds case.
        let graph = build(
            &[],
            0,
            &[route("show", "GET", "/show", &[], &["POST"])],
            &[model("Comment", "app::models::Comment", "comments")],
            &[],
            &[],
        );
        assert!(graph.edges.is_empty(), "{:?}", graph.edges);
    }

    #[test]
    fn a_windows_source_path_is_normalized() {
        assert_eq!(
            location(r"C:\src\app\src\models.rs", 3),
            "C:/src/app/src/models.rs:3"
        );
    }

    #[test]
    fn the_document_carries_its_own_limits() {
        let graph = build(&[], 0, &[], &[], &[], &[]);
        assert_eq!(graph.limits.len(), LIMITS.len());
        assert!(graph.limits.iter().any(|l| l.contains("helper function")));
    }

    #[test]
    fn the_graph_round_trips_through_json() {
        let graph = build(
            &[mounted("POST", "/posts", "create")],
            0,
            &[route(
                "create",
                "POST",
                "/posts",
                &["PgPostRepository"],
                &["posts"],
            )],
            &[model("Post", "app::models::Post", "posts")],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[job("digest", JobKind::Job, &["posts"])],
        );
        let decoded: ArchitectureGraph =
            serde_json::from_str(&graph.to_json()).expect("graph must round-trip");
        assert_eq!(decoded, graph);
    }

    #[test]
    fn the_dump_is_recovered_from_a_noisy_stdout() {
        let graph = build(&[], 0, &[], &[], &[], &[]);
        let json = serde_json::to_string(&graph).expect("serialize");
        let stdout = format!("starting up\n{ARCHITECTURE_GRAPH_MARKER}{json}\ndone\n");
        assert_eq!(parse_manifest_dump(&stdout), Some(graph));
    }

    #[test]
    fn parsing_stdout_without_the_marker_yields_nothing() {
        assert!(parse_manifest_dump("nothing to see here").is_none());
    }

    #[test]
    fn nodes_and_edges_are_emitted_in_a_stable_order() {
        let models = [
            model("Zebra", "app::models::Zebra", "zebras"),
            model("Alpha", "app::models::Alpha", "alphas"),
        ];
        let first = build(&[], 0, &[], &models, &[], &[]);
        let reversed: Vec<ModelGraphDescriptor> = models.iter().rev().copied().collect();
        let second = build(&[], 0, &[], &reversed, &[], &[]);
        assert_eq!(
            first, second,
            "registration order must not change the document"
        );
        assert_eq!(first.nodes[0].name, "Alpha");
    }

    #[test]
    fn the_summary_names_every_section_it_has_rows_for() {
        let graph = build(
            &[mounted("POST", "/posts", "create")],
            0,
            &[route(
                "create",
                "POST",
                "/posts",
                &["PgPostRepository"],
                &[],
            )],
            &[model("Post", "app::models::Post", "posts")],
            &[repository(
                "PostRepository",
                "PgPostRepository",
                "Post",
                "posts",
            )],
            &[job("digest", JobKind::Job, &["posts"])],
        );
        let summary = graph.summary();
        assert!(summary.contains("POST /posts"), "{summary}");
        assert!(summary.contains("Post (table posts)"), "{summary}");
        assert!(summary.contains("PostRepository over Post"), "{summary}");
        assert!(summary.contains("Limits of this derivation"), "{summary}");
    }
}
