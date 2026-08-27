//! Build-time cache-coherence proof (issue #1716).
//!
//! Autumn owns both ends of the staleness dependency: cached reads flow through
//! [`#[cached]`](autumn_macros::cached), the fragment cache and the read-through
//! cache; writes flow through [`#[repository]`](autumn_macros::repository).
//! Until now nothing linked the two, so a cached value derived from `Post` rows
//! kept being served after a `PostRepository::save` inserted a new one and the
//! build said nothing.
//!
//! This module is the link. The macros publish what they know as `inventory`
//! descriptors — which models a cached read is derived from, which model each
//! repository write mutates, and which reads a write declares it invalidates —
//! and [`check`] proves, over the whole binary, that no mutation can strand a
//! cached value.
//!
//! # The rule
//!
//! A `(read, mutation)` pair is a **violation** when all four hold:
//!
//! 1. the read's dependency set is known (its provenance is not
//!    [`DependencyProvenance::Undetermined`]),
//! 2. neither side is acknowledged-stale,
//! 3. the read's dependency set intersects the mutation's model, and
//! 4. the mutation does not declare an invalidation naming that read.
//!
//! Reads whose dependency set could **not** be established are never failed by
//! the default gate — a checker that cries wolf gets deleted from CI. They are
//! reported as [`undetermined_reads`] instead, and `--strict` turns them into a
//! failure for an app that wants the stronger posture.
//!
//! # Identity
//!
//! A cached read is identified by its **cache-key namespace** —
//! `concat!(module_path!(), "::", <fn name>)`, the exact prefix
//! [`make_cache_key`](super::make_cache_key) already stamps on every entry. That
//! keeps the manifest's identity and the runtime's key space the same string.
//!
//! Models are matched on their **last path segment**, because the two sides
//! learn the name differently: a `#[repository]` always has the model *type* in
//! scope and publishes `core::any::type_name` (`blog::models::Post`), while a
//! dependency recovered from a `#[cached]` body may only be the bare ident
//! (`Post`). Two same-named models in different modules therefore
//! over-approximate — a false *failure*, never a false pass — which is the safe
//! direction for a coherence gate and is dischargeable with
//! `acknowledge_stale`.
//!
//! See `docs/guide/cache-coherence.md`.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Schema version of the emitted cache-coherence manifest. Bumped only on
/// breaking changes to the document shape.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Machine-readable stdout marker preceding the manifest JSON emitted by the
/// `AUTUMN_DUMP_CACHE_COHERENCE=1` dump mode.
///
/// A process-boundary protocol: `autumn cache audit` runs the built binary as a
/// child and scans its stdout for this marker, so an app that prints anything
/// else during startup cannot corrupt the parse.
pub const COHERENCE_MANIFEST_MARKER: &str = "[autumn:cache-coherence] ";

// ── Descriptors published by the macros ──────────────────────────────

/// Which cache surface a read is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadKind {
    /// A `#[cached]` memoized function.
    Cached,
    /// A `cache_fragment(...)` template fragment.
    Fragment,
    /// A `get_or_compute(...)` read-through entry.
    ReadThrough,
}

impl ReadKind {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Fragment => "fragment",
            Self::ReadThrough => "read_through",
        }
    }
}

/// How a cached read's dependency set was established.
///
/// The classes are strictly decreasing in strength, and an entry may only claim
/// the class it can defend — the same discipline
/// `docs/guide/security-posture-manifest.md` applies to the security manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyProvenance {
    /// The developer wrote `#[cached(reads(Post, Comment))]`. The strongest
    /// claim: the set is exactly what was declared.
    Declared,
    /// Recovered by the macro from the function body (repository types, model
    /// finder calls). Sound for what it found; it can miss a dependency reached
    /// through a helper it cannot read.
    Derived,
    /// Nothing could be established. The read is reported, never gated.
    Undetermined,
}

impl DependencyProvenance {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Derived => "derived",
            Self::Undetermined => "undetermined",
        }
    }
}

/// A cached read, as published by the macro that created it.
///
/// `'static` and const-constructible so it can be `inventory::submit!`ed from
/// macro-generated code with no runtime cost.
pub struct CachedReadDescriptor {
    /// The cache-key namespace: `concat!(module_path!(), "::", <fn name>)`.
    pub id: &'static str,
    /// Which cache surface serves this read.
    pub kind: ReadKind,
    /// The models this read is derived from.
    ///
    /// Function pointers rather than plain strings because a model's identity is
    /// its *type*: where the type is nameable at the declaration site the entry
    /// is `|| core::any::type_name::<Post>()`, and where derivation only
    /// recovered an ident it is `|| "Post"`.
    pub reads: &'static [fn() -> &'static str],
    /// How [`Self::reads`] was established.
    pub provenance: DependencyProvenance,
    /// `Some(reason)` when the developer opted this read out of the gate.
    pub acknowledged_stale: Option<&'static str>,
    /// `file:line` of the declaration.
    pub location: &'static str,
}

inventory::collect!(CachedReadDescriptor);

/// A repository write method, as published by `#[repository]`.
pub struct MutationDescriptor {
    /// The repository trait name (e.g. `PostRepository`).
    pub repository: &'static str,
    /// The write method (e.g. `save`).
    pub method: &'static str,
    /// `core::any::type_name` of the mutated model.
    pub model: fn() -> &'static str,
    /// The mutated table.
    pub table: &'static str,
    /// Cache-read ids this write declares it invalidates.
    ///
    /// Populated from `#[repository(..., invalidates(path::to::cached_fn))]`,
    /// which resolves to the `#[cached]` function's own generated id constant —
    /// so rustc, not a string table, proves the target exists.
    pub invalidates: &'static [&'static str],
    /// `Some(reason)` when the developer opted this write out of the gate.
    pub acknowledged_stale: Option<&'static str>,
    /// `file:line` of the declaration.
    pub location: &'static str,
}

inventory::collect!(MutationDescriptor);

// ── Owned views (what the checker works over) ────────────────────────

/// An owned, comparable view of a [`CachedReadDescriptor`].
///
/// The checker is deliberately a pure function over owned values rather than
/// over `inventory` iterators, so the rule can be tested — and a seeded corpus
/// of staleness bugs exercised — without linking a whole app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedRead {
    /// The cache-key namespace identifying this read.
    pub id: String,
    /// Which cache surface serves it.
    pub kind: ReadKind,
    /// The models it is derived from.
    pub reads: Vec<String>,
    /// How [`Self::reads`] was established.
    pub provenance: DependencyProvenance,
    /// `Some(reason)` when opted out of the gate.
    pub acknowledged_stale: Option<String>,
    /// `file:line` of the declaration.
    pub location: String,
}

/// An owned, comparable view of a [`MutationDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutation {
    /// The repository trait name.
    pub repository: String,
    /// The write method.
    pub method: String,
    /// The mutated model's type name.
    pub model: String,
    /// The mutated table.
    pub table: String,
    /// Cache-read ids this write declares it invalidates.
    pub invalidates: Vec<String>,
    /// `Some(reason)` when opted out of the gate.
    pub acknowledged_stale: Option<String>,
    /// `file:line` of the declaration.
    pub location: String,
}

impl Mutation {
    /// `Repository::method` — how a mutation is named in every diagnostic.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::{}", self.repository, self.method)
    }
}

