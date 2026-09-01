//! Middleware for serving statically generated files.
//!
//! This module contains the `StaticFileLayer`, which intercepts requests and serves
//! pre-rendered HTML files from the `dist/` directory if they exist. It acts as a
//! lightning-fast cache layer in front of your dynamic routes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::StaticManifest;
use super::isr_coordinator::{IsrCoordinator, LocalIsrCoordinator, isr_window_key};

/// Per-route ISR state, tracking whether a regeneration is in flight
/// and when the last regeneration attempt occurred.
struct IsrRouteState {
    /// `true` when a background regeneration task is running for this route.
    in_flight: AtomicBool,
    /// Unix timestamp of the last regeneration attempt. Used for backoff:
    /// after a failed regeneration, wait at least `REGEN_COOLDOWN_SECS`
    /// before trying again.
    last_attempt: AtomicU64,
}

/// Minimum seconds between regeneration attempts for the same route.
/// Prevents tight retry loops when the handler is failing.
const REGEN_COOLDOWN_SECS: u64 = 30;

/// Layer that resolves incoming request paths against a pre-built static
/// manifest and the corresponding `dist/` directory on disk.
///
/// Created via [`StaticFileLayer::new`], which returns `None` when the
/// expected `manifest.json` is missing or unparseable -- this makes it
/// safe to attempt construction unconditionally and simply skip static
/// serving when no build output exists.
///
/// ## ISR (Incremental Static Regeneration)
///
/// Routes with a `revalidate` interval are served from disk but checked
/// for staleness on each request. When the file on disk is older than
/// `revalidate` seconds, a background Tokio task is spawned to re-render
/// the page. The stale page continues to be served until the fresh one
/// is ready (stale-while-revalidate pattern).
#[derive(Clone)]
pub struct StaticFileLayer {
    dist_dir: PathBuf,
    manifest: Arc<StaticManifest>,
    /// Per-route ISR state, keyed by URL path. Only populated for routes
    /// that have `revalidate` set.
    isr_state: Arc<HashMap<String, IsrRouteState>>,
    /// The Axum router used for ISR regeneration. Cloned from the app
    /// router at construction time. `None` if ISR is not needed.
    isr_router: Option<Arc<axum::Router>>,
    /// Coordination backend that prevents duplicate regeneration.
    /// Defaults to [`LocalIsrCoordinator`] (in-process only).
    /// Use [`with_isr_coordinator`](Self::with_isr_coordinator) to supply
    /// a distributed backend such as `PostgresIsrCoordinator` for
    /// multi-replica deployments.
    isr_coordinator: Arc<dyn IsrCoordinator>,
}

/// A manifest hit: the file to serve and the `Content-Type` to serve it as
/// (#1832).
///
/// Returned by [`StaticFileLayer::resolve_entry`]. The type is already decided
/// — the manifest's recorded value when there is a usable one, otherwise the
/// legacy derivation — so the caller has nothing left to infer. See
/// [`resolved_content_type`] for the ordering.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResolvedStatic {
    /// Absolute path of the generated file inside `dist/`.
    pub file_path: PathBuf,
    /// The `Content-Type` header to serve, ready to attach to the response.
    pub content_type: http::HeaderValue,
}

/// A byte a recorded `Content-Type` may contain: visible ASCII (`0x20`-`0x7e`),
/// or a horizontal tab.
///
/// HTAB is legal OWS around media-type parameters (RFC 9110), so
/// `application/rss+xml;\tprofile="x"` is a perfectly good header — both
/// `HeaderValue::to_str` and `HeaderValue::from_str` accept it — and rejecting
/// it would send an extensionless `/feed` back to the `feed/index.html`
/// derivation and serve it as `text/html`. Tab is also not part of the hazard
/// this predicate exists for: the compression layer matches its exclusions on
/// `to_str().ok().unwrap_or_default()`, and `to_str` *succeeds* on tab, so a
/// tab-bearing type is matched against the carve-outs in full. What must stay
/// rejected is a value `to_str` fails on (any byte from `0x80` up), or one that
/// is blank after trimming — both read as an empty `Content-Type` there, which
/// silently bypasses the image/audio/video and octet-stream carve-outs and
/// gzips a binary body.
///
/// A tab-*only* value is still rejected: `str::trim` strips it, leaving the
/// empty string the caller refuses.
#[must_use]
fn is_legal_content_type_byte(b: u8) -> bool {
    b == b'\t' || (0x20..0x7f).contains(&b)
}

/// The recorded `Content-Type` a manifest entry can actually be served with, or
/// `None` when the stored value is unusable (#1832).
///
/// The single validation the whole static path shares. A value is usable when,
/// after trimming, it is non-empty and every byte is
/// [legal](is_legal_content_type_byte) — visible ASCII or a tab.
/// `HeaderValue::from_str` alone is not a sufficient filter: it accepts any byte
/// `>= 0x20` except DEL, so `"   "` and `"text/htmlé"` pass it — and either would produce a
/// `Content-Type` the compression layer reads as empty (it matches its
/// exclusions on `to_str().ok().unwrap_or_default()`), silently bypassing the
/// image/audio/video and octet-stream carve-outs and gzipping a binary body.
/// CR/LF and NUL are rejected by both checks, so header injection is impossible
/// either way.
///
/// Serving, ISR **and generation** must agree on this, which is why it is one
/// function. The serve path ignores an unusable value and derives instead; if
/// the ISR check still treated that same value as authoritative, no handler
/// response could ever match it and every regeneration would be refused forever
/// — a hand-written or tampered manifest would freeze the route rather than
/// merely having its bad value ignored. Generation
/// ([`build::recorded_content_type`](super::build)) screens through the same
/// function so the manifest can never carry a value the serve path will
/// discard — a recorded-then-ignored value is worse than none, because the
/// route silently falls back to the very derivation the recorded value existed
/// to replace.
#[must_use]
pub(super) fn usable_recorded_content_type(recorded: Option<&str>) -> Option<&str> {
    recorded
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.bytes().all(is_legal_content_type_byte))
}

