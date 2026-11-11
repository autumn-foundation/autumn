//! Table-ownership boundaries, checked on the architecture graph (#3549).
//!
//! A boundary owns tables. Code in one boundary must not reach a table that
//! another boundary owns, unless the boundary lists that table as an
//! exception. Two rules check this:
//!
//! * **Graph rule.** An edge from a node in boundary A to a table that
//!   boundary B owns is a violation.
//! * **Crate rule.** A crate in boundary A that depends on a crate that holds
//!   B's schema is a violation. The graph cannot follow a call into a helper.
//!   This rule finds that case.
//!
//! A [`Baseline`] holds the violations that exist today. A new violation
//! fails. A fixed violation must leave the baseline, so the baseline only
//! shrinks.
//!
//! This module does no IO. `autumn graph check-boundaries` and
//! `autumn doctor` read the files and `cargo metadata`.
//!
//! See `docs/guide/table-ownership.md`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::manifest::{AUTO_API_ID_PREFIX, ArchitectureGraph, GraphNode};
use super::{Access, NodeKind, Provenance};

/// The file that declares the boundaries, at the project root.
pub const CONFIG_FILE: &str = "boundaries.toml";

/// The schema version of a [`Baseline`] file.
pub const BASELINE_VERSION: u32 = 1;

// ── Configuration ────────────────────────────────────────────────────

/// The contents of `boundaries.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryConfig {
    /// The baseline file, relative to the config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// A committed architecture graph, relative to the config file.
    /// `autumn doctor` checks this file because it does not build the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
    /// The boundaries, by name.
    #[serde(default)]
    pub boundaries: BTreeMap<String, Boundary>,
}

/// One boundary: the tables it owns and the code that is in it.
///
/// Every list holds glob patterns. `*` matches any run of characters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    /// The tables this boundary owns. Matched without case.
    #[serde(default)]
    pub owns: Vec<String>,
    /// The crates (and plugins) in this boundary. `-` and `_` are equal.
    #[serde(default)]
    pub crates: Vec<String>,
    /// The module paths in this boundary. `app::billing` also matches
    /// `app::billing::routes`.
    #[serde(default)]
    pub modules: Vec<String>,
    /// Tables of other boundaries that this boundary may read.
    #[serde(default)]
    pub may_read: Vec<String>,
    /// Tables of other boundaries that this boundary may read and write.
    #[serde(default)]
    pub may_write: Vec<String>,
    /// The crates that hold this boundary's schema. When empty, the crates
    /// that declare the models of the owned tables are used.
    #[serde(default)]
    pub schema_crates: Vec<String>,
    /// The Postgres schema of the owned tables. When unset, `public`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The database role of this boundary. When set, `autumn db grants`
    /// gives this role access to the owned tables and the exceptions only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl Boundary {
    /// The Postgres schema of the owned tables: [`Self::schema`] or `public`.
    #[must_use]
    pub fn schema_name(&self) -> &str {
        self.schema.as_deref().unwrap_or(DEFAULT_SCHEMA)
    }
}

/// Role names that Postgres reserves. `PUBLIC` means every role.
const RESERVED_ROLES: &[&str] = &[
    "public",
    "none",
    "current_user",
    "session_user",
    "current_role",
];

/// The schema of a boundary that sets no `schema`.
pub const DEFAULT_SCHEMA: &str = "public";

impl BoundaryConfig {
    /// Parse `boundaries.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`BoundaryError::Parse`] for bad TOML or an unknown key, and
    /// [`BoundaryError::Empty`] for a boundary that declares nothing.
    pub fn parse(text: &str) -> Result<Self, BoundaryError> {
        let config: Self = toml::from_str(text).map_err(|e| BoundaryError::Parse(e.to_string()))?;
        for (name, boundary) in &config.boundaries {
            if boundary.owns.is_empty() && boundary.crates.is_empty() && boundary.modules.is_empty()
            {
                return Err(BoundaryError::Empty(name.clone()));
            }
            for (key, value) in [("schema", &boundary.schema), ("role", &boundary.role)] {
                let Some(value) = value.as_deref() else {
                    continue;
                };
                if value.trim().is_empty() || value.chars().any(char::is_control) {
                    return Err(BoundaryError::Parse(format!(
                        "boundary `{name}` has an empty `{key}`, or one with a control character"
                    )));
                }
            }
            if let Some(role) = boundary.role.as_deref()
                && (RESERVED_ROLES.contains(&role.to_ascii_lowercase().as_str())
                    || role.to_ascii_lowercase().starts_with("pg_"))
            {
                return Err(BoundaryError::Parse(format!(
                    "boundary `{name}` names the reserved role `{role}`; use a role of its own"
                )));
            }
        }
        Ok(config)
    }

    /// Add the tables a plugin crate declares that it owns.
    ///
    /// The config has the last word. A table that a config boundary already
    /// owns stays there. When a config boundary lists the crate, the tables
    /// join that boundary. Else the plugin gets a boundary of its own name.
    pub fn add_plugin(&mut self, krate: &str, owns: &[String]) {
        let key = crate_key(krate);
        let tables: Vec<String> = owns
            .iter()
            .filter(|t| {
                let t = t.to_ascii_lowercase();
                !self
                    .boundaries
                    .values()
                    .flat_map(|b| &b.owns)
                    .any(|p| glob(&p.to_ascii_lowercase(), &t))
            })
            .cloned()
            .collect();
        let listed = self
            .boundaries
            .iter()
            .flat_map(|(name, b)| b.crates.iter().map(move |p| (name, p)))
            .filter(|(_, p)| glob(&crate_key(p), &key))
            .max_by_key(|(_, p)| specificity(p))
            .map(|(name, _)| name.clone());
        let name = listed.clone().unwrap_or_else(|| krate.to_owned());
        let boundary = self.boundaries.entry(name).or_default();
        for table in tables {
            if !boundary.owns.contains(&table) {
                boundary.owns.push(table);
            }
        }
        if listed.is_none() && !boundary.crates.iter().any(|c| c == krate) {
            boundary.crates.push(krate.to_owned());
        }
    }
}

/// Why a boundary check cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BoundaryError {
    /// `boundaries.toml` is not valid.
    #[error("boundaries.toml is not valid: {0}")]
    Parse(String),
    /// A boundary declares no tables, crates or modules.
    #[error("boundary `{0}` declares no `owns`, `crates` or `modules`")]
    Empty(String),
    /// Two boundaries claim one name with equally specific patterns.
    #[error(
        "{what} `{name}` matches boundary `{first}` and boundary `{second}` equally; \
         make one pattern more specific"
    )]
    Ambiguous {
        /// What is claimed: `table`, `node` or `crate`.
        what: &'static str,
        /// The claimed name.
        name: String,
        /// One boundary.
        first: String,
        /// The other boundary.
        second: String,
    },
}

// ── Inputs ───────────────────────────────────────────────────────────

/// The normal (not dev, not build) dependencies of each crate, by package
/// key.
///
/// A key is the package name. A package that resolves at two versions has
/// the key `name@version` for each, so their dependencies stay apart. The
/// CLI fills this from `cargo metadata`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CrateGraph {
    /// Package key to the package keys it depends on.
    pub deps: BTreeMap<String, BTreeSet<String>>,
    /// Package key to library name, where the two differ. A module path
    /// starts with the library name.
    pub libs: BTreeMap<String, String>,
    /// Package key to its binary target names that differ from the package
    /// name. A module path in a binary starts with the binary name.
    pub bins: BTreeMap<String, BTreeSet<String>>,
}

impl CrateGraph {
    /// Record that `krate` depends on `dep`.
    pub fn add(&mut self, krate: &str, dep: &str) {
        self.deps
            .entry(krate.to_owned())
            .or_default()
            .insert(dep.to_owned());
        self.deps.entry(dep.to_owned()).or_default();
    }

    /// The normalized names of a package key: the package name, without a
    /// version, and the library name.
    ///
    /// A dependent links the library only. Binary names are not here.
    fn names(&self, package: &str) -> Vec<String> {
        let mut names = vec![crate_key(package_name(package))];
        if let Some(lib) = self.libs.get(package) {
            names.push(crate_key(lib));
        }
        names
    }
}

/// Normalized target name to the normalized names of its packages.
type Aliases = BTreeMap<String, BTreeSet<String>>;

/// The library aliases and the binary aliases of the packages.
///
/// A binary name that is also a package or library name is not an alias. A
/// module path with that name stays with the library.
fn aliases(crates: &CrateGraph) -> (Aliases, Aliases) {
    let mut libs = Aliases::new();
    for (package, lib) in &crates.libs {
        libs.entry(crate_key(lib))
            .or_default()
            .insert(crate_key(package_name(package)));
    }
    let direct: BTreeSet<String> = crates
        .deps
        .keys()
        .chain(crates.libs.keys())
        .chain(crates.bins.keys())
        .map(|p| crate_key(package_name(p)))
        .chain(libs.keys().cloned())
        .collect();
    let mut bins = Aliases::new();
    for (package, names) in &crates.bins {
        for bin in names.iter().map(|b| crate_key(b)) {
            if !direct.contains(&bin) {
                bins.entry(bin)
                    .or_default()
                    .insert(crate_key(package_name(package)));
            }
        }
    }
    (libs, bins)
}

// ── Findings ─────────────────────────────────────────────────────────

