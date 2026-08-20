//! The edge-safe half of the app: handlers that run at the origin *and* inside
//! the capsule, from this one source file.
//!
//! # The rule for this module
//!
//! Everything in here compiles for `wasm32-wasip1`, where `autumn-web` is not
//! in the dependency graph at all. Two consequences, both load bearing:
//!
//! 1. **Only `#[edge]` routes live here.** A plain `#[get]` emits a native
//!    companion that names `::autumn_web`, and the wasm build would stop on it.
//!    That is by design: the compiler, not a convention, is what keeps this
//!    module edge-safe. Origin-only routes live in [`crate::origin`].
//! 2. **Imports come from `autumn_edge`.** [`autumn_edge::prelude`] carries the
//!    route macros and exactly the extractors the edge can mediate. An
//!    extractor that is not there (`Db`, `Session`, `Clock`, `Rng`) does not
//!    satisfy the `EdgeHandler` bound, and the diagnostic says so.
//!
//! # Why these six routes
//!
//! Each one is a class of thing that could plausibly differ between two
//! compilations of "the same" code, which is what
//! `tests/conformance.rs` exists to disprove:
//!
//! | Route | What it pins down |
//! | --- | --- |
//! | `/greet/{name}` | path capture, percent-decoding, a handler-set header |
//! | `/note/{key}` | the mediated KV seam — hit and miss, `#[edge(needs(kv))]` |
//! | `/stats` | repeated query keys and float formatting |
//! | `/stats/count` | a primitive (`usize`) return value |
//! | `/whoami` | credential invisibility: both lanes strip the same headers |
//! | `/boom` | a panic: a trap at the edge, a 500 at the origin |

use autumn_edge::prelude::*;

/// How many routes the edge lane carries. Rendered by `/stats` and
/// `/stats/count` so both lanes have a number to disagree about.
pub const EDGE_ROUTE_COUNT: usize = 6;

/// `GET /greet/{name}` — the shape of an edge route in one screen.
///
/// The header is set by the handler, so it is part of the byte-identity
/// contract: the conformance suite requires the origin to emit it too, with the
/// same value.
#[get("/greet/{name}")]
#[edge]
pub async fn greet(
    Path(name): Path<String>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (
        StatusCode::OK,
        [("x-greeting-lane", "edge-eligible")],
        format!("Hello, {name}!"),
    )
}

/// `GET /note/{key}` — the one mediated platform seam (AC-4).
///
/// [`EdgeCache`] is the same extractor at the origin and at the edge. At the
/// origin it reads through `AppBuilder::with_edge_kv`; at the edge each `get`
/// is a round trip to the host over the capsule dialogue. The handler cannot
/// tell, which is exactly the point.
///
/// A miss is a normal answer — an edge replica is allowed not to have the key —
/// so the handler renders something sensible either way rather than failing.
#[get("/note/{key}")]
#[edge(needs(kv))]
pub async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
    match cache.get_string(&key) {
        Some(note) => format!("{key}: {note}\n"),
        None => format!("{key}: (not cached at this replica)\n"),
    }
}

/// `GET /stats` — repeated query keys and a float, rendered identically on both
/// substrates.
///
/// `Query<Vec<(String, String)>>` keeps every occurrence of a repeated key, in
/// order, instead of collapsing them into a map — a `HashMap` here would make
/// the rendered output depend on hash iteration order, which is the classic way
/// a "deterministic" handler stops being one.
#[get("/stats")]
#[edge]
pub async fn stats(Query(tags): Query<Vec<(String, String)>>) -> String {
    let rendered: Vec<String> = tags
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    // A ratio that does not terminate in binary: if the two lanes ever disagreed
    // about float formatting, this is where it would show.
    let ratio = f64::from(u32::try_from(tags.len()).unwrap_or(u32::MAX)) / 3.0;
    format!(
        "routes={EDGE_ROUTE_COUNT} tags=[{}] ratio={ratio:.6}\n",
        rendered.join(" ")
    )
}

/// `GET /stats/count` — a primitive return value.
///
/// A `usize` is not an axum response on its own, so the route macro wraps this
/// handler in a stringifying shim. The edge companion mounts the *same* wrapper
/// the origin mounts, so the two lanes cannot disagree about how a bare number
/// becomes a body.
#[get("/stats/count")]
#[edge]
pub async fn count() -> usize {
    EDGE_ROUTE_COUNT
}

/// `GET /whoami` — proof that neither lane lets a handler see credentials.
///
/// The wire strips `cookie`/`authorization`/`proxy-authorization` before the
/// capsule dispatches, and the `#[edge]` macro gives the *origin* mount the
/// same strip — so this handler renders the same answer on both substrates
/// even when the client sent credentials. Without that mirror, a handler
/// branching on these headers would be a silent lane divergence.
#[get("/whoami")]
#[edge]
pub async fn whoami(headers: HeaderMap) -> String {
    let visible: Vec<&str> = ["cookie", "authorization", "proxy-authorization"]
        .into_iter()
        .filter(|name| headers.contains_key(*name))
        .collect();
    if visible.is_empty() {
        "anonymous at this lane (credentials are origin-middleware business)\n".to_owned()
    } else {
        format!("credentials visible: {}\n", visible.join(","))
    }
}

/// `GET /boom` — a handler that panics, on purpose.
///
/// At the edge a panic aborts, which traps the module; the host reports
/// [`FallthroughReason::CapsuleError`](autumn_edge::wire::FallthroughReason::CapsuleError)
/// and the request goes to the origin, which answers it with its own error
/// page. A broken edge handler degrades to origin-serving — it does not serve a
/// broken page.
#[get("/boom")]
#[edge]
pub async fn boom() -> String {
    panic!("this handler panics on purpose: see docs/guide/edge.md#failure-is-a-fallthrough")
}

/// The edge lane's route table.
///
/// `edge_routes![]` collects the hidden companion each `#[edge]` route emits,
/// exactly as `routes![]` collects the native ones. `src/bin/edge-capsule.rs`
/// hands the result to `autumn_edge::serve`.
#[must_use]
pub fn edge_routes() -> Vec<EdgeRoute> {
    edge_routes![greet, note, stats, count, whoami, boom]
}
