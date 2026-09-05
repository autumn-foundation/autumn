//! Queries over the application architecture graph (issue #1747).
//!
//! Two verbs, both a breadth-first walk of the graph's reverse edges:
//!
//! * [`touches`] — which routes and jobs reach a model or table;
//! * [`impact`] — everything a change to a model would affect, repositories
//!   included.
//!
//! The walk is transitive on purpose. A route that never names `Post` but takes
//! `PgPostRepository` as an extractor *does* touch the `posts` table, and an
//! impact answer that omitted it would be a false negative — the one failure
//! this feature cannot afford.

use std::collections::{BTreeSet, VecDeque};

use super::NodeKind;
use super::manifest::{ArchitectureGraph, GraphNode};

/// Which routes and jobs reach a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touches<'g> {
    /// The node the query resolved to.
    pub target: &'g GraphNode,
    /// Routes (including static routes) that reach it, sorted by node id.
    pub routes: Vec<&'g GraphNode>,
    /// Jobs, scheduled tasks and one-off tasks that reach it, sorted by node id.
    pub jobs: Vec<&'g GraphNode>,
}

/// Everything a change to a target would affect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impact<'g> {
    /// The node the query resolved to.
    pub target: &'g GraphNode,
    /// Repositories built over it, sorted by node id.
    pub repositories: Vec<&'g GraphNode>,
    /// Routes that reach it, directly or through a repository.
    pub routes: Vec<&'g GraphNode>,
    /// Jobs that reach it, directly or through a repository.
    pub jobs: Vec<&'g GraphNode>,
}

/// Resolve a query string to a node.
///
/// Accepts, in order of specificity: an exact node id, a model's
/// module-qualified path, a model or repository name, a table name, a
/// repository's generated implementation type, or a job name. Matching is
/// case-sensitive for type names — `Post` and `post` are different Rust
/// items — but a table name is matched case-insensitively, because SQL is.
///
/// Returns `None` when nothing answers to the name, and the *first* match in
/// node-id order when several do, so the answer is deterministic.
#[must_use]
pub fn resolve<'g>(graph: &'g ArchitectureGraph, name: &str) -> Option<&'g GraphNode> {
    if let Some(node) = graph.node(name) {
        return Some(node);
    }
    let by = |f: &dyn Fn(&GraphNode) -> bool| graph.nodes.iter().find(|n| f(n));
    by(&|n: &GraphNode| n.id.strip_prefix("model:") == Some(name))
        .or_else(|| by(&|n: &GraphNode| n.kind == NodeKind::Model && n.name == name))
        .or_else(|| {
            by(&|n: &GraphNode| {
                n.model
                    .as_ref()
                    .is_some_and(|m| m.table.eq_ignore_ascii_case(name))
            })
        })
        .or_else(|| by(&|n: &GraphNode| n.kind == NodeKind::Repository && n.name == name))
        .or_else(|| {
            by(&|n: &GraphNode| {
                n.repository
                    .as_ref()
                    .is_some_and(|r| r.implementation == name)
            })
        })
        .or_else(|| by(&|n: &GraphNode| n.job.is_some() && n.name == name))
}

/// Every node that reaches `target`, walking edges backwards.
///
/// Excludes the target itself. Cycle-safe: a visited set bounds the walk, so a
/// repository pair that referenced each other could not spin here.
fn dependents<'g>(graph: &'g ArchitectureGraph, target: &str) -> Vec<&'g GraphNode> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(target);
    seen.insert(target);
    let mut found: BTreeSet<&str> = BTreeSet::new();
    while let Some(current) = queue.pop_front() {
        for edge in graph.edges.iter().filter(|e| e.to == current) {
            if seen.insert(edge.from.as_str()) {
                found.insert(edge.from.as_str());
                queue.push_back(edge.from.as_str());
            }
        }
    }
    found
        .into_iter()
        .filter_map(|id| graph.node(id))
        .collect()
}