/// A node in one boundary with an edge to a table another boundary owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableViolation {
    /// The boundary of the node.
    pub boundary: String,
    /// The boundary that owns the table.
    pub owner: String,
    /// The node id.
    pub node: String,
    /// The node as the report shows it, for example `GET /report`.
    pub label: String,
    /// The node kind.
    pub kind: NodeKind,
    /// The table.
    pub table: String,
    /// The strongest access over all edges from the node to the table.
    pub access: Access,
    /// The evidence of the edge.
    pub provenance: Provenance,
    /// The symbol that made the edge. Empty for a declaration.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub symbol: String,
    /// The repository the node reaches the table through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// A crate in one boundary that depends on a crate with another
/// boundary's schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateViolation {
    /// The boundary of the crate.
    pub boundary: String,
    /// The boundary whose schema the dependency holds.
    pub owner: String,
    /// The crate.
    #[serde(rename = "crate")]
    pub krate: String,
    /// The schema crate.
    pub schema_crate: String,
    /// The dependency path, from `krate` to `schema_crate`.
    pub path: Vec<String>,
}

/// One boundary violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Violation {
    /// The graph rule.
    Table(TableViolation),
    /// The crate rule.
    Crate(CrateViolation),
}

impl Violation {
    /// The identity of the violation in a baseline.
    ///
    /// It holds the access, so a new write to a table that the baseline only
    /// reads is a new violation. It holds no evidence, line or crate version,
    /// so a refactor that keeps the query keeps the key.
    #[must_use]
    pub fn key(&self) -> BaselineEntry {
        match self {
            Self::Table(v) => BaselineEntry::Table {
                boundary: v.boundary.clone(),
                owner: v.owner.clone(),
                node: base_id(&v.node).to_owned(),
                table: v.table.clone(),
                access: v.access,
            },
            Self::Crate(v) => BaselineEntry::Crate {
                boundary: v.boundary.clone(),
                owner: v.owner.clone(),
                krate: package_name(&v.krate).to_owned(),
                schema_crate: package_name(&v.schema_crate).to_owned(),
            },
        }
    }

    /// The violation as report lines. The first line names both boundaries.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Table(v) => {
                let verb = match v.access {
                    Access::Read => "reads",
                    Access::Write => "writes",
                    Access::ReadWrite => "reads and writes",
                };
                let mut text = format!(
                    "{} -> {}: {} {} {verb} table `{}`\n    node: {}\n    edge: {}",
                    v.boundary, v.owner, v.kind, v.label, v.table, v.node, v.provenance
                );
                if !v.symbol.is_empty() {
                    let _ = write!(text, ", symbol `{}`", v.symbol);
                }
                if let Some(via) = &v.via {
                    let _ = write!(text, ", via {via}");
                }
                text
            }
            Self::Crate(v) => format!(
                "{} -> {}: crate {} depends on {}, which holds the schema of `{}`\n    path: {}",
                v.boundary,
                v.owner,
                v.krate,
                v.schema_crate,
                v.owner,
                v.path.join(" -> ")
            ),
        }
    }
}

/// Which rules a check ran.
///
/// A rule that did not run finds nothing. Its baseline entries are then not
/// fixed, only unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesRun {
    /// The graph rule ran.
    pub graph: bool,
    /// The crate rule ran, and it knew every schema crate.
    pub crates: bool,
}

/// The result of a check before the baseline is applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Every violation, sorted by key.
    pub violations: Vec<Violation>,
    /// Patterns that match nothing, and other notes that do not fail.
    pub warnings: Vec<String>,
    /// The rules that ran.
    pub rules: RulesRun,
}

/// Check the graph rule and the crate rule.
///
/// Give `None` for an input to skip its rule. The crate rule also reads the
/// graph, to find the crate of each model.
///
/// # Errors
///
/// Returns [`BoundaryError::Ambiguous`] when one table, node or crate matches
/// two boundaries equally.
pub fn check(
    config: &BoundaryConfig,
    graph: Option<&ArchitectureGraph>,
    crates: Option<&CrateGraph>,
) -> Result<Report, BoundaryError> {
    let resolver = Resolver::new(config, graph, crates);
    let mut found = Found::default();
    let mut warnings = Vec::new();
    let mut rules = RulesRun::default();
    if let Some(graph) = graph {
        graph_rule(&resolver, graph, &mut found)?;
        rules.graph = true;
    }
    if let Some(crates) = crates {
        rules.crates = crate_rule(&resolver, graph, crates, &mut found, &mut warnings)?;
    }
    unmatched_patterns(&resolver, graph, crates, &mut warnings);
    let mut violations: BTreeMap<BaselineEntry, Violation> = BTreeMap::new();
    for v in found.tables.into_values() {
        let v = Violation::Table(v);
        violations.insert(v.key(), v);
    }
    for v in found.crates {
        violations.entry(v.key()).or_insert(v);
    }
    Ok(Report {
        violations: violations.into_values().collect(),
        warnings,
        rules,
    })
}

/// Violations while the rules run.
#[derive(Default)]
struct Found {
    /// By (boundary, owner, node, table), before the access is known.
    tables: BTreeMap<(String, String, String, String), TableViolation>,
    /// Crate-rule violations.
    crates: Vec<Violation>,
}

impl Found {
    /// Add a table violation. One is kept per node and table: the access is
    /// the strongest, and a direct edge is the better evidence.
    fn add_table(&mut self, v: TableViolation) {
        let key = (
            v.boundary.clone(),
            v.owner.clone(),
            base_id(&v.node).to_owned(),
            v.table.clone(),
        );
        match self.tables.get_mut(&key) {
            None => {
                self.tables.insert(key, v);
            }
            Some(old) => {
                let access = stronger(old.access, v.access);
                if old.via.is_some() && v.via.is_none() {
                    *old = v;
                }
                old.access = access;
            }
        }
    }
}

/// The stronger of two accesses. A write is stronger than a read.
const fn stronger(a: Access, b: Access) -> Access {
    match (a, b) {
        (Access::ReadWrite, _)
        | (_, Access::ReadWrite)
        | (Access::Read, Access::Write)
        | (Access::Write, Access::Read) => Access::ReadWrite,
        (Access::Write, Access::Write) => Access::Write,
        (Access::Read, Access::Read) => Access::Read,
    }
}

/// The graph rule: every edge from a node in a boundary to a table another
/// boundary owns. A repository also reaches its own table.
fn graph_rule(
    r: &Resolver<'_>,
    graph: &ArchitectureGraph,
    found: &mut Found,
) -> Result<(), BoundaryError> {
    // Resolve every table and node first, so a conflict in the config is an
    // error even where no edge crosses it.
    for node in &graph.nodes {
        if let Some(model) = &node.model {
            r.owner(&model.table)?;
        }
        if let Some(repository) = &node.repository {
            r.owner(&repository.table)?;
        }
        r.node_boundary(node)?;
    }
    let nodes: BTreeMap<&str, &GraphNode> =
        graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut report = |from: &GraphNode, boundary: &str, table: String, how: Edge| {
        let Ok(Some(owner)) = r.owner(&table) else {
            return;
        };
        if owner == boundary || r.allows(boundary, &table, how.access) {
            return;
        }
        found.add_table(TableViolation {
            boundary: boundary.to_owned(),
            owner: owner.to_owned(),
            node: from.id.clone(),
            label: from.label(),
            kind: from.kind,
            table,
            access: how.access,
            provenance: how.provenance,
            symbol: how.symbol,
            via: how.via,
        });
    };
    for node in &graph.nodes {
        let (Some(repository), Some(boundary)) = (&node.repository, r.node_boundary(node)?) else {
            continue;
        };
        // A repository that overrides its table queries that table, and the
        // graph has no edge for it.
        let how = Edge {
            access: Access::ReadWrite,
            provenance: Provenance::Declaration,
            symbol: String::new(),
            via: None,
        };
        report(node, boundary, repository.table.clone(), how);
    }
    for edge in &graph.edges {
        let Some(from) = nodes.get(edge.from.as_str()) else {
            continue;
        };
        let Some(boundary) = r.node_boundary(from)? else {
            continue;
        };
        for (table, via) in reached_tables(graph, &nodes, &edge.to) {
            let how = Edge {
                access: edge.access,
                provenance: edge.provenance,
                symbol: edge.symbol.clone(),
                via,
            };
            report(from, boundary, table, how);
        }
    }
    Ok(())
}

/// The evidence for one reached table.
struct Edge {
    access: Access,
    provenance: Provenance,
    symbol: String,
    via: Option<String>,
}

/// The tables an edge target stands for, with the repository in between.
///
/// A model is its table. A repository is its own table and the tables of
/// the models it has edges to, which include its declared relations.
fn reached_tables(
    graph: &ArchitectureGraph,
    nodes: &BTreeMap<&str, &GraphNode>,
    target: &str,
) -> Vec<(String, Option<String>)> {
    let Some(node) = nodes.get(target) else {
        return Vec::new();
    };
    if let Some(model) = &node.model {
        return vec![(model.table.clone(), None)];
    }
    let Some(repository) = &node.repository else {
        return Vec::new();
    };
    let mut tables: BTreeSet<String> = graph
        .edges
        .iter()
        .filter(|e| e.from == node.id)
        .filter_map(|e| nodes.get(e.to.as_str()))
        .filter_map(|n| n.model.as_ref().map(|m| m.table.clone()))
        .collect();
    tables.insert(repository.table.clone());
    tables
        .into_iter()
        .map(|t| (t, Some(node.id.clone())))
        .collect()
}

