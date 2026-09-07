//! The compile-time application architecture graph (issue #1747).
//!
//! Autumn declares every architectural element through framework-owned
//! proc-macros — `#[route]`/`#[static_get]`, `#[model]`, `#[repository]`,
//! `#[job]`/`#[scheduled]`/`#[task]`. This module retains that knowledge as a
//! typed, queryable graph instead of letting it evaporate after expansion: one
//! node per declared element, one edge per declared or derived relationship,
//! assembled inside the binary itself.
//!
//! # Why it is assembled from `inventory`
//!
//! Which elements an application actually contains is a whole-binary fact. A
//! model declared in one crate can be written by a route declared in another,
//! or in a plugin the app merely depends on, and link-time `inventory`
//! collection is the only place all of those registrations exist together.
//! `autumn graph` therefore builds the app and runs it under
//! `AUTUMN_DUMP_GRAPH=1` to read the graph back — the same shape as
//! `autumn data-flow` (#1654), `autumn agents manifest` (#1691) and
//! `autumn routes audit` (#1604). The same graph is served from the running
//! binary at `/actuator/graph`, so nothing has to be kept in sync with a side
//! file.
//!
//! # What the derivation can and cannot see
//!
//! Node identity is *declared*: a node exists because a macro expanded, so no
//! model, route, repository or job can be missing from the graph.
//!
//! Edges are different. `#[repository(Post)]` states its model outright, so
//! repository → model edges are declarations. A route or job, by contrast, is
//! linked by the names its own tokens mention — the repository it takes as an
//! extractor, the model or `diesel` table module its body names, the table a
//! raw-SQL literal names. That is a *name-based* derivation over one item's
//! tokens, deliberately biased toward over-reporting:
//!
//! * it cannot follow a call into a helper function in another module — the
//!   dynamic call-graph tracing the first slice explicitly excludes;
//! * it cannot resolve a type alias or a `use ... as ...` rename;
//! * it matches on names, so a model named like a common type is linked
//!   wherever that name appears.
//!
//! Every edge therefore carries its [`Provenance`], and the manifest carries
//! these limits in its own `limits` section, so the document cannot be read as
//! more than it is.
//!
//! See `docs/guide/architecture-graph.md`.

pub mod manifest;
pub mod query;

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use manifest::ArchitectureGraph;

/// The graph this process serves from `/actuator/graph`.
///
/// A process-wide `OnceLock` rather than a field on `AppState`: the graph is a
/// property of the *binary*, identical for every request and every clone of the
/// state, and threading an immutable whole-binary fact through the request state
/// would only create a second place for it to go stale.
static SERVED_GRAPH: OnceLock<ServedGraph> = OnceLock::new();

/// The installed graph, alongside the bytes `/actuator/graph` serves.
///
/// The document can never change once installed, so re-serializing it per
/// request would walk every node and edge and allocate the whole body again —
/// on what is comfortably the largest actuator payload an app has.
struct ServedGraph {
    graph: ArchitectureGraph,
    json: Vec<u8>,
}

/// Publish the graph this process serves.
///
/// `pub(crate)` on purpose: whichever caller wins decides what
/// `/actuator/graph` reports for the life of the process, and that is the
/// framework's own account of the binary — not something a dependency should
/// be able to pre-empt during static initialization.
///
/// Idempotent: the first call wins and later ones are ignored, so a process
/// that builds several routers cannot make the endpoint's answer depend on
/// which one was built last.
pub(crate) fn install(graph: ArchitectureGraph) {
    let _ = SERVED_GRAPH.set(ServedGraph {
        json: serde_json::to_vec(&graph).unwrap_or_default(),
        graph,
    });
}

/// The graph this process serves, if one has been installed.
#[must_use]
pub fn served() -> Option<&'static ArchitectureGraph> {
    SERVED_GRAPH.get().map(|served| &served.graph)
}

/// The pre-serialized bytes of [`served`], if one has been installed.
pub(crate) fn served_json() -> Option<&'static [u8]> {
    SERVED_GRAPH.get().map(|served| served.json.as_slice())
}

// ── Descriptors published by the macros ──────────────────────────────

