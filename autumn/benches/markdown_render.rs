//! Renders realistic user-submitted Markdown through
//! `autumn_web::markdown::render_user_content_html` — the path a comment,
//! forum post, or wiki body takes on every write and (for any app that
//! doesn't cache the rendered HTML) every read, per issue #1255. No other
//! committed bench touches the `markdown` module.
//!
//! The source is a realistic ~300-word rich-text body: three paragraphs of
//! prose, a fenced code block with a language hint (exercises the
//! `attribute_filter` closure's `language-*` validation), a bulleted list, a
//! 3x3 table (exercises the `text-align` `style` allowlisting), two links —
//! one `https:` (kept), one `javascript:` (rejected, degraded to text) — so
//! both the `SafeEvents` link-scheme filter and its "keep the text, drop the
//! anchor" path run on every render, not just the happy path.
//!
//! Two entry points are exercised end to end on every call:
//! `pulldown_cmark::Parser` → `SafeEvents` → `pulldown_cmark::html::push_html`
//! (the Markdown-to-HTML half) and `ammonia::Builder::clean` (the
//! allowlist-sanitizer half, itself a full html5ever parse + serialize). The
//! `LazyLock` sanitizer is built once at first use, same as production — the
//! warm-up rounds below absorb that one-time cost before the measured loop.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check on the output shape: it is a workload to
//! point a profiler at. `required-features` since `markdown` is an opt-in
//! feature, unlike most of the other benches in this crate.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench markdown_render --features markdown
//! BIN=$(find target/release/deps -maxdepth 1 -name "markdown_render-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 2000
//! callgrind_annotate --threshold=80 callgrind.out | head -60
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs, subtracted, isolate the marginal per-render cost from process
//! # startup + the one-time ammonia sanitizer construction (see
//! # `request_pipeline.rs` for why `--iterations 0` is the base to subtract).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 500
//! ```
//!
//! `--iterations N` renders the fixed body N times after a fixed 50-render
//! warm-up (which also absorbs the one-time `SANITIZER` `LazyLock` init so
//! the measured rounds never pay it).

use std::hint::black_box;

use autumn_web::markdown::render_user_content_html;

/// A realistic rich-text comment body: prose, a fenced code block with a
/// language hint, a list, a table with aligned columns, and both an allowed
/// and a rejected link scheme — the same shape `tests/integration/rich_text.rs`
/// exercises, sized like a real forum reply rather than a one-line smoke test.
const BODY: &str = r"# Re: migrating the worker queue

Thanks for writing this up — we hit almost exactly the same issue last
quarter. The short version is that **backpressure** matters more than raw
throughput once you have more than a handful of consumers, and it's easy to
miss that until production traffic finds it for you.

A few things that helped us:

- Cap the in-flight batch size per worker, not just the poll interval
- Emit a counter for `queue_depth` so the dashboard actually shows the
  problem before it pages someone
- Retry with jitter, *not* a fixed backoff — see [this write-up](https://example.com/backoff-jitter)
  for the reasoning
- Don't trust [this one weird trick](javascript:alert(1)), obviously

Here's roughly what our dispatch loop looks like now:

```rust
loop {
    let batch = queue.poll(MAX_BATCH).await?;
    if batch.is_empty() {
        sleep(POLL_INTERVAL).await;
        continue;
    }
    for job in batch {
        worker.dispatch(job).await?;
    }
}
```

And the rollout numbers, for anyone comparing notes:

| Phase | p50 latency | Error rate |
| :--- | ---: | :---: |
| Before | 420ms | 2.1% |
| Canary | 260ms | 0.4% |
| Full rollout | 180ms | 0.1% |

Happy to share the full config if it's useful — just didn't want to paste a
wall of YAML into a comment thread. Let me know if the jitter formula above
doesn't match what you're seeing; it's possible we got lucky with our
particular traffic shape rather than this being universally correct.
";

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);

    for _ in 0..50 {
        let html = render_user_content_html(BODY);
        assert!(
            html.contains("<pre>") && !html.contains("javascript:"),
            "warm-up render lost its expected shape"
        );
    }

    for _ in 0..iterations {
        let html = black_box(render_user_content_html(black_box(BODY)));
        assert!(
            html.contains("<pre>") && !html.contains("javascript:"),
            "measured render lost its expected shape — corrupt run"
        );
        black_box(html.len());
    }

    println!("completed {} renders", iterations + 50);
}