/// The crate rule: every crate in a boundary that depends on another
/// boundary's schema crate.
///
/// The search goes through crates in no boundary, such as a shared helper
/// crate. It stops at a crate in a boundary: that crate is checked on its own,
/// or it is in a `modules` boundary that the graph rule checks.
///
/// Returns whether the rule knew every schema crate.
fn crate_rule(
    r: &Resolver<'_>,
    graph: Option<&ArchitectureGraph>,
    crates: &CrateGraph,
    found: &mut Found,
    warnings: &mut Vec<String>,
) -> Result<bool, BoundaryError> {
    let mut complete = true;
    if graph.is_none() {
        for (name, b) in &r.config.boundaries {
            if !b.owns.is_empty() && b.schema_crates.is_empty() {
                complete = false;
                warnings.push(format!(
                    "boundary `{name}`: with no graph, its schema crates are unknown; set \
                     `schema_crates` or check a graph"
                ));
            }
        }
    }
    let schema = schema_owners(r, graph, crates)?;
    for start in crates.deps.keys() {
        let Some(boundary) = r.crate_boundary(start, crates)? else {
            continue;
        };
        let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
        let mut queue = VecDeque::from([start.as_str()]);
        while let Some(current) = queue.pop_front() {
            for dep in crates.deps.get(current).into_iter().flatten() {
                if dep == start || parent.contains_key(dep.as_str()) {
                    continue;
                }
                parent.insert(dep, current);
                let dep_boundary = r.crate_boundary(dep, crates)?;
                for owner in schema.get(dep.as_str()).into_iter().flatten() {
                    if *owner == boundary
                        || dep_boundary == Some(boundary)
                        || r.allows_schema_of(boundary, owner)
                    {
                        continue;
                    }
                    let mut path = vec![dep.clone()];
                    let mut at = dep.as_str();
                    while let Some(prev) = parent.get(at) {
                        path.push((*prev).to_owned());
                        at = prev;
                    }
                    path.reverse();
                    found.crates.push(Violation::Crate(CrateViolation {
                        boundary: boundary.to_owned(),
                        owner: (*owner).to_owned(),
                        krate: start.clone(),
                        schema_crate: dep.clone(),
                        path,
                    }));
                }
                if dep_boundary.is_none() && !r.in_modules_boundary(dep, crates) {
                    queue.push_back(dep);
                }
            }
        }
    }
    Ok(complete)
}

/// The boundaries whose schema each crate holds.
///
/// `schema_crates` names them. When it is empty, they are the crates that
/// declare a model of an owned table.
fn schema_owners<'a>(
    r: &Resolver<'a>,
    graph: Option<&ArchitectureGraph>,
    crates: &CrateGraph,
) -> Result<BTreeMap<String, BTreeSet<&'a str>>, BoundaryError> {
    let mut owners: BTreeMap<String, BTreeSet<&'a str>> = BTreeMap::new();
    for (name, boundary) in &r.config.boundaries {
        for package in crates.deps.keys() {
            if boundary.schema_crates.iter().any(|p| {
                let p = crate_key(p);
                crates.names(package).iter().any(|n| glob(&p, n))
            }) {
                owners
                    .entry(package.clone())
                    .or_default()
                    .insert(name.as_str());
            }
        }
    }
    // Derived from models, keyed by normalized crate name: a module path
    // spells `-` as `_`.
    let mut by_crate: BTreeMap<String, BTreeSet<&'a str>> = BTreeMap::new();
    for node in graph.iter().flat_map(|g| &g.nodes) {
        let Some(model) = &node.model else {
            continue;
        };
        let Some(owner) = r.owner(&model.table)? else {
            continue;
        };
        if r.config.boundaries[owner].schema_crates.is_empty() {
            by_crate
                .entry(crate_key(crate_root(&node.module)))
                .or_default()
                .insert(owner);
        }
    }
    for package in crates.deps.keys() {
        for name in crates.names(package) {
            if let Some(set) = by_crate.get(&name) {
                owners.entry(package.clone()).or_default().extend(set);
            }
        }
    }
    Ok(owners)
}

/// A warning for each pattern that matches nothing in the inputs.
///
/// A typo in a pattern would make the check pass in silence. The warning
/// makes the typo visible.
fn unmatched_patterns(
    r: &Resolver<'_>,
    graph: Option<&ArchitectureGraph>,
    crates: Option<&CrateGraph>,
    warnings: &mut Vec<String>,
) {
    let mut crate_names: BTreeSet<String> = graph
        .iter()
        .flat_map(|g| &g.nodes)
        .map(|n| crate_key(crate_root(&n.module)))
        .collect();
    if let Some(c) = crates {
        crate_names.extend(c.deps.keys().flat_map(|k| c.names(k)));
    }
    for (name, b) in &r.config.boundaries {
        let mut warn = |list: &str, pattern: &str, what: &str| {
            warnings.push(format!(
                "boundary `{name}`: `{list}` pattern `{pattern}` matches no {what}"
            ));
        };
        if let Some(graph) = graph {
            for p in &b.owns {
                let p_lower = p.to_ascii_lowercase();
                if !r.tables.iter().any(|t| glob(&p_lower, t)) {
                    warn("owns", p, "table of a #[model] in the graph");
                }
            }
            for p in &b.modules {
                if !graph.nodes.iter().any(|n| module_matches(p, &n.module)) {
                    warn("modules", p, "node in the graph");
                }
            }
        }
        if graph.is_some() || crates.is_some() {
            for p in &b.crates {
                let key = crate_key(p);
                if !crate_names.iter().any(|c| glob(&key, c)) {
                    warn("crates", p, "crate");
                }
            }
        }
        if let Some(crates) = crates {
            for p in &b.schema_crates {
                let key = crate_key(p);
                let hit = crates
                    .deps
                    .keys()
                    .any(|c| crates.names(c).iter().any(|n| glob(&key, n)));
                if !hit {
                    warn("schema_crates", p, "crate");
                }
            }
        }
        for (list, entries) in [("may_read", &b.may_read), ("may_write", &b.may_write)] {
            for t in entries {
                if r.exception_owners(t).is_empty() {
                    warn(list, t, "table that a boundary owns");
                }
            }
        }
    }
}

// ── Baseline and ratchet ─────────────────────────────────────────────

/// The identity of one known violation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case", deny_unknown_fields)]
pub enum BaselineEntry {
    /// A graph-rule violation.
    Table {
        /// The boundary of the node.
        boundary: String,
        /// The boundary that owns the table.
        owner: String,
        /// The node id, with no crate version.
        node: String,
        /// The table.
        table: String,
        /// The strongest access of the node to the table.
        access: Access,
    },
    /// A crate-rule violation.
    Crate {
        /// The boundary of the crate.
        boundary: String,
        /// The boundary whose schema the dependency holds.
        owner: String,
        /// The crate.
        #[serde(rename = "crate")]
        krate: String,
        /// The schema crate.
        schema_crate: String,
    },
}

impl std::fmt::Display for BaselineEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table {
                boundary,
                owner,
                node,
                table,
                access,
            } => write!(f, "{boundary} -> {owner}.{table} ({access}, {node})"),
            Self::Crate {
                boundary,
                owner,
                krate,
                schema_crate,
            } => write!(
                f,
                "{boundary} -> {owner} (crate {krate} depends on {schema_crate})"
            ),
        }
    }
}

/// The violations an app accepts today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Always [`BASELINE_VERSION`].
    pub version: u32,
    /// The known violations, sorted.
    pub violations: Vec<BaselineEntry>,
}

impl Baseline {
    /// A baseline that holds `violations`.
    #[must_use]
    pub fn from_violations(violations: &[Violation]) -> Self {
        let keys: BTreeSet<BaselineEntry> = violations.iter().map(Violation::key).collect();
        Self {
            version: BASELINE_VERSION,
            violations: keys.into_iter().collect(),
        }
    }

    /// Parse a baseline file.
    ///
    /// # Errors
    ///
    /// Returns a message for bad JSON or an unknown version.
    pub fn parse(text: &str) -> Result<Self, String> {
        let baseline: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if baseline.version != BASELINE_VERSION {
            return Err(format!(
                "baseline version {} is not supported (expected {BASELINE_VERSION})",
                baseline.version
            ));
        }
        Ok(baseline)
    }

    /// The baseline as pretty JSON, with a final newline.
    ///
    /// # Panics
    ///
    /// Never: every field is a string or a number.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(self)
            .unwrap_or_else(|e| panic!("a baseline is always serializable: {e}"));
        json.push('\n');
        json
    }
}

/// A report compared with a baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ratchet {
    /// Violations the baseline does not hold. These fail.
    pub new: Vec<Violation>,
    /// Violations the baseline holds. These pass.
    pub known: Vec<Violation>,
    /// Baseline entries that no longer occur. These fail until the baseline
    /// is updated, so the baseline cannot keep a slot for a regression.
    pub fixed: Vec<BaselineEntry>,
    /// Notes that do not fail.
    pub warnings: Vec<String>,
}

impl Ratchet {
    /// Whether the check passes.
    #[must_use]
    pub const fn passes(&self) -> bool {
        self.new.is_empty() && self.fixed.is_empty()
    }

