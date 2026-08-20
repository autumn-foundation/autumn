//! The guest dialogue loop — the whole of a capsule's `main`.
//!
//! ```rust,ignore
//! // src/bin/edge-capsule.rs
//! fn main() {
//!     autumn_edge::serve(my_app::handlers::edge_routes());
//! }
//! ```
//!
//! [`serve`] reads request frames from stdin until EOF and answers each with
//! exactly one terminal frame on stdout. Everything interesting lives in
//! [`serve_io`], which takes the transport as arguments — so the entire
//! protocol is exercised natively, over in-memory pipes, with no wasm target
//! and no host in the loop.
//!
//! # Order of decisions
//!
//! Each request walks the same ladder, and the first rung that fails produces
//! a fallthrough rather than a partial answer:
//!
//! 1. **Parse.** A frame that is not well-formed JSON, or is not a request
//!    frame, is a [`FallthroughReason::CapsuleError`] — and the loop keeps
//!    going, because a host that garbles one line will probably send a good
//!    one next.
//! 2. **Version.** A `wire_version` other than [`WIRE_VERSION`] is a
//!    [`FallthroughReason::CapsuleError`]. The capsule never guesses at a
//!    protocol it does not speak.
//! 3. **Method.** Anything but `GET`/`HEAD` is
//!    [`FallthroughReason::MethodNotEdgeEligible`]. The edge lane is read-path
//!    only; a write belongs to the origin, which is the authority.
//! 4. **Capability, before dispatch.** If the matched route declares a seam
//!    this host did not provide, the answer is
//!    [`FallthroughReason::MissingCapability`] and *not one line of handler
//!    code runs*. Half-executing a handler and then discovering it needs a KV
//!    is how you get a duplicated side effect.
//! 5. **Dispatch.** The request goes through the same axum router the origin
//!    builds. An unmatched path lands on the router's fallback, which sets the
//!    sentinel, which becomes [`FallthroughReason::UnknownRoute`].
//!
//! # The sentinel
//!
//! A response carrying the [`FALLTHROUGH_SENTINEL`](crate::wire::FALLTHROUGH_SENTINEL)
//! header is a decline, not
//! an answer: the runtime converts it to a fallthrough and neither the header
//! nor the response body ever reaches the wire. That is how the 404 fallback,
//! the [`EdgeCache`] rejection, and a handler's own explicit opt-out all reach
//! the host through one channel.
//!
//! # Panics and traps
//!
//! A handler that panics *natively* is out of scope here — the origin's
//! existing panic handling owns that case. On `wasm32-wasip1` a panic aborts,
//! which traps the module; the **host** observes the trap and reports
//! [`FallthroughReason::CapsuleError`], so a panicking edge handler degrades
//! to origin-serving rather than to a broken page. See
//! [`host`](crate::host).

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Router;
use tower::ServiceExt as _;

use crate::extract::EdgeCache;
use crate::kv::EdgeKv;
use crate::route::{EdgeCapability, EdgeRoute};
use crate::router::{CapabilityProbe, build_edge_router};
use crate::wire::{
    EdgeOutcome, EdgeRequest, EdgeResponse, FallthroughReason, GuestFrame, HostFrame, WIRE_VERSION,
    canonicalize_headers, from_line, strip_sensitive_headers, strip_sentinel, to_line,
};

/// Serve edge requests over stdin/stdout until the host closes the stream.
///
/// This is the entire body of a capsule's `main`. It returns on EOF; a
/// transport error is unrecoverable at the edge (there is nowhere left to
/// report it) and also ends the loop.
pub fn serve(routes: Vec<EdgeRoute>) {
    let reader = std::io::BufReader::new(std::io::stdin());
    if let Err(err) = serve_io(routes, reader, std::io::stdout()) {
        // Best effort: stdout is the protocol channel, so a diagnostic can
        // only go to stderr, which the host is free to ignore.
        let _ = writeln!(std::io::stderr(), "autumn-edge: transport error: {err}");
    }
}

/// The testable core of [`serve`]: the same loop over an arbitrary transport.
///
/// `reader` and `writer` are the two halves of the host dialogue. They are
/// shared with the dialogue-backed [`EdgeKv`] the runtime installs, so a
/// handler's `EdgeCache::get` performs a real round trip mid-request.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if reading a frame or writing an
/// answer fails. Reaching EOF is a normal, successful end.
pub fn serve_io<R, W>(routes: Vec<EdgeRoute>, reader: R, writer: W) -> std::io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let probe = CapabilityProbe::new(&routes);
    let dispatch_router = build_edge_router(routes);
    let transport = Arc::new(Mutex::new(Transport { reader, writer }));

    loop {
        // The lock is released before dispatch: the dialogue-backed KV takes
        // it again from inside the handler.
        let line = lock(&transport).read_line()?;
        let Some(line) = line else { return Ok(()) };
        if line.trim().is_empty() {
            continue;
        }

        let outcome = handle(&dispatch_router, &probe, &transport, &line);
        lock(&transport).write_frame(&GuestFrame::from_outcome(outcome))?;
    }
}