/// Which routes and jobs touch the model, table or repository `name` denotes.
#[must_use]
pub fn touches<'g>(graph: &'g ArchitectureGraph, name: &str) -> Option<Touches<'g>> {
    let target = resolve(graph, name)?;
    let dependents = dependents(graph, &target.id);
    Some(Touches {
        target,
        routes: dependents
            .iter()
            .copied()
            .filter(|n| matches!(n.kind, NodeKind::Route | NodeKind::StaticRoute))
            .collect(),
        jobs: dependents
            .iter()
            .copied()
            .filter(|n| {
                matches!(
                    n.kind,
                    NodeKind::Job | NodeKind::ScheduledTask | NodeKind::OneOffTask
                )
            })
            .collect(),
    })
}

/// The transitive set of elements a change to `name` would affect.
#[must_use]
pub fn impact<'g>(graph: &'g ArchitectureGraph, name: &str) -> Option<Impact<'g>> {
    let target = resolve(graph, name)?;
    let dependents = dependents(graph, &target.id);
    Some(Impact {
        target,
        repositories: dependents
            .iter()
            .copied()
            .filter(|n| n.kind == NodeKind::Repository)
            .collect(),
        routes: dependents
            .iter()
            .copied()
            .filter(|n| matches!(n.kind, NodeKind::Route | NodeKind::StaticRoute))
            .collect(),
        jobs: dependents
            .iter()
            .copied()
            .filter(|n| {
                matches!(
                    n.kind,
                    NodeKind::Job | NodeKind::ScheduledTask | NodeKind::OneOffTask
                )
            })
            .collect(),
    })
}

/// Render a [`Touches`] answer as a human report.
#[must_use]
pub fn format_touches(answer: &Touches<'_>) -> String {
    let mut out = format!(
        "{} {} is touched by {} route(s) and {} job(s)\n",
        answer.target.kind,
        answer.target.name,
        answer.routes.len(),
        answer.jobs.len()
    );
    for node in &answer.routes {
        out.push_str(&format!("  route  {}\n", node.label()));
    }
    for node in &answer.jobs {
        out.push_str(&format!("  {:<6} {}\n", node.kind.to_string(), node.label()));
    }
    out
}