    /// The human report.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for warning in &self.warnings {
            let _ = writeln!(out, "warning: {warning}");
        }
        if !self.new.is_empty() {
            let _ = writeln!(
                out,
                "\u{2717} {} new boundary violation(s):",
                self.new.len()
            );
            for v in &self.new {
                let _ = writeln!(out, "  {}", v.render().replace('\n', "\n  "));
            }
        }
        if !self.fixed.is_empty() {
            let _ = writeln!(
                out,
                "\u{2717} {} baseline entry(s) no longer occur. Run with --update-baseline \
                 to remove them:",
                self.fixed.len()
            );
            for entry in &self.fixed {
                let _ = writeln!(out, "  - {entry}");
            }
        }
        if !self.known.is_empty() {
            let _ = writeln!(
                out,
                "{} known violation(s) in the baseline.",
                self.known.len()
            );
        }
        if self.passes() {
            let _ = writeln!(out, "\u{2713} No new boundary violations.");
        }
        out
    }
}

/// Compare a report with a baseline. No baseline holds nothing.
///
/// A baseline entry of a rule that did not run is not fixed.
#[must_use]
pub fn ratchet(report: Report, baseline: Option<&Baseline>) -> Ratchet {
    let accepted: BTreeSet<&BaselineEntry> = baseline.iter().flat_map(|b| &b.violations).collect();
    let current: BTreeSet<BaselineEntry> = report.violations.iter().map(Violation::key).collect();
    let ran = |entry: &BaselineEntry| match entry {
        BaselineEntry::Table { .. } => report.rules.graph,
        BaselineEntry::Crate { .. } => report.rules.crates,
    };
    let fixed = accepted
        .iter()
        .filter(|e| ran(e) && !current.contains(**e))
        .map(|e| (*e).clone())
        .collect();
    let (known, new) = report
        .violations
        .into_iter()
        .partition(|v| accepted.contains(&v.key()));
    Ratchet {
        new,
        known,
        fixed,
        warnings: report.warnings,
    }
}

// ── Matching ─────────────────────────────────────────────────────────

/// Finds the boundary of a table, node or crate.
struct Resolver<'a> {
    config: &'a BoundaryConfig,
    /// The tables of the models in the graph, lowercase.
    tables: BTreeSet<String>,
    /// Library name to its packages. A module path starts with the library
    /// name; the config can name the package.
    lib_aliases: Aliases,
    /// Binary name to its packages. These apply only to a node that no
    /// pattern claims by its own names.
    bin_aliases: Aliases,
}

impl<'a> Resolver<'a> {
    fn new(
        config: &'a BoundaryConfig,
        graph: Option<&ArchitectureGraph>,
        crates: Option<&CrateGraph>,
    ) -> Self {
        let tables = graph
            .iter()
            .flat_map(|g| &g.nodes)
            .filter_map(|n| n.model.as_ref().map(|m| m.table.to_ascii_lowercase()))
            .collect();
        let (lib_aliases, bin_aliases) = crates.map(aliases).unwrap_or_default();
        Self {
            config,
            tables,
            lib_aliases,
            bin_aliases,
        }
    }

    /// The boundary that owns `table`.
    fn owner(&self, table: &str) -> Result<Option<&'a str>, BoundaryError> {
        let table = table.to_ascii_lowercase();
        let mut candidates = Vec::new();
        for (name, b) in &self.config.boundaries {
            for p in &b.owns {
                if glob(&table_pattern(p), &table) {
                    candidates.push((name.as_str(), specificity(p)));
                }
            }
        }
        pick("table", &table, &candidates)
    }

    /// The boundary of a graph node, by its crate or its module path.
    ///
    /// The crate is the name at the start of the module path, or the name
    /// of a package whose library has that name. When no pattern matches
    /// these names, the package of a binary with that name is the crate.
    fn node_boundary(&self, node: &GraphNode) -> Result<Option<&'a str>, BoundaryError> {
        let root = crate_key(crate_root(&node.module));
        let mut names = vec![root.clone()];
        names.extend(self.lib_aliases.get(&root).into_iter().flatten().cloned());
        let mut candidates = self.crate_candidates(&names);
        for (name, b) in &self.config.boundaries {
            for p in &b.modules {
                if module_matches(p, &node.module) {
                    candidates.push((name.as_str(), specificity(p)));
                }
            }
        }
        if candidates.is_empty()
            && let Some(packages) = self.bin_aliases.get(&root)
        {
            let packages: Vec<String> = packages.iter().cloned().collect();
            candidates = self.crate_candidates(&packages);
        }
        pick("node", &node.id, &candidates)
    }

    /// The boundaries with a `crates` pattern that matches one of `names`.
    fn crate_candidates(&self, names: &[String]) -> Vec<(&'a str, usize)> {
        let mut candidates = Vec::new();
        for (name, b) in &self.config.boundaries {
            for p in &b.crates {
                let p_key = crate_key(p);
                if names.iter().any(|n| glob(&p_key, n)) {
                    candidates.push((name.as_str(), specificity(p)));
                }
            }
        }
        candidates
    }

    /// The boundary of a package, by its name or its library name.
    fn crate_boundary(
        &self,
        package: &str,
        crates: &CrateGraph,
    ) -> Result<Option<&'a str>, BoundaryError> {
        let candidates = self.crate_candidates(&crates.names(package));
        pick("crate", package, &candidates)
    }

    /// Whether a `modules` pattern of a boundary matches the crate root.
    fn in_modules_boundary(&self, package: &str, crates: &CrateGraph) -> bool {
        let names = crates.names(package);
        self.config
            .boundaries
            .values()
            .flat_map(|b| &b.modules)
            .any(|p| names.iter().any(|n| module_matches(p, n)))
    }

    /// Whether `boundary` lists `table` as an exception for `access`.
    ///
    /// `may_write` allows every access. `may_read` allows a read only.
    fn allows(&self, boundary: &str, table: &str, access: Access) -> bool {
        let Some(b) = self.config.boundaries.get(boundary) else {
            return false;
        };
        let table = table.to_ascii_lowercase();
        let hit = |list: &[String]| list.iter().any(|p| glob(&table_pattern(p), &table));
        hit(&b.may_write) || (access == Access::Read && hit(&b.may_read))
    }

    /// The boundaries that own a table an exception pattern matches.
    ///
    /// The pattern is matched against the tables in the graph, and read as a
    /// table name too. A tie between two boundaries counts for neither.
    fn exception_owners(&self, pattern: &str) -> BTreeSet<&'a str> {
        let lower = table_pattern(pattern);
        self.tables
            .iter()
            .filter(|t| glob(&lower, t))
            .map(String::as_str)
            .chain(std::iter::once(lower.as_str()))
            .filter_map(|t| self.owner(t).ok().flatten())
            .collect()
    }

    /// Whether `boundary` lists a table of `owner` as an exception. That
    /// exception needs a dependency on `owner`'s schema crate.
    fn allows_schema_of(&self, boundary: &str, owner: &str) -> bool {
        let Some(b) = self.config.boundaries.get(boundary) else {
            return false;
        };
        b.may_read
            .iter()
            .chain(&b.may_write)
            .any(|t| self.exception_owners(t).contains(owner))
    }
}

/// The boundary with the most specific match. A tie between two boundaries
/// is an error, because the config does not say which one is meant.
pub(super) fn pick<'a>(
    what: &'static str,
    name: &str,
    candidates: &[(&'a str, usize)],
) -> Result<Option<&'a str>, BoundaryError> {
    let Some(top) = candidates.iter().map(|c| c.1).max() else {
        return Ok(None);
    };
    let winners: BTreeSet<&str> = candidates
        .iter()
        .filter(|c| c.1 == top)
        .map(|c| c.0)
        .collect();
    let mut winners = winners.into_iter();
    match (winners.next(), winners.next()) {
        (Some(first), Some(second)) => Err(BoundaryError::Ambiguous {
            what,
            name: name.to_owned(),
            first: first.to_owned(),
            second: second.to_owned(),
        }),
        (first, _) => Ok(first),
    }
}

/// A table pattern as the graph reads it: lower case, with no schema. The
/// graph names a table without its Postgres schema, so `public.contacts`
/// matches the table `contacts` here. The grant planner reads the schema.
fn table_pattern(pattern: &str) -> String {
    let lower = pattern.to_ascii_lowercase();
    match lower.rsplit_once('.') {
        Some((_, table)) => table.to_owned(),
        None => lower,
    }
}

/// How specific a pattern is: its count of characters that are not `*`.
pub(super) fn specificity(pattern: &str) -> usize {
    pattern.chars().filter(|c| *c != '*').count()
}

/// Whether a module pattern matches a module path or one of its children.
fn module_matches(pattern: &str, module: &str) -> bool {
    glob(pattern, module) || glob(&format!("{pattern}::*"), module)
}

/// The crate name at the start of a module path.
fn crate_root(module: &str) -> &str {
    module.split_once("::").map_or(module, |(root, _)| root)
}

/// A crate name with `-` as `_`, as `module_path!()` spells it.
fn crate_key(name: &str) -> String {
    name.replace('-', "_")
}

/// A package key without the `@<version>` that a second resolved version
/// adds.
fn package_name(key: &str) -> &str {
    key.split_once('@').map_or(key, |(name, _)| name)
}

/// A node id without the `@<version>` that a second crate version adds.
fn base_id(id: &str) -> &str {
    if id.starts_with(AUTO_API_ID_PREFIX) {
        return id;
    }
    id.split_once('@').map_or(id, |(base, _)| base)
}