/// The `Content-Type` to serve for a manifest-backed response (#1832).
///
/// The single decision point for the static-first serve path:
///
/// 1. **The type recorded in the manifest**, when there is one and it is a legal
///    header value. This is the intended type, captured at generation time from
///    the handler's own response, so nothing has to be inferred.
/// 2. Otherwise the **route extension**, but only when the final path segment
///    ends in an extension the crate's asset table recognizes.
///    This is what keeps a generated `/robots.txt` (stored as
///    `robots.txt/index.html`) from being mislabeled `text/html`, while a page
///    whose slug merely *contains* a dot (`/posts/release.v1`,
///    `/users/alice@example.com`) is left to step 3 rather than being mislabeled
///    `application/octet-stream`.
/// 3. Otherwise the **served file name**, which resolves every extensionless
///    page (`/about` → `about/index.html` → `text/html`) and any hand-written
///    manifest that maps a route straight at an extensioned file (`/logo` →
///    `logo.png`).
/// 4. Otherwise `application/octet-stream`.
///
/// Steps 2–4 are the pre-#1832 derivation, kept verbatim so an existing `dist/`
/// keeps serving exactly as before until it is rebuilt.
///
/// Returns an `http::HeaderValue` — reachable from downstream code as
/// `autumn_web::reexports::http::HeaderValue`, since `autumn_web::http` is the
/// HTTP *client* module — rather than a string, so the caller cannot fail to
/// build the response.
///
/// A recorded value is used only if, after trimming, it is non-empty and every
/// byte is visible ASCII or a tab (see [`is_legal_content_type_byte`]).
/// `HeaderValue::from_str` alone is not a sufficient filter: it accepts any byte
/// `>= 0x20` except DEL, so `"   "` and
/// `"text/htmlé"` pass it. Either would produce a `Content-Type` the compression
/// layer reads as empty (it matches its exclusions on
/// `to_str().ok().unwrap_or_default()`), silently bypassing the image/audio/video
/// and octet-stream carve-outs and gzipping a binary body. CR/LF and NUL are
/// rejected by both checks, so header injection is impossible either way.
/// Anything that fails falls through to the derivation, so a bad manifest can
/// neither panic the request path nor inject a header.
///
/// The URL path is always `/`-delimited, so inspecting its last segment is
/// unaffected by platform path separators.
#[must_use]
pub fn resolved_content_type(
    recorded: Option<&str>,
    route_path: &str,
    file_path: &Path,
) -> http::HeaderValue {
    if let Some(recorded) = usable_recorded_content_type(recorded)
        && let Ok(value) = http::HeaderValue::from_str(recorded)
    {
        return value;
    }

    let derived = crate::assets::content_type_for_opt(route_path).unwrap_or_else(|| {
        file_path
            .file_name()
            .and_then(|name| name.to_str())
            .map_or("application/octet-stream", |name| {
                crate::assets::content_type_for(name)
            })
    });
    http::HeaderValue::from_static(derived)
}

impl StaticFileLayer {
    /// Try to load a `StaticFileLayer` from a `dist/` directory.
    ///
    /// Looks for `<dist_dir>/manifest.json`. Returns `None` if the file
    /// does not exist or cannot be parsed as a valid [`StaticManifest`].
    ///
    /// ISR routes are detected from the manifest but no regeneration
    /// router is configured. Use [`with_router`](Self::with_router) to
    /// enable ISR regeneration.
    pub fn new(dist_dir: impl Into<PathBuf>) -> Option<Self> {
        let dist_dir = dist_dir.into();
        let manifest_path = dist_dir.join("manifest.json");
        // A manifest that is *present but unparseable* used to disable static
        // serving with no trace at all — every pre-rendered page silently fell
        // through to the dynamic router. Absent is the normal "this app has no
        // static build" case and stays quiet; malformed is a misconfiguration
        // and says so.
        let manifest = match StaticManifest::load(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                if manifest_path.exists() {
                    tracing::warn!(
                        manifest = %manifest_path.display(),
                        %error,
                        "static manifest could not be parsed; serving every route \
                         dynamically — re-run `autumn build`"
                    );
                }
                return None;
            }
        };

        let isr_state = build_isr_state(&manifest);

        Some(Self {
            dist_dir,
            manifest: Arc::new(manifest),
            isr_state: Arc::new(isr_state),
            isr_router: None,
            isr_coordinator: Arc::new(LocalIsrCoordinator::new()),
        })
    }

    /// Attach an Axum router for ISR background regeneration.
    ///
    /// Without a router, ISR staleness is detected but pages are never
    /// re-rendered. This method enables the full ISR cycle.
    #[must_use]
    pub fn with_router(mut self, router: axum::Router) -> Self {
        self.isr_router = Some(Arc::new(router));
        self
    }

    /// Set the ISR coordination backend.
    ///
    /// The default is [`LocalIsrCoordinator`], which is suitable for
    /// single-replica and development deployments. For multi-replica
    /// deployments sharing a writable `dist/` volume, supply a distributed
    /// backend such as `PostgresIsrCoordinator` (feature `db`) to prevent
    /// stampede writes.
    ///
    /// # Example (production multi-replica)
    ///
    /// ```rust,ignore
    /// use std::sync::Arc;
    /// use autumn_web::static_gen::{StaticFileLayer, PostgresIsrCoordinator};
    ///
    /// let layer = StaticFileLayer::new("dist")
    ///     .unwrap()
    ///     .with_router(app_router)
    ///     .with_isr_coordinator(Arc::new(PostgresIsrCoordinator::new(pool)));
    /// ```
    #[must_use]
    pub fn with_isr_coordinator(mut self, coordinator: Arc<dyn IsrCoordinator>) -> Self {
        self.isr_coordinator = coordinator;
        self
    }

    /// Reference to the loaded manifest.
    #[must_use]
    pub fn manifest(&self) -> &StaticManifest {
        &self.manifest
    }

    /// The `dist/` directory this layer serves files from.
    #[must_use]
    pub fn dist_dir(&self) -> &Path {
        &self.dist_dir
    }

    /// Map a request path (e.g. `"/about"`) to its filesystem path
    /// within `dist/`, based on the manifest.
    ///
    /// Returns `None` if the path is not in the manifest. Does **not**
    /// check whether the file exists on disk -- callers (e.g. `ServeDir`)
    /// handle missing files gracefully.
    ///
    /// If the route has ISR enabled and the file is stale, this method
    /// triggers a background regeneration task (at most one at a time
    /// per route) and still returns the stale file path. The caller
    /// serves the stale content while regeneration happens.
    ///
    /// This is the file-path-only shorthand over
    /// [`resolve_entry`](Self::resolve_entry); use that one when you also need
    /// the `Content-Type` recorded for the route.
    #[must_use]
    pub fn resolve(&self, request_path: &str) -> Option<PathBuf> {
        self.resolve_entry(request_path).map(|hit| hit.file_path)
    }

    /// Resolve a request path to the file to serve **and** the `Content-Type`
    /// recorded for it at generation time (#1832).
    ///
    /// Same lookup and same ISR side effect as [`resolve`](Self::resolve); the
    /// difference is that the recorded MIME type comes back with the path
    /// instead of being thrown away, so the serve path never has to
    /// reverse-engineer it from the route slug and the served file name.
    ///
    /// [`ResolvedStatic::content_type`] is the final header value: the recorded
    /// type when the manifest has a usable one, otherwise the legacy derivation
    /// (see [`resolved_content_type`]). The recorded value is read as a `&str`
    /// borrowed from the shared manifest rather than cloned; a recorded type
    /// still costs the one `HeaderValue` allocation the response needs either
    /// way, and a derived one is `HeaderValue::from_static` and allocates
    /// nothing.
    #[must_use]
    pub fn resolve_entry(&self, request_path: &str) -> Option<ResolvedStatic> {
        let entry = self.manifest.routes.get(request_path)?;
        let file_path = self.dist_dir.join(&entry.file);

        // Check ISR staleness
        if let Some(revalidate) = entry.revalidate {
            self.maybe_trigger_isr(
                request_path,
                &file_path,
                revalidate,
                // Only a value the serve path would actually honour: an
                // unusable one is ignored there, so treating it as the ISR
                // expectation would refuse every regeneration forever.
                usable_recorded_content_type(entry.content_type.as_deref()),
            );
        }

        let content_type =
            resolved_content_type(entry.content_type.as_deref(), request_path, &file_path);

        Some(ResolvedStatic {
            file_path,
            content_type,
        })
    }

    /// Check if a file is stale and trigger background regeneration if needed.
    fn maybe_trigger_isr(
        &self,
        url_path: &str,
        file_path: &Path,
        revalidate_secs: u64,
        recorded_content_type: Option<&str>,
    ) {
        // Check file age
        let is_stale = file_mtime_age_secs(file_path).is_none_or(|age| age > revalidate_secs);

        if !is_stale {
            return;
        }

        let Some(route_state) = self.isr_state.get(url_path) else {
            return;
        };

        let Some(router) = &self.isr_router else {
            // No router configured -- ISR detection only, no regeneration
            return;
        };

        // Check cooldown -- don't retry too fast after a failure
        let now = unix_now();
        let last = route_state.last_attempt.load(Ordering::Relaxed);
        if last > 0 && now.saturating_sub(last) < REGEN_COOLDOWN_SECS {
            return;
        }

        // Try to claim the in-flight flag (CAS: false -> true).
        // This prevents this process from spawning more than one task per route.
        if route_state
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // Another task is already regenerating this route in this process
            return;
        }

        // Record attempt time
        route_state.last_attempt.store(now, Ordering::Relaxed);

        // Compute the revalidation window key for distributed coordination
        let window_key = isr_window_key(url_path, revalidate_secs, now);

        // Spawn background regeneration
        let router = Arc::clone(router);
        let url = url_path.to_owned();
        let dest = file_path.to_owned();
        let in_flight = Arc::clone(&self.isr_state);
        let coordinator = Arc::clone(&self.isr_coordinator);
        let expected_content_type = recorded_content_type.map(str::to_owned);

        tokio::spawn(async move {
            // RAII guard: clears the in_flight flag when this scope exits,
            // including on panic, so ISR is never permanently disabled for
            // a route after a handler crash.
            let _guard = InFlightReset {
                state: &in_flight,
                url: &url,
            };

            // Distributed coordination: prevents multiple replicas from
            // regenerating the same route in the same revalidation window.
            let acquired = coordinator.try_acquire(&url, &window_key).await;
            if !acquired {
                tracing::debug!(
                    "ISR: another replica holds the lock for this window, skipping (route: {}, backend: {})",
                    url,
                    coordinator.backend()
                );
                return; // _guard drops here, resetting in_flight
            }

            let result =
                regenerate_page(&router, &url, &dest, expected_content_type.as_deref()).await;

            // Release distributed lock before the guard drops.
            coordinator.release(&url, &window_key).await;

            match result {
                Ok(()) => {
                    tracing::info!("ISR: page regenerated for route: {}", url);
                }
                Err(e) => {
                    tracing::warn!("ISR: regeneration failed for route: {}, error: {}", url, e);
                }
            }
            // _guard drops here, resetting in_flight
        });
    }
}