// ── transport ────────────────────────────────────────────────────────

struct Transport<R, W> {
    reader: R,
    writer: W,
}

impl<R: BufRead, W: Write> Transport<R, W> {
    /// One NDJSON line, or `None` at EOF.
    fn read_line(&mut self) -> std::io::Result<Option<String>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }

    fn write_frame(&mut self, frame: &GuestFrame) -> std::io::Result<()> {
        let line = to_line(frame).map_err(std::io::Error::other)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }
}

/// Lock the transport, recovering from a poisoned mutex rather than panicking:
/// a poisoned lock means an earlier request panicked, which says nothing about
/// whether *this* one can be served.
fn lock<R, W>(transport: &Arc<Mutex<Transport<R, W>>>) -> MutexGuard<'_, Transport<R, W>> {
    transport.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the ladder ───────────────────────────────────────────────────────

fn handle<R, W>(
    router: &Router<()>,
    probe: &CapabilityProbe,
    transport: &Arc<Mutex<Transport<R, W>>>,
    line: &str,
) -> EdgeOutcome
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let frame = match from_line::<HostFrame>(line) {
        Ok(frame) => frame,
        Err(err) => {
            return EdgeOutcome::fallthrough(
                FallthroughReason::CapsuleError,
                format!("could not parse the host frame: {err}"),
            );
        }
    };

    let HostFrame::Request {
        wire_version,
        provided_capabilities,
        request,
    } = frame
    else {
        return EdgeOutcome::fallthrough(
            FallthroughReason::CapsuleError,
            "expected a request frame; the host sent a kv_value out of turn",
        );
    };

    if wire_version != WIRE_VERSION {
        return EdgeOutcome::fallthrough(
            FallthroughReason::CapsuleError,
            format!(
                "unsupported wire version {wire_version}; this capsule speaks wire version \
                 {WIRE_VERSION}"
            ),
        );
    }

    let Some(method) = edge_eligible_method(&request.method) else {
        return EdgeOutcome::fallthrough(
            FallthroughReason::MethodNotEdgeEligible,
            format!(
                "`{}` is not an edge-eligible method; the edge lane serves GET and HEAD only, and \
                 the origin is the authority for every write",
                request.method
            ),
        );
    };

    if let Some(missing) = missing_capability(probe, &request.uri, &provided_capabilities) {
        return EdgeOutcome::fallthrough(
            FallthroughReason::MissingCapability,
            format!(
                "the route serving {} needs the `{missing}` capability, which this host did not \
                 provide (provided: [{}])",
                request.uri,
                render_capabilities(&provided_capabilities)
            ),
        );
    }

    dispatch(router, transport, method, request, &provided_capabilities)
}

/// `GET` and `HEAD` only. `HEAD` is answered by the `GET` handler; axum's
/// `MethodRouter` strips the body.
fn edge_eligible_method(raw: &str) -> Option<http::Method> {
    let method = http::Method::from_bytes(raw.as_bytes()).ok()?;
    (method == http::Method::GET || method == http::Method::HEAD).then_some(method)
}

/// The first declared capability the host did not provide, if any. `None` for
/// an unmatched path: that is the router's answer to give, not the gate's.
fn missing_capability(
    probe: &CapabilityProbe,
    uri: &str,
    provided: &[EdgeCapability],
) -> Option<EdgeCapability> {
    probe
        .needs(uri)?
        .into_iter()
        .find(|needed| !provided.contains(needed))
}