/// Render an [`Impact`] answer as a human report.
#[must_use]
pub fn format_impact(answer: &Impact<'_>) -> String {
    let mut out = format!(
        "Changing {} {} affects {} repositor(y/ies), {} route(s) and {} job(s)\n",
        answer.target.kind,
        answer.target.name,
        answer.repositories.len(),
        answer.routes.len(),
        answer.jobs.len()
    );
    for node in &answer.repositories {
        out.push_str(&format!("  repository {}\n", node.label()));
    }
    for node in &answer.routes {
        out.push_str(&format!("  route      {}\n", node.label()));
    }
    for node in &answer.jobs {
        out.push_str(&format!("  {:<10} {}\n", node.kind.to_string(), node.label()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::manifest::build;
    use super::super::{
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
        api: "/api/posts",
        module_path: "app::repositories",
        file: "src/repositories.rs",
        line: 1,
    }];

    const ROUTES: &[RouteGraphDescriptor] = &[
        // Reaches `Post` only through the repository extractor.
        RouteGraphDescriptor {
            handler: "create",
            module_path: "app::routes::posts",
            method: "POST",
            path: "/posts",
            static_route: false,
            file: "src/routes/posts.rs",
            line: 1,
            signature_symbols: &["PgPostRepository"],
            body_symbols: &[],
        },
        // Reaches `Post` directly, by naming the diesel table module.
        RouteGraphDescriptor {
            handler: "index",
            module_path: "app::routes::posts",
            method: "GET",
            path: "/posts",
            static_route: false,
            file: "src/routes/posts.rs",
            line: 2,
            signature_symbols: &[],
            body_symbols: &["posts"],
        },
        // Reaches nothing.
        RouteGraphDescriptor {
            handler: "about",
            module_path: "app::routes::about",
            method: "GET",
            path: "/about",
            static_route: true,
            file: "src/routes/about.rs",
            line: 1,
            signature_symbols: &[],
            body_symbols: &[],
        },
    ];

    const JOBS: &[JobGraphDescriptor] = &[JobGraphDescriptor {
        name: "hot-rank",
        kind: JobKind::Scheduled,
        handler: "recalculate_hot_ranks",
        module_path: "app::tasks",
        schedule: "15m",
        mutating: true,
        file: "src/tasks.rs",
        line: 1,
        signature_symbols: &[],
        body_symbols: &["posts"],
    }];

    fn graph() -> ArchitectureGraph {
        build(&[], ROUTES, MODELS, REPOSITORIES, JOBS)
    }

    #[test]
    fn a_model_resolves_by_name_path_and_table() {
        let g = graph();
        for name in ["Post", "app::models::Post", "posts", "model:app::models::Post"] {
            assert_eq!(
                resolve(&g, name).map(|n| n.id.as_str()),
                Some("model:app::models::Post"),
                "{name} must resolve to the Post model"
            );
        }
    }

    #[test]
    fn a_table_name_resolves_case_insensitively() {
        let g = graph();
        assert_eq!(
            resolve(&g, "POSTS").map(|n| n.name.as_str()),
            Some("Post")
        );
    }

    #[test]
    fn a_repository_resolves_by_trait_and_implementation_name() {
        let g = graph();
        for name in ["PostRepository", "PgPostRepository"] {
            assert_eq!(
                resolve(&g, name).map(|n| n.kind),
                Some(NodeKind::Repository),
                "{name} must resolve to the repository"
            );
        }
    }

    #[test]
    fn an_unknown_name_resolves_to_nothing() {
        assert!(resolve(&graph(), "Nope").is_none());
    }

    #[test]
    fn touches_reports_routes_reached_through_a_repository() {
        let g = graph();
        let answer = touches(&g, "Post").expect("Post must resolve");
        let handlers: Vec<&str> = answer.routes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            handlers.contains(&"create"),
            "a route reaching Post only through PgPostRepository must be reported: {handlers:?}"
        );
        assert!(handlers.contains(&"index"), "{handlers:?}");
        assert!(
            !handlers.contains(&"about"),
            "a route that reaches nothing must not be reported: {handlers:?}"
        );
    }

    #[test]
    fn touches_reports_jobs() {
        let g = graph();
        let answer = touches(&g, "posts").expect("posts must resolve");
        assert_eq!(
            answer.jobs.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["hot-rank"]
        );
    }

    #[test]
    fn impact_includes_the_repository_the_routes_go_through() {
        let g = graph();
        let answer = impact(&g, "Post").expect("Post must resolve");
        assert_eq!(
            answer
                .repositories
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["PostRepository"]
        );
        assert_eq!(answer.routes.len(), 2, "{:?}", answer.routes);
        assert_eq!(answer.jobs.len(), 1);
    }

    #[test]
    fn impact_of_a_repository_is_its_own_dependents_not_the_models() {
        let g = graph();
        let answer = impact(&g, "PostRepository").expect("repository must resolve");
        assert_eq!(
            answer.routes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["create"],
            "only the route holding the extractor depends on the repository"
        );
        assert!(answer.repositories.is_empty());
    }

    #[test]
    fn an_element_nothing_touches_reports_an_empty_answer() {
        let g = build(&[], &[], MODELS, &[], &[]);
        let answer = impact(&g, "Post").expect("Post must resolve");
        assert!(answer.routes.is_empty());
        assert!(answer.jobs.is_empty());
        assert!(answer.repositories.is_empty());
    }

    #[test]
    fn answers_are_deterministic_and_sorted_by_node_id() {
        let g = graph();
        let answer = touches(&g, "Post").expect("Post must resolve");
        let ids: Vec<&str> = answer.routes.iter().map(|n| n.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn the_reports_name_the_target_and_every_row() {
        let g = graph();
        let touch = format_touches(&touches(&g, "Post").expect("resolve"));
        assert!(touch.contains("model Post"), "{touch}");
        assert!(touch.contains("POST /posts"), "{touch}");
        let imp = format_impact(&impact(&g, "Post").expect("resolve"));
        assert!(imp.contains("Changing model Post"), "{imp}");
        assert!(imp.contains("PostRepository"), "{imp}");
        assert!(imp.contains("hot-rank"), "{imp}");
    }
}