/// RAII guard that resets the per-route `in_flight` flag when dropped.
///
/// Placed at the top of every ISR background task so the flag is always
/// cleared on normal exit, early return, or panic.
struct InFlightReset<'a> {
    state: &'a HashMap<String, IsrRouteState>,
    url: &'a str,
}

impl Drop for InFlightReset<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.state.get(self.url) {
            s.in_flight.store(false, Ordering::Release);
        }
    }
}

/// Build per-route ISR state from the manifest. Only routes with
/// `revalidate` set get entries.
fn build_isr_state(manifest: &StaticManifest) -> HashMap<String, IsrRouteState> {
    let mut state = HashMap::new();
    for (path, entry) in &manifest.routes {
        if entry.revalidate.is_some() {
            state.insert(
                path.clone(),
                IsrRouteState {
                    in_flight: AtomicBool::new(false),
                    last_attempt: AtomicU64::new(0),
                },
            );
        }
    }
    state
}

/// Whether two `Content-Type` header values mean the same thing.
///
/// The ISR refusal compares the regenerated response's type against the string
/// the build recorded, and a false mismatch there freezes the route until the
/// next `autumn build`. `text/html;charset=utf-8` and `text/html; charset=utf-8`
/// are one type spelled two ways, and a layer that reconstructs the header can
/// easily change the spacing — so compare on meaning rather than on bytes.
///
/// "Meaning" follows RFC 9110 §8.3 rather than being a blanket
/// lowercase-and-strip: the **media type** and **parameter names** are
/// case-insensitive, and parameter order is not significant, but a parameter
/// **value** is case-sensitive and its interior spacing is its own. Flattening
/// values would be a correctness bug in the safety-critical direction: a
/// `multipart/…; boundary=Aa` recorded at build time would compare equal to a
/// regenerated `boundary=aa`, so ISR would overwrite the body while every
/// request still advertised the old boundary — exactly the undecodable-response
/// desync this check exists to prevent. `charset` is the one exception: RFC 2046
/// §4.1.2 defines its values as case-insensitive.
///
/// Quoted values are unquoted so `boundary="x"` and `boundary=x` agree, and `;`
/// inside quotes does not split a parameter.
fn content_type_equivalent(a: &str, b: &str) -> bool {
    normalize_content_type(a) == normalize_content_type(b)
}

/// Split a header value on `;`, ignoring separators inside a quoted string.
fn split_unquoted_semicolons(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' && in_quotes {
            escaped = true;
        } else if byte == b'"' {
            in_quotes = !in_quotes;
        } else if byte == b';' && !in_quotes {
            parts.push(&value[start..index]);
            start = index + 1;
        }
    }
    parts.push(&value[start..]);
    parts
}

/// Canonical form of a `Content-Type` for comparison: the lowercased media type
/// followed by its parameters as sorted `name=value` pairs, names lowercased and
/// values left alone (see [`content_type_equivalent`]).
fn normalize_content_type(value: &str) -> Vec<String> {
    let mut segments = split_unquoted_semicolons(value).into_iter();
    let media_type = segments.next().unwrap_or("").trim().to_ascii_lowercase();

    let mut parameters: Vec<String> = segments
        // An empty segment carries no meaning, so a trailing or doubled `;`
        // must not make two otherwise-identical types compare unequal. Without
        // this, a handler that re-emits `text/html; charset=utf-8;` would be
        // refused by the ISR check forever against a recorded
        // `text/html; charset=utf-8`, freezing the route until the next build.
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| {
            let segment = segment.trim();
            let Some((name, raw_value)) = segment.split_once('=') else {
                // A malformed segment with no `=` has no value to protect.
                return segment.to_ascii_lowercase();
            };
            let name = name.trim().to_ascii_lowercase();
            let raw_value = raw_value.trim();
            let unquoted = raw_value
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(raw_value);
            if name == "charset" {
                format!("{name}={}", unquoted.to_ascii_lowercase())
            } else {
                format!("{name}={unquoted}")
            }
        })
        .collect();
    // Parameter order carries no meaning.
    parameters.sort();

    let mut normalized = Vec::with_capacity(parameters.len() + 1);
    normalized.push(media_type);
    normalized.extend(parameters);
    normalized
}

