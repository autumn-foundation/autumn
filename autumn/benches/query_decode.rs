//! Decodes a realistic list-endpoint query string through
//! `autumn_web::query_string::from_query_str` — the function behind every
//! `Query<T>` extraction (`extract.rs`), so every request carrying a query
//! string pays it. No other committed bench touches it.
//!
//! The handler shape is a filterable, paginated index: scalar search/paging/sort
//! params, a repeated-key tag list, and a bracketed `filter[...]` object, with
//! percent-encoded values. A second, flat-only shape (`?q=&page=&per_page=`)
//! covers the common simple case.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check on the decoded output: it is a workload to
//! point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench query_decode
//! BIN=$(find target/release/deps -maxdepth 1 -name "query_decode-*" -type f ! -name "*.d")
//!
//! # Instruction profile (marginal cost = run(N) - run(0))
//! valgrind --tool=callgrind --callgrind-out-file=cg.out "$BIN" --iterations 2000
//! callgrind_annotate --threshold=90 cg.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool)
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 500
//! ```
//!
//! `--iterations N` decodes both shapes N times after a fixed 50-round warm-up.

use std::hint::black_box;

use autumn_web::query_string::from_query_str;
use serde::Deserialize;

// Fields exist only to be decoded; the bench asserts on shape, not content.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Filter {
    status: Option<String>,
    owner: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListParams {
    q: Option<String>,
    page: u32,
    per_page: u32,
    sort: String,
    order: String,
    #[serde(default)]
    tags: Vec<String>,
    filter: Option<Filter>,
}

#[derive(Debug, Deserialize)]
struct SimpleParams {
    q: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

const RICH: &str = "q=autumn%20web%20framework&page=3&per_page=25&sort=created_at&order=desc\
&tags=rust&tags=web&tags=htmx&filter%5Bstatus%5D=open&filter%5Bowner%5D=alice%40example.com";
const SIMPLE: &str = "q=hello%20world&page=2&per_page=50";

fn round() {
    let rich: ListParams = from_query_str(black_box(RICH)).unwrap();
    assert_eq!(rich.tags.len(), 3);
    assert!(rich.filter.is_some() && rich.q.is_some() && rich.per_page == 25);
    black_box((&rich.sort, &rich.order, rich.page, &rich.filter));
    let simple: SimpleParams = from_query_str(black_box(SIMPLE)).unwrap();
    assert_eq!(simple.page, Some(2));
    black_box((simple.q, simple.per_page));
}

fn main() {
    let iterations: u32 = std::env::args()
        .skip_while(|a| a != "--iterations")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    for _ in 0..50 {
        round();
    }
    for _ in 0..iterations {
        round();
    }
}