/// One `#[route]`/`#[static_get]` handler, published by the route macros.
///
/// Registered from the macro rather than read off the mounted route table so a
/// handler that is declared but never passed to `routes![]` is still visible:
/// the completeness section names it, which is the drift the build gate exists
/// to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteGraphDescriptor {
    /// Handler function name, e.g. `"show"`.
    pub handler: &'static str,
    /// Module path of the handler, from `module_path!()`. With [`Self::handler`]
    /// this is the join key against the mounted route table, so two modules
    /// that each define a `show` cannot collapse into one node.
    pub module_path: &'static str,
    /// Uppercase HTTP method, e.g. `"GET"`.
    pub method: &'static str,
    /// Path as declared on the macro, *before* any scope prefix is applied.
    /// The mounted path replaces it when the route is found in the route table.
    pub path: &'static str,
    /// Whether the handler was declared with `#[static_get]`.
    pub static_route: bool,
    /// `file!()` of the handler.
    pub file: &'static str,
    /// `line!()` of the handler.
    pub line: u32,
    /// Candidate names read off the handler's *signature* — its extractors.
    pub signature_symbols: &'static [&'static str],
    /// Candidate names read off the handler's *body*, including identifiers
    /// recovered from raw-SQL string literals.
    pub body_symbols: &'static [&'static str],
    /// The subset of [`Self::body_symbols`] that came from a SQL literal.
    ///
    /// Only these may be matched against a table name case-insensitively: an
    /// unquoted SQL identifier folds, a Rust type name does not. Without the
    /// split, a DTO named `Posts` would resolve to the `posts` table.
    pub sql_symbols: &'static [&'static str],
}

inventory::collect!(RouteGraphDescriptor);

/// One `#[model]` struct, published by the model macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelGraphDescriptor {
    /// The model type's name, e.g. `"Post"`.
    pub model: &'static str,
    /// The model type's module-qualified path — the join key, so two crates
    /// that each define a `Post` cannot share one node.
    pub model_path: &'static str,
    /// The database table the model maps to.
    pub table: &'static str,
    /// Further tables the model's *declared relations* touch, sorted.
    ///
    /// `#[votable(by = User)]` puts `react`/`reaction_of` on the model's
    /// repository, and those write the `votes` edge table; `#[commentable]`
    /// does the same for the shared comments table. A route holding the
    /// repository reaches those tables without ever naming them, so the
    /// relation has to be declared here or the edge cannot exist at all.
    pub relations: &'static [&'static str],
    /// Module path the model was declared in.
    pub module_path: &'static str,
    /// `file!()` of the declaration.
    pub file: &'static str,
    /// `line!()` of the declaration.
    pub line: u32,
}

inventory::collect!(ModelGraphDescriptor);

/// One `#[repository]` trait, published by the repository macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryGraphDescriptor {
    /// The declared trait name, e.g. `"PostRepository"`.
    pub repository: &'static str,
    /// The generated implementation type, e.g. `"PgPostRepository"` — the name
    /// a handler writes when it takes the repository as an extractor.
    pub implementation: &'static str,
    /// The model the repository is declared over.
    pub model: &'static str,
    /// The table that model maps to, as resolved by the repository macro
    /// (which honours a `table = "..."` override).
    pub table: &'static str,
    /// Mount prefix of the generated REST auto-API, or `""` when the
    /// repository declares none.
    pub api: &'static str,
    /// Module path the repository was declared in.
    pub module_path: &'static str,
    /// `file!()` of the declaration.
    pub file: &'static str,
    /// `line!()` of the declaration.
    pub line: u32,
}

inventory::collect!(RepositoryGraphDescriptor);

/// What kind of background work a [`JobGraphDescriptor`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// `#[job]` — a durable queued job.
    Job,
    /// `#[scheduled]` — a recurring scheduled task.
    Scheduled,
    /// `#[task]` — a one-off operator-invoked task.
    Task,
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Job => "job",
            Self::Scheduled => "scheduled",
            Self::Task => "task",
        })
    }
}

/// One `#[job]`, `#[scheduled]` or `#[task]` handler, published by the job macros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobGraphDescriptor {
    /// The declared job/task name — its runtime identity.
    pub name: &'static str,
    /// Which macro declared it.
    pub kind: JobKind,
    /// Handler function name.
    pub handler: &'static str,
    /// Module path of the handler.
    pub module_path: &'static str,
    /// Schedule expression for `#[scheduled]`, `""` otherwise.
    pub schedule: &'static str,
    /// Whether the handler's tokens carry evidence of a mutation.
    pub mutating: bool,
    /// `file!()` of the handler.
    pub file: &'static str,
    /// `line!()` of the handler.
    pub line: u32,
    /// Candidate names read off the handler's signature.
    pub signature_symbols: &'static [&'static str],
    /// Candidate names read off the handler's body.
    pub body_symbols: &'static [&'static str],
    /// The subset of [`Self::body_symbols`] that came from a SQL literal.
    pub sql_symbols: &'static [&'static str],
}