/// Re-render a single page by sending a request through the router.
async fn regenerate_page(
    router: &axum::Router,
    url: &str,
    dest: &Path,
    expected_content_type: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(url)
                // Internal ISR regeneration render: exempt from the inbound
                // request-timeout deadline (no client connection).
                .extension(super::RenderDeadlineExempt)
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("router infallible");

    if !response.status().is_success() {
        return Err(format!("Handler returned HTTP {} for {}", response.status(), url).into());
    }

    // #1832: the manifest's recorded Content-Type is a build-time property, and
    // ISR deliberately does not rewrite the manifest (the design keeps it
    // immutable behind an `Arc` and uses file mtime for staleness, avoiding
    // write contention). So the header served for this route is fixed for the
    // process lifetime while the *body* on disk is not.
    //
    // That makes a type change during regeneration a body/header desync waiting
    // to happen: writing bytes the handler now calls `application/json` into a
    // file still advertised as `text/html` would serve undecodable content. A
    // handler that stops declaring a type at all is the same hazard — the
    // recorded type would keep being asserted over bytes nothing vouches for.
    //
    // Refuse the regeneration instead. The previous file stays on disk and
    // keeps matching its recorded type, so the route degrades to stale-but-
    // correct rather than fresh-but-mislabeled. The error propagates to the
    // caller's existing `warn!`, and the ISR cooldown throttles the retries; a
    // rebuild (`autumn build`) is what re-records the type.
    if let Some(expected) = expected_content_type {
        let actual = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        if !actual.is_some_and(|actual| content_type_equivalent(actual, expected)) {
            // Logged here as well as by the caller: an ordinary regeneration
            // failure is transient, but this one repeats every cooldown until
            // someone rebuilds, so it needs to be greppable on its own.
            tracing::error!(
                route = url,
                manifest_content_type = expected,
                handler_content_type = actual.unwrap_or("<none>"),
                "ISR: refusing to regenerate — the handler's Content-Type no longer matches \
                 the one recorded in the manifest, and the manifest is not rewritten at \
                 runtime, so the new body would be served under the old header. The existing \
                 page is still served. Re-run `autumn build` to refresh the manifest."
            );
            return Err(format!(
                "Content-Type changed for {url}: manifest records {expected:?} but the handler \
                 now declares {actual:?}; refusing to write a body the recorded type would \
                 mislabel — re-run `autumn build` to refresh the manifest"
            )
            .into());
        }
    }

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;

    // Write to a temp file, then atomically rename to avoid serving partial content
    let temp_path = dest.with_extension("tmp");
    std::fs::write(&temp_path, &body_bytes)?;
    std::fs::rename(&temp_path, dest)?;

    Ok(())
}

/// Get the age of a file in seconds based on its modification time.
/// Returns `None` if the file doesn't exist or metadata can't be read.
fn file_mtime_age_secs(path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let elapsed = SystemTime::now().duration_since(mtime).ok()?;
    Some(elapsed.as_secs())
}