/// One proven staleness bug: a mutation that can strand a cached value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalenessFinding {
    /// The cached read left stale.
    pub read: String,
    /// Where that read is declared.
    pub read_location: String,
    /// `Repository::method` of the mutation that dirties it.
    pub mutation: String,
    /// Where that mutation is declared.
    pub mutation_location: String,
    /// The model both sides touch — the reason they are linked.
    pub model: String,
}

// ── Identity ─────────────────────────────────────────────────────────

/// Normalize a model name to its bare type name: the last path segment, with
/// references and generic arguments stripped.
///
/// `&blog::models::Post` and `Post` both key on `Post`; `Wrapper<Post>` keys on
/// `Wrapper`. Use [`models_match`] to compare two names — this is the fallback
/// half of that comparison, not the whole of it.
#[must_use]
pub fn model_key(name: &str) -> &str {
    let name = normalize_model_path(name);
    name.rsplit("::").next().unwrap_or(name).trim()
}

/// Strip the decoration around a model's path: references, pointers, a `mut`
/// binding mode, and any generic argument list.
fn normalize_model_path(name: &str) -> &str {
    let name = name.trim().trim_start_matches(['&', '*']).trim();
    let name = name.strip_prefix("mut ").unwrap_or(name).trim();
    name.split_once('<')
        .map_or(name, |(head, _)| head)
        .trim_end()
}

/// Whether two model names refer to the same model.
///
/// The two sides of a dependency learn the name differently. A `#[repository]`
/// always has the model *type* in scope and publishes `core::any::type_name`
/// (`blog::models::Post`); so does a `#[cached(reads(...))]` declaration. But a
/// dependency the macro *derived* from a function body may only be the bare
/// ident (`Post`), because the model type is often not in scope at the cached
/// function at all — a `PgPostRepository` parameter does not bring `Post` with
/// it.
///
/// So: when both sides are fully qualified, compare the full paths. That keeps
/// `plugin::models::User` and `crate::models::User` apart — two same-named
/// models in different modules are an ordinary shape, and collapsing them would
/// fail a correct app. Fall back to the bare type name only when at least one
/// side is all the analysis could recover, where over-approximating is the safe
/// direction.
#[must_use]
pub fn models_match(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_model_path(a), normalize_model_path(b));
    if a.contains("::") && b.contains("::") {
        return a == b;
    }
    model_key(a) == model_key(b)
}

// ── The rule ─────────────────────────────────────────────────────────

/// Prove that no mutation can strand a cached value.
///
/// Returns one [`StalenessFinding`] per uncovered `(read, mutation)` pair,
/// deterministically ordered by read, then mutation, then model, so a manifest
/// diffed across builds only changes when the app does.
///
/// See the module docs for the exact rule, and why reads with an
/// [`Undetermined`](DependencyProvenance::Undetermined) dependency set are
/// reported by [`undetermined_reads`] rather than failed here.
#[must_use]
pub fn check(reads: &[CachedRead], mutations: &[Mutation]) -> Vec<StalenessFinding> {
    let mut findings = Vec::new();

    for read in reads {
        if read.provenance == DependencyProvenance::Undetermined
            || read.acknowledged_stale.is_some()
        {
            continue;
        }
        for mutation in mutations {
            if mutation.acknowledged_stale.is_some() {
                continue;
            }
            // `any` — not a count — so a read that names one model twice (a
            // `declare_cached_read!` listing it under two paths, say) still
            // yields a single finding per mutation.
            if !read
                .reads
                .iter()
                .any(|dep| models_match(dep, &mutation.model))
            {
                continue;
            }
            if mutation.invalidates.iter().any(|id| id == &read.id) {
                continue;
            }
            findings.push(StalenessFinding {
                read: read.id.clone(),
                read_location: read.location.clone(),
                mutation: mutation.qualified_name(),
                mutation_location: mutation.location.clone(),
                model: mutation.model.clone(),
            });
        }
    }

    findings
        .sort_by(|a, b| (&a.read, &a.mutation, &a.model).cmp(&(&b.read, &b.mutation, &b.model)));
    findings
}

/// Every cached read whose dependency set could not be established.
///
/// These gate nothing by default — see the module docs — but a green audit that
/// is mostly `undetermined` proves very little, so they are always reported and
/// `--strict` turns them into a failure.
#[must_use]
pub fn undetermined_reads(reads: &[CachedRead]) -> Vec<&CachedRead> {
    reads
        .iter()
        .filter(|r| r.provenance == DependencyProvenance::Undetermined)
        .collect()
}

/// Whether the gate fails for this manifest.
///
/// The one place the rule lives, so `autumn cache audit`, an app's own test,
/// and the tests that pin the behavior are all asking the same function rather
/// than three copies of one condition.
///
/// Fails on a proven violation always, and on an undetermined dependency set
/// only under `strict` — the default never fails on what the analysis merely
/// could not read.
#[must_use]
pub const fn gate_failed(manifest: &CoherenceManifest, strict: bool) -> bool {
    !manifest.violations.is_empty() || (strict && !manifest.undetermined_reads.is_empty())
}

/// Exit code for the gate: `1` when [`gate_failed`], `0` otherwise.
#[must_use]
pub fn audit_exit_code(manifest: &CoherenceManifest, strict: bool) -> i32 {
    i32::from(gate_failed(manifest, strict))
}

// ── Diagnostics ──────────────────────────────────────────────────────

/// Render the precise diagnostic for a set of findings: for each, the cached
/// read, the mutations that can strand it, the model they share, and the two
/// ways to discharge it.
///
/// Grouped by `(read, model)` rather than printed one block per pair. A single
/// missing `invalidates(...)` on a `soft_delete` repository produces a dozen
/// findings — one per generated write method — and a dozen identical six-line
/// blocks with the same one-line fix is the output shape that gets a gate
/// switched off. The manifest keeps every pair; the human report names the one
/// edit.
#[must_use]
pub fn format_diagnostic(findings: &[StalenessFinding]) -> String {
    if findings.is_empty() {
        return String::new();
    }

    // Preserves `check`'s ordering: findings arrive sorted, so first-seen order
    // of each (read, model) group is already deterministic.
    let mut groups: Vec<(&StalenessFinding, Vec<&StalenessFinding>)> = Vec::new();
    for finding in findings {
        match groups
            .iter_mut()
            .find(|(head, _)| head.read == finding.read && head.model == finding.model)
        {
            Some((_, members)) => members.push(finding),
            None => groups.push((finding, vec![finding])),
        }
    }

    let stale_reads: usize = {
        let mut reads: Vec<&str> = groups.iter().map(|(head, _)| head.read.as_str()).collect();
        reads.sort_unstable();
        reads.dedup();
        reads.len()
    };

    let mut out = format!(
        "error: {stale_reads} cached read{} can be left stale by a repository write\n\n",
        if stale_reads == 1 { "" } else { "s" }
    );
    for (head, members) in &groups {
        let writes: Vec<&str> = members.iter().map(|f| f.mutation.as_str()).collect();
        // Every write method generated by one `#[repository]` carries that
        // attribute's own location, so this is normally a single site — but a
        // model with two repositories has two, and naming both is the point.
        let mut sites: Vec<&str> = members
            .iter()
            .map(|f| f.mutation_location.as_str())
            .collect();
        sites.dedup();
        let _ = writeln!(
            out,
            "  the cached read {read} is derived from {model},\n    \
             which {count} repository write{plural} mutate{verb} without invalidating it:\n      \
             {writes}\n    \
             read at    {read_loc}\n    \
             written at {sites}\n    \
             fix: add invalidates({read}) to the repository, or\n         \
             acknowledge the staleness with #[acknowledge_stale(reason = \"…\")].\n",
            read = head.read,
            model = head.model,
            count = writes.len(),
            plural = if writes.len() == 1 { "" } else { "s" },
            verb = if writes.len() == 1 { "s" } else { "" },
            writes = writes.join(", "),
            read_loc = head.read_location,
            sites = sites.join(", "),
        );
    }
    out
}

