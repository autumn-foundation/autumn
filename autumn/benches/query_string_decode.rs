//! Decodes realistic list-page query strings through the public
//! `query_string::from_query_str` entry point (what the `Query<T>` extractor
//! calls on every request that carries a query string), so the bracket-grammar
//! tree build and serde walk can be attributed in a profiler.
//!
//! The mix: flat pagination/sort, a faceted filter with nested structs and
//! repeated `tags[]` sequences, and an admin search with percent-encoding.
//! Asserts nothing — `harness = false`, a workload for a profiler.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench query_string_decode
//! BIN=$(find target/release/deps -maxdepth 1 -name "query_string_decode-*" -type f ! -name "*.d")
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 2000
//! callgrind_annotate --threshold=90 callgrind.out | head -40
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 1000
//! ```
//!
//! `--iterations N` decodes each of the 3 shapes N times after a 50-round warm-up.

use std::collections::HashMap;
use std::hint::black_box;

use autumn_web::query_string::from_query_str;
use serde::Deserialize;

#[derive(Deserialize)]
#[allow(dead_code)]
struct Listing {
    page: Option<u32>,
    per_page: Option<u32>,
    sort: Option<String>,
    order: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Range {
    min: Option<u32>,
    max: Option<u32>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Filter {
    status: Option<String>,
    category: Option<String>,
    tags: Vec<String>,
    price: Option<Range>,
    q: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Faceted {
    page: Option<u32>,
    per_page: Option<u32>,
    sort: Option<String>,
    filter: Option<Filter>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Search {
    q: String,
    #[serde(default)]
    ids: Vec<u64>,
    #[serde(default)]
    extra: HashMap<String, String>,
}

const FLAT: &str = "page=3&per_page=25&sort=created_at&order=desc";
const FACETED: &str = "page=2&per_page=50&sort=price&filter[status]=published&filter[category]=books\
&filter[tags][]=rust&filter[tags][]=web&filter[tags][]=async&filter[price][min]=10&filter[price][max]=250\
&filter[q]=hello+world";
const SEARCH: &str = "q=caf%C3%A9%20au%20lait%20%26%20cr%C3%AApes&ids[]=101&ids[]=102&ids[]=103&ids[]=104\
&extra[utm_source]=newsletter&extra[utm_medium]=email&extra[ref]=home";

fn round() {
    black_box(from_query_str::<Listing>(black_box(FLAT)).ok());
    black_box(from_query_str::<Faceted>(black_box(FACETED)).ok());
    black_box(from_query_str::<Search>(black_box(SEARCH)).ok());
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    for _ in 0..50 {
        round();
    }
    for _ in 0..iterations {
        round();
    }
}