/// Whether `text` matches `pattern`, where `*` matches any run.
pub(super) fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let middle: Vec<&str> = parts.collect();
    let Some((last, middle)) = middle.split_last() else {
        // No `*`: the whole text must be the pattern.
        return rest.is_empty();
    };
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::manifest::build;
    use crate::graph::{ModelGraphDescriptor, RepositoryGraphDescriptor, RouteGraphDescriptor};

    const fn model(
        name: &'static str,
        table: &'static str,
        module_path: &'static str,
    ) -> ModelGraphDescriptor {
        ModelGraphDescriptor {
            model: name,
            model_path: name,
            table,
            relations: &[],
            module_path,
            file: "src/models.rs",
            line: 1,
            crate_version: "0.1.0",
        }
    }

    const fn route(
        handler: &'static str,
        module_path: &'static str,
        method: &'static str,
        path: &'static str,
    ) -> RouteGraphDescriptor {
        RouteGraphDescriptor {
            handler,
            module_path,
            method,
            path,
            static_route: false,
            file: "src/routes.rs",
            line: 1,
            crate_version: "0.1.0",
            signature_symbols: &[],
            body_symbols: &[],
            sql_symbols: &[],
        }
    }

    const MODELS: &[ModelGraphDescriptor] = &[
        model("Contact", "contacts", "crm_core::models"),
        model("Campaign", "campaigns", "crm_marketing::models"),
        model("Touch", "attribution_touches", "crm_marketing::models"),
    ];

    const REPOSITORIES: &[RepositoryGraphDescriptor] = &[RepositoryGraphDescriptor {
        repository: "ContactRepository",
        implementation: "PgContactRepository",
        model: "Contact",
        table: "contacts",
        api: "",
        module_path: "crm_core::repositories",
        file: "src/repositories.rs",
        line: 1,
        crate_version: "0.1.0",
    }];

    const ROUTES: &[RouteGraphDescriptor] = &[
        // Core reads its own table.
        RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("list", "crm_core::routes", "GET", "/contacts")
        },
        // Marketing reads a core table through the Diesel DSL.
        RouteGraphDescriptor {
            body_symbols: &["Campaign", "contacts"],
            ..route("report", "crm_marketing::routes", "GET", "/report")
        },
        // Marketing joins a core table in raw SQL.
        RouteGraphDescriptor {
            body_symbols: &["CONTACTS", "c", "campaigns", "contact_id", "ct", "id"],
            sql_symbols: &["CONTACTS", "c", "campaigns", "contact_id", "ct", "id"],
            ..route("joined", "crm_marketing::routes", "GET", "/joined")
        },
        // Marketing writes a core table through the core repository.
        RouteGraphDescriptor {
            signature_symbols: &["PgContactRepository"],
            ..route("sync", "crm_marketing::routes", "POST", "/sync")
        },
        // Glue in no boundary is not checked.
        RouteGraphDescriptor {
            body_symbols: &["contacts", "campaigns"],
            ..route("home", "crm_app::routes", "GET", "/")
        },
    ];

    fn graph() -> ArchitectureGraph {
        build(&[], 0, ROUTES, MODELS, REPOSITORIES, &[])
    }

    const CONFIG: &str = r#"
[boundaries.core]
owns = ["contacts"]
crates = ["crm-core*"]