/// Render the human summary line printed by `autumn cache audit`.
///
/// A convenience over [`CoherenceManifest::summary`] for callers that hold the
/// values rather than the assembled manifest; both share one implementation of
/// the counting.
#[must_use]
pub fn format_summary(reads: &[CachedRead], mutations: &[Mutation]) -> String {
    CoherenceManifest::build(reads, mutations).summary()
}

// ── Manifest ─────────────────────────────────────────────────────────

/// A provenance-tagged manifest dimension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dimension<T> {
    /// `provable` / `declared` / `runtime-only`.
    pub provenance: String,
    /// Where the fact comes from (e.g. `macro:#[cached]`).
    pub source: String,
    /// The one part of the story a build cannot prove. Empty when there is none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runtime_caveat: String,
    /// The dimension's entries, stable-ordered.
    pub entries: Vec<T>,
}

/// One cached read in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRead {
    /// The cache-key namespace.
    pub id: String,
    /// `cached` / `fragment` / `read_through`.
    pub kind: String,
    /// Models this read is derived from, stable-ordered.
    pub reads: Vec<String>,
    /// `declared` / `derived` / `undetermined`.
    pub provenance: String,
    /// The acknowledged-stale reason, when opted out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_stale: Option<String>,
    /// `file:line`.
    pub location: String,
}

/// One repository write in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestMutation {
    /// `Repository::method`.
    pub name: String,
    /// The mutated model's type name.
    pub model: String,
    /// The mutated table.
    pub table: String,
    /// The acknowledged-stale reason, when opted out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledged_stale: Option<String>,
    /// `file:line`.
    pub location: String,
}

/// One declared invalidation edge in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestInvalidation {
    /// `Repository::method` of the write that declares the edge.
    pub mutation: String,
    /// The cached read it covers.
    pub read: String,
}

/// A cached read the analysis could not link to any model.
///
/// Carries its source location for the same reason every other diagnostic in
/// this feature does: an id alone makes the reader grep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeterminedRead {
    /// The read's cache-key namespace.
    pub id: String,
    /// `file:line` of the declaration.
    pub location: String,
}

/// A dimension deliberately left out of the manifest, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedDimension {
    /// The dimension's name.
    pub dimension: String,
    /// The provenance class it could eventually claim.
    pub eventual_provenance: String,
    /// Why it is not emitted today.
    pub reason: String,
}

/// The manifest's emitted dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dimensions {
    /// Every cached read the binary registers.
    pub cached_reads: Dimension<ManifestRead>,
    /// Every repository write the binary registers.
    pub mutations: Dimension<ManifestMutation>,
    /// Every declared invalidation edge.
    pub invalidations: Dimension<ManifestInvalidation>,
}

/// The cache-coherence manifest: what the build can prove about which cached
/// reads a write can strand, and how it knows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoherenceManifest {
    /// Document shape version.
    pub schema_version: u32,
    /// The emitted dimensions.
    pub dimensions: Dimensions,
    /// Proven staleness bugs, stable-ordered.
    pub violations: Vec<StalenessFinding>,
    /// Reads whose dependency set could not be established.
    pub undetermined_reads: Vec<UndeterminedRead>,
    /// Dimensions deliberately not emitted, and why.
    pub excluded: Vec<ExcludedDimension>,
}

impl CoherenceManifest {
    /// Assemble the manifest from the app's registered reads and mutations.
    #[must_use]
    pub fn build(reads: &[CachedRead], mutations: &[Mutation]) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            dimensions: Dimensions {
                cached_reads: cached_reads_dimension(reads),
                mutations: mutations_dimension(mutations),
                invalidations: invalidations_dimension(mutations),
            },
            violations: check(reads, mutations),
            undetermined_reads: undetermined_dimension(reads),
            excluded: excluded_dimensions(),
        }
    }

    /// The one-line human summary printed by `autumn cache audit`.
    ///
    /// Computed from the manifest's own entries rather than from the values it
    /// was built from, so the CLI and the framework can never disagree about
    /// the counts — and so a manifest read back from a file summarizes exactly
    /// the same way as one just built.
    #[must_use]
    pub fn summary(&self) -> String {
        let reads = &self.dimensions.cached_reads.entries;
        let count = |class: &str| reads.iter().filter(|e| e.provenance == class).count();
        let declared = count("declared");
        let derived = count("derived");
        let undetermined = reads.len() - declared - derived;
        let acknowledged = reads
            .iter()
            .filter(|e| e.acknowledged_stale.is_some())
            .count()
            + self
                .dimensions
                .mutations
                .entries
                .iter()
                .filter(|e| e.acknowledged_stale.is_some())
                .count();
        format!(
            "{} cached reads ({declared} declared, {derived} derived, {undetermined} \
             undetermined), {} repository mutations, {acknowledged} acknowledged-stale",
            reads.len(),
            self.dimensions.mutations.entries.len(),
        )
    }

    /// Serialize the manifest as pretty JSON.
    ///
    /// # Panics
    ///
    /// Never in practice: every field is a plain owned value with an infallible
    /// `Serialize` impl.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("cache-coherence manifest is always serializable")
    }
}

/// The `cached_reads` dimension: every read the binary registers, stable-ordered.
fn cached_reads_dimension(reads: &[CachedRead]) -> Dimension<ManifestRead> {
    let mut entries: Vec<ManifestRead> = reads
        .iter()
        .map(|r| {
            let mut models: Vec<String> = r.reads.clone();
            models.sort();
            models.dedup();
            ManifestRead {
                id: r.id.clone(),
                kind: r.kind.as_str().to_string(),
                reads: models,
                provenance: r.provenance.as_str().to_string(),
                acknowledged_stale: r.acknowledged_stale.clone(),
                location: r.location.clone(),
            }
        })
        .collect();
    entries.sort_by(|a, b| (&a.id, &a.location).cmp(&(&b.id, &b.location)));
    Dimension {
        provenance: "provable".to_string(),
        source: "macro:#[cached] / declare_cached_read!".to_string(),
        runtime_caveat: "each entry's EXISTENCE is proven — a `#[cached]` function registers \
                         itself — but its dependency set is only as strong as the entry's own \
                         `provenance` field says: `declared` is trusted verbatim (a `reads(...)` \
                         naming the wrong model audits clean), `derived` is what a syntactic \
                         analysis of the function could see, and `undetermined` is nothing at \
                         all. A `declare_cached_read!` entry is hand-written throughout, \
                         including the fact that the call site exists; an undeclared \
                         `cache_fragment` / `get_or_compute` call is invisible here — see the \
                         undeclared_cache_call_sites entry in `excluded`."
            .to_string(),
        entries,
    }
}

