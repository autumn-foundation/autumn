//! Renders a realistic Atom feed of blog posts through `autumn_web::feed`,
//! the same `Feed::atom(...).entries(...)` shape `examples/blog`'s
//! `feed_xml` handler builds from real `Post` rows (title + full body as the
//! entry summary), so `valgrind --tool=callgrind|dhat` can attribute the
//! per-render cost of `Feed::render` and the private `escape` helper it
//! calls on every text field of every entry.
//!
//! The entry bodies are realistic prose paragraphs, not a stress payload:
//! mostly plain ASCII sentences, several containing an apostrophe/contraction
//! or an ampersand in the title (`escape`'s slow, character-escaping path),
//! most containing none of the five escaped characters at all (a candidate
//! fast path). This mirrors real blog content rather than gaming the
//! benchmark toward one path.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing: it is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench feed_render
//! BIN=$(find target/release/deps -maxdepth 1 -name "feed_render-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 2000
//! callgrind_annotate --threshold=80 callgrind.out | head -60
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 2000
//! ```
//!
//! `--iterations N` renders the 30-entry feed N times after a fixed 50-render
//! warm-up.

use std::hint::black_box;

use autumn_web::feed::{Feed, FeedEntry};
use chrono::{TimeZone, Utc};

/// Realistic post-body paragraphs, cycled across entries. Most are plain
/// ASCII prose with no XML-special characters; several carry an apostrophe
/// (`'`) or an ampersand (`&`) — real English prose does too ("don't",
/// "Rust & WebAssembly") — so the workload isn't artificially all-clean.
const BODIES: &[&str] = &[
    "We shipped named futures this week, a small change that makes async \
     stack traces dramatically easier to read when a request hangs. The \
     motivation came from a real production incident where the on-call \
     engineer spent twenty minutes staring at an unhelpful poll() frame \
     before finding the actual stuck task.",
    "A reader asked why we don't support connection pooling for SQLite the \
     same way we do for Postgres. The short answer is that SQLite's \
     single-writer model makes a traditional pool mostly pointless, but \
     it's a fair question and we've written up the reasoning in the docs.",
    "This release focuses on developer experience: faster incremental \
     builds, clearer error messages when a migration is unsafe, and a new \
     doctor command that catches common misconfigurations before they \
     reach production. Full changelog is linked below.",
    "Rust & WebAssembly keep coming up in our roadmap discussions. We're \
     not ready to commit to a timeline yet, but early prototypes of the \
     edge capsule runtime are promising enough that we wanted to share \
     progress publicly.",
    "Migrating a five-year-old Rails app to Autumn taught us a lot about \
     what scaffolding needs to generate by default. The biggest surprise \
     was how much time the team spent on admin CRUD screens that our \
     generator now produces in seconds.",
    "A short note on testing philosophy: we prefer integration tests that \
     exercise the real router over unit tests that mock half the stack. \
     It costs more compile time up front but catches an entire class of \
     bugs that mocks quietly paper over.",
    "The community call this month covered the new i18n fallback chain, \
     three community plugins worth checking out, and a lively debate about \
     whether the CLI should default to Postgres or SQLite for new projects. \
     Recording is up on the usual channel.",
    "Someone in the forum pointed out that our default rate limiter's \
     documentation didn't mention the per-token bucket size, so that's \
     fixed now. Small thing, but it's exactly the kind of gap that trips up \
     a first-time reader.",
];

const TITLES: &[&str] = &[
    "Named futures and readable stack traces",
    "Why no connection pool for SQLite",
    "Faster builds, clearer errors",
    "Rust & WebAssembly: an early look",
    "Five years of Rails, ported in a weekend",
    "How we think about testing",
    "Community call recap",
    "Docs fix: rate limiter bucket size",
];

fn sample_feed() -> Feed {
    let published = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    Feed::atom(
        "Autumn Blog",
        "https://autumn-demo.example.com/",
        "https://autumn-demo.example.com/feed.xml",
    )
    .author("Autumn Blog")
    .entries((0..30).map(|i| {
        let url = format!("https://autumn-demo.example.com/posts/post-{i}");
        FeedEntry::new(url.clone(), TITLES[i % TITLES.len()], url)
            .summary(BODIES[i % BODIES.len()])
            .published(published)
            .updated(published)
    }))
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    for _ in 0..50 {
        black_box(sample_feed().render());
    }

    for _ in 0..iterations {
        black_box(sample_feed().render());
    }

    println!("completed {} feed renders", iterations + 50);
}
