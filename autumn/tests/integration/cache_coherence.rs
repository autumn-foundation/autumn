//! Build-time cache-coherence proof (issue #1716) — end-to-end.
//!
//! Two halves:
//!
//! * **Wiring** — real `#[cached]` and `#[repository]` invocations, read back
//!   through `inventory` exactly as `autumn cache audit` reads them, proving
//!   the macros publish what the checker needs.
//! * **The success metric** — a seeded corpus of intentional staleness bugs
//!   (a cached read plus a mutation with a missing invalidation) which the
//!   checker must flag 100% of, and a correctly-invalidated control app it
//!   must not fail at all.

use autumn_web::cache::coherence::{
    self, CachedRead, DependencyProvenance, Mutation, ReadKind, model_key,
};

mod schema {
    autumn_web::reexports::diesel::table! {
        coherence_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        coherence_tags (id) {
            id -> Int8,
            label -> Text,
        }
    }
}

use schema::{coherence_posts, coherence_tags};

#[autumn_web::model]
pub struct CoherencePost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::model]
pub struct CoherenceTag {
    #[id]
    pub id: i64,
    pub label: String,
}

// ── The app under test ───────────────────────────────────────────────

/// A cached read with a DECLARED dependency set, covered by an invalidation.
#[autumn_web::cached(reads(CoherencePost))]
pub async fn coherence_recent_titles() -> Vec<String> {
    Vec::new()
}

/// The seeded bug: derived from the same model, invalidated by nothing.
#[autumn_web::cached(reads(CoherencePost))]
pub async fn coherence_post_count() -> i64 {
    0
}

/// A read over a model nothing in this module writes — must stay clean.
#[autumn_web::cached(reads(CoherenceTag))]
pub async fn coherence_tag_labels() -> Vec<String> {
    Vec::new()
}

/// An opted-out read: the gate must leave it alone.
#[autumn_web::cached(
    reads(CoherencePost),
    acknowledge_stale = "5s TTL is tight enough here"
)]
pub async fn coherence_ticker() -> i64 {
    0
}

/// A read nothing can be recovered from — reported, never gated.
#[autumn_web::cached]
pub async fn coherence_opaque(seed: i64) -> i64 {
    seed
}

#[autumn_web::repository(CoherencePost, invalidates(coherence_recent_titles))]
pub trait CoherencePostRepository {}

// ── The escape-hatch name (#2357) ────────────────────────────────────
//
// A repository whose trait name says nothing about its model. Deriving the
// model from the NAME would register `CoherenceModeration`, which does not
// exist, and a `CoherenceTag` write would then intersect nothing — the audit
// would report clean on a read that really can go stale. The repository states
// its own model instead.

#[autumn_web::repository(CoherenceTag)]
pub trait CoherenceModerationRepository {}