/// The `mutations` dimension: every repository write the binary registers.
fn mutations_dimension(mutations: &[Mutation]) -> Dimension<ManifestMutation> {
    let mut entries: Vec<ManifestMutation> = mutations
        .iter()
        .map(|m| ManifestMutation {
            name: m.qualified_name(),
            model: m.model.clone(),
            table: m.table.clone(),
            acknowledged_stale: m.acknowledged_stale.clone(),
            location: m.location.clone(),
        })
        .collect();
    // `location` and `table` join the key so a tie cannot fall back to
    // `inventory`'s link order, which is unspecified: two same-named repository
    // traits in different modules would otherwise swap rows between
    // byte-identical builds.
    entries.sort_by(|a, b| {
        (&a.name, &a.model, &a.table, &a.location).cmp(&(&b.name, &b.model, &b.table, &b.location))
    });
    Dimension {
        provenance: "provable".to_string(),
        source: "macro:#[repository]".to_string(),
        runtime_caveat: "every entry is proven, but the SET is not exhaustive: only \
                         `#[repository]` write methods are here. Nothing proves an app's writes \
                         all go through one — see the writes_outside_repository entry in \
                         `excluded` — and a write that reaches another model's table through a \
                         `counter_cache` is registered under its own model, not the counter's."
            .to_string(),
        entries,
    }
}

/// The `invalidations` dimension: every declared edge from a write to a read.
///
/// `provable`, not `declared`: the edge is recovered from macro-expanded code
/// with no config read and no process started, which is question 1 of the
/// provenance rubric in `docs/guide/security-posture-manifest.md`. The weak step
/// is an ADJACENT one — whether the invalidator is called — and the rubric's
/// tie-breaker is a `runtime_caveat`, not a demotion that would understate what
/// the build proves.
fn invalidations_dimension(mutations: &[Mutation]) -> Dimension<ManifestInvalidation> {
    let mut entries: Vec<ManifestInvalidation> = mutations
        .iter()
        .flat_map(|m| {
            m.invalidates.iter().map(move |read| ManifestInvalidation {
                mutation: m.qualified_name(),
                read: read.clone(),
            })
        })
        .collect();
    entries.sort_by(|a, b| (&a.mutation, &a.read).cmp(&(&b.mutation, &b.read)));
    Dimension {
        provenance: "provable".to_string(),
        source: "macro:#[repository(..., invalidates(...))]".to_string(),
        runtime_caveat: "the edge's target is proven — `invalidates(path)` resolves to the \
                         `#[cached]` function's own generated id constant, so rustc rejects a \
                         path that names anything else. That the invalidator is actually CALLED \
                         on the write path is not proven by this slice: the generated \
                         `Repository::invalidate_declared_caches()` helper must be invoked by the \
                         app (or a commit hook). Automatic invocation is deliberately out of the \
                         first slice — see docs/guide/cache-coherence.md."
            .to_string(),
        entries,
    }
}

/// Reads whose dependency set could not be established, stable-ordered and
/// carrying their locations.
fn undetermined_dimension(reads: &[CachedRead]) -> Vec<UndeterminedRead> {
    let mut entries: Vec<UndeterminedRead> = undetermined_reads(reads)
        .into_iter()
        .map(|r| UndeterminedRead {
            id: r.id.clone(),
            location: r.location.clone(),
        })
        .collect();
    entries.sort_by(|a, b| (&a.id, &a.location).cmp(&(&b.id, &b.location)));
    entries
}

/// Dimensions this slice deliberately does not emit, and why — so a reader can
/// tell "we checked and it was fine" from "we never looked".
fn excluded_dimensions() -> Vec<ExcludedDimension> {
    vec![
        ExcludedDimension {
            dimension: "row_and_column_scoped_dependencies".to_string(),
            eventual_provenance: "provable".to_string(),
            reason: "model/table granularity is what this slice proves; a read derived from one \
                     column of one row is currently treated as depending on the whole model, \
                     which over-approximates (a false failure, never a false pass)"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "invalidation_call_sites".to_string(),
            eventual_provenance: "provable".to_string(),
            reason: "whether the declared invalidator actually runs on the write path is carried \
                     as the invalidations dimension's runtime_caveat; wiring it automatically \
                     through repository commit hooks is the next slice"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "writes_outside_repository".to_string(),
            eventual_provenance: "provable".to_string(),
            reason: "only `#[repository]` write methods are in the mutated set. A hand-rolled \
                     repository, a raw diesel `update`/`insert`/`delete` against the handle, a \
                     migration, or a job that writes directly is invisible here, so a cached \
                     read those dirty audits clean. Declare such a write's effect by routing it \
                     through a `#[repository]`, or acknowledge the read"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "undeclared_cache_call_sites".to_string(),
            eventual_provenance: "provable".to_string(),
            reason: "`cache_fragment` and `get_or_compute` are plain function calls with a \
                     runtime key — there is no annotated item for a macro to find, so a call \
                     site that does not opt in with `declare_cached_read!` is not in this \
                     manifest at all. A clean audit says nothing about it"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "counter_cache_and_cascade_writes".to_string(),
            eventual_provenance: "provable".to_string(),
            reason: "a `#[belongs_to(Parent, counter_cache)]` column is declared on the MODEL, \
                     which `#[repository]` cannot see, so a child's write updating the parent's \
                     counter is registered under the child's model only. `dependent(...)` \
                     cascades ARE registered against the child model they delete or nullify"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "http_response_cache".to_string(),
            eventual_provenance: "provable".to_string(),
            reason: "`CacheResponseLayer` caches whole HTTP responses keyed by URI, with no \
                     annotated item naming what the response was derived from. It has no \
                     ReadKind and no declaration form in this slice; a response cache over \
                     mutable data is not covered by this gate"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "cross_service_coherence".to_string(),
            eventual_provenance: "runtime-only".to_string(),
            reason: "a second service's writes are not in this binary's dependency graph; \
                     single-app scope by design"
                .to_string(),
        },
        ExcludedDimension {
            dimension: "ttl_expiry".to_string(),
            eventual_provenance: "declared".to_string(),
            reason: "time-based expiry is orthogonal to derived-data coherence: a TTL bounds how \
                     long a value stays stale, it does not stop it becoming stale"
                .to_string(),
        },
    ]
}

// ── Manual registration for non-macro cache surfaces ─────────────────

