//! Drives real CRUD traffic through a `#[repository]`-generated repository
//! against a real Postgres connection, so `valgrind --tool=callgrind|dhat`
//! can attribute the query-building / row-mapping / JSON-shaping cost that
//! `benches/request_pipeline.rs` deliberately excludes (its handlers are
//! trivial, non-DB, by design — see that file's header) and that no other
//! committed bench touches: `version_history`/`form_render`/
//! `attribute_encryption` all measure pure-Rust logic with zero DB access.
//!
//! Every call goes through the actual public repository API `#[repository]`
//! generates (`with_pool_untracked`, `save`, `find_by_id`, `page`) — the same
//! methods a scaffolded handler calls (`autumn-cli/src/generate/scaffold.rs`),
//! not internal helpers reached only by this harness.
//!
//! **Requires a reachable, TLS-free Postgres.** Point `DATABASE_URL` at one
//! (a local `postgres` service works — no testcontainer needed, this harness
//! owns its own table and drops it on start). Defaults to
//! `postgres://postgres:postgres@127.0.0.1:5432/postgres`. Both connections
//! this harness opens go through diesel's/diesel-async's stock `NoTls`
//! establish path, not `autumn::db`'s rustls setup callback (`pub(crate)`,
//! unreachable from a bench) — a `sslmode=require`/`verify-full` URL is
//! rejected up front with an explanatory error rather than failing deep
//! inside libpq. Point this at a local/trusted database, not a
//! TLS-requiring one.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench repository_crud
//! BIN=$(find target/release/deps -maxdepth 1 -name "repository_crud-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 500
//! callgrind_annotate --threshold=90 callgrind.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs, subtracted: `--iterations 0` pays for connecting, seeding, and
//! # pool construction so it can be subtracted from a longer run, leaving the
//! # MARGINAL per-operation cost rather than one amortised over the run length.
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 200
//! ```
//!
//! `--iterations N` runs N rounds after a fixed 20-round warm-up; each round
//! is one `save` (create), one `find_by_id` (point read of a pre-seeded row),
//! and one `page` (a 20-row listing) — the same three-shape mix
//! `benches/request_pipeline.rs` uses for its trivial-handler ingress
//! workload, here run against the real write/read-by-id/list paths instead.
//!
//! `--fast-recycle` switches the pool's `ManagerConfig::recycling_method`
//! from diesel-async's default (`Verified`, a `SELECT 1` round trip on every
//! checkout) to `Fast` (no round trip). It exists to reproduce the finding
//! recorded in issue-tracking for this harness: `autumn::db::create_pool`
//! never overrides `recycling_method` away from that default, and
//! `#[repository]`'s generated `__autumn_acquire_from` already issues its own
//! round trip (`SET statement_timeout`) on every checkout, so every
//! repository call in a deployed app currently pays for two liveness-style
//! round trips where one would do. Compare with/without to reproduce:
//!
//! ```sh
//! valgrind --tool=callgrind --callgrind-out-file=verified.out "$BIN" --iterations 5000
//! valgrind --tool=callgrind --callgrind-out-file=fast.out     "$BIN" --iterations 5000 --fast-recycle
//! ```

use std::hint::black_box;

use autumn_web::pagination::PageRequest;
use diesel::Connection;
use diesel::connection::SimpleConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;

mod schema {
    diesel::table! {
        bolt_bench_products (id) {
            id -> Int8,
            name -> Text,
            description -> Text,
            price_cents -> Int8,
            in_stock -> Bool,
            created_at -> Timestamp,
        }
    }
}
use schema::bolt_bench_products;