fn render_capabilities(capabilities: &[EdgeCapability]) -> String {
    capabilities
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// ── dispatch ─────────────────────────────────────────────────────────

fn dispatch<R, W>(
    router: &Router<()>,
    transport: &Arc<Mutex<Transport<R, W>>>,
    method: http::Method,
    request: EdgeRequest,
    provided: &[EdgeCapability],
) -> EdgeOutcome
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let uri = request.uri.clone();

    let mut builder = http::Request::builder().method(method).uri(&request.uri);
    // Defensive second strip: the host is supposed to have done this, but a
    // capsule cannot audit the host it runs under.
    for (name, value) in strip_sensitive_headers(&request.headers) {
        builder = builder.header(name, value);
    }

    let mut http_request = match builder.body(axum::body::Body::from(request.body)) {
        Ok(request) => request,
        Err(err) => {
            return EdgeOutcome::fallthrough(
                FallthroughReason::CapsuleError,
                format!("could not rebuild the request for {uri}: {err}"),
            );
        }
    };

    if provided.contains(&EdgeCapability::Kv) {
        http_request
            .extensions_mut()
            .insert(EdgeCache::new(Arc::new(DialogueKv {
                transport: Arc::clone(transport),
            })));
    }

    let router = router.clone();
    futures::executor::block_on(async move {
        // `Router<()>`'s error type is `Infallible`, so this pattern is
        // irrefutable — which is exactly why the dispatch needs no fallible
        // arm and no `unwrap`.
        let Ok(response) = router.oneshot(http_request).await;

        let status = response.status().as_u16();
        // Header values cross the wire as JSON strings, so they must be valid
        // UTF-8. A lossy conversion here would silently serve *different
        // bytes* than the origin (which sends header values verbatim), so a
        // non-UTF-8 value is a deterministic decline instead: the origin
        // serves the request with the value intact.
        let mut headers = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers() {
            // Cookies are session state, and sessions are origin-only: a
            // cached edge response carrying `set-cookie` would smear one
            // client's cookie across every client behind the CDN. Declining
            // lets the origin — which owns cookie semantics — serve it.
            if name.as_str().eq_ignore_ascii_case("set-cookie") {
                return EdgeOutcome::fallthrough(
                    FallthroughReason::CapsuleError,
                    format!(
                        "the handler for {uri} set a cookie; cookies are session state, which \
                         is origin-only — the edge lane never sets cookies"
                    ),
                );
            }
            let Ok(text) = std::str::from_utf8(value.as_bytes()) else {
                return EdgeOutcome::fallthrough(
                    FallthroughReason::CapsuleError,
                    format!(
                        "the handler for {uri} set a non-UTF-8 value on the `{name}` response \
                         header, which cannot cross the edge wire byte-faithfully"
                    ),
                );
            };
            headers.push((name.as_str().to_owned(), text.to_owned()));
        }
        let sentinel = strip_sentinel(&mut headers);

        if let Some(raw) = sentinel {
            // The declining response's body is internal detail: it is a
            // message for a developer, not bytes anyone asked for, so it is
            // deliberately not forwarded.
            return raw.parse::<FallthroughReason>().map_or_else(
                |_| {
                    EdgeOutcome::fallthrough(
                        FallthroughReason::CapsuleError,
                        format!(
                            "the edge lane declined {uri} with an unrecognised fallthrough \
                             reason `{raw}`"
                        ),
                    )
                },
                |reason| {
                    EdgeOutcome::fallthrough(
                        reason,
                        format!("the edge lane declined {uri}: {reason}"),
                    )
                },
            );
        }

        let body = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
            Ok(body) => body.to_vec(),
            Err(err) => {
                return EdgeOutcome::fallthrough(
                    FallthroughReason::CapsuleError,
                    format!("could not collect the response body for {uri}: {err}"),
                );
            }
        };

        // Safety net behind the `#[edge]` macro's `Extension<…>` refusal: an
        // alias can smuggle the extractor past that token-level check, and the
        // capsule installs no request extensions, so axum answers with this
        // 500. Declining is safe even if a handler deliberately produced the
        // same bytes — a fallthrough re-runs the identical handler at the
        // origin, which serves whatever it genuinely returns.
        if status == 500 && body.starts_with(b"Missing request extension") {
            return EdgeOutcome::fallthrough(
                FallthroughReason::CapsuleError,
                format!(
                    "the handler for {uri} required a request extension the capsule does not \
                     install; extensions are origin-only — use the `EdgeCache` seam, or serve \
                     this route from the origin"
                ),
            );
        }

        EdgeOutcome::Served(EdgeResponse {
            status,
            headers: canonicalize_headers(&headers),
            body,
        })
    })
}

// ── the dialogue-backed KV ───────────────────────────────────────────

/// The edge-side [`EdgeKv`]: every read is a round trip to the host, mid
/// request, over the same stdio the request itself arrived on.
struct DialogueKv<R, W> {
    transport: Arc<Mutex<Transport<R, W>>>,
}

impl<R, W> EdgeKv for DialogueKv<R, W>
where
    R: BufRead + Send,
    W: Write + Send,
{
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        // The lock is held for the round trip and released before the value is
        // decoded, so a slow parse never blocks the next frame.
        let line = {
            let mut transport = lock(&self.transport);
            transport
                .write_frame(&GuestFrame::KvGet {
                    key: key.to_owned(),
                })
                .ok()?;
            transport.read_line().ok()??
        };

        // Any protocol failure — a transport error, EOF, a garbled line, an
        // out-of-turn frame — is reported as a miss. A miss is always a legal
        // answer from this seam (see `kv`), so a broken host degrades the page
        // rather than the request.
        match from_line::<HostFrame>(&line).ok()? {
            HostFrame::KvValue { value } => value,
            HostFrame::Request { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_get_and_head_are_edge_eligible() {
        assert_eq!(edge_eligible_method("GET"), Some(http::Method::GET));
        assert_eq!(edge_eligible_method("HEAD"), Some(http::Method::HEAD));
        assert_eq!(edge_eligible_method("POST"), None);
        assert_eq!(edge_eligible_method("DELETE"), None);
        assert_eq!(edge_eligible_method("get"), None);
        assert_eq!(edge_eligible_method("not a method"), None);
        assert_eq!(edge_eligible_method(""), None);
    }

    #[test]
    fn capability_rendering_is_readable_when_empty() {
        assert_eq!(render_capabilities(&[]), "");
        assert_eq!(render_capabilities(&[EdgeCapability::Kv]), "kv");
    }

    #[test]
    fn the_sentinel_name_is_namespaced_so_no_handler_sets_it_by_accident() {
        assert!(crate::wire::FALLTHROUGH_SENTINEL.starts_with("x-autumn-edge-"));
    }
}