/// Register a cached read the proc macros cannot see.
///
/// `#[cached]` registers itself, but the fragment cache
/// ([`cache_fragment`](super::cache_fragment)) and the read-through cache
/// ([`get_or_compute`](super::get_or_compute)) are plain function calls with a
/// runtime key — there is no annotated item for a macro to analyse. Declaring
/// the read here puts it in the same manifest and under the same gate.
///
/// The `id` must be the same key namespace the call site passes to the cache,
/// so an invalidation edge and the runtime key agree.
///
/// ```rust,ignore
/// autumn_web::declare_cached_read! {
///     id = "blog::sidebar_fragment",
///     kind = Fragment,
///     reads = [crate::models::Post, crate::models::Tag],
/// }
/// ```
///
/// An acknowledged-stale opt-out takes a trailing `acknowledge_stale = "…"`.
#[macro_export]
macro_rules! declare_cached_read {
    // At least one model is required, for the same reason `#[cached(reads())]`
    // is a compile error: an EMPTY declared set is trivially coherent and would
    // silence the gate while inflating the `declared` count — a hatch with none
    // of the visibility the real hatches carry.
    (
        id = $id:expr,
        kind = $kind:ident,
        reads = [$first:path $(, $rest:path)* $(,)?] $(,)?
    ) => {
        $crate::declare_cached_read!(
            id = $id,
            kind = $kind,
            reads = [$first $(, $rest)*],
            acknowledged_stale = ::core::option::Option::None,
        );
    };
    (
        id = $id:expr,
        kind = $kind:ident,
        reads = [$first:path $(, $rest:path)* $(,)?],
        acknowledge_stale = $reason:literal $(,)?
    ) => {
        // Same rule the attribute macros enforce: an escape hatch without a
        // justification is the one nobody can review.
        const _: () = assert!(
            !$reason.is_empty(),
            "acknowledge_stale requires a non-empty reason",
        );
        $crate::declare_cached_read!(
            id = $id,
            kind = $kind,
            reads = [$first $(, $rest)*],
            acknowledged_stale = ::core::option::Option::Some($reason),
        );
    };
    (
        id = $id:expr,
        kind = $kind:ident,
        reads = [$($model:path),+ $(,)?],
        acknowledged_stale = $ack:expr $(,)?
    ) => {
        $crate::reexports::inventory::submit! {
            $crate::cache::coherence::CachedReadDescriptor {
                id: $id,
                kind: $crate::cache::coherence::ReadKind::$kind,
                reads: &[$(|| ::core::any::type_name::<$model>()),*],
                // A hand-declared set is exactly as strong a claim as
                // `#[cached(reads(...))]`: a human wrote it down.
                provenance: $crate::cache::coherence::DependencyProvenance::Declared,
                acknowledged_stale: $ack,
                location: concat!(file!(), ":", line!()),
            }
        }
    };
}

// ── Namespace invalidation ───────────────────────────────────────────

/// Per-function cache stores, keyed by the read's cache-key namespace.
///
/// A `#[cached]` function's dedicated Moka store holds **only** that function's
/// entries, so clearing it *is* namespace invalidation — no key enumeration
/// required. The store registers itself here the first time the function runs,
/// which is what lets the generated `__autumn_cache_invalidate__<fn>()` reach a
/// store that lives inside the function body.
static NAMESPACE_STORES: std::sync::RwLock<
    Option<std::collections::HashMap<&'static str, std::sync::Arc<dyn super::Cache>>>,
> = std::sync::RwLock::new(None);

/// Register a cached read's dedicated store so [`invalidate_namespace`] can
/// reach it. Called from `#[cached]`-generated code on first use.
///
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
pub fn register_namespace_store(namespace: &'static str, store: std::sync::Arc<dyn super::Cache>) {
    NAMESPACE_STORES
        .write()
        .expect("cache namespace store lock poisoned")
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(namespace, store);
}

/// Drop every entry belonging to one cached read.
///
/// Clears the read's own dedicated in-process store **and**, when a
/// process-level shared backend is registered via
/// [`set_global_cache`](super::set_global_cache), asks that backend to drop the
/// namespace too — which is where the value actually lives once one is
/// configured.
///
/// Returns whether the invalidation was **complete**. It is complete when there
/// is no shared backend, or when the shared backend could drop the namespace
/// ([`MokaCache`](super::MokaCache) by iteration, `RedisCache` by `SCAN
/// MATCH`). A custom [`Cache`](super::Cache) implementation that cannot
/// pattern-match its key space returns `false` from
/// [`Cache::invalidate_namespace`](super::Cache::invalidate_namespace), and that
/// `false` is reported here verbatim — the honest signal that a
/// backend-specific mechanism is needed.
///
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
pub fn invalidate_namespace(namespace: &str) -> bool {
    // Clone the handle out and DROP the guard before calling into the backend.
    // `register_namespace_store` is public and `Cache` is user-implementable, so
    // a `clear()` that happens to call a `#[cached]` function would otherwise
    // re-enter this non-reentrant `RwLock` and deadlock.
    let dedicated = NAMESPACE_STORES
        .read()
        .expect("cache namespace store lock poisoned")
        .as_ref()
        .and_then(|map| map.get(namespace).cloned());
    if let Some(store) = dedicated {
        store.clear();
    }

    // The dedicated store is only half the story: once a process-level backend
    // is registered, that is where the value actually lives, and the per-
    // function store holds nothing. Ask the backend — `MokaCache` and
    // `RedisCache` both drop the namespace; a backend that cannot says so, and
    // that `false` is what the caller reports.
    super::global_cache().is_none_or(|global| global.invalidate_namespace(namespace))
}

// ── Reading the binary's own registrations ───────────────────────────

/// Every cached read registered in this binary, as owned values.
#[must_use]
pub fn registered_reads() -> Vec<CachedRead> {
    inventory::iter::<CachedReadDescriptor>()
        .map(|d| CachedRead {
            id: d.id.to_string(),
            kind: d.kind,
            reads: d.reads.iter().map(|f| f().to_string()).collect(),
            provenance: d.provenance,
            acknowledged_stale: d.acknowledged_stale.map(ToString::to_string),
            location: d.location.to_string(),
        })
        .collect()
}

/// Every repository mutation registered in this binary, as owned values.
#[must_use]
pub fn registered_mutations() -> Vec<Mutation> {
    inventory::iter::<MutationDescriptor>()
        .map(|d| Mutation {
            repository: d.repository.to_string(),
            method: d.method.to_string(),
            model: (d.model)().to_string(),
            table: d.table.to_string(),
            invalidates: d.invalidates.iter().map(|s| (*s).to_string()).collect(),
            acknowledged_stale: d.acknowledged_stale.map(ToString::to_string),
            location: d.location.to_string(),
        })
        .collect()
}

/// Prove cache coherence over everything this binary registers.
///
/// This is the library form of the `autumn cache audit` gate: an app can assert
/// on it from its own test suite (`assert!(autumn_web::cache::coherence::audit()
/// .violations.is_empty())`) and get a red `cargo test` the moment a write can
/// strand a cached read.
#[must_use]
pub fn audit() -> CoherenceManifest {
    CoherenceManifest::build(&registered_reads(), &registered_mutations())
}

/// Whether the app was started to dump its cache-coherence manifest.
#[must_use]
pub fn is_dump_mode() -> bool {
    std::env::var("AUTUMN_DUMP_CACHE_COHERENCE").as_deref() == Ok("1")
}

/// Print the manifest on the marker line `autumn cache audit` parses.
///
/// # Panics
///
/// Never in practice: every field is a plain owned value with an infallible
/// `Serialize` impl.
pub fn print_manifest_dump(manifest: &CoherenceManifest) {
    println!(
        "{COHERENCE_MANIFEST_MARKER}{}",
        serde_json::to_string(manifest).expect("cache-coherence manifest is always serializable")
    );
}