inventory::collect!(JobGraphDescriptor);

// ── Inputs the app supplies ──────────────────────────────────────────

/// The declared authorization requirement of a mounted route.
///
/// Read off the two sources `route_listing::classify` reads, so the graph
/// states the same auth posture `autumn routes audit` proves rather than a
/// second derivation of it: the route's `ApiDoc`, which the
/// `#[secured]`/`#[authorize]`/`#[public]` macros populate, **and** its
/// `RepositoryApiMeta`, which is where a `#[repository(api = "...")]` auto-API
/// route's own policy and scope guards live. Reading only the first reported
/// those generated-but-guarded endpoints as `auth: none`.
///
/// The bools are deliberately independent rather than one enum: a route can be
/// `#[secured]` *and* policy-guarded, and the document has to say which of the
/// declarations is present, not merely how protected the route ends up.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a serialized document field per independent, co-occurring declaration"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteAuth {
    /// Whether the handler carries `#[secured]`.
    pub secured: bool,
    /// Roles required by `#[secured("role")]`, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Scopes required by `#[secured(scopes = [...])]`, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Whether the route is guarded by dynamic policy authorization.
    ///
    /// True for a handler carrying `#[authorize]`, and also for a repository
    /// auto-API route whose `#[repository(api = "...", policy = ...)]` registers
    /// the guard on the *repository* rather than the generated handler — the
    /// generated `ApiDoc` leaves this at its default in that case, which is why
    /// `route_listing::classify` ORs the two and why this does too.
    #[serde(default)]
    pub policy: bool,
    /// Whether a repository auto-API route is gated by a registered scope check.
    ///
    /// Separate from [`scopes`](Self::scopes) because there is no scope *name*
    /// to record: `#[repository(api = "...", scope = ...)]` enforces the scope
    /// through a type-erased registry probe, leaving both the handler `ApiDoc`
    /// and `RepositoryApiMeta::has_policy` at their defaults. Without this the
    /// route would serialize as `auth: none` despite being gated.
    ///
    /// Omitted from the document when false, so a graph containing no such
    /// route serializes exactly as it did before this field existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repository_scope: bool,
    /// Whether the route is explicitly declared `#[public]`.
    #[serde(default)]
    pub public: bool,
}

impl RouteAuth {
    /// A one-line rendering of the requirement, for the human report.
    #[must_use]
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.secured {
            parts.push("secured".to_owned());
        }
        if !self.roles.is_empty() {
            parts.push(format!("roles[{}]", self.roles.join(",")));
        }
        if !self.scopes.is_empty() {
            parts.push(format!("scopes[{}]", self.scopes.join(",")));
        }
        if self.policy {
            parts.push("policy".to_owned());
        }
        if self.repository_scope {
            parts.push("repository-scope".to_owned());
        }
        if self.public {
            parts.push("public".to_owned());
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join("+")
        }
    }
}

/// The slice of a mounted [`Route`](crate::Route) the graph needs.
///
/// Built in `AppBuilder`'s dump mode from the route table, so this module needs
/// no dependency on the router. The *mounted* path is the one carried here: a
/// scoped group's children hold only their child path on the `Route` itself,
/// and recording `/items` for a route an operator calls at `/api/v1/items`
/// would make a scope rename invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedRoute {
    /// Uppercase HTTP method.
    pub method: String,
    /// Full mounted path, with any scope prefix applied.
    pub path: String,
    /// Handler function name.
    pub handler: String,
    /// Module path of the handler, from the route's `ApiDoc`.
    pub module_path: String,
    /// The route's declared authorization requirement.
    pub auth: RouteAuth,
    /// The `api = "..."` prefix of the `#[repository]` that generated this
    /// route, when one did.
    ///
    /// Carried from the route's own `RepositoryApiMeta` rather than inferred
    /// from its path: a CRUD surface mounted inside `.scoped("/v1", …)` no
    /// longer *starts* with the declared prefix, and a hand-written route can
    /// sit under one without being generated by it. Reconstructing ownership
    /// from the path got both wrong (Codex round 5).
    pub repository_api: Option<String>,
}

// ── Graph vocabulary ─────────────────────────────────────────────────

/// What an element in the graph *is*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A `#[route]` handler.
    Route,
    /// A `#[static_get]` handler — a route that is also pre-rendered.
    StaticRoute,
    /// A `#[model]` struct.
    Model,
    /// A `#[repository]` trait.
    Repository,
    /// A `#[job]` handler.
    Job,
    /// A `#[scheduled]` handler.
    ScheduledTask,
    /// A `#[task]` handler.
    OneOffTask,
}

