//! Benchmark: `MemorySearchBackend::vector_search` ranking cost over a
//! realistic embedded corpus.
//!
//! Drives the public `SearchBackend::vector_search` entry point — the same
//! call `SearchClient::similar_to`/`similar_to_vector` makes — against a
//! 5,000-document index, each carrying a 384-dimension embedding (the width
//! of a typical local sentence-embedding model such as `all-MiniLM-L6-v2`),
//! with k-NN queries asking for a realistic "find similar" page (top 10).
//!
//! `MemorySearchBackend` is a reference/dev backend (no DB, no Docker), which
//! is what makes it profilable in isolation: the production Postgres backend
//! pushes nearest-neighbour ranking into `pgvector`, so this is the only
//! backend where the ranking algorithm itself — `sort_hits` +
//! [`autumn_search::embedding::cosine_similarity`] in `src/memory.rs` — is
//! Rust code this crate controls.
//!
//! Like the other benches in this workspace this is `harness = false` and
//! asserts nothing beyond a sanity check that hits come back: it is a
//! workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-search --bench vector_search
//! BIN=$(find target/release/deps -maxdepth 1 -name "vector_search-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 2000
//! callgrind_annotate --threshold=90 callgrind.out | head -60
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Take TWO runs and subtract: `--iterations 0` measures corpus construction
//! # and indexing plus warm-up, so subtracting it leaves the MARGINAL
//! # per-query cost rather than one amortised over the run length.
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 2000
//! ```

use std::hint::black_box;

use autumn_search::{
    IndexDefinition, IndexedDocument, MemorySearchBackend, SearchBackend, SearchDocument,
    SearchIndexField, VectorQuery,
};

const DOC_COUNT: i64 = 5_000;
const EMBED_DIM: usize = 384;
const QUERY_COUNT: usize = 50;
const TOP_K: usize = 10;
const INDEX_NAME: &str = "bench_articles_vec";

const FIELDS: &[SearchIndexField] = &[SearchIndexField::new("body", 'A')];

/// xorshift64* — enough spread for a reproducible synthetic corpus, no
/// external `rand` dependency. Same generator `keyword_search.rs` uses.
struct Rng(u64);

impl Rng {
    const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A pseudo-random component in `[-1.0, 1.0]`.
    ///
    /// Both widening casts below are exact, not lossy: `bits >> 40` and
    /// `1u32 << 24` are masked down to 24 significant bits, which `f32`'s
    /// 23-bit mantissa plus implicit leading bit represents exactly.
    #[allow(clippy::cast_precision_loss)]
    fn component(&mut self) -> f32 {
        let bits = self.next_u64();
        // Top 24 bits give a clean, evenly spread mantissa.
        let top24 = (bits >> 40) as u32;
        let unit = top24 as f32 / (1u32 << 24) as f32;
        unit * 2.0 - 1.0
    }
}

/// A dense, non-degenerate (never all-zero) unit vector — the shape a real
/// embedder returns, not sparse or axis-aligned data that would let cosine
/// similarity short-circuit.
fn random_unit_vector(rng: &mut Rng, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim).map(|_| rng.component()).collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm = if norm > 0.0 { norm } else { 1.0 };
    for x in &mut v {
        *x /= norm;
    }
    v
}

const fn definition() -> IndexDefinition {
    IndexDefinition::new(INDEX_NAME, "english", FIELDS, Some("body"), false)
}

/// `DOC_COUNT` documents, each carrying a `body` field (so the index is a
/// realistic mixed keyword+vector index, the shape `#[searchable(embed)]`
/// produces) and a random unit embedding.
fn build_corpus() -> Vec<IndexedDocument> {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    (0..DOC_COUNT)
        .map(|id| {
            let embedding = random_unit_vector(&mut rng, EMBED_DIM);
            IndexedDocument::new(SearchDocument::new(INDEX_NAME, id).with_field(
                "body",
                'A',
                "indexed document",
            ))
            .with_embedding(embedding)
        })
        .collect()
}

/// `QUERY_COUNT` random unit query vectors — no closer to the corpus'
/// distribution than a real caller-supplied query embedding would be.
fn build_queries() -> Vec<Vec<f32>> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    (0..QUERY_COUNT)
        .map(|_| random_unit_vector(&mut rng, EMBED_DIM))
        .collect()
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let backend = MemorySearchBackend::new();
    let definition = definition();
    let corpus = build_corpus();
    let queries = build_queries();

    rt.block_on(async {
        backend.ensure_index(&definition).await.expect("ensure_index");
        // Indexed in batches, like a real backfill, rather than one call for
        // the whole corpus — irrelevant to the measured cost (indexing runs
        // once, outside every iteration count below) but keeps the setup
        // representative of `SearchClient::backfill`'s batching.
        for chunk in corpus.chunks(500) {
            backend.index(&definition, chunk).await.expect("index");
        }

        let mut total_hits: usize = 0;

        for i in 0..20 {
            let query = queries[i % queries.len()].clone();
            let hits = backend
                .vector_search(&definition, &VectorQuery::new(query, TOP_K))
                .await
                .expect("vector_search");
            black_box(&hits);
        }

        for i in 0..iterations {
            let query = queries[i as usize % queries.len()].clone();
            let hits = backend
                .vector_search(&definition, &VectorQuery::new(query, TOP_K))
                .await
                .expect("vector_search");
            total_hits += hits.len();
            black_box(&hits);
        }

        println!(
            "completed {iterations} vector searches over {DOC_COUNT} documents ({total_hits} total hits)"
        );
    });
}