/// Extract the manifest JSON from a child process's stdout.
///
/// Returns `None` when the marker line is absent — an app built before this
/// feature, or one that failed before the dump.
#[must_use]
pub fn parse_manifest_dump(stdout: &str) -> Option<CoherenceManifest> {
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix(COHERENCE_MANIFEST_MARKER))?;
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(id: &str, models: &[&str]) -> CachedRead {
        CachedRead {
            id: id.to_string(),
            kind: ReadKind::Cached,
            reads: models.iter().map(|m| (*m).to_string()).collect(),
            provenance: DependencyProvenance::Declared,
            acknowledged_stale: None,
            location: "src/views.rs:10".to_string(),
        }
    }

    fn mutation(repo: &str, method: &str, model: &str) -> Mutation {
        Mutation {
            repository: repo.to_string(),
            method: method.to_string(),
            model: model.to_string(),
            table: "posts".to_string(),
            invalidates: Vec::new(),
            acknowledged_stale: None,
            location: "src/repositories.rs:20".to_string(),
        }
    }

    // ── The rule ─────────────────────────────────────────────────────

    #[test]
    fn intersecting_model_without_invalidation_is_a_violation() {
        let reads = vec![read("blog::views::recent_posts", &["blog::models::Post"])];
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        let findings = check(&reads, &muts);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].read, "blog::views::recent_posts");
        assert_eq!(findings[0].mutation, "PostRepository::save");
        assert_eq!(findings[0].model, "blog::models::Post");
    }

    #[test]
    fn disjoint_models_are_coherent() {
        let reads = vec![read("blog::views::recent_posts", &["blog::models::Post"])];
        let muts = vec![mutation("TagRepository", "save", "blog::models::Tag")];
        assert!(check(&reads, &muts).is_empty());
    }

    #[test]
    fn declared_invalidation_discharges_the_obligation() {
        let reads = vec![read("blog::views::recent_posts", &["blog::models::Post"])];
        let mut m = mutation("PostRepository", "save", "blog::models::Post");
        m.invalidates = vec!["blog::views::recent_posts".to_string()];
        assert!(check(&reads, &[m]).is_empty());
    }

    #[test]
    fn invalidation_of_a_different_read_does_not_discharge() {
        let reads = vec![read("blog::views::recent_posts", &["blog::models::Post"])];
        let mut m = mutation("PostRepository", "save", "blog::models::Post");
        m.invalidates = vec!["blog::views::post_count".to_string()];
        assert_eq!(check(&reads, &[m]).len(), 1);
    }

    #[test]
    fn acknowledged_stale_read_is_exempt() {
        let mut r = read("blog::views::recent_posts", &["blog::models::Post"]);
        r.acknowledged_stale = Some("ttl of 5s is short enough".to_string());
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        assert!(check(&[r], &muts).is_empty());
    }

    #[test]
    fn acknowledged_stale_mutation_is_exempt() {
        let reads = vec![read("blog::views::recent_posts", &["blog::models::Post"])];
        let mut m = mutation("PostRepository", "save", "blog::models::Post");
        m.acknowledged_stale = Some("import-only path".to_string());
        assert!(check(&reads, &[m]).is_empty());
    }

    #[test]
    fn undetermined_reads_never_fail_the_default_gate() {
        let mut r = read("blog::views::mystery", &[]);
        r.provenance = DependencyProvenance::Undetermined;
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        assert!(check(&[r], &muts).is_empty());
    }

    #[test]
    fn undetermined_reads_are_reported_separately() {
        let mut r = read("blog::views::mystery", &[]);
        r.provenance = DependencyProvenance::Undetermined;
        let ok = read("blog::views::recent_posts", &["blog::models::Post"]);
        let all = [r, ok];
        let undetermined = undetermined_reads(&all);
        assert_eq!(undetermined.len(), 1);
        assert_eq!(undetermined[0].id, "blog::views::mystery");
    }

    #[test]
    fn one_read_over_two_models_reports_one_finding_per_mutation() {
        let reads = vec![read("blog::views::feed", &["Post", "Comment"])];
        let muts = vec![
            mutation("PostRepository", "save", "blog::models::Post"),
            mutation("CommentRepository", "save", "blog::models::Comment"),
        ];
        assert_eq!(check(&reads, &muts).len(), 2);
    }

    #[test]
    fn a_model_named_twice_by_one_read_still_yields_one_finding() {
        // A read may carry both a declared and a derived spelling of the same
        // dependency; that must not double-report.
        let reads = vec![read("blog::views::feed", &["Post", "Post"])];
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        assert_eq!(check(&reads, &muts).len(), 1);
    }

    #[test]
    fn findings_are_stable_ordered() {
        let reads = vec![read("z::read", &["Post"]), read("a::read", &["Post"])];
        let muts = vec![
            mutation("PostRepository", "update", "Post"),
            mutation("PostRepository", "delete_by_id", "Post"),
        ];
        let findings = check(&reads, &muts);
        let keys: Vec<_> = findings
            .iter()
            .map(|f| format!("{}|{}", f.read, f.mutation))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            keys, sorted,
            "findings must come back deterministically ordered"
        );
    }

    // ── Model identity ───────────────────────────────────────────────

    #[test]
    fn model_key_ignores_generic_arguments_and_references() {
        assert_eq!(model_key("&blog::models::Post"), "Post");
        assert_eq!(
            model_key("blog::models::Wrapper<blog::models::Post>"),
            "Wrapper"
        );
        assert_eq!(model_key("  Post  "), "Post");
        assert_eq!(model_key("&mut blog::Post"), "Post");
        assert_eq!(model_key("*const blog::Post"), "Post");
    }

    #[test]
    fn a_derived_bare_ident_matches_a_qualified_type_name() {
        // The read names the bare ident derivation recovered; the mutation
        // names the fully-qualified `type_name`. Same model.
        let reads = vec![read("blog::views::recent_posts", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        assert_eq!(check(&reads, &muts).len(), 1);
    }

    #[test]
    fn two_qualified_same_named_models_are_kept_apart() {
        // `plugin::models::User` and `crate::models::User` are different models.
        // Collapsing them would fail a perfectly correct app.
        assert!(!models_match("plugin::models::User", "app::models::User"));
        assert!(models_match("app::models::User", "app::models::User"));

        let reads = vec![read("app::views::roster", &["app::models::User"])];
        let muts = vec![mutation("UserRepository", "save", "plugin::models::User")];
        assert!(
            check(&reads, &muts).is_empty(),
            "a different module's same-named model must not be flagged"
        );
    }

    #[test]
    fn a_bare_ident_still_over_approximates_across_modules() {
        // When one side is all the analysis could recover, matching falls back
        // to the bare name — a false failure, never a false pass.
        assert!(models_match("User", "plugin::models::User"));
    }

    // ── Diagnostics ──────────────────────────────────────────────────

    #[test]
    fn diagnostic_names_read_mutation_and_shared_model() {
        let reads = vec![read("blog::views::recent_posts", &["blog::models::Post"])];
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        let text = format_diagnostic(&check(&reads, &muts));
        assert!(text.contains("blog::views::recent_posts"), "{text}");
        assert!(text.contains("PostRepository::save"), "{text}");
        assert!(text.contains("blog::models::Post"), "{text}");
        assert!(text.contains("src/views.rs:10"), "{text}");
        assert!(text.contains("src/repositories.rs:20"), "{text}");
        assert!(text.contains("invalidates("), "must name the fix: {text}");
    }

    #[test]
    fn diagnostic_groups_one_missing_edge_into_one_block() {
        // A missing `invalidates(...)` on a repository produces one finding per
        // generated write method. Twelve identical blocks with the same one-line
        // fix is the output shape that gets a gate switched off.
        let reads = vec![read("blog::views::recent_posts", &["Post"])];
        let muts: Vec<Mutation> = [
            "save",
            "update",
            "delete_by_id",
            "save_many",
            "update_many",
            "delete_many",
        ]
        .iter()
        .map(|m| mutation("PostRepository", m, "Post"))
        .collect();

        let findings = check(&reads, &muts);
        assert_eq!(findings.len(), 6, "the manifest still carries every pair");

        let text = format_diagnostic(&findings);
        assert_eq!(
            text.matches("fix: add invalidates(").count(),
            1,
            "one edit, one fix line: {text}"
        );
        assert!(text.contains("1 cached read can be left stale"), "{text}");
        for method in ["save", "update", "delete_by_id", "delete_many"] {
            assert!(
                text.contains(&format!("PostRepository::{method}")),
                "every uncovered write is still named: {text}"
            );
        }
    }

    #[test]
    fn diagnostic_separates_two_reads_over_the_same_model() {
        let reads = vec![read("a::read", &["Post"]), read("b::read", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "Post")];
        let text = format_diagnostic(&check(&reads, &muts));
        assert_eq!(text.matches("fix: add invalidates(").count(), 2, "{text}");
        assert!(text.contains("2 cached reads can be left stale"), "{text}");
    }

    #[test]
    fn diagnostic_is_empty_when_nothing_is_wrong() {
        assert!(format_diagnostic(&[]).is_empty());
    }

    #[test]
    fn summary_counts_each_provenance_class_and_every_acknowledgement() {
        let mut undetermined = read("u", &[]);
        undetermined.provenance = DependencyProvenance::Undetermined;
        let mut derived = read("d", &["Post"]);
        derived.provenance = DependencyProvenance::Derived;
        let mut declared = read("c", &["Post"]);
        declared.acknowledged_stale = Some("deliberate".to_string());
        let mut m = mutation("PostRepository", "save", "Post");
        m.acknowledged_stale = Some("seed only".to_string());

        let text = format_summary(&[undetermined, derived, declared], &[m]);
        assert!(text.contains("3 cached reads"), "{text}");
        assert!(text.contains("1 declared"), "{text}");
        assert!(text.contains("1 derived"), "{text}");
        assert!(text.contains("1 undetermined"), "{text}");
        assert!(text.contains("1 repository mutations"), "{text}");
        assert!(text.contains("2 acknowledged-stale"), "{text}");
    }

    // ── Manifest ─────────────────────────────────────────────────────

    #[test]
    fn manifest_is_provenance_tagged_and_stable_ordered() {
        let reads = vec![read("z::read", &["Post"]), read("a::read", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "Post")];
        let manifest = CoherenceManifest::build(&reads, &muts);
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.dimensions.cached_reads.provenance, "provable");
        assert_eq!(manifest.dimensions.mutations.provenance, "provable");
        assert_eq!(manifest.dimensions.invalidations.provenance, "provable");
        assert!(
            !manifest.dimensions.invalidations.runtime_caveat.is_empty(),
            "a `declared` dimension must carry its caveat"
        );
        let ids: Vec<_> = manifest
            .dimensions
            .cached_reads
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a::read", "z::read"]);
        assert_eq!(manifest.violations.len(), 2);
    }

    #[test]
    fn manifest_carries_every_entry_through_json() {
        let mut r = read("blog::views::recent_posts", &["blog::models::Post"]);
        r.acknowledged_stale = Some("deliberate".to_string());
        let mut m = mutation("PostRepository", "save", "blog::models::Post");
        m.invalidates = vec!["blog::views::recent_posts".to_string()];

        let manifest = CoherenceManifest::build(&[r], &[m]);
        let json: serde_json::Value = serde_json::from_str(&manifest.to_json()).unwrap();

        assert_eq!(json["schema_version"], MANIFEST_SCHEMA_VERSION);
        let entry = &json["dimensions"]["cached_reads"]["entries"][0];
        assert_eq!(entry["id"], "blog::views::recent_posts");
        assert_eq!(entry["kind"], "cached");
        assert_eq!(entry["provenance"], "declared");
        assert_eq!(entry["reads"][0], "blog::models::Post");
        assert_eq!(entry["acknowledged_stale"], "deliberate");
        assert_eq!(entry["location"], "src/views.rs:10");

        let write = &json["dimensions"]["mutations"]["entries"][0];
        assert_eq!(write["name"], "PostRepository::save");
        assert_eq!(write["model"], "blog::models::Post");
        assert_eq!(write["table"], "posts");

        let edge = &json["dimensions"]["invalidations"]["entries"][0];
        assert_eq!(edge["mutation"], "PostRepository::save");
        assert_eq!(edge["read"], "blog::views::recent_posts");
    }

    #[test]
    fn manifest_omits_an_absent_runtime_caveat_rather_than_emitting_an_empty_one() {
        let json: serde_json::Value =
            serde_json::from_str(&CoherenceManifest::build(&[], &[]).to_json()).unwrap();
        // Every dimension here carries a caveat, so the `skip_serializing_if`
        // is exercised by constructing one directly rather than via `build`.
        let bare: Dimension<ManifestRead> = Dimension {
            provenance: "provable".to_string(),
            source: "test".to_string(),
            runtime_caveat: String::new(),
            entries: Vec::new(),
        };
        let bare_json: serde_json::Value = serde_json::to_value(&bare).unwrap();
        assert!(bare_json["runtime_caveat"].is_null());
        assert!(json["dimensions"]["invalidations"]["runtime_caveat"].is_string());
    }

    #[test]
    fn manifest_undetermined_entries_carry_their_location() {
        let mut r = read("blog::views::mystery", &[]);
        r.provenance = DependencyProvenance::Undetermined;
        let manifest = CoherenceManifest::build(&[r], &[]);
        assert_eq!(manifest.undetermined_reads.len(), 1);
        assert_eq!(manifest.undetermined_reads[0].id, "blog::views::mystery");
        assert_eq!(manifest.undetermined_reads[0].location, "src/views.rs:10");
    }

    #[test]
    fn excluded_dimensions_name_the_holes_this_slice_leaves() {
        // The manifest's whole discipline is "say what we did not look at". A
        // reader must be able to tell "checked and fine" from "never looked".
        let named: Vec<&str> = excluded_dimensions()
            .iter()
            .map(|d| d.dimension.as_str())
            .collect::<Vec<_>>()
            .iter()
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
            .collect();
        for expected in [
            "row_and_column_scoped_dependencies",
            "invalidation_call_sites",
            "writes_outside_repository",
            "undeclared_cache_call_sites",
            "counter_cache_and_cascade_writes",
            "http_response_cache",
            "cross_service_coherence",
            "ttl_expiry",
        ] {
            assert!(named.contains(&expected), "missing {expected} in {named:?}");
        }
        assert!(
            excluded_dimensions().iter().all(|d| !d.reason.is_empty()),
            "an exclusion without a reason is not an exclusion"
        );
    }

    #[test]
    fn round_trips_through_the_dump_protocol() {
        let reads = vec![read("blog::views::recent_posts", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "Post")];
        let manifest = CoherenceManifest::build(&reads, &muts);
        let dump = format!(
            "some unrelated startup logging\n{COHERENCE_MANIFEST_MARKER}{}\n",
            serde_json::to_string(&manifest).unwrap()
        );
        let parsed = parse_manifest_dump(&dump).expect("marker line must be found");
        assert_eq!(parsed.violations.len(), 1);
        assert_eq!(parsed.summary(), manifest.summary());
    }

    #[test]
    fn a_dump_without_the_marker_is_not_mistaken_for_an_empty_manifest() {
        assert!(parse_manifest_dump("no marker here\n").is_none());
    }

    #[test]
    fn a_corrupt_manifest_is_not_mistaken_for_an_empty_one() {
        let dump = format!("{COHERENCE_MANIFEST_MARKER}{{\"schema_version\": \n");
        assert!(parse_manifest_dump(&dump).is_none());
    }

    // ── The gate ─────────────────────────────────────────────────────

    #[test]
    fn exit_code_is_nonzero_iff_a_violation_exists() {
        let reads = vec![read("r", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "Post")];
        assert_eq!(
            audit_exit_code(&CoherenceManifest::build(&reads, &muts), false),
            1
        );
        assert_eq!(
            audit_exit_code(&CoherenceManifest::build(&[], &[]), false),
            0
        );
    }

    #[test]
    fn strict_mode_fails_on_undetermined_reads() {
        let mut r = read("blog::views::mystery", &[]);
        r.provenance = DependencyProvenance::Undetermined;
        let manifest = CoherenceManifest::build(std::slice::from_ref(&r), &[]);
        assert!(!gate_failed(&manifest, false));
        assert!(gate_failed(&manifest, true));
    }

    // ── Namespace invalidation ───────────────────────────────────────

    #[cfg(feature = "cache-moka")]
    #[test]
    fn invalidating_a_namespace_clears_only_that_reads_store() {
        // Serialized against every other test that mutates the process-wide
        // global cache: `invalidate_namespace` reads it.
        let _guard = super::super::GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        super::super::clear_global_cache();

        let a = std::sync::Arc::new(super::super::MokaCache::new(16, None));
        let b = std::sync::Arc::new(super::super::MokaCache::new(16, None));
        register_namespace_store("tests::ns_a", a.clone());
        register_namespace_store("tests::ns_b", b.clone());
        super::super::insert(&*a, "tests::ns_a:1", 1_i32);
        super::super::insert(&*b, "tests::ns_b:1", 2_i32);

        assert!(invalidate_namespace("tests::ns_a"));

        assert_eq!(super::super::get::<i32>(&*a, "tests::ns_a:1"), None);
        assert_eq!(super::super::get::<i32>(&*b, "tests::ns_b:1"), Some(2));
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn invalidating_an_unregistered_namespace_is_complete_and_harmless() {
        let _guard = super::super::GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        super::super::clear_global_cache();
        assert!(invalidate_namespace("tests::never_registered"));
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn a_shared_backend_is_asked_to_drop_the_namespace_too() {
        // Once a process-level backend is registered THAT is where the value
        // lives; clearing only the per-function store would be a no-op dressed
        // up as an invalidation.
        let _guard = super::super::GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let global = std::sync::Arc::new(super::super::MokaCache::new(64, None));
        super::super::set_global_cache(global.clone());
        super::super::insert(&*global, "tests::shared_ns:aaa", 1_i32);
        super::super::insert(&*global, "tests::shared_ns:bbb", 2_i32);
        super::super::insert(&*global, "tests::other_ns:aaa", 3_i32);

        assert!(
            invalidate_namespace("tests::shared_ns"),
            "a backend that can drop a namespace reports a complete invalidation"
        );
        assert_eq!(
            super::super::get::<i32>(&*global, "tests::shared_ns:aaa"),
            None
        );
        assert_eq!(
            super::super::get::<i32>(&*global, "tests::shared_ns:bbb"),
            None
        );
        assert_eq!(
            super::super::get::<i32>(&*global, "tests::other_ns:aaa"),
            Some(3),
            "a neighbouring namespace must survive"
        );

        super::super::clear_global_cache();
    }

    #[test]
    fn a_backend_that_cannot_enumerate_reports_an_incomplete_invalidation() {
        // The honest signal: a custom backend with no way to pattern-match its
        // key space must not let a caller believe the value is gone.
        struct OpaqueBackend;
        impl super::super::Cache for OpaqueBackend {
            fn get_value(
                &self,
                _key: &str,
            ) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
                None
            }
            fn insert_value(
                &self,
                _key: &str,
                _value: std::sync::Arc<dyn std::any::Any + Send + Sync>,
            ) {
            }
            fn invalidate(&self, _key: &str) {}
            fn clear(&self) {}
        }

        let _guard = super::super::GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        super::super::set_global_cache(std::sync::Arc::new(OpaqueBackend));
        assert!(!invalidate_namespace("tests::opaque_ns"));
        super::super::clear_global_cache();
    }

    // ── declare_cached_read! ─────────────────────────────────────────

    struct DeclaredModelA;
    struct DeclaredModelB;

    crate::declare_cached_read! {
        id = "autumn_web::cache::coherence::tests::declared_fragment",
        kind = Fragment,
        reads = [DeclaredModelA, DeclaredModelB],
    }

    crate::declare_cached_read! {
        id = "autumn_web::cache::coherence::tests::declared_read_through",
        kind = ReadThrough,
        reads = [DeclaredModelA],
        acknowledge_stale = "the sidebar tolerates a stale count",
    }

    #[test]
    fn a_manually_declared_read_reaches_the_manifest() {
        let declared = registered_reads()
            .into_iter()
            .find(|r| r.id.ends_with("tests::declared_fragment"))
            .expect("declare_cached_read! must register the read");
        assert_eq!(declared.kind, ReadKind::Fragment);
        assert_eq!(declared.provenance, DependencyProvenance::Declared);
        assert_eq!(declared.reads.len(), 2);
        assert!(
            declared
                .reads
                .iter()
                .any(|m| model_key(m) == "DeclaredModelA"),
            "{:?}",
            declared.reads
        );
        assert!(declared.location.contains("coherence.rs"));
    }

    #[test]
    fn a_manually_declared_read_can_acknowledge_staleness() {
        let declared = registered_reads()
            .into_iter()
            .find(|r| r.id.ends_with("tests::declared_read_through"))
            .expect("declare_cached_read! must register the read");
        assert_eq!(declared.kind, ReadKind::ReadThrough);
        assert_eq!(
            declared.acknowledged_stale.as_deref(),
            Some("the sidebar tolerates a stale count")
        );
    }

    #[test]
    fn read_kind_spellings_are_stable() {
        // These strings are the manifest's schema; renaming one silently would
        // break every consumer.
        assert_eq!(ReadKind::Cached.as_str(), "cached");
        assert_eq!(ReadKind::Fragment.as_str(), "fragment");
        assert_eq!(ReadKind::ReadThrough.as_str(), "read_through");
        assert_eq!(DependencyProvenance::Declared.as_str(), "declared");
        assert_eq!(DependencyProvenance::Derived.as_str(), "derived");
        assert_eq!(DependencyProvenance::Undetermined.as_str(), "undetermined");
    }
}
