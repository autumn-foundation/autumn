//! Benchmark: `MemorySearchBackend::keyword_search` ranking cost over a
//! realistic multi-field corpus.
//!
//! Drives the public `SearchBackend::keyword_search` entry point — the same
//! call `SearchClient::search` makes — against a 5,000-document, two-field
//! (`title` weight `A`, `body` weight `B`) index, the shape `#[searchable]`
//! produces for a typical article/post model. Body fields are ~200 words,
//! titles ~6 words, drawn from a fixed vocabulary with a deterministic PRNG so
//! the corpus and query mix are reproducible without a `rand` dependency.
//!
//! `MemorySearchBackend` is a reference/dev backend (no DB, no Docker), which
//! is what makes it profilable in isolation: the production Postgres backend
//! pushes ranking into `tsvector`/`ts_rank`, so this is the only backend where
//! the ranking algorithm itself — [`score`] in `src/memory.rs`, plus the
//! shared [`autumn_search::tokenize`] — is Rust code this crate controls.
//!
//! Like the benches in `autumn-web` this is `harness = false` and asserts
//! nothing: it is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-search --bench keyword_search
//! BIN=$(find target/release/deps -maxdepth 1 -name "keyword_search-*" -type f ! -name "*.d")
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
    IndexDefinition, IndexedDocument, KeywordQuery, MemorySearchBackend, PageRequest,
    SearchBackend, SearchDocument, SearchIndexField,
};

const DOC_COUNT: i64 = 5_000;
const TITLE_WORDS: usize = 6;
const BODY_WORDS: usize = 200;
const QUERY_COUNT: usize = 50;
const INDEX_NAME: &str = "bench_articles";

const FIELDS: &[SearchIndexField] = &[
    SearchIndexField::new("title", 'A'),
    SearchIndexField::new("body", 'B'),
];

/// A small, realistic-looking vocabulary (common English prose words) rather
/// than random bytes, so tokenization and matching behave like real text: mixed
/// word lengths, repeats across documents, no pathological single-character
/// tokens.
const VOCAB: &[&str] = &[
    "autumn",
    "framework",
    "request",
    "response",
    "handler",
    "router",
    "middleware",
    "session",
    "database",
    "query",
    "index",
    "search",
    "document",
    "field",
    "weight",
    "score",
    "rank",
    "ranking",
    "token",
    "text",
    "article",
    "post",
    "author",
    "title",
    "body",
    "summary",
    "content",
    "release",
    "version",
    "feature",
    "bug",
    "fix",
    "performance",
    "profile",
    "benchmark",
    "allocation",
    "memory",
    "vector",
    "embedding",
    "similarity",
    "cosine",
    "tenant",
    "filter",
    "page",
    "pagination",
    "backend",
    "engine",
    "keyword",
    "match",
    "relevance",
    "user",
    "server",
    "client",
    "config",
    "plugin",
    "schema",
    "migration",
    "table",
    "column",
    "record",
    "model",
    "route",
    "layer",
    "service",
    "future",
    "async",
    "await",
    "runtime",
    "thread",
    "lock",
    "store",
    "cache",
    "job",
    "queue",
    "worker",
    "event",
    "log",
    "trace",
    "metric",
    "error",
    "result",
    "value",
    "string",
    "number",
    "list",
    "map",
    "set",
    "key",
    "id",
    "name",
    "type",
    "struct",
    "trait",
    "module",
    "crate",
    "package",
    "test",
    "suite",
    "coverage",
    "report",
    "issue",
    "pull",
    "review",
    "merge",
    "branch",
    "commit",
    "deploy",
    "build",
    "compile",
    "link",
    "binary",
];

/// xorshift64* — enough spread for a reproducible synthetic corpus, no
/// external `rand` dependency.
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

    fn word(&mut self) -> &'static str {
        let index = self.next_u64() % (VOCAB.len() as u64);
        VOCAB[usize::try_from(index).expect("index < VOCAB.len() always fits usize")]
    }

    fn text(&mut self, words: usize) -> String {
        (0..words)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

const fn definition() -> IndexDefinition {
    IndexDefinition::new(INDEX_NAME, "english", FIELDS, None, false)
}

/// `DOC_COUNT` documents, each with a `title` and a `body` field drawn from
/// `VOCAB` — the same shape a scaffolded article/post model produces.
fn build_corpus() -> Vec<IndexedDocument> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    (0..DOC_COUNT)
        .map(|id| {
            let title = rng.text(TITLE_WORDS);
            let body = rng.text(BODY_WORDS);
            IndexedDocument::new(
                SearchDocument::new(INDEX_NAME, id)
                    .with_field("title", 'A', title)
                    .with_field("body", 'B', body),
            )
        })
        .collect()
}

/// `QUERY_COUNT` realistic two-word queries drawn from the same vocabulary, so
/// most queries match a meaningful subset of the corpus rather than nothing.
fn build_queries() -> Vec<String> {
    let mut rng = Rng(0xC2B2_AE3D_27D4_EB4F);
    (0..QUERY_COUNT)
        .map(|_| format!("{} {}", rng.word(), rng.word()))
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
            let text = queries[i % queries.len()].clone();
            let query = KeywordQuery::new(text, PageRequest::new(1, 20));
            let page = backend
                .keyword_search(&definition, &query)
                .await
                .expect("keyword_search");
            black_box(&page);
        }

        for i in 0..iterations {
            let text = queries[i as usize % queries.len()].clone();
            let query = KeywordQuery::new(text, PageRequest::new(1, 20));
            let page = backend
                .keyword_search(&definition, &query)
                .await
                .expect("keyword_search");
            total_hits += page.content.len();
            black_box(&page);
        }

        println!(
            "completed {iterations} keyword searches over {DOC_COUNT} documents ({total_hits} total hits)"
        );
    });
}