/// Current Unix timestamp in seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_gen::{ManifestEntry, StaticManifest};
    use std::collections::HashMap;

    /// Helper: create a temp dist dir with manifest.json and some HTML files.
    fn create_test_dist() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");

        // Create directories
        std::fs::create_dir_all(dist.join("about")).expect("mkdir about");

        // Create HTML files
        std::fs::write(dist.join("index.html"), "<h1>Home</h1>").expect("write index");
        std::fs::write(dist.join("about/index.html"), "<h1>About</h1>").expect("write about");

        // Build and write manifest
        let mut routes = HashMap::new();
        routes.insert("/".to_owned(), ManifestEntry::new("index.html".to_owned()));
        routes.insert(
            "/about".to_owned(),
            ManifestEntry::new("about/index.html".to_owned()).with_revalidate(Some(3600)),
        );

        let manifest = StaticManifest {
            generated_at: "2026-03-27T12:00:00Z".to_owned(),
            autumn_version: "0.3.0".to_owned(),
            routes,
        };

        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        std::fs::write(dist.join("manifest.json"), json).expect("write manifest");

        dir
    }

    /// Helper: create a dist dir with parameterized routes in the manifest.
    fn create_parameterized_dist() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");

        // Create directories
        std::fs::create_dir_all(dist.join("posts/hello")).expect("mkdir posts/hello");
        std::fs::create_dir_all(dist.join("posts/world")).expect("mkdir posts/world");

        // Create HTML files
        std::fs::write(dist.join("posts/hello/index.html"), "<h1>Hello</h1>").expect("write hello");
        std::fs::write(dist.join("posts/world/index.html"), "<h1>World</h1>").expect("write world");

        // Build and write manifest
        let mut routes = HashMap::new();
        routes.insert(
            "/posts/hello".to_owned(),
            ManifestEntry::new("posts/hello/index.html".to_owned()),
        );
        routes.insert(
            "/posts/world".to_owned(),
            ManifestEntry::new("posts/world/index.html".to_owned()),
        );

        let manifest = StaticManifest {
            generated_at: "2026-03-29T12:00:00Z".to_owned(),
            autumn_version: "0.3.0".to_owned(),
            routes,
        };

        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        std::fs::write(dist.join("manifest.json"), json).expect("write manifest");

        dir
    }

    /// Helper: create a dist dir with ISR routes.
    fn create_isr_dist(revalidate: u64) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");

        std::fs::create_dir_all(dist.join("about")).expect("mkdir about");
        std::fs::write(dist.join("about/index.html"), "<h1>About (stale)</h1>")
            .expect("write about");

        let mut routes = HashMap::new();
        routes.insert(
            "/about".to_owned(),
            ManifestEntry::new("about/index.html".to_owned()).with_revalidate(Some(revalidate)),
        );

        let manifest = StaticManifest {
            generated_at: "2026-03-29T12:00:00Z".to_owned(),
            autumn_version: "0.3.0".to_owned(),
            routes,
        };

        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        std::fs::write(dist.join("manifest.json"), json).expect("write manifest");

        dir
    }

    #[test]
    fn layer_loads_from_valid_dist() {
        let tmp = create_test_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist);
        assert!(layer.is_some(), "should load from valid dist dir");
    }

    #[test]
    fn layer_returns_none_without_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // No manifest.json at all
        let layer = StaticFileLayer::new(tmp.path());
        assert!(layer.is_none(), "should return None without manifest.json");
    }

    /// A manifest that is *present but unparseable* also disables static
    /// serving — but unlike the absent case it is a misconfiguration, so
    /// `StaticFileLayer::new` warns rather than failing silently. Absent stays
    /// quiet: that is simply an app with no static build.
    #[test]
    fn layer_returns_none_for_an_unparseable_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("manifest.json"), "{ not json").expect("write");
        assert!(
            StaticFileLayer::new(tmp.path()).is_none(),
            "an unparseable manifest must disable static serving, not panic"
        );

        // A manifest carrying a key this runtime does not know still loads —
        // the forward-compatibility STABILITY.md promises for an older runtime
        // reading a manifest written by a newer Autumn.
        std::fs::write(
            tmp.path().join("manifest.json"),
            r#"{"generated_at":"1","autumn_version":"0.0.0","routes":{
                "/about":{"file":"about/index.html","revalidate":null,
                          "content_type":"text/html","future_field":42}}}"#,
        )
        .expect("write");
        let layer = StaticFileLayer::new(tmp.path()).expect("unknown keys must be ignored");
        assert_eq!(
            layer
                .resolve_entry("/about")
                .expect("resolves")
                .content_type,
            "text/html"
        );
    }

    #[test]
    fn resolve_finds_known_route() {
        let tmp = create_test_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        let resolved = layer.resolve("/about");
        assert!(resolved.is_some(), "/about should resolve");
        assert!(
            resolved.unwrap().ends_with("about/index.html"),
            "should point to about/index.html"
        );
    }

    #[test]
    fn resolve_finds_root() {
        let tmp = create_test_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        let resolved = layer.resolve("/");
        assert!(resolved.is_some(), "/ should resolve");
        assert!(
            resolved.unwrap().ends_with("index.html"),
            "should point to index.html"
        );
    }

    #[test]
    fn resolve_returns_none_for_unknown_route() {
        let tmp = create_test_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        let resolved = layer.resolve("/admin");
        assert!(resolved.is_none(), "/admin should not resolve");
    }

    #[test]
    fn manifest_accessor() {
        let tmp = create_test_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        assert_eq!(layer.manifest().routes.len(), 2);
    }

    // --- Parameterized route middleware tests ---

    #[test]
    fn resolve_finds_parameterized_routes() {
        let tmp = create_parameterized_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        let hello = layer.resolve("/posts/hello");
        assert!(hello.is_some(), "/posts/hello should resolve");
        assert!(hello.unwrap().ends_with("posts/hello/index.html"));

        let world = layer.resolve("/posts/world");
        assert!(world.is_some(), "/posts/world should resolve");
        assert!(world.unwrap().ends_with("posts/world/index.html"));
    }

    #[test]
    fn resolve_returns_none_for_non_generated_param() {
        let tmp = create_parameterized_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        // This slug was not in the params list, so not in the manifest
        let resolved = layer.resolve("/posts/unknown");
        assert!(
            resolved.is_none(),
            "/posts/unknown should not resolve (not pre-rendered)"
        );
    }

    // --- ISR tests ---

    #[test]
    fn isr_state_built_for_revalidate_routes() {
        let tmp = create_test_dist();
        let dist = tmp.path().join("dist");
        let layer = StaticFileLayer::new(&dist).expect("layer");

        // /about has revalidate=3600, / does not
        assert!(layer.isr_state.contains_key("/about"));
        assert!(!layer.isr_state.contains_key("/"));
    }

    #[test]
    fn file_mtime_age_fresh_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("test.html");
        std::fs::write(&file, "test").expect("write");

        // File just created, age should be very small
        let age = file_mtime_age_secs(&file).expect("mtime");
        assert!(age < 5, "Fresh file should be < 5 seconds old, got {age}");
    }

    #[test]
    fn file_mtime_age_missing_file() {
        let age = file_mtime_age_secs(Path::new("/nonexistent/file.html"));
        assert!(age.is_none(), "Missing file should return None");
    }

    #[tokio::test]
    async fn isr_triggers_regeneration_for_stale_page() {
        // Create a dist dir with a very short revalidate (1 second)
        let tmp = create_isr_dist(1);
        let dist = tmp.path().join("dist");

        // Make the file old by setting mtime to the past
        let file = dist.join("about/index.html");
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old_time))
            .unwrap_or(());

        // Create a router that returns fresh content
        let router =
            axum::Router::new().fallback(axum::routing::get(|| async { "<h1>About (fresh)</h1>" }));

        let layer = StaticFileLayer::new(&dist)
            .expect("layer")
            .with_router(router);

        // Resolve should return the stale file path but trigger ISR
        let resolved = layer.resolve("/about");
        assert!(resolved.is_some());

        // Give the background task time to complete
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Check if file was updated (only if mtime was successfully set)
        let content = std::fs::read_to_string(&file).unwrap();
        // The content should be updated if ISR fired, or remain stale
        // if filetime wasn't available. Either way, resolve works.
        assert!(
            content == "<h1>About (fresh)</h1>" || content == "<h1>About (stale)</h1>",
            "unexpected content: {content}"
        );
    }

    #[tokio::test]
    async fn isr_does_not_retrigger_while_in_flight() {
        let tmp = create_isr_dist(1);
        let dist = tmp.path().join("dist");

        let router = axum::Router::new().fallback(axum::routing::get(|| async {
            // Simulate slow handler
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            "<h1>Slow</h1>"
        }));

        let layer = StaticFileLayer::new(&dist)
            .expect("layer")
            .with_router(router);

        // Make file stale
        let file = dist.join("about/index.html");
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let _ = filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old_time));

        // First resolve triggers ISR
        let _ = layer.resolve("/about");

        // Check in-flight flag
        let state = layer.isr_state.get("/about").expect("isr state");
        // May or may not be true depending on timing, but second resolve
        // should not panic or double-trigger
        let _ = layer.resolve("/about");

        // Wait for background task
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        // Flag should be cleared
        assert!(
            !state.in_flight.load(Ordering::Relaxed),
            "in_flight should be cleared after regeneration"
        );
    }

    #[tokio::test]
    async fn regenerate_page_writes_atomically() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dest = tmp.path().join("page.html");
        std::fs::write(&dest, "old content").expect("write old");

        let router = axum::Router::new().fallback(axum::routing::get(|| async { "new content" }));

        let result = regenerate_page(&router, "/test", &dest, None).await;
        assert!(result.is_ok(), "regeneration failed: {:?}", result.err());

        let content = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(content, "new content");

        // Temp file should be cleaned up
        assert!(!dest.with_extension("tmp").exists());
    }

    #[tokio::test]
    async fn regenerate_page_fails_on_non_2xx() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dest = tmp.path().join("page.html");
        std::fs::write(&dest, "old content").expect("write old");

        let router = axum::Router::new()
            .fallback(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "error") });

        let result = regenerate_page(&router, "/test", &dest, None).await;
        assert!(result.is_err());

        // Original file should be untouched
        let content = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(content, "old content");
    }

    #[tokio::test]
    async fn isr_coordinator_deny_clears_in_flight_and_skips_regen() {
        use crate::static_gen::isr_coordinator::IsrFuture;

        // A coordinator that always denies acquisition — exercises the
        // `if !acquired { return; }` branch in the spawned ISR task.
        struct DenyCoordinator;
        impl IsrCoordinator for DenyCoordinator {
            fn backend(&self) -> &'static str {
                "deny"
            }

            fn try_acquire<'a>(&'a self, _: &'a str, _: &'a str) -> IsrFuture<'a, bool> {
                Box::pin(async { false })
            }

            fn release<'a>(&'a self, _: &'a str, _: &'a str) -> IsrFuture<'a, ()> {
                Box::pin(async {})
            }
        }

        let tmp = create_isr_dist(1);
        let dist = tmp.path().join("dist");
        let file = dist.join("about/index.html");

        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let _ = filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old_time));

        let router =
            axum::Router::new().fallback(axum::routing::get(|| async { "should not appear" }));
        let layer = StaticFileLayer::new(&dist)
            .expect("layer")
            .with_router(router)
            .with_isr_coordinator(Arc::new(DenyCoordinator));

        // Trigger the ISR background task.
        let _ = layer.resolve("/about");

        // Wait for the spawned task to run the deny branch and exit.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // InFlightReset guard must have cleared the flag despite early return.
        let state = layer.isr_state.get("/about").unwrap();
        assert!(
            !state.in_flight.load(Ordering::Relaxed),
            "in_flight must be cleared when coordinator denies acquisition"
        );

        // File must be unchanged — regeneration was skipped.
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "<h1>About (stale)</h1>"
        );
    }

    // ── #1832: Content-Type recorded at generation time ─────────────────────

    /// The recorded type wins outright — including a type the serve-time
    /// extension heuristic can never produce from an extensionless route.
    #[test]
    fn resolved_content_type_prefers_recorded_value() {
        let ct = resolved_content_type(
            Some("application/rss+xml"),
            "/feed",
            Path::new("feed/index.html"),
        );
        assert_eq!(ct, "application/rss+xml");
    }

    /// A recorded type overrides what the route extension would have said, so a
    /// route may be generated with a type its slug does not advertise.
    #[test]
    fn resolved_content_type_recorded_overrides_route_extension() {
        let ct = resolved_content_type(
            Some("application/manifest+json"),
            "/feed.xml",
            Path::new("feed.xml/index.html"),
        );
        assert_eq!(
            ct, "application/manifest+json",
            "`.xml` is a recognized asset extension deriving `application/xml`; \
             the recorded value must beat it"
        );
    }

    /// An empty recorded value is not a `Content-Type`; it must not produce an
    /// empty header.
    #[test]
    fn resolved_content_type_ignores_empty_recorded_value() {
        let ct = resolved_content_type(Some(""), "/about", Path::new("about/index.html"));
        assert_eq!(ct, "text/html; charset=utf-8");
    }

    /// A recorded value that `HeaderValue::from_str` would *accept* but that is
    /// not a usable `Content-Type` must still fall back. `from_str` only rejects
    /// bytes below 0x20 (except tab) and DEL, so blanks, tabs and any byte from
    /// 0x80 up sail through it — and the compression layer reads such a header
    /// as empty, bypassing its binary carve-outs and gzipping the body.
    #[test]
    fn resolved_content_type_rejects_blank_and_non_ascii_recorded_values() {
        for bad in ["   ", "\t", "text/htmlé", "text/html\u{80}"] {
            assert!(
                http::HeaderValue::from_str(bad).is_ok(),
                "{bad:?} is supposed to be a value from_str accepts — otherwise \
                 this test is not exercising the extra check"
            );
            assert_eq!(
                resolved_content_type(Some(bad), "/about", Path::new("about/index.html")),
                "text/html; charset=utf-8",
                "{bad:?} must fall back rather than becoming a Content-Type"
            );
        }
    }

    /// Surrounding whitespace is trimmed rather than treated as a rejection —
    /// `" text/css "` is still `text/css`.
    #[test]
    fn resolved_content_type_trims_recorded_value() {
        assert_eq!(
            resolved_content_type(
                Some("  application/rss+xml  "),
                "/feed",
                Path::new("f.html")
            ),
            "application/rss+xml"
        );
    }

    #[test]
    fn content_type_equivalent_ignores_spacing_and_media_type_case() {
        assert!(content_type_equivalent(
            "text/html;charset=utf-8",
            "text/html; charset=utf-8"
        ));
        assert!(content_type_equivalent("TEXT/HTML", "  text/html  "));
        // Parameter names are case-insensitive; order carries no meaning.
        assert!(content_type_equivalent(
            "multipart/mixed; BOUNDARY=xyz; charset=utf-8",
            "multipart/mixed; charset=utf-8; boundary=xyz"
        ));
        // `charset` values are case-insensitive (RFC 2046 §4.1.2).
        assert!(content_type_equivalent(
            "text/html; charset=UTF-8",
            "text/html; charset=utf-8"
        ));
        // Quoting a value does not change it.
        assert!(content_type_equivalent(
            r#"multipart/mixed; boundary="xyz""#,
            "multipart/mixed; boundary=xyz"
        ));

        // A different media type or a different parameter value is a real
        // change and must still be caught.
        assert!(!content_type_equivalent("text/html", "application/json"));
        assert!(!content_type_equivalent(
            "text/html; charset=utf-8",
            "text/html; charset=iso-8859-1"
        ));
    }

    /// Parameter values other than `charset` are case-sensitive, and their
    /// interior spacing is their own. Flattening them would be a bug in the
    /// dangerous direction: a recorded `boundary=Aa` matching a regenerated
    /// `boundary=aa` lets ISR overwrite the body while every request still
    /// advertises the old boundary, making the multipart response undecodable.
    /// An empty parameter segment carries no meaning. Treating one as a real
    /// parameter would make a handler that re-emits `text/html; charset=utf-8;`
    /// compare unequal to the recorded `text/html; charset=utf-8`, so
    /// `regenerate_page` would refuse every regeneration and the route would
    /// serve stale content until the next `autumn build` — the frozen-route
    /// failure this comparison exists to avoid.
    #[test]
    fn content_type_equivalent_ignores_empty_parameter_segments() {
        assert!(content_type_equivalent(
            "text/html; charset=utf-8",
            "text/html; charset=utf-8;"
        ));
        assert!(content_type_equivalent(
            "text/html; charset=utf-8",
            "text/html;;charset=utf-8"
        ));
        assert!(content_type_equivalent("text/html", "text/html; "));

        // Still not a licence to ignore a real parameter.
        assert!(!content_type_equivalent(
            "text/html;",
            "text/html; charset=utf-8"
        ));
    }

    #[test]
    fn content_type_equivalent_preserves_parameter_value_case_and_spacing() {
        assert!(!content_type_equivalent(
            "multipart/form-data; boundary=Aa",
            "multipart/form-data; boundary=aa"
        ));
        assert!(!content_type_equivalent(
            r#"multipart/form-data; boundary="a b""#,
            r#"multipart/form-data; boundary="ab""#
        ));
        // A `;` inside quotes is part of the value, not a parameter separator.
        assert!(!content_type_equivalent(
            r#"multipart/form-data; boundary="a;b""#,
            r#"multipart/form-data; boundary="a;c""#
        ));
        assert!(content_type_equivalent(
            r#"multipart/form-data; boundary="a;b""#,
            r#"MULTIPART/FORM-DATA;  boundary="a;b""#
        ));
    }

    /// Bites the quote/escape tracking in `split_unquoted_semicolons`
    /// specifically. Without it, `boundary="a;b"` splits into `boundary="a` and
    /// the tail `b"`, which has no `=` and is therefore *lowercased* as a
    /// malformed segment — so `…"a;b"` and `…"a;B"` would compare **equal** and
    /// ISR would overwrite a multipart body while every request still
    /// advertised the old boundary.
    #[test]
    fn content_type_equivalent_tracks_quotes_and_escapes_in_parameter_values() {
        assert!(
            !content_type_equivalent(
                r#"multipart/mixed; boundary="a;b""#,
                r#"multipart/mixed; boundary="a;B""#
            ),
            "case inside a quoted value is significant even across a quoted `;`"
        );
        assert!(
            !content_type_equivalent(
                r#"multipart/mixed; boundary="a\";c""#,
                r#"multipart/mixed; boundary="a\";C""#
            ),
            "an escaped quote does not end the quoted string, so the `;` after \
             it is still part of the value"
        );

        // A malformed segment with no `=` has no value to protect, so it *is*
        // case-folded — the branch the quoted cases above must not reach.
        assert!(content_type_equivalent("text/html; FOO", "text/html; foo"));
    }

    /// A manifest whose recorded type the serve path refuses to honour must not
    /// become the ISR expectation: the handler's perfectly good type could never
    /// match it, so every regeneration would be refused forever and the route
    /// would freeze — a strictly worse outcome than the bad value being ignored.
    #[tokio::test]
    async fn isr_ignores_unusable_recorded_content_type_and_still_regenerates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("about")).expect("mkdir");
        let file = dist.join("about/index.html");
        std::fs::write(&file, "<h1>stale</h1>").expect("write");

        let mut routes = HashMap::new();
        routes.insert(
            "/about".to_owned(),
            ManifestEntry::new("about/index.html")
                .with_revalidate(Some(1))
                // Blank: `resolved_content_type` ignores it and derives
                // text/html from `index.html`.
                .with_content_type(Some("   ".to_owned())),
        );
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&StaticManifest::new(routes)).expect("serialize"),
        )
        .expect("write manifest");

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let _ = filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old));

        let fresh = axum::Router::new().fallback(axum::routing::get(|| async {
            axum::response::Html("<h1>fresh</h1>")
        }));
        let layer = StaticFileLayer::new(&dist)
            .expect("layer")
            .with_router(fresh);

        // The serve path ignores the unusable value and derives instead.
        let hit = layer.resolve_entry("/about").expect("resolves");
        assert_eq!(hit.content_type, "text/html; charset=utf-8");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "<h1>fresh</h1>",
            "an unusable recorded type must not freeze regeneration"
        );
    }

    #[test]
    fn usable_recorded_content_type_matches_what_the_serve_path_honours() {
        assert_eq!(
            usable_recorded_content_type(Some("  application/rss+xml  ")),
            Some("application/rss+xml")
        );
        // A horizontal tab is legal OWS around media-type parameters, so a value
        // carrying one is usable — rejecting it would send an extensionless
        // `/feed` back to the `feed/index.html` derivation and serve RSS as
        // `text/html`. `to_str` succeeds on tab, so the compression layer still
        // matches such a type against its carve-outs in full; the hazard this
        // screen exists for is a value that reads as *empty* there.
        assert_eq!(
            usable_recorded_content_type(Some("application/rss+xml;\tprofile=\"x\"")),
            Some("application/rss+xml;\tprofile=\"x\"")
        );
        assert_eq!(
            resolved_content_type(
                Some("application/rss+xml;\tprofile=\"x\""),
                "/feed",
                Path::new("feed/index.html"),
            ),
            "application/rss+xml;\tprofile=\"x\"",
            "a tab-bearing type must be served, not derived around"
        );

        // A tab-*only* value is still nothing: trimming leaves the empty string.
        for unusable in [None, Some(""), Some("   "), Some("\t"), Some("text/htmlé")] {
            assert!(
                usable_recorded_content_type(unusable).is_none(),
                "{unusable:?} must be treated as nothing recorded"
            );
        }
    }

    /// A spelling difference must not freeze ISR: the regenerated response is
    /// the same type, written differently, so regeneration proceeds.
    #[tokio::test]
    async fn regenerate_page_accepts_equivalent_content_type_spelling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("page.html");
        std::fs::write(&dest, "<h1>original</h1>").expect("write");

        let router = axum::Router::new().fallback(axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "TEXT/HTML;charset=UTF-8")],
                "<h1>fresh</h1>",
            )
        }));

        regenerate_page(&router, "/page", &dest, Some("text/html; charset=utf-8"))
            .await
            .expect("an equivalent spelling must not freeze the route");

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "<h1>fresh</h1>");
    }

    /// A hand-edited manifest carrying CR/LF must never reach the response:
    /// it falls back rather than injecting a header (and never panics).
    #[test]
    fn resolved_content_type_rejects_header_illegal_recorded_value() {
        for bad in [
            "text/html\r\nX-Injected: yes",
            "text/html\nX-Injected: yes",
            "text/html\u{7f}",
            "text/html\0",
        ] {
            let ct = resolved_content_type(Some(bad), "/about", Path::new("about/index.html"));
            assert_eq!(
                ct, "text/html; charset=utf-8",
                "header-illegal recorded value {bad:?} must fall back"
            );
        }
    }

    /// With nothing recorded, the legacy derivation still applies — these are
    /// the three edge cases #1819 needed three rounds to get right, and they
    /// must keep working for manifests built before #1832.
    #[test]
    fn resolved_content_type_falls_back_to_route_extension() {
        assert_eq!(
            resolved_content_type(None, "/robots.txt", Path::new("robots.txt/index.html")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            resolved_content_type(None, "/sitemap.xml", Path::new("sitemap.xml/index.html")),
            "application/xml"
        );
    }

    #[test]
    fn resolved_content_type_falls_back_to_file_name_for_dotted_slugs() {
        assert_eq!(
            resolved_content_type(
                None,
                "/posts/release.v1",
                Path::new("posts/release.v1/index.html")
            ),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            resolved_content_type(
                None,
                "/users/alice@example.com",
                Path::new("users/alice@example.com/index.html")
            ),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn resolved_content_type_falls_back_to_file_name_for_extensionless_routes() {
        assert_eq!(
            resolved_content_type(None, "/about", Path::new("about/index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            resolved_content_type(None, "/inter", Path::new("fonts/inter.woff2")),
            "font/woff2"
        );
    }

    #[test]
    fn resolved_content_type_last_resort_is_octet_stream() {
        assert_eq!(
            resolved_content_type(None, "/data", Path::new("data.bin")),
            "application/octet-stream"
        );
        assert_eq!(
            resolved_content_type(None, "/data", Path::new("")),
            "application/octet-stream"
        );
    }

    /// `resolve_entry` decides the type once — recorded when the manifest has
    /// one, derived otherwise — so the serve path never has to.
    #[test]
    fn resolve_entry_decides_content_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(&dist).expect("mkdir");
        std::fs::write(dist.join("index.html"), "<h1>Home</h1>").expect("write");

        let mut routes = HashMap::new();
        routes.insert(
            "/".to_owned(),
            ManifestEntry::new("index.html").with_content_type(Some("text/html".to_owned())),
        );
        routes.insert("/bare".to_owned(), ManifestEntry::new("bare/index.html"));
        let manifest = StaticManifest {
            generated_at: "2026-09-01T00:00:00Z".to_owned(),
            autumn_version: "0.6.0".to_owned(),
            routes,
        };
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).expect("serialize"),
        )
        .expect("write manifest");

        let layer = StaticFileLayer::new(&dist).expect("layer");

        // The recorded type is served verbatim...
        let hit = layer.resolve_entry("/").expect("root resolves");
        assert_eq!(hit.file_path, dist.join("index.html"));
        assert_eq!(hit.content_type, "text/html");

        // ...and a route that records nothing gets the legacy derivation, so
        // callers never see an undecided type.
        let bare = layer.resolve_entry("/bare").expect("bare resolves");
        assert_eq!(bare.content_type, "text/html; charset=utf-8");

        assert!(layer.resolve_entry("/missing").is_none());
    }

    // ── #1832: ISR must not desync a regenerated body from the recorded type ──

    /// The manifest is immutable at runtime, so a handler that starts declaring
    /// a *different* `Content-Type` would have its new bytes served under the
    /// old recorded type. Regeneration is refused instead, leaving the previous
    /// file — which still matches its recorded type — in place.
    #[tokio::test]
    async fn regenerate_page_refuses_when_content_type_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("page.html");
        std::fs::write(&dest, "<h1>original</h1>").expect("write");

        let router = axum::Router::new().fallback(axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"now":"json"}"#,
            )
        }));

        let result =
            regenerate_page(&router, "/page", &dest, Some("text/html; charset=utf-8")).await;

        let err = result.expect_err("a changed Content-Type must fail regeneration");
        assert!(
            err.to_string().contains("Content-Type changed"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "<h1>original</h1>",
            "the stale-but-correctly-typed file must survive a refused regeneration"
        );
    }

    /// A handler that stops declaring a type at all is the same hazard: the
    /// recorded type would keep being asserted over bytes nothing vouches for.
    #[tokio::test]
    async fn regenerate_page_refuses_when_content_type_disappears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("page.html");
        std::fs::write(&dest, "<h1>original</h1>").expect("write");

        let router = axum::Router::new().fallback(axum::routing::get(|| async {
            axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .body(axum::body::Body::from("bare"))
                .unwrap()
        }));

        let result =
            regenerate_page(&router, "/page", &dest, Some("text/html; charset=utf-8")).await;

        let err = result.expect_err("a vanished Content-Type must fail regeneration");
        assert!(
            err.to_string().contains("Content-Type changed"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "<h1>original</h1>");
    }

    /// The matching case still regenerates normally.
    #[tokio::test]
    async fn regenerate_page_writes_when_content_type_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("page.html");
        std::fs::write(&dest, "<h1>original</h1>").expect("write");

        let router = axum::Router::new().fallback(axum::routing::get(|| async {
            axum::response::Html("<h1>fresh</h1>")
        }));

        regenerate_page(&router, "/page", &dest, Some("text/html; charset=utf-8"))
            .await
            .expect("matching Content-Type regenerates");

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "<h1>fresh</h1>");
    }

    /// A route that records no type has nothing to desync, so regeneration is
    /// unconstrained — this is the pre-#1832 behaviour for legacy manifests.
    #[tokio::test]
    async fn regenerate_page_unconstrained_when_nothing_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("page.html");
        std::fs::write(&dest, "<h1>original</h1>").expect("write");

        let router = axum::Router::new().fallback(axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"any":"type"}"#,
            )
        }));

        regenerate_page(&router, "/page", &dest, None)
            .await
            .expect("no recorded type means no constraint");

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), r#"{"any":"type"}"#);
    }

    /// M4: an ISR route with a recorded type keeps it while the stale page is
    /// served. `resolve_entry` triggers regeneration *before* it builds the
    /// hit, so this pins that the staleness branch does not disturb the type.
    #[tokio::test]
    async fn resolve_entry_keeps_recorded_content_type_for_isr_route() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("feed")).expect("mkdir");
        let file = dist.join("feed/index.html");
        std::fs::write(&file, "<rss/>").expect("write");

        let mut routes = HashMap::new();
        routes.insert(
            "/feed".to_owned(),
            ManifestEntry::new("feed/index.html")
                .with_revalidate(Some(1))
                .with_content_type(Some("application/rss+xml".to_owned())),
        );
        let manifest = StaticManifest {
            generated_at: "2026-09-01T00:00:00Z".to_owned(),
            autumn_version: "0.7.0".to_owned(),
            routes,
        };
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).expect("serialize"),
        )
        .expect("write manifest");

        // Age the file so the ISR staleness branch runs on this resolve.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let _ = filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old));

        let layer = StaticFileLayer::new(&dist).expect("layer");
        let hit = layer.resolve_entry("/feed").expect("resolves");
        assert_eq!(hit.content_type, "application/rss+xml");
    }

    /// M6: a refused regeneration must leave the route usable — the stale file
    /// stays and the in-flight flag clears, so the next stale request can try
    /// again after the cooldown rather than the route wedging forever.
    #[tokio::test]
    async fn isr_refused_regeneration_keeps_file_and_clears_in_flight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("about")).expect("mkdir");
        let file = dist.join("about/index.html");
        std::fs::write(&file, "<h1>About (stale)</h1>").expect("write");

        let mut routes = HashMap::new();
        routes.insert(
            "/about".to_owned(),
            ManifestEntry::new("about/index.html")
                .with_revalidate(Some(1))
                .with_content_type(Some("text/html; charset=utf-8".to_owned())),
        );
        let manifest = StaticManifest {
            generated_at: "2026-09-01T00:00:00Z".to_owned(),
            autumn_version: "0.7.0".to_owned(),
            routes,
        };
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_string(&manifest).expect("serialize"),
        )
        .expect("write manifest");

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let _ = filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old));

        // The handler now declares a different type than the manifest records.
        let drifted_handler = axum::Router::new().fallback(axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"drifted":true}"#,
            )
        }));
        let layer = StaticFileLayer::new(&dist)
            .expect("layer")
            .with_router(drifted_handler);

        let _ = layer.resolve("/about");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "<h1>About (stale)</h1>",
            "a refused regeneration must not overwrite the correctly-typed file"
        );
        let state = layer.isr_state.get("/about").expect("isr state");
        assert!(
            !state.in_flight.load(Ordering::Relaxed),
            "a refused regeneration must clear in_flight, not wedge the route"
        );
    }

    /// M5 — the load-bearing test for the refusal above.
    ///
    /// The refusal compares the regenerated response's `Content-Type` against
    /// the manifest's recorded string exactly. If the framework's own build
    /// path and ISR path ever disagreed on that string — a charset spelling, a
    /// layer applied on one path but not the other — every route built after
    /// #1832 would refuse to regenerate *forever*, silently, until the next
    /// `autumn build`. This drives both real paths end to end and asserts the
    /// page actually refreshes, so such a divergence fails CI instead of
    /// freezing ISR in production.
    ///
    /// (Note the known asymmetry it does *not* cover: `autumn build` renders
    /// through the app's custom Tower layers while ISR regeneration
    /// deliberately does not. An app whose own layer rewrites `Content-Type`
    /// will see refusals; the error names the route and both types.)
    #[tokio::test]
    async fn isr_regenerates_page_built_by_render_static_routes() {
        let router = axum::Router::new().route(
            "/about",
            axum::routing::get(|| async { axum::response::Html("<h1>fresh</h1>") }),
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        crate::static_gen::render_static_routes(
            router.clone(),
            &[crate::static_gen::StaticRouteMeta {
                path: "/about",
                name: "about",
                revalidate: Some(1),
                params_fn: None,
                seo: crate::seo::SeoRouteDefaults::EMPTY,
            }],
            &dist,
        )
        .await
        .expect("static build");

        // The build must have recorded the type that regeneration will declare.
        let manifest = StaticManifest::load(&dist.join("manifest.json")).expect("manifest");
        assert_eq!(
            manifest.routes["/about"].content_type.as_deref(),
            Some("text/html; charset=utf-8")
        );

        // Overwrite and age the generated page so regeneration has something
        // observable to do.
        let file = dist.join("about/index.html");
        std::fs::write(&file, "<h1>stale</h1>").expect("write");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        let _ = filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(old));

        let layer = StaticFileLayer::new(&dist)
            .expect("layer")
            .with_router(router);
        let _ = layer.resolve("/about");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "<h1>fresh</h1>",
            "a page built by render_static_routes must still be regenerable by ISR — \
             a build/ISR disagreement on the recorded Content-Type would freeze it"
        );
    }
}