[boundaries.marketing]
owns = ["campaigns", "attribution_*"]
crates = ["crm-marketing"]
"#;

    fn config(extra: &str) -> BoundaryConfig {
        let mut text = CONFIG.to_owned();
        text.push_str(extra);
        BoundaryConfig::parse(&text).expect("valid config")
    }

    fn tables(report: &Report) -> Vec<&TableViolation> {
        report
            .violations
            .iter()
            .filter_map(|v| match v {
                Violation::Table(t) => Some(t),
                Violation::Crate(_) => None,
            })
            .collect()
    }

    fn crate_violations(report: &Report) -> Vec<&CrateViolation> {
        report
            .violations
            .iter()
            .filter_map(|v| match v {
                Violation::Crate(c) => Some(c),
                Violation::Table(_) => None,
            })
            .collect()
    }

    fn table_violation<'a>(report: &'a Report, handler: &str) -> Option<&'a TableViolation> {
        tables(report)
            .into_iter()
            .find(|v| v.node.ends_with(&format!("::{handler}")))
    }

    // ── Config ───────────────────────────────────────────────────────

    #[test]
    fn the_config_parses_every_list() {
        let config = config(
            r#"
[boundaries.billing]
owns = ["invoices"]
modules = ["app::billing"]
may_read = ["contacts"]
may_write = ["campaigns"]
schema_crates = ["billing-schema"]
"#,
        );
        let billing = &config.boundaries["billing"];
        assert_eq!(billing.modules, ["app::billing"]);
        assert_eq!(billing.may_read, ["contacts"]);
        assert_eq!(billing.may_write, ["campaigns"]);
        assert_eq!(billing.schema_crates, ["billing-schema"]);
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = BoundaryConfig::parse("[boundaries.core]\nowns = [\"a\"]\nmay_reed = []\n")
            .expect_err("a typo must not pass");
        assert!(
            matches!(err, BoundaryError::Parse(ref m) if m.contains("may_reed")),
            "{err}"
        );
    }

    #[test]
    fn an_empty_boundary_is_rejected() {
        let err = BoundaryConfig::parse("[boundaries.core]\nmay_read = [\"a\"]\n")
            .expect_err("a boundary with no members must not pass");
        assert_eq!(err, BoundaryError::Empty("core".into()));
    }

    #[test]
    fn a_plugin_joins_the_boundary_that_lists_its_crate() {
        let mut config = BoundaryConfig::parse("[boundaries.admin]\ncrates = [\"admin-plugin\"]\n")
            .expect("valid");
        config.add_plugin("admin-plugin", &["admin_log".to_owned()]);
        assert_eq!(config.boundaries.len(), 1);
        assert_eq!(config.boundaries["admin"].owns, ["admin_log"]);
    }

    #[test]
    fn a_plugin_table_the_config_owns_stays_with_the_config() {
        let mut config =
            BoundaryConfig::parse("[boundaries.admin]\nowns = [\"admin_*\"]\n").expect("valid");
        config.add_plugin("admin-plugin", &["admin_log".to_owned()]);
        let plugin = &config.boundaries["admin-plugin"];
        assert!(plugin.owns.is_empty(), "{plugin:?}");
        assert!(check(&config, Some(&graph()), None).is_ok());
    }

    #[test]
    fn a_plugin_adds_a_boundary_of_its_own() {
        let mut config = config("");
        config.add_plugin("autumn-admin-plugin", &["admin_audit_log".to_owned()]);
        let plugin = &config.boundaries["autumn-admin-plugin"];
        assert_eq!(plugin.owns, ["admin_audit_log"]);
        assert_eq!(plugin.crates, ["autumn-admin-plugin"]);
    }

    // ── Graph rule ───────────────────────────────────────────────────

    #[test]
    fn a_route_that_reads_another_boundarys_table_is_a_violation() {
        let report = check(&config(""), Some(&graph()), None).expect("check runs");
        let v = table_violation(&report, "report").expect("report reads contacts");
        assert_eq!(v.boundary, "marketing");
        assert_eq!(v.owner, "core");
        assert_eq!(v.table, "contacts");
        assert_eq!(v.access, Access::Read);
        assert_eq!(v.provenance, Provenance::Body);
        assert_eq!(v.symbol, "contacts");
        assert_eq!(v.via, None);
        let text = Violation::Table(v.clone()).render();
        for needle in [
            "marketing",
            "core",
            "contacts",
            "GET /report",
            "body",
            "`contacts`",
        ] {
            assert!(text.contains(needle), "{needle} missing from {text}");
        }
    }

    #[test]
    fn may_read_clears_a_read_violation() {
        let report = check(&config("may_read = [\"contacts\"]\n"), Some(&graph()), None)
            .expect("check runs");
        assert!(table_violation(&report, "report").is_none());
        assert!(table_violation(&report, "joined").is_none());
    }

    /// The grant planner reads `public.contacts` as one schema and table;
    /// the graph has no schemas, so it reads the table part.
    #[test]
    fn a_schema_qualified_exception_clears_a_read_violation() {
        let report = check(
            &config("may_read = [\"public.contacts\"]\n"),
            Some(&graph()),
            None,
        )
        .expect("check runs");
        assert!(table_violation(&report, "report").is_none());
        assert!(table_violation(&report, "joined").is_none());
    }

    #[test]
    fn a_raw_sql_join_is_a_violation() {
        let report = check(&config(""), Some(&graph()), None).expect("check runs");
        // Only the SQL path folds `CONTACTS` to the `contacts` table.
        let v = table_violation(&report, "joined").expect("the JOIN reaches contacts");
        assert_eq!((v.owner.as_str(), v.table.as_str()), ("core", "contacts"));
        assert_eq!(v.provenance, Provenance::Body);
        assert_eq!(v.symbol, "CONTACTS");
    }

    #[test]
    fn a_write_through_a_repository_names_the_repository() {
        let report = check(&config(""), Some(&graph()), None).expect("check runs");
        let v = table_violation(&report, "sync").expect("sync writes contacts");
        assert_eq!(v.access, Access::Write);
        assert_eq!(v.provenance, Provenance::Signature);
        assert_eq!(
            v.via.as_deref(),
            Some("repository:crm_core::repositories::ContactRepository")
        );
        assert!(
            Violation::Table(v.clone())
                .render()
                .contains("ContactRepository")
        );
    }

    #[test]
    fn a_direct_edge_wins_over_a_repository_edge() {
        const BOTH: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            signature_symbols: &["PgContactRepository"],
            body_symbols: &["contacts"],
            ..route("both", "crm_marketing::routes", "GET", "/both")
        }];
        let mut graph = build(&[], 0, BOTH, MODELS, REPOSITORIES, &[]);
        // `build` sorts the model edge first. Reverse the order, so the
        // repository edge comes first and the direct edge must replace it.
        graph.edges.reverse();
        let report = check(&config(""), Some(&graph), None).expect("check runs");
        let found = tables(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].via, None);
        assert_eq!(found[0].symbol, "contacts");
    }

    #[test]
    fn a_repository_with_an_overridden_table_reaches_that_table() {
        const ARCHIVE: &[RepositoryGraphDescriptor] = &[RepositoryGraphDescriptor {
            repository: "ArchiveRepository",
            implementation: "PgArchiveRepository",
            model: "Contact",
            table: "contacts_archive",
            api: "",
            module_path: "crm_core::repositories",
            file: "src/repositories.rs",
            line: 1,
            crate_version: "0.1.0",
        }];
        const USER: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            signature_symbols: &["PgArchiveRepository"],
            ..route("archive", "crm_marketing::routes", "GET", "/archive")
        }];
        let config = config(
            "may_write = [\"contacts\"]\n\
             [boundaries.archive]\nowns = [\"contacts_archive\"]\nmodules = [\"crm_archive\"]\n",
        );
        let graph = build(&[], 0, USER, MODELS, ARCHIVE, &[]);
        let report = check(&config, Some(&graph), None).expect("check runs");
        let route = table_violation(&report, "archive").expect("the route reaches the archive");
        assert_eq!(
            (route.owner.as_str(), route.table.as_str()),
            ("archive", "contacts_archive")
        );
        // The core repository over the archive table is a violation too.
        assert!(
            tables(&report)
                .iter()
                .any(|v| v.kind == NodeKind::Repository && v.table == "contacts_archive"),
            "{:?}",
            report.violations
        );
    }

    #[test]
    fn an_ambiguous_repository_table_is_an_error() {
        // No model maps `archive_rows`; only the repository names it.
        const ARCHIVE: &[RepositoryGraphDescriptor] = &[RepositoryGraphDescriptor {
            repository: "ArchiveRepository",
            implementation: "PgArchiveRepository",
            model: "Contact",
            table: "archive_rows",
            api: "",
            module_path: "crm_core::repositories",
            file: "src/repositories.rs",
            line: 1,
            crate_version: "0.1.0",
        }];
        let config = BoundaryConfig::parse(
            "[boundaries.a]\nowns = [\"archive_*\"]\n[boundaries.b]\nowns = [\"*ive_rows\"]\n",
        )
        .expect("valid");
        let graph = build(&[], 0, &[], MODELS, ARCHIVE, &[]);
        let err = check(&config, Some(&graph), None).expect_err("ambiguous");
        assert!(
            matches!(err, BoundaryError::Ambiguous { what: "table", .. }),
            "{err}"
        );
    }

    #[test]
    fn a_versioned_package_key_matches_by_its_name() {
        // The CLI keys a package that resolves at two versions as
        // `name@version`, so their dependencies stay apart.
        let deps = crates(&[("crm-marketing", "crm-core@0.2.0")]);
        let report = check(&config(""), Some(&graph()), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].schema_crate, "crm-core@0.2.0");
        // The key has no version, so an upgrade keeps the baseline entry.
        let key = Violation::Crate(found[0].clone()).key();
        assert!(!key.to_string().contains('@'), "{key}");
    }

    #[test]
    fn an_ambiguous_node_is_an_error() {
        let config = BoundaryConfig::parse(
            "[boundaries.a]\nmodules = [\"crm_marketing\"]\n\
             [boundaries.b]\nmodules = [\"crm_marketing\"]\n",
        )
        .expect("valid");
        let err = check(&config, Some(&graph()), None).expect_err("ambiguous");
        assert!(
            matches!(err, BoundaryError::Ambiguous { what: "node", .. }),
            "{err}"
        );
    }

    #[test]
    fn a_key_holds_the_base_id_and_the_access() {
        let report = current();
        let Violation::Table(mut v) = report.violations[0].clone() else {
            panic!("a table violation");
        };
        let read = Violation::Table(v.clone()).key();
        v.access = Access::Write;
        assert_ne!(
            Violation::Table(v.clone()).key(),
            read,
            "a new write is a new violation"
        );
        v.node.push_str("@0.2.0");
        let BaselineEntry::Table { node, .. } = Violation::Table(v).key() else {
            panic!("a table key");
        };
        assert!(!node.contains('@'), "{node}");
    }

    #[test]
    fn may_read_does_not_clear_a_write() {
        let report = check(&config("may_read = [\"contacts\"]\n"), Some(&graph()), None)
            .expect("check runs");
        assert!(table_violation(&report, "sync").is_some());
    }

    #[test]
    fn may_write_clears_a_write() {
        let report = check(
            &config("may_write = [\"contacts\"]\n"),
            Some(&graph()),
            None,
        )
        .expect("check runs");
        assert!(tables(&report).is_empty(), "{:?}", report.violations);
    }

    #[test]
    fn own_tables_and_unbounded_nodes_are_not_checked() {
        let report = check(&config(""), Some(&graph()), None).expect("check runs");
        assert!(table_violation(&report, "list").is_none());
        assert!(table_violation(&report, "home").is_none());
        // The core repository over its own model is not a violation.
        assert!(
            !tables(&report)
                .iter()
                .any(|v| v.kind == NodeKind::Repository)
        );
        assert_eq!(tables(&report).len(), 3, "{:?}", report.violations);
    }

    #[test]
    fn a_repository_in_one_boundary_over_another_boundarys_model_is_a_violation() {
        let mut config = config("");
        config
            .boundaries
            .get_mut("marketing")
            .expect("marketing")
            .modules
            .push("crm_core::repositories".into());
        let report = check(&config, Some(&graph()), None).expect("check runs");
        // The module pattern is longer, so it beats `crm-core*`.
        let v = tables(&report)
            .into_iter()
            .find(|v| v.kind == NodeKind::Repository)
            .expect("the repository is now in marketing");
        assert_eq!(v.access, Access::ReadWrite);
        assert_eq!(v.provenance, Provenance::Declaration);
    }

    #[test]
    fn the_more_specific_pattern_wins() {
        const ADS: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("ads", "crm_core_ad::routes", "GET", "/ads")
        }];
        // `crm-core*` and `crm-core-ad*` both match `crm_core_ad`.
        let config = BoundaryConfig::parse(
            r#"
[boundaries.core]
owns = ["contacts"]
crates = ["crm-core*"]
[boundaries.marketing]
owns = ["campaigns"]
crates = ["crm-core-ad*"]
"#,
        )
        .expect("valid");
        let report =
            check(&config, Some(&build(&[], 0, ADS, MODELS, &[], &[])), None).expect("runs");
        let v = table_violation(&report, "ads").expect("ads is in marketing");
        assert_eq!(v.boundary, "marketing");
    }

    #[test]
    fn an_owns_glob_matches_a_table_without_case() {
        const READER: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["Touch"],
            ..route("touches", "crm_core::routes", "GET", "/touches")
        }];
        let config = BoundaryConfig::parse(
            "[boundaries.marketing]\nowns = [\"ATTRIBUTION_*\"]\nmodules = [\"x\"]\n\
             [boundaries.core]\nowns = []\nmodules = [\"crm_core\"]\n",
        )
        .expect("valid");
        let report = check(
            &config,
            Some(&build(&[], 0, READER, MODELS, &[], &[])),
            None,
        )
        .expect("runs");
        let v = table_violation(&report, "touches").expect("core reads attribution_touches");
        assert_eq!(
            (v.owner.as_str(), v.table.as_str()),
            ("marketing", "attribution_touches")
        );
    }

    #[test]
    fn a_table_two_boundaries_claim_equally_is_an_error() {
        let config = BoundaryConfig::parse(
            "[boundaries.a]\nowns = [\"contacts\"]\n[boundaries.b]\nowns = [\"contacts\"]\n",
        )
        .expect("valid");
        let err = check(&config, Some(&graph()), None).expect_err("ambiguous");
        assert!(
            matches!(err, BoundaryError::Ambiguous { what: "table", .. }),
            "{err}"
        );
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_a_warning() {
        let report = check(
            &config("[boundaries.ghost]\nowns = [\"nothing_here\"]\nmodules = [\"no::such\"]\n"),
            Some(&graph()),
            None,
        )
        .expect("check runs");
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("nothing_here"), "{warnings}");
        assert!(warnings.contains("no::such"), "{warnings}");
    }

    // ── Crate rule ───────────────────────────────────────────────────

    fn crates(edges: &[(&str, &str)]) -> CrateGraph {
        let mut graph = CrateGraph::default();
        for (from, to) in edges {
            graph.add(from, to);
        }
        graph
    }

    #[test]
    fn a_crate_that_depends_on_another_boundarys_schema_crate_is_a_violation() {
        let deps = crates(&[("crm-marketing", "crm-core"), ("crm-app", "crm-core")]);
        let report = check(&config(""), Some(&graph()), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        let v = found[0];
        assert_eq!(
            (v.boundary.as_str(), v.owner.as_str()),
            ("marketing", "core")
        );
        assert_eq!(v.krate, "crm-marketing");
        assert_eq!(v.schema_crate, "crm-core");
        assert_eq!(v.path, ["crm-marketing", "crm-core"]);
        let text = Violation::Crate(v.clone()).render();
        assert!(text.contains("crm-marketing -> crm-core"), "{text}");
    }

    #[test]
    fn a_dependency_through_an_unbounded_helper_crate_is_a_violation() {
        let deps = crates(&[
            ("crm-marketing", "crm-helpers"),
            ("crm-helpers", "crm-core"),
        ]);
        let report = check(&config(""), Some(&graph()), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, ["crm-marketing", "crm-helpers", "crm-core"]);
    }

    #[test]
    fn schema_crates_name_the_crate_that_holds_the_schema() {
        let mut config = config("");
        config
            .boundaries
            .get_mut("core")
            .expect("core")
            .schema_crates = vec!["crm-core-schema".into()];
        // `crm-core` holds the models but is no longer the schema crate.
        let deps = crates(&[
            ("crm-marketing", "crm-core"),
            ("crm-marketing", "crm-core-schema"),
        ]);
        let report = check(&config, Some(&graph()), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].schema_crate, "crm-core-schema");
    }

    #[test]
    fn may_read_allows_the_dependency_it_needs() {
        let deps = crates(&[("crm-marketing", "crm-core")]);
        let report = check(
            &config("may_read = [\"contacts\"]\n"),
            Some(&graph()),
            Some(&deps),
        )
        .expect("runs");
        assert!(crate_violations(&report).is_empty());
    }

    #[test]
    fn may_write_allows_the_dependency_it_needs() {
        let deps = crates(&[("crm-marketing", "crm-core")]);
        let report = check(
            &config("may_write = [\"contacts\"]\n"),
            Some(&graph()),
            Some(&deps),
        )
        .expect("runs");
        assert!(crate_violations(&report).is_empty());
    }

    #[test]
    fn a_glob_exception_allows_the_dependency_and_does_not_warn() {
        let deps = crates(&[("crm-marketing", "crm-core")]);
        let report = check(
            &config("may_read = [\"contact*\"]\n"),
            Some(&graph()),
            Some(&deps),
        )
        .expect("runs");
        assert!(
            crate_violations(&report).is_empty(),
            "{:?}",
            report.violations
        );
        assert!(
            !report.warnings.iter().any(|w| w.contains("contact*")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn the_search_stops_at_a_crate_in_a_boundary() {
        // `crm-core-api` is in core (by `crm-core*`) and depends on the core
        // schema. Marketing depends on the API crate only. That crate is
        // checked on its own; marketing is not.
        let mut config = config("");
        config
            .boundaries
            .get_mut("core")
            .expect("core")
            .schema_crates = vec!["crm-core-schema".into()];
        let deps = crates(&[
            ("crm-marketing", "crm-core-api"),
            ("crm-core-api", "crm-core-schema"),
        ]);
        let report = check(&config, Some(&graph()), Some(&deps)).expect("runs");
        assert!(
            crate_violations(&report).is_empty(),
            "{:?}",
            report.violations
        );
    }

    #[test]
    fn the_search_stops_at_a_crate_in_a_modules_boundary() {
        let config = BoundaryConfig::parse(
            "[boundaries.core]\nowns = [\"contacts\"]\nmodules = [\"crm_core\"]\n\
             [boundaries.billing]\nowns = [\"campaigns\"]\ncrates = [\"crm-billing\"]\n\
             schema_crates = [\"crm-billing\"]\n\
             [boundaries.marketing]\nowns = []\ncrates = [\"crm-marketing\"]\n",
        )
        .expect("valid");
        let deps = crates(&[("crm-marketing", "crm-core"), ("crm-core", "crm-billing")]);
        let report = check(&config, Some(&graph()), Some(&deps)).expect("runs");
        // `crm-core` is in core by `modules`, so the search does not go
        // through it to the billing schema.
        let found = crate_violations(&report);
        assert!(!found.iter().any(|v| v.owner == "billing"), "{found:?}");
        assert!(found.iter().any(|v| v.owner == "core"), "{found:?}");
    }

    #[test]
    fn a_schema_crate_matches_its_library_name() {
        // The package `crm-core-pkg` builds the library `crm_core`, which is
        // the name in the module paths of the models.
        let mut deps = crates(&[("crm-marketing", "crm-core-pkg")]);
        deps.libs.insert("crm-core-pkg".into(), "crm_core".into());
        let report = check(&config(""), Some(&graph()), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].schema_crate, "crm-core-pkg");
    }

    #[test]
    fn a_versioned_package_with_a_library_name_keeps_its_nodes() {
        // Two versions resolve, so the CLI keys the package `crm-mkt-pkg@1.0.0`.
        const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("versioned_report", "crm_mkt::routes", "GET", "/versioned")
        }];
        let config = BoundaryConfig::parse(
            "[boundaries.core]\nowns = [\"contacts\"]\ncrates = [\"crm-core\"]\n\
             [boundaries.marketing]\nowns = [\"campaigns\"]\ncrates = [\"crm-mkt-pkg\"]\n",
        )
        .expect("valid");
        let mut deps = crates(&[("crm-mkt-pkg@1.0.0", "crm-core")]);
        deps.libs
            .insert("crm-mkt-pkg@1.0.0".into(), "crm_mkt".into());
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let report = check(&config, Some(&graph), Some(&deps)).expect("runs");
        let v = table_violation(&report, "versioned_report").expect("the route is in marketing");
        assert_eq!(v.boundary, "marketing");
    }

    #[test]
    fn a_node_joins_the_boundary_of_its_package_by_library_name() {
        // The package `crm-mkt-pkg` builds the library `crm_mkt`. Its module
        // paths start with `crm_mkt`, but the config names the package.
        const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("lib_report", "crm_mkt::routes", "GET", "/lib-report")
        }];
        let config = BoundaryConfig::parse(
            "[boundaries.core]\nowns = [\"contacts\"]\ncrates = [\"crm-core\"]\n\
             [boundaries.marketing]\nowns = [\"campaigns\"]\ncrates = [\"crm-mkt-pkg\"]\n",
        )
        .expect("valid");
        let mut deps = crates(&[("crm-mkt-pkg", "crm-core")]);
        deps.libs.insert("crm-mkt-pkg".into(), "crm_mkt".into());
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let report = check(&config, Some(&graph), Some(&deps)).expect("runs");
        let v = table_violation(&report, "lib_report").expect("the route is in marketing");
        assert_eq!(v.boundary, "marketing");
    }

    /// `core` owns `contacts`. `marketing` is the package `crm-marketing`.
    fn package_config() -> BoundaryConfig {
        BoundaryConfig::parse(
            "[boundaries.core]\nowns = [\"contacts\"]\ncrates = [\"crm-core\"]\n\
             [boundaries.marketing]\nowns = [\"campaigns\"]\ncrates = [\"crm-marketing\"]\n",
        )
        .expect("valid")
    }

    fn bins(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn a_node_joins_the_boundary_of_its_package_by_binary_name() {
        // `crm-marketing` has `[[bin]] name = "marketing-server"`. Its module
        // paths start with `marketing_server`.
        const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("bin_report", "marketing_server::routes", "GET", "/bin")
        }];
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let mut deps = crates(&[("crm-marketing", "crm-core")]);
        let report = check(&package_config(), Some(&graph), Some(&deps)).expect("runs");
        assert!(
            table_violation(&report, "bin_report").is_none(),
            "with no binary name, no boundary claims the route"
        );
        deps.bins
            .insert("crm-marketing".into(), bins(&["marketing_server"]));
        let report = check(&package_config(), Some(&graph), Some(&deps)).expect("runs");
        let v = table_violation(&report, "bin_report").expect("the route is in marketing");
        assert_eq!(v.boundary, "marketing");
    }

    #[test]
    fn a_package_with_a_library_and_a_binary_claims_nodes_of_both() {
        const ROUTES: &[RouteGraphDescriptor] = &[
            RouteGraphDescriptor {
                body_symbols: &["contacts"],
                ..route("lib_report", "crm_mkt::routes", "GET", "/lib")
            },
            RouteGraphDescriptor {
                body_symbols: &["contacts"],
                ..route("bin_report", "marketing_server::routes", "GET", "/bin")
            },
        ];
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let mut deps = crates(&[("crm-marketing@1.0.0", "crm-core")]);
        deps.libs
            .insert("crm-marketing@1.0.0".into(), "crm_mkt".into());
        deps.bins
            .insert("crm-marketing@1.0.0".into(), bins(&["marketing_server"]));
        let report = check(&package_config(), Some(&graph), Some(&deps)).expect("runs");
        for handler in ["lib_report", "bin_report"] {
            let v = table_violation(&report, handler).expect("the route is in marketing");
            assert_eq!(v.boundary, "marketing", "{handler}");
        }
    }

    #[test]
    fn a_binary_name_of_two_packages_in_two_boundaries_is_an_error() {
        // Two packages build a binary `server`. The module path cannot tell
        // which package the node is in.
        const ROUTES: &[RouteGraphDescriptor] = &[route("home", "server::routes", "GET", "/")];
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let config = BoundaryConfig::parse(
            "[boundaries.a]\nowns = [\"contacts\"]\ncrates = [\"crm-a\"]\n\
             [boundaries.b]\nowns = [\"campaigns\"]\ncrates = [\"crm-b\"]\n",
        )
        .expect("valid");
        let mut deps = CrateGraph::default();
        deps.bins.insert("crm-a".into(), bins(&["server"]));
        deps.bins.insert("crm-b".into(), bins(&["server"]));
        let err = check(&config, Some(&graph), Some(&deps)).expect_err("ambiguous");
        assert!(
            matches!(err, BoundaryError::Ambiguous { what: "node", .. }),
            "{err}"
        );
    }

    #[test]
    fn a_binary_with_a_library_name_does_not_claim_its_nodes() {
        // `crm-marketing` has a binary `crm-shared`, the name of the library
        // of the package `crm-shared`. A node under `crm_shared` stays with
        // that library, which is in no boundary.
        const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("shared_report", "crm_shared::routes", "GET", "/shared")
        }];
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let mut deps = crates(&[("crm-marketing", "crm-shared")]);
        deps.bins
            .insert("crm-marketing".into(), bins(&["crm_shared"]));
        let report = check(&package_config(), Some(&graph), Some(&deps)).expect("runs");
        assert!(table_violation(&report, "shared_report").is_none());
    }

    #[test]
    fn a_pattern_on_a_binary_name_wins_over_its_package() {
        // `workers` names the binary. `billing` names its package.
        const ROUTES: &[RouteGraphDescriptor] = &[RouteGraphDescriptor {
            body_symbols: &["contacts"],
            ..route("work", "crm_billing_worker::jobs", "POST", "/work")
        }];
        let config = BoundaryConfig::parse(
            "[boundaries.core]\nowns = [\"contacts\"]\ncrates = [\"crm-core\"]\n\
             [boundaries.billing]\nowns = [\"invoices\"]\ncrates = [\"crm-billing\"]\n\
             [boundaries.workers]\nowns = [\"jobs\"]\ncrates = [\"crm-*-worker\"]\n",
        )
        .expect("valid");
        let graph = build(&[], 0, ROUTES, MODELS, &[], &[]);
        let mut deps = crates(&[("crm-billing", "crm-core")]);
        deps.bins
            .insert("crm-billing".into(), bins(&["crm_billing_worker"]));
        let report = check(&config, Some(&graph), Some(&deps)).expect("runs");
        let v = table_violation(&report, "work").expect("the node is in workers");
        assert_eq!(v.boundary, "workers");
        // The package stays in billing for the crate rule.
        let found = crate_violations(&report);
        assert!(
            found
                .iter()
                .any(|v| v.krate == "crm-billing" && v.boundary == "billing"),
            "{found:?}"
        );
    }

    #[test]
    fn a_model_in_a_binary_does_not_make_its_package_a_schema_crate() {
        // A dependent links the library of `crm-marketing`, not its binary.
        const MODELS: &[ModelGraphDescriptor] = &[
            model("Contact", "contacts", "crm_core::models"),
            model("Campaign", "campaigns", "marketing_server::models"),
        ];
        let config = BoundaryConfig::parse(
            "[boundaries.marketing]\nowns = [\"campaigns\"]\ncrates = [\"crm-marketing\"]\n\
             [boundaries.sales]\nowns = [\"deals\"]\ncrates = [\"crm-sales\"]\n",
        )
        .expect("valid");
        let graph = build(&[], 0, &[], MODELS, &[], &[]);
        let mut deps = crates(&[("crm-sales", "crm-marketing")]);
        deps.bins
            .insert("crm-marketing".into(), bins(&["marketing_server"]));
        let report = check(&config, Some(&graph), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_modules_pattern_on_a_binary_does_not_stop_the_crate_search() {
        // `modules` names a binary of `crm-shared`. Its library is in no
        // boundary, so the search goes through it to the core schema.
        let config = BoundaryConfig::parse(
            "[boundaries.core]\nowns = [\"contacts\"]\ncrates = [\"crm-core\"]\n\
             [boundaries.admin]\nowns = [\"admins\"]\nmodules = [\"shared_admin\"]\n\
             [boundaries.marketing]\nowns = [\"campaigns\"]\ncrates = [\"crm-marketing\"]\n",
        )
        .expect("valid");
        let mut deps = crates(&[("crm-marketing", "crm-shared"), ("crm-shared", "crm-core")]);
        deps.bins
            .insert("crm-shared".into(), bins(&["shared_admin"]));
        let report = check(&config, Some(&graph()), Some(&deps)).expect("runs");
        let found = crate_violations(&report);
        assert!(
            found
                .iter()
                .any(|v| v.path == ["crm-marketing", "crm-shared", "crm-core"]),
            "{found:?}"
        );
    }

    #[test]
    fn without_a_graph_a_derived_schema_crate_is_a_warning() {
        let deps = crates(&[("crm-marketing", "crm-core")]);
        let report = check(&config(""), None, Some(&deps)).expect("runs");
        assert!(
            report.warnings.iter().any(|w| w.contains("schema_crates")),
            "{:?}",
            report.warnings
        );
    }

    // ── Baseline and ratchet ─────────────────────────────────────────

    fn current() -> Report {
        check(&config(""), Some(&graph()), None).expect("check runs")
    }

    #[test]
    fn with_no_baseline_every_violation_is_new() {
        let result = ratchet(current(), None);
        assert_eq!(result.new.len(), 3);
        assert!(!result.passes());
    }

    #[test]
    fn a_baseline_of_today_passes() {
        let report = current();
        let baseline = Baseline::from_violations(&report.violations);
        assert_eq!(baseline.violations.len(), 3);
        let result = ratchet(report, Some(&baseline));
        assert!(result.passes(), "{}", result.render());
        assert_eq!(result.known.len(), 3);
    }

    #[test]
    fn a_new_violation_fails_against_the_baseline() {
        let report = current();
        let mut baseline = Baseline::from_violations(&report.violations);
        let dropped = baseline.violations.remove(0);
        let result = ratchet(report, Some(&baseline));
        assert!(!result.passes());
        assert_eq!(result.new.len(), 1);
        assert_eq!(result.new[0].key(), dropped);
        assert!(result.render().contains("new"), "{}", result.render());
    }

    #[test]
    fn a_fixed_violation_must_leave_the_baseline() {
        let report = current();
        let mut baseline = Baseline::from_violations(&report.violations);
        let gone = BaselineEntry::Table {
            boundary: "marketing".into(),
            owner: "core".into(),
            node: "route:crm_marketing::routes::removed".into(),
            table: "contacts".into(),
            access: Access::Read,
        };
        baseline.violations.push(gone.clone());
        let result = ratchet(report, Some(&baseline));
        assert!(!result.passes());
        assert_eq!(result.fixed, [gone]);
        assert!(
            result.render().contains("--update-baseline"),
            "{}",
            result.render()
        );
    }

    #[test]
    fn fixing_one_violation_shrinks_the_baseline_by_one() {
        let report = current();
        let baseline = Baseline::from_violations(&report.violations);
        // Allow the reads. Only the write through the repository is left.
        let fixed =
            check(&config("may_read = [\"contacts\"]\n"), Some(&graph()), None).expect("runs");
        let result = ratchet(fixed.clone(), Some(&baseline));
        assert_eq!(result.fixed.len(), 2, "{:?}", result.fixed);
        assert_eq!(result.known.len(), 1);
        let shrunk = Baseline::from_violations(&fixed.violations);
        assert_eq!(shrunk.violations.len(), 1);
        assert!(ratchet(fixed, Some(&shrunk)).passes());
    }

    #[test]
    fn a_rule_that_did_not_run_fixes_nothing() {
        let report = current();
        let baseline = Baseline::from_violations(&report.violations);
        // No graph: the graph rule did not run, so its entries are not fixed.
        let without_graph = check(&config(""), None, None).expect("runs");
        let result = ratchet(without_graph, Some(&baseline));
        assert!(result.fixed.is_empty(), "{:?}", result.fixed);
        assert!(result.passes());
    }

    #[test]
    fn the_baseline_round_trips_and_is_sorted() {
        let report = current();
        let baseline = Baseline::from_violations(&report.violations);
        let mut sorted = baseline.violations.clone();
        sorted.sort();
        assert_eq!(baseline.violations, sorted);
        let parsed = Baseline::parse(&baseline.to_json()).expect("round trip");
        assert_eq!(parsed, baseline);
        assert!(Baseline::parse("{\"version\":2,\"violations\":[]}").is_err());
    }

    #[test]
    fn a_key_ignores_the_evidence() {
        let report = current();
        let Violation::Table(mut v) = report.violations[0].clone() else {
            panic!("a table violation");
        };
        let before = Violation::Table(v.clone()).key();
        v.provenance = Provenance::Signature;
        v.symbol = "Other".into();
        assert_eq!(Violation::Table(v).key(), before);
    }

    // ── Matching ─────────────────────────────────────────────────────

    #[test]
    fn glob_matches_runs() {
        assert!(glob("attribution_*", "attribution_touches"));
        assert!(glob("*", ""));
        assert!(glob("a*c*e", "abcde"));
        assert!(glob("contacts", "contacts"));
        assert!(!glob("contacts", "contacts2"));
        assert!(!glob("a*c", "abd"));
        assert!(!glob("*s", "contact"));
    }
}