/// Shape of a typical scaffolded model: a mix of text, numeric, and boolean
/// fields plus a framework-populated timestamp, matching the variety
/// `autumn generate scaffold` produces (same spirit as `form_render.rs`'s
/// `Article` fixture, here mapped to a real table instead of rendered).
#[autumn_web::model(table = "bolt_bench_products")]
pub struct BenchProduct {
    #[id]
    pub id: i64,
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    pub in_stock: bool,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

#[autumn_web::repository(BenchProduct, table = "bolt_bench_products")]
pub trait BenchProductRepository {}

const SEEDED_ROWS: i64 = 5_000;
const PAGE_SIZE: u32 = 20;

/// Whether `s` looks like a `postgres://`/`postgresql://` URL rather than a
/// libpq keyword/value string. Mirrors `autumn::db::pg_conn_str::is_url`
/// (`pub(crate)`, unreachable from a bench — duplicated rather than reused).
fn is_url_connection_string(s: &str) -> bool {
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

/// Parse a libpq-style `key = value` connection string into its pairs:
/// whitespace is allowed around `=`, values may be single-quoted, and `\`
/// escapes the next character both inside and outside quotes (matches
/// tokio-postgres's own keyword/value parser, e.g. `host=db sslmode = require`
/// or `sslmode='verify-full'`). Mirrors
/// `autumn::db::pg_conn_str::keyword_value_pairs` (`pub(crate)`, unreachable
/// from a bench — duplicated rather than reused; see that module for the full
/// spec and its test cases, which this shares).
fn keyword_value_pairs(s: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut it = s.chars().peekable();
    loop {
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        let mut key = String::new();
        while let Some(&c) = it.peek() {
            if c.is_whitespace() || c == '=' {
                break;
            }
            key.push(c);
            it.next();
        }
        if key.is_empty() {
            return pairs;
        }
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        if it.next() != Some('=') {
            return pairs;
        }
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if it.next_if_eq(&'\'').is_some() {
            loop {
                match it.next() {
                    Some('\'') | None => break,
                    Some('\\') => {
                        if let Some(escaped) = it.next() {
                            value.push(escaped);
                        }
                    }
                    Some(c) => value.push(c),
                }
            }
        } else {
            while let Some(&c) = it.peek() {
                if c.is_whitespace() {
                    break;
                }
                it.next();
                if c == '\\' {
                    if let Some(c2) = it.next() {
                        value.push(c2);
                    }
                } else {
                    value.push(c);
                }
            }
        }
        pairs.push((key, value));
    }
}

/// The connection string's effective `sslmode` (last occurrence wins,
/// matching libpq/tokio-postgres), from either a URL's query string or
/// libpq keyword/value syntax. `None` when absent.
fn sslmode_of(database_url: &str) -> Option<String> {
    let pairs: Vec<(String, String)> = if is_url_connection_string(database_url) {
        url::Url::parse(database_url)
            .map(|u| {
                u.query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        keyword_value_pairs(database_url)
    };
    pairs
        .into_iter()
        .rev()
        .find(|(k, _)| k == "sslmode")
        .map(|(_, v)| v)
}

fn database_url() -> String {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:5432/postgres".to_owned());
    // Both connections this harness opens (the sync `PgConnection` below and
    // the async pool in `main`) go through diesel's/diesel-async's stock,
    // `NoTls`-hardcoded establish path — neither reuses `autumn::db`'s rustls
    // setup callback, which is `pub(crate)` and unreachable from a bench (a
    // separate crate). `sslmode=require`/`verify-full`/`verify-ca` would
    // otherwise fail deep inside libpq/tokio-postgres with a
    // connection-refused-shaped error that gives no hint why. Reject it here,
    // at the one place both callers route through, with an error that says
    // why (Codex review on #2486; `sslmode_of` handles the spaced/quoted
    // keyword-value forms a plain substring check misses, per its follow-up).
    if let Some(mode) = sslmode_of(&url) {
        assert!(
            !matches!(mode.as_str(), "require" | "verify-full" | "verify-ca"),
            "repository_crud is a local profiling harness: it connects with diesel's/\
             diesel-async's stock NoTls establish path, not autumn::db's TLS-aware pool (that \
             setup callback is crate-private). Point DATABASE_URL at a local/trusted Postgres \
             with sslmode absent, disable, or prefer — not {mode:?}."
        );
    }
    url
}

/// Fresh table, seeded with a realistic-shaped fixture — skewed enough that
/// no two rows are identical, cheap enough to seed in one `INSERT ... SELECT`.
fn setup_and_seed(url: &str) {
    let mut conn = diesel::PgConnection::establish(url).expect("sync db connection");
    conn.batch_execute("DROP TABLE IF EXISTS bolt_bench_products")
        .expect("drop table");
    conn.batch_execute(
        "CREATE TABLE bolt_bench_products ( \
            id BIGSERIAL PRIMARY KEY, \
            name TEXT NOT NULL, \
            description TEXT NOT NULL, \
            price_cents BIGINT NOT NULL, \
            in_stock BOOLEAN NOT NULL, \
            created_at TIMESTAMP NOT NULL DEFAULT now() \
         )",
    )
    .expect("create table");
    conn.batch_execute(&format!(
        "INSERT INTO bolt_bench_products (name, description, price_cents, in_stock) \
         SELECT \
           'Product ' || i, \
           'A realistic product description, long enough to be representative, for item number ' || i || '.', \
           500 + (i * 137) % 100000, \
           (i % 4) != 0 \
         FROM generate_series(1, {SEEDED_ROWS}) AS i"
    ))
    .expect("seed bolt_bench_products");
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    let url = database_url();
    setup_and_seed(&url);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let mut manager_config =
        diesel_async::pooled_connection::ManagerConfig::<autumn_web::RuntimeConnection>::default();
    if std::env::args().any(|a| a == "--fast-recycle") {
        manager_config.recycling_method = diesel_async::pooled_connection::RecyclingMethod::Fast;
    }
    let config = AsyncDieselConnectionManager::<autumn_web::RuntimeConnection>::new_with_config(
        &url,
        manager_config,
    );
    let pool = Pool::builder(config).build().expect("pool");
    let repo = PgBenchProductRepository::with_pool_untracked(pool);

    let page_req = PageRequest::new(1, PAGE_SIZE);
    let seeded_ids: Vec<i64> = (1..=SEEDED_ROWS).collect();

    let run_round = |i: u32, rt: &tokio::runtime::Runtime| {
        rt.block_on(async {
            let new_product = NewBenchProduct {
                name: format!("New product {i}"),
                description: format!(
                    "A freshly created product from round {i}, with enough text to be realistic."
                ),
                price_cents: 1000 + i64::from(i % 5000),
                in_stock: !i.is_multiple_of(3),
            };
            let created = repo.save(&new_product).await.expect("save");
            black_box(created.id);

            let lookup_id = seeded_ids[(i as usize) % seeded_ids.len()];
            let found = repo
                .find_by_id(lookup_id)
                .await
                .expect("find_by_id")
                .expect("seeded row must exist");
            black_box(found.name.len());

            let page = repo.page(&page_req).await.expect("page");
            black_box(page.content.len());
        });
    };

    for i in 0..20 {
        run_round(i, &rt);
    }
    for i in 0..iterations {
        run_round(i, &rt);
    }

    println!(
        "completed {} rounds (save+find_by_id+page each)",
        iterations + 20
    );
}
