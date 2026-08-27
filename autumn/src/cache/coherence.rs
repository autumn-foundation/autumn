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

use std::collections::BTreeSet;
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

/// Reduce a model name to the identity two independently-derived spellings can
/// agree on: the last path segment, with references and generic arguments
/// stripped.
///
/// `&blog::models::Post` and `Post` are the same model; `Wrapper<Post>` is a
/// `Wrapper`. See the module docs for why matching is deliberately this coarse.
#[must_use]
pub fn model_key(name: &str) -> &str {
    let name = name.trim();
    // Strip references/pointers, then take everything before the first generic
    // argument list so `Wrapper<Post>` keys on `Wrapper`, not on `Post`.
    let name = name.trim_start_matches(['&', '*']).trim();
    let name = name.strip_prefix("mut ").unwrap_or(name).trim();
    let head = name.split_once('<').map_or(name, |(head, _)| head);
    head.rsplit("::").next().unwrap_or(head).trim()
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
        // A read may legitimately name the same model twice (declared *and*
        // derived spellings of one dependency); dedupe so one model can only
        // produce one finding per mutation.
        let dependency_keys: BTreeSet<&str> =
            read.reads.iter().map(|m| model_key(m)).collect();

        for mutation in mutations {
            if mutation.acknowledged_stale.is_some() {
                continue;
            }
            if !dependency_keys.contains(model_key(&mutation.model)) {
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

    findings.sort_by(|a, b| {
        (&a.read, &a.mutation, &a.model).cmp(&(&b.read, &b.mutation, &b.model))
    });
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

/// Exit code for the gate: non-zero when a violation was proven, or — under
/// `strict` — when any read's dependency set is undetermined.
#[must_use]
pub fn audit_exit_code(
    findings: &[StalenessFinding],
    undetermined: &[&CachedRead],
    strict: bool,
) -> i32 {
    i32::from(!findings.is_empty() || (strict && !undetermined.is_empty()))
}

// ── Diagnostics ──────────────────────────────────────────────────────

/// Render the precise diagnostic for a set of findings: for each, the cached
/// read, the mutation, the model they share, and the two ways to discharge it.
#[must_use]
pub fn format_diagnostic(findings: &[StalenessFinding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "error: {} cached read{} can be left stale by a repository write\n\n",
        findings.len(),
        if findings.len() == 1 { "" } else { "s" }
    );
    for f in findings {
        let _ = writeln!(
            out,
            "  {mutation} mutates {model}\n    \
             but the cached read {read} is derived from it and is never invalidated.\n      \
             read     {read_loc}\n      \
             mutation {mut_loc}\n    \
             fix: add #[invalidates({read})] to the write, or\n         \
             acknowledge the staleness with #[acknowledge_stale(reason = \"…\")].\n",
            mutation = f.mutation,
            model = f.model,
            read = f.read,
            read_loc = f.read_location,
            mut_loc = f.mutation_location,
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
    pub undetermined_reads: Vec<String>,
    /// Dimensions deliberately not emitted, and why.
    pub excluded: Vec<ExcludedDimension>,
}

impl CoherenceManifest {
    /// Assemble the manifest from the app's registered reads and mutations.
    #[must_use]
    pub fn build(reads: &[CachedRead], mutations: &[Mutation]) -> Self {
        let mut read_entries: Vec<ManifestRead> = reads
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
        read_entries.sort_by(|a, b| a.id.cmp(&b.id));

        let mut mutation_entries: Vec<ManifestMutation> = mutations
            .iter()
            .map(|m| ManifestMutation {
                name: m.qualified_name(),
                model: m.model.clone(),
                table: m.table.clone(),
                acknowledged_stale: m.acknowledged_stale.clone(),
                location: m.location.clone(),
            })
            .collect();
        mutation_entries.sort_by(|a, b| (&a.name, &a.model).cmp(&(&b.name, &b.model)));

        let mut invalidation_entries: Vec<ManifestInvalidation> = mutations
            .iter()
            .flat_map(|m| {
                m.invalidates.iter().map(move |read| ManifestInvalidation {
                    mutation: m.qualified_name(),
                    read: read.clone(),
                })
            })
            .collect();
        invalidation_entries.sort_by(|a, b| (&a.mutation, &a.read).cmp(&(&b.mutation, &b.read)));

        let mut undetermined: Vec<String> = undetermined_reads(reads)
            .into_iter()
            .map(|r| r.id.clone())
            .collect();
        undetermined.sort();

        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            dimensions: Dimensions {
                cached_reads: Dimension {
                    provenance: "provable".to_string(),
                    source: "macro:#[cached] / declare_cached_read!".to_string(),
                    runtime_caveat: String::new(),
                    entries: read_entries,
                },
                mutations: Dimension {
                    provenance: "provable".to_string(),
                    source: "macro:#[repository]".to_string(),
                    runtime_caveat: String::new(),
                    entries: mutation_entries,
                },
                invalidations: Dimension {
                    provenance: "declared".to_string(),
                    source: "macro:#[repository(..., invalidates(...))]".to_string(),
                    runtime_caveat:
                        "the edge's target is proven — `invalidates(path)` resolves to the \
                         `#[cached]` function's own generated id constant, so rustc rejects a \
                         path that names anything else. That the invalidator is actually CALLED \
                         on the write path is not proven by this slice: the generated \
                         `Repository::invalidate_declared_caches()` helper must be invoked by the \
                         app (or a commit hook). Automatic invocation is deliberately out of the \
                         first slice — see docs/guide/cache-coherence.md."
                            .to_string(),
                    entries: invalidation_entries,
                },
            },
            violations: check(reads, mutations),
            undetermined_reads: undetermined,
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
    (
        id = $id:expr,
        kind = $kind:ident,
        reads = [$($model:ty),* $(,)?] $(,)?
    ) => {
        $crate::declare_cached_read!(
            id = $id,
            kind = $kind,
            reads = [$($model),*],
            acknowledged_stale = ::core::option::Option::None,
        );
    };
    (
        id = $id:expr,
        kind = $kind:ident,
        reads = [$($model:ty),* $(,)?],
        acknowledge_stale = $reason:expr $(,)?
    ) => {
        $crate::declare_cached_read!(
            id = $id,
            kind = $kind,
            reads = [$($model),*],
            acknowledged_stale = ::core::option::Option::Some($reason),
        );
    };
    (
        id = $id:expr,
        kind = $kind:ident,
        reads = [$($model:ty),* $(,)?],
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
/// Returns whether the invalidation was **complete**. It is complete when the
/// read is served only from its own dedicated in-process store (which was just
/// cleared, or was never created because the function has not run yet). It is
/// *incomplete* — `false` — when a process-level shared backend is registered
/// via [`set_global_cache`](super::set_global_cache): that backend keys every
/// cached function into one store and exposes no way to enumerate a namespace,
/// so entries there survive. Callers that need cross-replica invalidation must
/// use a backend-specific mechanism; this is the honest signal that they do.
///
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
pub fn invalidate_namespace(namespace: &str) -> bool {
    if let Some(map) = NAMESPACE_STORES
        .read()
        .expect("cache namespace store lock poisoned")
        .as_ref()
        && let Some(store) = map.get(namespace)
    {
        store.clear();
    }
    super::global_cache().is_none()
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
    fn model_identity_matches_on_the_last_path_segment() {
        // The read names the bare ident recovered by derivation; the mutation
        // names the fully-qualified `type_name`. They are the same model.
        let reads = vec![read("blog::views::recent_posts", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "blog::models::Post")];
        assert_eq!(check(&reads, &muts).len(), 1);
    }

    #[test]
    fn model_identity_ignores_generic_arguments_and_references() {
        assert_eq!(model_key("&blog::models::Post"), "Post");
        assert_eq!(model_key("blog::models::Wrapper<blog::models::Post>"), "Wrapper");
        assert_eq!(model_key("  Post  "), "Post");
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
    fn findings_are_stable_ordered() {
        let reads = vec![
            read("z::read", &["Post"]),
            read("a::read", &["Post"]),
        ];
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
        assert_eq!(keys, sorted, "findings must come back deterministically ordered");
    }

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
        assert!(text.contains("#[invalidates("), "must name the fix: {text}");
    }

    #[test]
    fn manifest_is_provenance_tagged_and_stable_ordered() {
        let reads = vec![
            read("z::read", &["Post"]),
            read("a::read", &["Post"]),
        ];
        let muts = vec![mutation("PostRepository", "save", "Post")];
        let manifest = CoherenceManifest::build(&reads, &muts);
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.dimensions.cached_reads.provenance, "provable");
        assert_eq!(manifest.dimensions.mutations.provenance, "provable");
        assert_eq!(manifest.dimensions.invalidations.provenance, "declared");
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
    fn manifest_serializes_to_stable_json() {
        let manifest = CoherenceManifest::build(&[], &[]);
        let json = manifest.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], MANIFEST_SCHEMA_VERSION);
        assert!(parsed["dimensions"]["cached_reads"].is_object());
        assert!(parsed["excluded"].is_array());
    }

    #[test]
    fn summary_counts_each_provenance_class() {
        let mut undetermined = read("u", &[]);
        undetermined.provenance = DependencyProvenance::Undetermined;
        let mut derived = read("d", &["Post"]);
        derived.provenance = DependencyProvenance::Derived;
        let declared = read("c", &["Post"]);
        let text = format_summary(&[undetermined, derived, declared], &[]);
        assert!(text.contains("3 cached reads"), "{text}");
        assert!(text.contains("1 declared"), "{text}");
        assert!(text.contains("1 derived"), "{text}");
        assert!(text.contains("1 undetermined"), "{text}");
    }

    #[test]
    fn exit_code_is_nonzero_iff_a_violation_exists() {
        let reads = vec![read("r", &["Post"])];
        let muts = vec![mutation("PostRepository", "save", "Post")];
        assert_eq!(audit_exit_code(&check(&reads, &muts), &[], false), 1);
        assert_eq!(audit_exit_code(&[], &[], false), 0);
    }


    #[test]
    fn invalidating_a_namespace_clears_only_that_reads_store() {
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

    #[test]
    fn invalidating_an_unregistered_namespace_is_complete_and_harmless() {
        assert!(invalidate_namespace("tests::never_registered"));
    }

    #[test]
    fn strict_mode_fails_on_undetermined_reads() {
        let mut r = read("blog::views::mystery", &[]);
        r.provenance = DependencyProvenance::Undetermined;
        let undetermined = undetermined_reads(std::slice::from_ref(&r));
        assert_eq!(audit_exit_code(&[], &undetermined, false), 0);
        assert_eq!(audit_exit_code(&[], &undetermined, true), 1);
    }
}