impl NodeKind {
    /// The `kind:` prefix this node kind uses in a node id.
    #[must_use]
    pub const fn id_prefix(self) -> &'static str {
        match self {
            Self::Route | Self::StaticRoute => "route",
            Self::Model => "model",
            Self::Repository => "repository",
            Self::Job | Self::ScheduledTask | Self::OneOffTask => "job",
        }
    }

    /// Whether this kind is an entry point — something that *acts on* data
    /// rather than being acted upon. These are the nodes an impact query
    /// reports.
    #[must_use]
    pub const fn is_entry_point(self) -> bool {
        matches!(
            self,
            Self::Route | Self::StaticRoute | Self::Job | Self::ScheduledTask | Self::OneOffTask
        )
    }
}

impl std::fmt::Display for NodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Route => "route",
            Self::StaticRoute => "static route",
            Self::Model => "model",
            Self::Repository => "repository",
            Self::Job => "job",
            Self::ScheduledTask => "scheduled task",
            Self::OneOffTask => "one-off task",
        })
    }
}

/// Why an edge exists — the evidence that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Stated outright by the macro, e.g. `#[repository(Post)]`.
    Declaration,
    /// Recovered from the item's signature — an extractor it declares.
    Signature,
    /// Recovered from the item's body tokens.
    Body,
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Declaration => "declaration",
            Self::Signature => "signature",
            Self::Body => "body",
        })
    }
}

/// Whether the edge's source reads or writes its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// No mutation evidence: a safe HTTP method with no mutating token.
    Read,
    /// A mutating token, or a route declared with a non-safe HTTP method.
    Write,
    /// A repository over its model: the generated CRUD surface does both.
    ReadWrite,
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read/write",
        })
    }
}

/// HTTP methods that declare no intent to mutate.
const SAFE_METHODS: &[&str] = &["GET", "HEAD", "OPTIONS", "TRACE"];

/// Whether a declared HTTP method is safe (read-only by declaration).
#[must_use]
pub fn is_safe_method(method: &str) -> bool {
    SAFE_METHODS.contains(&method.to_ascii_uppercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_auth_label_names_every_declared_requirement() {
        let auth = RouteAuth {
            secured: true,
            roles: vec!["admin".to_owned()],
            scopes: vec!["posts:write".to_owned()],
            policy: true,
            repository_scope: false,
            public: false,
        };
        assert_eq!(
            auth.label(),
            "secured+roles[admin]+scopes[posts:write]+policy"
        );
    }

    #[test]
    fn route_auth_label_says_none_when_nothing_is_declared() {
        assert_eq!(RouteAuth::default().label(), "none");
    }

    /// A repository auto-API route gated only by a registered scope check has
    /// nothing in `ApiDoc` to show for it — no `secured`, no policy, and no
    /// scope *name*, because the check is a type-erased registry probe. It must
    /// still not read as unauthenticated.
    #[test]
    fn a_repository_scope_guard_is_never_reported_as_no_auth() {
        let auth = RouteAuth {
            repository_scope: true,
            ..RouteAuth::default()
        };
        assert_ne!(auth.label(), "none");
        assert_eq!(auth.label(), "repository-scope");
    }

    /// The field is omitted when false, so adding it did not rewrite the
    /// serialization of every route node that has no repository scope guard.
    #[test]
    fn the_repository_scope_flag_is_absent_from_a_document_that_has_none() {
        let json = serde_json::to_string(&RouteAuth::default()).expect("serializable");
        assert!(
            !json.contains("repository_scope"),
            "an unguarded route must serialize as it did before the field existed: {json}"
        );
        let guarded = serde_json::to_string(&RouteAuth {
            repository_scope: true,
            ..RouteAuth::default()
        })
        .expect("serializable");
        assert!(guarded.contains("\"repository_scope\":true"), "{guarded}");
    }

    #[test]
    fn safe_methods_are_case_insensitive() {
        assert!(is_safe_method("get"));
        assert!(is_safe_method("GET"));
        assert!(!is_safe_method("post"));
    }

    #[test]
    fn only_routes_and_jobs_are_entry_points() {
        assert!(NodeKind::Route.is_entry_point());
        assert!(NodeKind::StaticRoute.is_entry_point());
        assert!(NodeKind::Job.is_entry_point());
        assert!(NodeKind::ScheduledTask.is_entry_point());
        assert!(NodeKind::OneOffTask.is_entry_point());
        assert!(!NodeKind::Model.is_entry_point());
        assert!(!NodeKind::Repository.is_entry_point());
    }
}