/// No `reads(...)`: the dependency set is DERIVED from the repository
/// parameter, and must come out as `CoherenceTag`.
#[autumn_web::cached(key(tenant))]
pub async fn coherence_moderated_labels(
    tenant: i64,
    repo: &PgCoherenceModerationRepository,
) -> Vec<String> {
    let _ = (tenant, repo);
    Vec::new()
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Only this module's registrations: the test binary links every other
/// integration test's `#[cached]` functions too.
fn this_modules_reads() -> Vec<CachedRead> {
    coherence::registered_reads()
        .into_iter()
        .filter(|r| r.id.contains("cache_coherence::coherence_"))
        .collect()
}

fn this_modules_mutations() -> Vec<Mutation> {
    coherence::registered_mutations()
        .into_iter()
        .filter(|m| m.repository == "CoherencePostRepository")
        .collect()
}

fn read_named(id: &str) -> CachedRead {
    this_modules_reads()
        .into_iter()
        .find(|r| r.id.ends_with(id))
        .unwrap_or_else(|| panic!("{id} was never registered"))
}

// ── Wiring ───────────────────────────────────────────────────────────

/// The escape-hatch case (#2357): a repository whose NAME differs from its
/// model must still register the model it actually reads.
///
/// The old derivation string-stripped the type name, so this read registered
/// `CoherenceModeration` — a model nothing writes and nothing can match. The
/// gate then had nothing to intersect and reported clean while a `CoherenceTag`
/// write could strand the value. The read side now resolves the model through
/// the repository's own `__AUTUMN_MODEL_NAME`, exactly as the mutation side
/// does, so rustc decides it.
#[test]
fn a_derived_dependency_comes_from_the_repositorys_model_not_its_name() {
    let read = read_named("coherence_moderated_labels");
    assert_eq!(read.provenance, DependencyProvenance::Derived);

    let models: Vec<&str> = read.reads.iter().map(|m| model_key(m)).collect();
    assert_eq!(
        models,
        vec!["CoherenceTag"],
        "the model must come from the repository, not from its name"
    );
    assert!(
        !models.iter().any(|m| m.contains("Moderation")),
        "a name-derived model would be a dependency that does not exist: {models:?}"
    );
}

/// And the gate must actually fire on it — a dependency set that is right but
/// unreachable by `check` would be no better than the wrong one.
#[test]
fn the_gate_catches_a_write_to_an_escape_hatch_named_repositorys_model() {
    let read = read_named("coherence_moderated_labels");
    let mutations: Vec<Mutation> = coherence::registered_mutations()
        .into_iter()
        .filter(|m| m.repository == "CoherenceModerationRepository")
        .collect();
    assert!(
        !mutations.is_empty(),
        "the repository must register its writes"
    );

    let findings = coherence::check(std::slice::from_ref(&read), &mutations);
    assert!(
        !findings.is_empty(),
        "a CoherenceTag write with no invalidation must be caught: {findings:?}"
    );
    // `model` is the fully-qualified `type_name`; `model_key` is what the
    // checker itself compares on.
    assert!(
        findings
            .iter()
            .all(|f| model_key(&f.model) == "CoherenceTag"),
        "every finding must be about the model the repository declares: {findings:?}"
    );
}

#[test]
fn cached_reads_register_their_declared_dependency_set() {
    let read = read_named("coherence_recent_titles");
    assert_eq!(read.kind, ReadKind::Cached);
    assert_eq!(read.provenance, DependencyProvenance::Declared);
    assert_eq!(
        read.reads.iter().map(|m| model_key(m)).collect::<Vec<_>>(),
        vec!["CoherencePost"]
    );
    assert!(
        read.location.contains("cache_coherence.rs"),
        "the descriptor must carry a usable source location: {}",
        read.location
    );
}

#[test]
fn a_cached_read_id_is_its_cache_key_namespace() {
    let read = read_named("coherence_recent_titles");
    let key = autumn_web::cache::make_cache_key(&read.id, &());
    assert!(
        key.starts_with(&format!("{}:", read.id)),
        "runtime key {key} must live under the registered namespace {}",
        read.id
    );
}

#[test]
fn an_underivable_read_is_undetermined_not_silently_coherent() {
    let read = read_named("coherence_opaque");
    assert_eq!(read.provenance, DependencyProvenance::Undetermined);
    assert!(read.reads.is_empty());
}

#[test]
fn an_acknowledged_read_carries_its_reason() {
    let read = read_named("coherence_ticker");
    assert_eq!(
        read.acknowledged_stale.as_deref(),
        Some("5s TTL is tight enough here")
    );
}

#[test]
fn every_repository_write_registers_a_mutation() {
    let mutations = this_modules_mutations();
    let methods: Vec<&str> = mutations.iter().map(|m| m.method.as_str()).collect();
    for expected in [
        "save",
        "update",
        "delete_by_id",
        "update_many",
        "delete_many",
    ] {
        assert!(
            methods.contains(&expected),
            "missing {expected} in {methods:?}"
        );
    }
    assert!(
        !methods.contains(&"find_all"),
        "a read must never be registered as a mutation: {methods:?}"
    );
    for m in &mutations {
        assert_eq!(model_key(&m.model), "CoherencePost");
        assert_eq!(m.table, "coherence_posts");
    }
}

#[test]
fn a_declared_invalidation_edge_reaches_every_write() {
    let recent = read_named("coherence_recent_titles").id;
    for m in this_modules_mutations() {
        assert!(
            m.invalidates.contains(&recent),
            "{}::{} dropped the trait-level invalidation edge",
            m.repository,
            m.method
        );
    }
}

#[test]
fn the_gate_catches_the_seeded_bug_and_only_the_seeded_bug() {
    let reads = this_modules_reads();
    let mutations = this_modules_mutations();
    let findings = coherence::check(&reads, &mutations);

    assert!(
        !findings.is_empty(),
        "the seeded staleness bug must be caught"
    );
    let stale: std::collections::BTreeSet<&str> =
        findings.iter().map(|f| f.read.as_str()).collect();
    assert_eq!(
        stale.len(),
        1,
        "exactly one read is uncovered, got {stale:?}"
    );
    assert!(
        stale
            .iter()
            .next()
            .unwrap()
            .ends_with("coherence_post_count"),
        "got {stale:?}"
    );

    // Every finding names the shared model, so the diagnostic can explain why.
    for f in &findings {
        assert_eq!(model_key(&f.model), "CoherencePost");
    }
}

#[test]
fn the_manifest_is_emitted_as_a_build_artifact() {
    let manifest =
        coherence::CoherenceManifest::build(&this_modules_reads(), &this_modules_mutations());
    let json: serde_json::Value = serde_json::from_str(&manifest.to_json()).unwrap();

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["dimensions"]["cached_reads"]["provenance"], "provable");
    assert_eq!(json["dimensions"]["mutations"]["provenance"], "provable");
    // `provable`, not `declared`: the edge is recovered from macro-expanded
    // code, and the adjacent step it cannot prove is carried as a caveat.
    assert_eq!(
        json["dimensions"]["invalidations"]["provenance"],
        "provable"
    );
    assert!(
        !json["dimensions"]["invalidations"]["runtime_caveat"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(!json["violations"].as_array().unwrap().is_empty());
    // Each undetermined read carries its location, so the reader never has to
    // grep for the one thing the manifest could not establish.
    let opaque = json["undetermined_reads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"].as_str().unwrap().ends_with("coherence_opaque"))
        .expect("the underivable read must be reported");
    assert!(
        opaque["location"]
            .as_str()
            .unwrap()
            .contains("cache_coherence.rs"),
        "{opaque}"
    );
}

#[test]
fn the_generated_invalidator_actually_drops_the_cached_value() {
    // A runtime check that the declared edge is not merely paperwork. Asserting
    // only the returned bool would pass with an empty function body, so this
    // watches the value itself: the read is memoized, the source changes, and
    // the next call must see the change only because the invalidator ran.
    static SOURCE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

    // Acknowledged deliberately: this fixture exists to be invalidated by hand,
    // so it is not one of the app's obligations. Without the opt-out it would
    // register as a second uncovered read and make
    // `the_gate_catches_the_seeded_bug_and_only_the_seeded_bug` a lie — which is
    // itself a small proof that the gate sees every `#[cached]` in the binary,
    // including one declared inside a test body.
    #[autumn_web::cached(
        reads(CoherencePost),
        acknowledge_stale = "a fixture for the invalidator, invalidated by hand below"
    )]
    async fn coherence_memoized_source() -> i64 {
        SOURCE.load(std::sync::atomic::Ordering::SeqCst)
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        assert_eq!(coherence_memoized_source().await, 1);
        SOURCE.store(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            coherence_memoized_source().await,
            1,
            "the value must really be memoized, or this test proves nothing"
        );

        assert!(
            __autumn_cache_invalidate__coherence_memoized_source(),
            "with no backend that refuses namespace invalidation, it must be complete"
        );

        assert_eq!(
            coherence_memoized_source().await,
            2,
            "the invalidator must have dropped the memoized value"
        );
    });

    // And the repository-wide helper the macro generates reaches its declared
    // edge the same way.
    assert!(PgCoherencePostRepository::invalidate_declared_caches());
}

// ── The success metric ───────────────────────────────────────────────

fn seeded_read(id: &str, model: &str) -> CachedRead {
    CachedRead {
        id: id.to_string(),
        kind: ReadKind::Cached,
        reads: vec![model.to_string()],
        provenance: DependencyProvenance::Declared,
        acknowledged_stale: None,
        location: format!("src/{id}.rs:1"),
    }
}

fn seeded_mutation(model: &str, method: &str) -> Mutation {
    Mutation {
        repository: format!("{model}Repository"),
        method: method.to_string(),
        model: format!("app::models::{model}"),
        table: format!("{}s", model.to_lowercase()),
        invalidates: Vec::new(),
        acknowledged_stale: None,
        location: format!("src/{model}_repo.rs:1"),
    }
}

/// A corpus of intentional staleness bugs, one per (model, write) shape.
fn seeded_corpus() -> (Vec<CachedRead>, Vec<Mutation>) {
    let models = ["Post", "Comment", "Tag", "Invoice", "LineItem", "Session"];
    let writes = ["save", "update", "delete_by_id", "update_many"];
    let reads = models
        .iter()
        .map(|m| seeded_read(&format!("app::views::{}_index", m.to_lowercase()), m))
        .collect();
    let mutations = models
        .iter()
        .flat_map(|m| writes.iter().map(move |w| seeded_mutation(m, w)))
        .collect();
    (reads, mutations)
}

#[test]
fn every_seeded_staleness_bug_is_flagged_before_runtime() {
    let (reads, mutations) = seeded_corpus();
    let findings = coherence::check(&reads, &mutations);

    // Every (read, mutation) pair over a shared model is a distinct bug.
    assert_eq!(
        findings.len(),
        reads.len() * 4,
        "expected one finding per seeded (read, write) pair"
    );
    let flagged: std::collections::BTreeSet<&str> =
        findings.iter().map(|f| f.read.as_str()).collect();
    assert_eq!(
        flagged.len(),
        reads.len(),
        "100% of seeded bugs must be flagged, got {flagged:?}"
    );
}

#[test]
fn the_correctly_invalidated_control_app_has_zero_false_failures() {
    let (reads, mut mutations) = seeded_corpus();
    // Discharge every obligation the way a developer would.
    for mutation in &mut mutations {
        let model = model_key(&mutation.model).to_lowercase();
        mutation
            .invalidates
            .push(format!("app::views::{model}_index"));
    }
    assert!(
        coherence::check(&reads, &mutations).is_empty(),
        "a correctly-invalidated app must produce zero findings"
    );
}

#[test]
fn an_acknowledged_stale_control_app_also_has_zero_false_failures() {
    let (mut reads, mutations) = seeded_corpus();
    for read in &mut reads {
        read.acknowledged_stale = Some("deliberately eventually-consistent".to_string());
    }
    assert!(coherence::check(&reads, &mutations).is_empty());
}

#[test]
fn a_disjoint_app_is_never_flagged() {
    let (reads, _) = seeded_corpus();
    let mutations: Vec<Mutation> = ["Audit", "Webhook", "Job"]
        .iter()
        .map(|m| seeded_mutation(m, "save"))
        .collect();
    assert!(coherence::check(&reads, &mutations).is_empty());
}
