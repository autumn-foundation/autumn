//! Mounting a sandboxed artifact as an ordinary Autumn [`Plugin`].
//!
//! A [`SandboxedPlugin`] is installed exactly like a native one:
//!
//! ```rust,no_run
//! # use std::path::Path;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let plugin = autumn_web::plugin_sandbox::SandboxedPlugin::from_file(
//!     Path::new("plugins/hello.autumn-plugin"),
//! )?;
//! let app = autumn_web::app().plugin(plugin);
//! # let _ = app;
//! # Ok(())
//! # }
//! ```
//!
//! …and from the application's point of view that is where the resemblance
//! ends. A native plugin's `build` receives the whole `AppBuilder`. This one
//! receives nothing: the mount is performed by the framework, from the
//! manifest, and consists of exactly one `nest` at the declared prefix plus the
//! declared route metadata.
//!
//! # The manifest is the mount
//!
//! The router is built from the manifest's `[[routes]]`, one axum route per
//! declared `(method, path)`. A request to an undeclared path under the prefix
//! is a 404 the guest never sees, and a request to a path *outside* the prefix
//! never reaches this router at all. So "which routes it mounts" is a property
//! the runtime enforces, not a claim the artifact makes.
//!
//! # What a misbehaving plugin costs
//!
//! | Its behaviour | What the caller gets | What the rest of the app gets |
//! | --- | --- | --- |
//! | answers | its answer, minus any header it may not set | nothing |
//! | traps, exits, answers badly | 502 on its own prefix | nothing |
//! | spins or floods output | 504 on its own prefix | nothing |
//! | is already at its concurrency ceiling | 503 with `Retry-After` | nothing |
//! | is sent a body over its ceiling | 413, guest never started | nothing |
//!
//! The last column is the point. Every failure mode is scoped to the plugin's
//! own prefix, because the interpreter runs on a blocking worker with a bounded
//! permit count and can only ever return a value.
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::borrow::Cow;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse as _, Response};
use axum::routing::MethodFilter;
use http::{HeaderName, HeaderValue, StatusCode};
use tokio::sync::Semaphore;

use super::artifact::{ArtifactError, SandboxArtifact};
use super::capability::{CapabilityRateLimiter, CapabilityServices, PluginActivityLog};
use super::host::{SandboxHost, SandboxLoadError};
use super::manifest::SandboxManifest;
use super::wire::SandboxRequest;
use crate::app::AppBuilder;
use crate::plugin::Plugin;

/// Response header naming the sandboxed plugin that produced a response.
///
/// Sandboxed responses are otherwise indistinguishable from the host's own, and
/// an operator reading a trace should never have to guess which side of the
/// boundary a byte came from.
pub const SANDBOX_ATTRIBUTION_HEADER: &str = "x-autumn-sandboxed";

/// Why a sandboxed plugin could not be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxPluginError {
    /// The artifact could not be read or verified.
    Artifact(ArtifactError),
    /// The module could not be loaded into the sandbox.
    Load(SandboxLoadError),
}

impl fmt::Display for SandboxPluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact(err) => write!(f, "{err}"),
            Self::Load(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SandboxPluginError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Artifact(err) => Some(err),
            Self::Load(err) => Some(err),
        }
    }
}

impl From<ArtifactError> for SandboxPluginError {
    fn from(err: ArtifactError) -> Self {
        Self::Artifact(err)
    }
}

impl From<SandboxLoadError> for SandboxPluginError {
    fn from(err: SandboxLoadError) -> Self {
        Self::Load(err)
    }
}

/// The most context entries one render hook is handed.
///
/// A slot's context is a handful of values a panel needs — an id, a locale, a
/// count. Far above that, and far below anything that could matter.
pub const MAX_RENDER_CONTEXT_ENTRIES: usize = 32;

/// The most bytes one render hook's context may carry across all its entries.
pub const MAX_RENDER_CONTEXT_BYTES: usize = 16 * 1024;

/// Take as much of `context` as the ceilings above allow.
///
/// Per-entry overhead is counted, not just the strings: a thousand empty pairs
/// cost real allocation and real serialization, and a budget measured only in
/// string length prices them at nothing.
fn bounded_context(context: &[(String, String)]) -> Vec<(String, String)> {
    /// What one entry costs beyond its bytes: two `String` headers, a tuple, and
    /// the JSON punctuation it becomes on the wire.
    const PER_ENTRY: usize = 64;

    let mut out = Vec::with_capacity(context.len().min(MAX_RENDER_CONTEXT_ENTRIES));
    let mut total = 0_usize;
    for (name, value) in context {
        if out.len() >= MAX_RENDER_CONTEXT_ENTRIES {
            break;
        }
        let weight = name
            .len()
            .saturating_add(value.len())
            .saturating_add(PER_ENTRY);
        if total.saturating_add(weight) > MAX_RENDER_CONTEXT_BYTES {
            break;
        }
        total = total.saturating_add(weight);
        out.push((name.clone(), value.clone()));
    }
    out
}

/// A sandboxed plugin, ready to mount.
///
/// `Clone` shares the compiled host, the concurrency permits, the capability
/// backends and the activity log rather than copying them — so registering a
/// clone with [`RenderSlots`](super::slots::RenderSlots) and then installing the
/// original as a [`Plugin`] gives one plugin with one ceiling and one ledger,
/// not two of each.
#[derive(Clone)]
pub struct SandboxedPlugin {
    host: Arc<SandboxHost>,
    /// One permit per concurrently-executing request, so `max_concurrency ×
    /// memory_bytes` bounds what this plugin can cost the host at any instant.
    permits: Arc<Semaphore>,
    /// The identity an operator reviewed, logged at mount so the grant in the
    /// log can be compared with the one on the review screen. `None` when the
    /// plugin was built from a bare host rather than an artifact — there is no
    /// container to have an identity, and saying so beats inventing one.
    artifact_sha256: Option<String>,
    /// The capability backends, minus the tenant (issue #1632).
    ///
    /// Held without a tenant and bound to one per request, because one compiled
    /// plugin serves every tenant: a `CapabilityServices` stored here with a
    /// tenant in it would be one tenant's, for everybody.
    services: CapabilityServices,
    /// What this plugin has been doing, for the operator audit surface.
    activity: Arc<PluginActivityLog>,
}

impl fmt::Debug for SandboxedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxedPlugin")
            .field("plugin", &self.host.manifest().name)
            .field("prefix", &self.host.manifest().prefix)
            .finish_non_exhaustive()
    }
}

impl SandboxedPlugin {
    /// Wrap an already-compiled host.
    #[must_use]
    pub fn new(host: SandboxHost) -> Self {
        let permits = Arc::new(Semaphore::new(host.manifest().limits.max_concurrency));
        // The rate limiter is built here rather than by the caller, and from
        // the manifest rather than from a parameter, so a plugin that names a
        // `calls_per_second` on its consent screen is limited by it whether or
        // not whoever mounted it remembered to say so.
        let services = CapabilityServices {
            rate: Some(Arc::new(CapabilityRateLimiter::new(
                host.manifest().quotas.calls_per_second,
            ))),
            ..CapabilityServices::none()
        };
        Self {
            host: Arc::new(host),
            permits,
            artifact_sha256: None,
            services,
            activity: Arc::new(PluginActivityLog::new()),
        }
    }

    /// Wire the backends this plugin's granted capabilities need (issue #1632).
    ///
    /// Anything left unwired stays unwired: a call to a capability with no
    /// backend is answered
    /// [`unavailable`](crate::plugin_sandbox::DenialReason::Unavailable) and
    /// recorded, which is the same shape as every other refusal and not a
    /// silent success.
    ///
    /// `services.tenant` is ignored — the tenant is the *request's*, resolved
    /// per request — and `services.rate` replaces the limiter built from the
    /// manifest, for an embedder that wants one shared across a fleet.
    #[must_use]
    pub fn with_services(mut self, services: CapabilityServices) -> Self {
        let rate = services.rate.clone().or_else(|| self.services.rate.clone());
        self.services = CapabilityServices {
            tenant: None,
            rate,
            ..services
        };
        self
    }

    /// What this plugin has done, for the operator audit surface.
    ///
    /// Shared with every mounted route, so a caller that keeps this handle sees
    /// activity as it happens rather than a copy taken at mount.
    #[must_use]
    pub fn activity(&self) -> Arc<PluginActivityLog> {
        Arc::clone(&self.activity)
    }

    /// Load a verified artifact.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxPluginError::Load`] if the module does not compile or
    /// imports something the sandbox does not provide.
    pub fn from_artifact(artifact: &SandboxArtifact) -> Result<Self, SandboxPluginError> {
        let mut plugin = Self::new(SandboxHost::load(artifact)?);
        // The whole container, not just the module: the grant an operator
        // reviewed is in the manifest, and the module digest does not move when
        // that does. See `SandboxArtifact::artifact_digest`.
        plugin.artifact_sha256 = Some(artifact.artifact_digest()?);
        Ok(plugin)
    }

    /// Read, verify and load a `.autumn-plugin` file.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxPluginError::Artifact`] if the file is missing,
    /// malformed, or does not match its declared digest, and
    /// [`SandboxPluginError::Load`] if the module cannot be sandboxed.
    pub fn from_file(path: &Path) -> Result<Self, SandboxPluginError> {
        Self::from_artifact(&SandboxArtifact::read_file(path)?)
    }

    /// The manifest this plugin is mounted under.
    #[must_use]
    pub fn manifest(&self) -> &SandboxManifest {
        self.host.manifest()
    }

    /// Fill one render slot (issue #1632).
    ///
    /// Returns `None` — meaning *omit the fragment* — for every failure there
    /// is: the slot was not granted, the guest trapped, it ran out of fuel, it
    /// answered with something this build will not emit, or it overran the
    /// `render_bytes` quota. A render hook is decoration on somebody else's
    /// page, and there is no failure of a plugin's that should become a failure
    /// of the page.
    ///
    /// Runs on a blocking worker for the same reason a request does, and holds
    /// a concurrency permit for the same reason: a slot on a hot page is as
    /// many concurrent interpreters as the page has readers.
    pub async fn render_slot(&self, slot: &str, context: &[(String, String)]) -> Option<String> {
        let plugin = self.manifest().name.clone();
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            tracing::warn!(
                plugin,
                slot,
                "sandboxed plugin is at its concurrency ceiling; omitting its render fragment"
            );
            return None;
        };
        let services = CapabilityServices {
            // Read here rather than in the closure; see the note in `serve`.
            tenant: crate::tenancy::CURRENT_TENANT
                .try_with(Clone::clone)
                .ok()
                .flatten(),
            ..self.services.clone()
        };
        let host = Arc::clone(&self.host);
        let slot_owned = slot.to_owned();
        let context = context.to_vec();
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = host.render(&slot_owned, &context, services);
            drop(permit);
            outcome
        })
        .await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::error!(
                    plugin,
                    slot,
                    error = %err,
                    "the sandbox render worker did not complete; omitting the fragment"
                );
                return None;
            }
        };
        self.activity.ingest(&plugin, outcome.activity);
        self.activity
            .ingest_dropped(&plugin, outcome.dropped_events);
        match outcome.fragment {
            Ok(fragment) => Some(fragment),
            Err(failure) => {
                tracing::warn!(
                    plugin,
                    slot,
                    failure = %failure,
                    stderr = outcome.stderr,
                    fuel_used = outcome.fuel_used,
                    "sandboxed plugin did not produce a render fragment; omitting it"
                );
                None
            }
        }
    }

    /// This plugin, mounted at the prefix its own manifest declares.
    ///
    /// The public door, for a test or an embedder that wants the routes without
    /// an [`AppBuilder`]. It nests at `self.manifest().prefix` and nowhere else:
    /// a caller that could choose the prefix could mount a plugin over a
    /// namespace its manifest never named, and the prefix would stop being a
    /// containment boundary.
    ///
    /// Generic over the router's state because the handlers use none — a
    /// sandboxed plugin has no access to application state, which is the point.
    ///
    /// Prefer installing it as a [`Plugin`], which also logs the capability
    /// grant and declares the routes for `autumn routes` and
    /// `autumn plugin-check`.
    pub fn mounted_router<S>(&self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        axum::Router::new().nest(&self.manifest().prefix.clone(), self.router())
    }

    /// The router for this plugin's declared routes, with paths **relative to
    /// the declared prefix** — the shape [`AppBuilder::nest`] expects.
    fn router<S>(&self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        let manifest = self.manifest();
        let mut router = axum::Router::new();
        for route in &manifest.routes {
            let nested = nested_path(&route.path, &manifest.prefix);
            // A declared HEAD on the same path mounts itself; adding the
            // implied one here as well would be an overlapping method route,
            // which axum refuses by panicking as the router is built.
            let implies_head = route.method == "GET"
                && !manifest
                    .routes
                    .iter()
                    .any(|other| other.method == "HEAD" && other.path == route.path);
            let Some(filter) = method_filter(&route.method, implies_head) else {
                // `SandboxManifest` validation already refused every method
                // outside the allowed set, so this is unreachable in practice.
                // Skipping rather than panicking keeps a future manifest
                // vocabulary from turning into a boot crash.
                tracing::error!(
                    plugin = manifest.name,
                    method = route.method,
                    "sandboxed plugin declared a method this build cannot mount; skipping it"
                );
                continue;
            };
            let host = Arc::clone(&self.host);
            let permits = Arc::clone(&self.permits);
            let services = self.services.clone();
            let activity = Arc::clone(&self.activity);
            let pattern = route.path.clone();
            router = router.route(
                &nested,
                axum::routing::on(
                    filter,
                    move |params: axum::extract::RawPathParams, request: axum::extract::Request| {
                        let mounted = Mounted {
                            host: Arc::clone(&host),
                            permits: Arc::clone(&permits),
                            services: services.clone(),
                            activity: Arc::clone(&activity),
                        };
                        let pattern = pattern.clone();
                        async move { serve(mounted, pattern, params, request).await }
                    },
                ),
            );
        }
        router
    }
}

impl Plugin for SandboxedPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.manifest().name.clone())
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let manifest = self.manifest().clone();
        // The grant reaches the operator's log before the plugin serves its
        // first request, so "what did we agree to run" is answerable from a
        // production log alone, without the artifact in hand.
        tracing::info!(
            plugin = manifest.name,
            version = manifest.version,
            prefix = manifest.prefix,
            module_sha256 = manifest.sha256,
            // The number to compare against the one `plugin inspect` printed:
            // it covers the manifest too, so a grant rewritten after review
            // does not match a log line from before it.
            artifact_sha256 = self
                .artifact_sha256
                .as_deref()
                .unwrap_or("(built from a host, not an artifact)"),
            capabilities = manifest
                .capabilities
                .iter()
                .map(|capability| capability.as_str())
                .collect::<Vec<_>>()
                .join(","),
            routes = manifest.routes.len(),
            fuel = manifest.limits.fuel,
            memory_bytes = manifest.limits.memory_bytes,
            max_concurrency = manifest.limits.max_concurrency,
            "mounting a sandboxed plugin under its declared capability grant"
        );
        app.nest(&manifest.prefix, self.router())
            .declare_plugin_routes(manifest.route_infos())
    }
}

/// The path a declared route takes inside a router nested at `prefix`.
///
/// A route *at* the prefix nests as `/`, which is how axum spells "the mount
/// point itself".
fn nested_path(path: &str, prefix: &str) -> String {
    match path.strip_prefix(prefix) {
        Some("") | None => "/".to_owned(),
        Some(rest) => rest.to_owned(),
    }
}

/// The filter a declared method mounts under.
///
/// `GET` mounts as `GET | HEAD` when the manifest does not declare a HEAD of
/// its own. HTTP defines HEAD as GET without a body, and axum's method router
/// already dispatches a HEAD with no HEAD route to the GET one — so the
/// alternative to naming it here is not "HEAD is refused", it is "HEAD is
/// served by an accident the manifest never mentions". When the manifest *does*
/// declare HEAD, that route mounts itself and the implication must be dropped:
/// two overlapping method routes on one path is a panic as the router builds.
/// [`SandboxManifest::route_infos`](crate::plugin_sandbox::SandboxManifest::route_infos)
/// reports the implied HEAD for the same reason.
fn method_filter(method: &str, implies_head: bool) -> Option<MethodFilter> {
    match method {
        "GET" if implies_head => Some(MethodFilter::GET.or(MethodFilter::HEAD)),
        "GET" => Some(MethodFilter::GET),
        "HEAD" => Some(MethodFilter::HEAD),
        "POST" => Some(MethodFilter::POST),
        "PUT" => Some(MethodFilter::PUT),
        "PATCH" => Some(MethodFilter::PATCH),
        "DELETE" => Some(MethodFilter::DELETE),
        "OPTIONS" => Some(MethodFilter::OPTIONS),
        _ => None,
    }
}

/// Serve one request through the sandbox.
/// Everything one mounted route needs to serve a request.
///
/// Gathered into a struct because the four move together into every route
/// closure and then into `serve`, and a five-argument call whose first four are
/// always the same four is a place for two of them to be swapped.
struct Mounted {
    host: Arc<SandboxHost>,
    permits: Arc<Semaphore>,
    services: CapabilityServices,
    activity: Arc<PluginActivityLog>,
}

async fn serve(
    mounted: Mounted,
    pattern: String,
    params: axum::extract::RawPathParams,
    request: axum::extract::Request,
) -> Response {
    let Mounted {
        host,
        permits,
        services,
        activity,
    } = mounted;
    let manifest = host.manifest();
    let limits = manifest.limits;
    let plugin = manifest.name.clone();

    // Refuse rather than queue. An unbounded queue in front of a bounded
    // interpreter converts a slow plugin into unbounded host memory; a 503 with
    // `Retry-After` says exactly what happened and costs nothing to produce.
    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
        tracing::warn!(
            plugin,
            max_concurrency = limits.max_concurrency,
            "sandboxed plugin is at its concurrency ceiling; shedding the request"
        );
        let mut response = sandbox_error(&plugin, StatusCode::SERVICE_UNAVAILABLE);
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };

    let request = match read_request(&plugin, limits, pattern, &params, request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };

    // `wasmi` is a synchronous interpreter: running it on the async runtime
    // would block a worker for the whole fuel budget. `spawn_blocking` also
    // means a panic anywhere in the host shim surfaces as a `JoinError` here
    // instead of unwinding through the request task.
    //
    // The permit moves INTO the closure. A `spawn_blocking` task is never
    // cancelled — dropping its handle detaches it — so a permit held by this
    // future would be released the instant a client disconnects while the
    // interpreter kept running. `max_concurrency` would then bound nothing: a
    // client that connects and immediately resets, in a loop, would fill the
    // shared blocking pool with interpreters nobody is waiting for.
    // Captured before the request is handed to the interpreter: the response
    // path needs to know a HEAD was asked for, and `request` moves.
    let request_method_is_head = request.method.eq_ignore_ascii_case("HEAD");

    // Read here, not inside the closure. `CURRENT_TENANT` is a task-local
    // scoped by the tenancy middleware around *this* task; a `spawn_blocking`
    // closure runs on a different thread with no task-local at all, so a
    // capability resolved in there would silently be the single-tenant
    // namespace for every tenant at once — the exact failure this whole
    // subsystem exists to make impossible.
    let services = CapabilityServices {
        tenant: crate::tenancy::CURRENT_TENANT
            .try_with(Clone::clone)
            .ok()
            .flatten(),
        ..services
    };

    // The permit still moves INTO the closure — a detached `spawn_blocking`
    // task must not outlive it — but it comes back out so the response body can
    // keep holding it while hyper delivers the bytes.
    let outcome = tokio::task::spawn_blocking(move || {
        let outcome = host.run_with(&request, services);
        (outcome, permit)
    })
    .await;

    let (outcome, permit) = match outcome {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!(
                plugin,
                error = %err,
                "the sandbox worker did not complete; serving 502 on the plugin's prefix"
            );
            return sandbox_error(&plugin, StatusCode::BAD_GATEWAY);
        }
    };

    // Before the response is built, so a request that fails on the way out
    // still leaves the record of what it did — a plugin whose denials are the
    // reason it failed is exactly the one an operator will ask about.
    activity.ingest(&plugin, outcome.activity);
    activity.ingest_dropped(&plugin, outcome.dropped_activity);

    match outcome.result {
        Ok(response) => {
            let built = build_response(&plugin, &response, permit);
            // Naming HEAD in the method filter is deliberate (see
            // `method_filter`), but it costs axum's GET-to-HEAD fallback, and
            // that fallback is what discards the body. Without this, a HEAD
            // gets the whole GET payload — against both the documented
            // behaviour and HTTP itself.
            //
            // The bodyless statuses go the same way. RFC 9110 forbids content
            // on 204, 205 and 304, and hyper drops it on the way out — but only
            // on the way out. `mounted_router` is public, so middleware and an
            // embedder see this `Response` itself, and there they would see
            // payload bytes on a status that cannot carry them. Discarding here
            // rather than refusing keeps it consistent with the HEAD case and
            // with what the wire would have done anyway.
            //
            // Dropping the body drops its share of the permit, not the permit:
            // the other share rides in the extensions, which `into_parts`
            // carries over, so the slot still lasts as long as the response.
            discard_body_if_it_cannot_be_sent(built, request_method_is_head)
        }
        Err(failure) => {
            tracing::warn!(
                plugin,
                failure = %failure,
                stderr = outcome.stderr,
                fuel_used = outcome.fuel_used,
                "sandboxed plugin failed to answer; serving an error on its own prefix"
            );
            sandbox_error(&plugin, failure.status())
        }
    }
}

/// Charge `bytes` against a request's running metadata total, refusing before
/// the caller clones anything more if the ceiling is crossed.
///
/// Three call sites charge into one running total — the URI, an allowlisted
/// header, and a dropped header's entry — and each must refuse at the same
/// point, so the charge and the refusal live together rather than being
/// repeated beside each one.
fn charge_metadata(
    plugin: &str,
    running: &mut usize,
    bytes: usize,
    over: &'static str,
) -> Result<(), Box<Response>> {
    *running = running.saturating_add(bytes);
    if *running > super::host::MAX_REQUEST_METADATA_BYTES {
        tracing::warn!(
            plugin,
            over,
            max_request_metadata_bytes = super::host::MAX_REQUEST_METADATA_BYTES,
            "request metadata over the sandbox ceiling; refusing before cloning it"
        );
        return Err(Box::new(sandbox_error(
            plugin,
            StatusCode::PAYLOAD_TOO_LARGE,
        )));
    }
    Ok(())
}

/// Turn an axum request into the frame the guest will see, or the response the
/// caller gets instead.
///
/// The error side is boxed: a `Response` is large enough that returning one by
/// value makes every `Ok` of this function carry its footprint too, on the path
/// every sandboxed request takes.
async fn read_request(
    plugin: &str,
    limits: super::manifest::ResourceLimits,
    pattern: String,
    params: &axum::extract::RawPathParams,
    request: axum::extract::Request,
) -> Result<SandboxRequest, Box<Response>> {
    let (parts, body) = request.into_parts();
    // The nested router rewrote the URI, so the concrete path the caller asked
    // for comes from the original.
    let path_str = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map_or_else(|| parts.uri.path(), |original| original.0.path());
    let query_str = parts.uri.query().unwrap_or_default();

    // Everything the frame will carry except the headers, measured on the
    // borrowed values. Headers got this treatment first; the rest of the
    // metadata is the same argument and was left cloning unconditionally — an
    // in-process caller could hand `mounted_router` a megabyte of query string
    // and have it duplicated, and held for the whole body-read deadline, before
    // the ceiling that exists to refuse it ever ran.
    let mut metadata = 0usize;
    charge_metadata(
        plugin,
        &mut metadata,
        parts
            .method
            .as_str()
            .len()
            .saturating_add(path_str.len())
            .saturating_add(query_str.len())
            .saturating_add(pattern.len())
            .saturating_add(
                params
                    .iter()
                    .map(|(name, value)| super::host::metadata_pair_bytes(name, value))
                    .fold(0usize, usize::saturating_add),
            ),
        "uri",
    )?;

    let path = path_str.to_owned();
    let query = query_str.to_owned();
    let path_params: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    // Charged as they are read, and refused before the next one is cloned. The
    // ceiling used to be applied inside `run`, which is after every header has
    // already been copied into owned strings — so an oversized set forced the
    // duplicate allocation the ceiling exists to prevent, and then held it for
    // the whole body-read deadline before being told no. The cost per pair comes
    // from the same function the ceiling counts with, so the two agree.
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in &parts.headers {
        // A dropped header is charged for its entry but not its contents.
        // `HostFrame::request` drops everything outside this allowlist, so
        // counting the rest measured bytes no guest will ever see: a request
        // carrying 256 KiB of `Cookie` — headers this sandbox promises to
        // withhold silently — would be refused outright, turning a
        // confidentiality guarantee into an availability one. But skipping it
        // free leaves the *count* unbounded, and the count is what this loop
        // and the two walks inside `run` each pay per entry. See
        // `DROPPED_PAIR_BYTES`.
        if !super::wire::request_header_allowed(name.as_str()) {
            charge_metadata(
                plugin,
                &mut metadata,
                super::host::DROPPED_PAIR_BYTES,
                "dropped-headers",
            )?;
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        charge_metadata(
            plugin,
            &mut metadata,
            super::host::metadata_pair_bytes(name.as_str(), value),
            "headers",
        )?;
        headers.push((name.as_str().to_owned(), value.to_owned()));
    }

    // The ceiling is applied while reading, so an oversized body is refused
    // without ever being buffered in full — and the *wait* is bounded too. The
    // permit is already held at this point, deliberately: that is what makes the
    // declared footprint a bound on the whole request rather than on the part a
    // guest is running. The cost of that choice is that a client dribbling a
    // body could otherwise hold a permit forever without starting a guest, so
    // the read gets a deadline. Unlike the interpreter, an async body read is
    // genuinely cancellable, so this is a real bound rather than a hopeful one.
    let deadline = std::time::Duration::from_millis(limits.request_body_timeout_ms);
    let read = tokio::time::timeout(
        deadline,
        axum::body::to_bytes(body, limits.max_request_body_bytes),
    )
    .await;
    let body = match read {
        Ok(Ok(body)) => body.to_vec(),
        Ok(Err(_)) => {
            tracing::warn!(
                plugin,
                max_request_body_bytes = limits.max_request_body_bytes,
                "request body over the sandboxed plugin's declared ceiling; refusing it"
            );
            return Err(Box::new(sandbox_error(
                plugin,
                StatusCode::PAYLOAD_TOO_LARGE,
            )));
        }
        Err(_) => {
            tracing::warn!(
                plugin,
                request_body_timeout_ms = limits.request_body_timeout_ms,
                "request body did not arrive within the plugin's deadline; releasing its permit"
            );
            return Err(Box::new(sandbox_error(plugin, StatusCode::REQUEST_TIMEOUT)));
        }
    };

    Ok(SandboxRequest {
        method: parts.method.as_str().to_owned(),
        route: pattern,
        path,
        query,
        path_params,
        headers,
        body,
    })
}

/// Turn a sanitized guest answer into an HTTP response.
/// The plugin's answer, owning the concurrency permit alongside its bytes.
///
/// The permit exists to bound what one plugin can keep resident: the decoded
/// response is handed to hyper as a buffer, and a client that reads slowly — or
/// stops reading — keeps it alive long after the interpreter returned. So the
/// permit must be released exactly when *the bytes* are freed.
///
/// It took three tries to learn that "exactly when the bytes are freed" is not
/// a moment any `poll_frame` can name. Releasing on drop of the body missed a
/// consumer that drained and kept the value; releasing at end-of-stream missed
/// a body that was already ended and never polled; releasing on the poll that
/// ends the stream missed the fact that the same poll *hands the buffer away* —
/// the body keeps nothing, so tying the permit to the body's state released it
/// while the consumer still held every byte.
///
/// Each fix moved the release to a different point in the body's life, and the
/// bytes do not live in the body for all of it. Owning the permit here, beneath
/// the `Bytes`, is what makes the question answerable: the permit is dropped
/// when the last handle to the buffer is, whether that is inside hyper, inside
/// middleware, or in the response nobody ever polled.
struct PermitBuf {
    data: Vec<u8>,
    /// Never read: it is held so that dropping this buffer returns the slot.
    ///
    /// A *share* of the permit rather than the permit, because the body is not
    /// the only part of the response that occupies the footprint — see
    /// [`PermitShare`].
    _permit: PermitShare,
}

/// A share of one concurrency slot, released when the last share drops.
///
/// The slot has two holders, because the response has two halves that occupy
/// the footprint and they are not dropped together. The body's bytes may
/// outlive the response (hyper holds them by `Bytes`), which is why
/// [`PermitBuf`] exists. But the *headers* may outlive the bytes: a guest may
/// answer with an empty body, and `check_size` charges header bytes against the
/// same response ceiling the body is charged against, so up to the whole
/// allowance can sit in `HeaderValue`s with no body at all. `Bytes::from_owner`
/// over an empty buffer drops its owner at construction, so a permit that lived
/// only in the buffer came back before such a response was even returned —
/// letting a caller retain header-only responses and admit another request for
/// each, outside `max_concurrency`.
///
/// Holding a share in each place makes the slot outlive whichever half is
/// dropped last, without either half needing to know about the other.
type PermitShare = Arc<tokio::sync::OwnedSemaphorePermit>;

/// The response's share of the slot, parked in its extensions so it lives
/// exactly as long as the head does.
#[derive(Clone)]
struct PermitExtension(#[allow(dead_code)] PermitShare);

impl AsRef<[u8]> for PermitBuf {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

/// Drop the body from a response that may not carry one.
///
/// Two cases, and the reason they are one function is that the reason is the
/// same: the status line and the method already say there is no content, so
/// bytes hanging off the response are bytes nothing should ever read.
///
/// - **HEAD.** Naming HEAD in the method filter is deliberate (see
///   `method_filter`), but it costs axum's GET-to-HEAD fallback, and that
///   fallback is what discards the body. Without this a HEAD gets the whole GET
///   payload, against both the documented behaviour and HTTP itself.
/// - **204, 205, 304.** RFC 9110 forbids content on these, and hyper drops it
///   on the way out — but only on the way out. `mounted_router` is public, so
///   middleware and an embedder see this `Response` itself, and there they
///   would see payload bytes on a status that cannot carry them.
///
/// Discarding rather than refusing keeps both cases consistent with each other
/// and with what the wire would have done anyway.
///
/// Dropping the body drops *its share* of the concurrency permit, not the
/// permit: the other share rides in the extensions, which `into_parts` carries
/// over, so the slot still lasts as long as the response does.
fn discard_body_if_it_cannot_be_sent(response: Response, method_is_head: bool) -> Response {
    let bodyless = matches!(
        response.status(),
        StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT | StatusCode::NOT_MODIFIED
    );
    if !method_is_head && !bodyless {
        return response;
    }
    let (parts, _body) = response.into_parts();
    Response::from_parts(parts, Body::empty())
}

fn build_response(
    plugin: &str,
    response: &super::wire::SandboxResponse,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let Ok(status) = StatusCode::from_u16(response.status) else {
        // `SandboxResponse::validate` already refused anything outside the HTTP
        // range, so this cannot happen; refusing again is cheaper than trusting.
        return sandbox_error(plugin, StatusCode::BAD_GATEWAY);
    };

    let mut out = Response::builder().status(status);
    let mut saw_content_type = false;
    for (name, value) in &response.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) else {
            return sandbox_error(plugin, StatusCode::BAD_GATEWAY);
        };
        if name == http::header::CONTENT_TYPE {
            saw_content_type = true;
        }
        out = out.header(name, value);
    }
    if !saw_content_type {
        // A body with no declared type is a body a browser will guess at.
        // Guessing is how a plugin that can only return bytes turns into one
        // that can return a script.
        out = out.header(http::header::CONTENT_TYPE, "application/octet-stream");
    }
    // The permit rides *inside* the buffer, not alongside the body that carries
    // it: hyper may hold these bytes long after `run` returned, and it holds
    // them by `Bytes` rather than by the body they came out of.
    //
    // It rides in the response's extensions too, because the headers are
    // charged against the same ceiling and are not dropped with the bytes. See
    // [`PermitShare`].
    let permit: PermitShare = Arc::new(permit);
    let body = Body::from(bytes::Bytes::from_owner(PermitBuf {
        data: response.body.clone(),
        _permit: Arc::clone(&permit),
    }));
    let Ok(mut built) = out.body(body) else {
        return sandbox_error(plugin, StatusCode::BAD_GATEWAY);
    };
    built.extensions_mut().insert(PermitExtension(permit));
    stamp_host_headers(&mut built, plugin);
    built
}

fn attribution(plugin: &str) -> Option<HeaderValue> {
    HeaderValue::try_from(plugin).ok()
}

/// Stamp the headers the host owns, replacing anything of the same name.
///
/// `Response::builder().header(..)` *appends*, and `HeaderMap::get` returns the
/// first value — so a guest that emitted `x-autumn-sandboxed` or
/// `x-content-type-options` of its own would win the lookup against the host's.
/// The response header allowlist already refuses both names; this is the second
/// lock on the same door, and it is what makes "every response on this prefix
/// is attributable" true of the 413 and 503 the guest never ran for.
fn stamp_host_headers(response: &mut Response, plugin: &str) {
    let headers = response.headers_mut();
    headers.insert(
        http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(value) = attribution(plugin) {
        headers.insert(HeaderName::from_static(SANDBOX_ATTRIBUTION_HEADER), value);
    }
}

/// The error a sandboxed plugin's own prefix serves. Never leaks a guest's
/// stderr or trap text to the caller — that is for the operator's log.
fn sandbox_error(plugin: &str, status: StatusCode) -> Response {
    let mut response = (
        status,
        [(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "the sandboxed plugin could not serve this request\n",
    )
        .into_response();
    stamp_host_headers(&mut response, plugin);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::manifest::ResourceLimits;
    use crate::plugin_sandbox::test_guests as guests;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;

    fn manifest_toml(routes: &str, limits: &str) -> String {
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"
{routes}
[limits]
{limits}
"#,
            digest = "a".repeat(64)
        )
    }

    fn plugin_from(wat: &str, routes: &str, limits: ResourceLimits) -> SandboxedPlugin {
        let mut manifest = SandboxManifest::parse(&manifest_toml(
            routes,
            "fuel = 200000000\nmemory_bytes = 33554432",
        ))
        .expect("valid manifest");
        manifest.limits = limits;
        let wasm = wat::parse_str(wat).expect("valid WAT");
        SandboxedPlugin::new(SandboxHost::from_module(manifest, &wasm).expect("loads"))
    }

    const GREET_ROUTE: &str = "[[routes]]\nmethod = \"GET\"\npath = \"/hello/greet\"\n";

    fn hello_plugin() -> SandboxedPlugin {
        plugin_from(guests::HELLO, GREET_ROUTE, ResourceLimits::default())
    }

    /// The app a test drives: a sandboxed plugin mounted beside an ordinary
    /// route, so "the rest of the app keeps serving" is something a request
    /// can actually prove.
    fn app(plugin: &SandboxedPlugin) -> axum::Router {
        axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .merge(plugin.mounted_router())
    }

    async fn send(app: axum::Router, method: &str, path: &str) -> (StatusCode, String) {
        send_with_body(app, method, path, Body::empty()).await
    }

    async fn send_with_body(
        app: axum::Router,
        method: &str,
        path: &str,
        body: Body,
    ) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(body)
            .expect("request");
        let response = app.oneshot(request).await.expect("infallible");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn a_sandboxed_plugin_serves_http_under_its_declared_prefix() {
        let plugin = hello_plugin();
        let (status, body) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "hello from the sandbox");
    }

    #[tokio::test]
    async fn the_manifest_is_the_mount_not_a_description_of_it() {
        // An undeclared path under the prefix is a 404 from the *host*: the
        // guest is never started, so the manifest bounds what the artifact can
        // serve rather than merely documenting it.
        let plugin = hello_plugin();
        let (status, _) = send(app(&plugin), "GET", "/hello/undeclared").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(app(&plugin), "POST", "/hello/greet").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_head_on_a_declared_get_reaches_the_guest_rather_than_axum_s_405() {
        // axum's method router dispatches a HEAD with no HEAD route to the GET
        // one (`call!(req, HEAD, get)`), which is what HTTP requires — so the
        // guest really does see `method: "HEAD"`, and the manifest says so.
        let plugin = hello_plugin();
        let request = Request::builder()
            .method("HEAD")
            .uri("/hello/greet")
            .body(Body::empty())
            .expect("request");
        let response = app(&plugin).oneshot(request).await.expect("infallible");
        // This fixture answers 405 for anything but GET — which is the point:
        // the *guest* decided, so the request crossed the boundary. axum's own
        // 405 would carry no attribution.
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            response.headers().contains_key(SANDBOX_ATTRIBUTION_HEADER),
            "the sandbox handler must have produced this"
        );
    }

    #[tokio::test]
    async fn a_manifest_declaring_both_get_and_head_still_mounts() {
        // Mounting GET as `GET | HEAD` and then mounting the declared HEAD on
        // the same path is an overlapping method route, which axum refuses by
        // panicking while the router is built — a valid manifest taking the
        // application down at boot, which is the one thing this lane must never
        // do.
        let plugin = plugin_from(
            guests::HELLO,
            "[[routes]]\nmethod = \"GET\"\npath = \"/hello/greet\"\n\n\
             [[routes]]\nmethod = \"HEAD\"\npath = \"/hello/greet\"\n",
            ResourceLimits::default(),
        );
        let (status, body) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "hello from the sandbox");
        let (status, _) = send(app(&plugin), "HEAD", "/hello/greet").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "the guest answered");
    }

    #[test]
    fn the_route_manifest_names_the_head_a_get_route_also_serves() {
        let builder = crate::app::app().plugin(hello_plugin());
        let routes = builder.plugin_route_infos().expect("route manifest");
        assert!(
            routes
                .iter()
                .any(|route| route.method == "HEAD" && route.path == "/hello/greet"),
            "{routes:?}"
        );
    }

    #[tokio::test]
    async fn responses_are_attributable_to_the_plugin_that_produced_them() {
        let plugin = hello_plugin();
        let request = Request::builder()
            .uri("/hello/greet")
            .body(Body::empty())
            .expect("request");
        let response = app(&plugin).oneshot(request).await.expect("infallible");
        assert_eq!(
            response
                .headers()
                .get(SANDBOX_ATTRIBUTION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("autumn-plugin-hello")
        );
    }

    #[tokio::test]
    async fn a_runaway_plugin_5xxs_on_its_own_prefix_while_the_app_keeps_serving() {
        let plugin = plugin_from(
            guests::CPU_SPIN,
            GREET_ROUTE,
            ResourceLimits {
                fuel: 5_000_000,
                ..ResourceLimits::default()
            },
        );
        let (status, _) = send(app(&plugin), "GET", "/hello/greet").await;
        assert!(status.is_server_error(), "{status}");

        let (status, body) = send(app(&plugin), "GET", "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn a_trapping_plugin_never_takes_the_host_with_it() {
        let plugin = plugin_from(guests::TRAP, GREET_ROUTE, ResourceLimits::default());
        let app = app(&plugin);
        let (status, _) = send(app.clone(), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        // The process is still here, and so is every other route.
        let (status, _) = send(app, "GET", "/healthz").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_plugin_that_exits_never_exits_the_host() {
        let plugin = plugin_from(guests::EXIT, GREET_ROUTE, ResourceLimits::default());
        let app = app(&plugin);
        let (status, _) = send(app.clone(), "GET", "/hello/greet").await;
        assert!(status.is_server_error(), "{status}");
        let (status, _) = send(app, "GET", "/healthz").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_body_over_the_declared_ceiling_never_reaches_the_guest() {
        let plugin = plugin_from(
            guests::HELLO,
            "[[routes]]\nmethod = \"POST\"\npath = \"/hello/greet\"\n",
            ResourceLimits {
                max_request_body_bytes: 16,
                ..ResourceLimits::default()
            },
        );
        let (status, _) = send_with_body(
            app(&plugin),
            "POST",
            "/hello/greet",
            Body::from(vec![b'x'; 4096]),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_concurrency_ceiling_is_enforced_rather_than_queued() {
        let plugin = plugin_from(
            guests::HELLO,
            GREET_ROUTE,
            ResourceLimits {
                max_concurrency: 1,
                ..ResourceLimits::default()
            },
        );
        // Hold the plugin's only permit, exactly as an in-flight request would.
        let held = std::sync::Arc::clone(&plugin.permits)
            .try_acquire_owned()
            .expect("the first permit is free");
        let (status, _) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
        let (status, _) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_never_arrives_does_not_pin_a_permit_forever() {
        // The permit is held from admission, which is what makes the declared
        // footprint a bound on the whole request. The cost of that is a client
        // dribbling a body, so the read has a deadline — and unlike the
        // interpreter, an async body read is genuinely cancellable.
        let plugin = plugin_from(
            guests::HELLO,
            "[[routes]]\nmethod = \"POST\"\npath = \"/hello/greet\"\n",
            ResourceLimits {
                max_concurrency: 1,
                request_body_timeout_ms: 250,
                ..ResourceLimits::default()
            },
        );
        let stalled = Body::from_stream(futures::stream::pending::<
            Result<bytes::Bytes, std::io::Error>,
        >());
        let (status, _) = send_with_body(app(&plugin), "POST", "/hello/greet", stalled).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);

        // …and the permit came back: the next request reaches the guest, which
        // answers 405 because this fixture only handles GET. Any guest-produced
        // answer proves the point — a pinned permit would have been a 503.
        let (status, _) =
            send_with_body(app(&plugin), "POST", "/hello/greet", Body::from("hi")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn an_empty_answer_returns_its_slot_on_drop_without_being_polled() {
        // Release must not ride on `poll_frame` reaching the end of the stream:
        // a body that is *already* ended need never be polled at all — a
        // consumer is entitled to read `is_end_stream` and stop there. Every
        // 204, and every empty error page a guest returns, takes that path.
        //
        // This test used to assert that such an answer held no slot at all,
        // which was the defect rather than the design: header bytes are charged
        // against the same response ceiling as body bytes, so "empty body" does
        // not mean "occupies nothing". The slot is now held until the response
        // is dropped, and — the part that still matters here — it comes back
        // then without anything having polled the body.
        use crate::plugin_sandbox::wire::SandboxResponse;

        let permits = Arc::new(Semaphore::new(1));
        let empty = SandboxResponse {
            status: 204,
            headers: Vec::new(),
            body: Vec::new(),
        };
        let response = build_response(
            "autumn-plugin-hello",
            &empty,
            Arc::clone(&permits).acquire_owned().await.expect("permit"),
        );
        assert!(
            http_body::Body::is_end_stream(response.body()),
            "the fixture must be the never-polled shape this test is about",
        );
        assert_eq!(
            permits.available_permits(),
            0,
            "the response is still resident, so its slot is still spent",
        );
        drop(response);
        assert_eq!(
            permits.available_permits(),
            1,
            "the slot must come back on drop, with no poll anywhere",
        );

        // The converse, so the assertion above is about the response being gone
        // and not about the permit having been dropped on the floor: bytes still
        // resident do hold a slot, until they are.
        let full = SandboxResponse {
            status: 200,
            headers: Vec::new(),
            body: b"hi".to_vec(),
        };
        let response = build_response(
            "autumn-plugin-hello",
            &full,
            Arc::clone(&permits).acquire_owned().await.expect("permit"),
        );
        assert_eq!(
            permits.available_permits(),
            0,
            "undelivered bytes keep their slot"
        );
        drop(response);
        assert_eq!(permits.available_permits(), 1, "and give it back on drop");
    }

    #[tokio::test]
    async fn a_bodyless_status_carries_no_body_to_an_in_process_caller() {
        // RFC 9110 forbids content on 204, 205 and 304. Hyper drops it on the
        // way out, so on a network connection this is invisible — but
        // `mounted_router` is public, and middleware or an embedder reads this
        // `Response` directly. There, payload bytes on a status that cannot
        // carry them are bytes a consumer may well act on.
        //
        // The permit is the reason to be careful here rather than just correct:
        // discarding the body drops its share of the slot, so this also pins
        // that the extensions share is what keeps the accounting honest.
        use crate::plugin_sandbox::wire::SandboxResponse;
        use http_body_util::BodyExt as _;

        for status in [204u16, 205, 304] {
            let permits = Arc::new(Semaphore::new(1));
            let answered = SandboxResponse {
                status,
                headers: Vec::new(),
                body: b"content a bodyless status may not carry".to_vec(),
            };
            let built = build_response(
                "autumn-plugin-hello",
                &answered,
                Arc::clone(&permits).acquire_owned().await.expect("permit"),
            );
            let response = discard_body_if_it_cannot_be_sent(built, false);

            let collected = response
                .into_body()
                .collect()
                .await
                .expect("the body collects")
                .to_bytes();
            assert!(
                collected.is_empty(),
                "{status} handed {} body bytes to an in-process caller",
                collected.len(),
            );
        }

        // The converse, so the loop above is about the status and not about
        // this function emptying everything it is given.
        let permits = Arc::new(Semaphore::new(1));
        let ok = SandboxResponse {
            status: 200,
            headers: Vec::new(),
            body: b"hi".to_vec(),
        };
        let built = build_response(
            "autumn-plugin-hello",
            &ok,
            Arc::clone(&permits).acquire_owned().await.expect("permit"),
        );
        let kept = discard_body_if_it_cannot_be_sent(built, false)
            .into_body()
            .collect()
            .await
            .expect("the body collects")
            .to_bytes();
        assert_eq!(kept.as_ref(), b"hi", "a 200 must keep the answer it gave");
    }

    #[tokio::test]
    async fn discarding_a_body_does_not_return_the_slot_the_headers_still_hold() {
        // `into_parts` carries the extensions over, so the response's share of
        // the permit survives its body being thrown away. Without that, every
        // HEAD and every bodyless status would release its slot early — the
        // header-only defect again, reached through the discard path.
        use crate::plugin_sandbox::wire::SandboxResponse;

        let permits = Arc::new(Semaphore::new(1));
        let answered = SandboxResponse {
            status: 204,
            headers: vec![("x-plugin-note".to_string(), "v".repeat(4096))],
            body: b"discarded".to_vec(),
        };
        let built = build_response(
            "autumn-plugin-hello",
            &answered,
            Arc::clone(&permits).acquire_owned().await.expect("permit"),
        );
        let response = discard_body_if_it_cannot_be_sent(built, false);
        assert_eq!(
            permits.available_permits(),
            0,
            "the slot came back when the body was discarded, while the headers remain",
        );
        drop(response);
        assert_eq!(
            permits.available_permits(),
            1,
            "and back once nothing holds it"
        );
    }

    #[tokio::test]
    async fn a_header_only_response_holds_its_permit_until_the_headers_go() {
        // `PermitBuf` tied the slot to the response *bytes*, which is right for
        // every answer that has some. A guest may answer with none: headers are
        // charged against the same response ceiling (`check_size` sums header
        // bytes and body bytes against one `max`), so up to the whole allowance
        // can sit in `HeaderValue`s with an empty body. `Bytes::from_owner`
        // drops its owner at construction for an empty buffer, so the slot came
        // back before the response was even returned — and a caller retaining
        // header-only responses could admit another request for each, holding
        // response-sized allocations outside `max_concurrency`.
        use crate::plugin_sandbox::wire::SandboxResponse;

        let permits = Arc::new(Semaphore::new(1));
        let header_only = SandboxResponse {
            status: 200,
            headers: vec![("x-plugin-note".to_string(), "v".repeat(4096))],
            body: Vec::new(),
        };
        let response = build_response(
            "autumn-plugin-hello",
            &header_only,
            Arc::clone(&permits).acquire_owned().await.expect("permit"),
        );

        // The body is empty and end-of-stream from the start, so nothing about
        // the *bytes* can be holding the slot here.
        assert!(http_body::Body::is_end_stream(response.body()));
        assert!(
            response.headers().contains_key("x-plugin-note"),
            "the header this response is made of must survive to be held",
        );
        assert_eq!(
            permits.available_permits(),
            0,
            "the slot came back while the response headers were still held",
        );

        // And it is the headers keeping it: dropping them returns it.
        drop(response);
        assert_eq!(
            permits.available_permits(),
            1,
            "the slot must come back once nothing holds the response",
        );
    }

    #[tokio::test]
    async fn the_permit_outlives_the_body_and_dies_with_the_bytes() {
        // The release point moved three times before it moved to the right
        // *place*. This is the case that showed the place was wrong: the poll
        // that ends the stream also hands the buffer away, so a permit tied to
        // the body's state came back while the consumer still held every byte —
        // admitting another full-sized response outside `max_concurrency`.
        //
        // Now the permit lives under the `Bytes`, so this test follows the
        // bytes rather than the body.
        use crate::plugin_sandbox::wire::SandboxResponse;
        use http_body_util::BodyExt as _;

        let permits = Arc::new(Semaphore::new(1));
        let full = SandboxResponse {
            status: 200,
            headers: Vec::new(),
            body: b"hi".to_vec(),
        };
        let mut response = build_response(
            "autumn-plugin-hello",
            &full,
            Arc::clone(&permits).acquire_owned().await.expect("permit"),
        );
        assert_eq!(permits.available_permits(), 0, "the bytes are undelivered");

        let frame = response
            .body_mut()
            .frame()
            .await
            .expect("a frame")
            .expect("not an error");
        let data = frame.into_data().expect("a data frame");
        assert_eq!(data.as_ref(), b"hi");

        // The body is exhausted and holds nothing…
        assert!(http_body::Body::is_end_stream(response.body()));
        // …and dropping it must not return the slot, because the bytes are
        // still here, in `data`.
        drop(response);
        assert_eq!(
            permits.available_permits(),
            0,
            "the slot came back while the response bytes were still held"
        );

        // Only the last handle to the buffer returns it.
        drop(data);
        assert_eq!(
            permits.available_permits(),
            1,
            "the slot did not come back when the bytes were freed"
        );
    }

    #[test]
    fn the_plugin_is_named_by_its_manifest() {
        assert_eq!(
            crate::plugin::Plugin::name(&hello_plugin()),
            "autumn-plugin-hello"
        );
    }

    #[test]
    fn mounting_declares_the_manifest_routes_with_plugin_attribution() {
        let builder = crate::app::app().plugin(hello_plugin());
        let routes = builder.plugin_route_infos().expect("route manifest");
        let sandboxed: Vec<_> = routes
            .iter()
            .filter(|route| {
                route.source
                    == crate::route_listing::RouteSource::Plugin("autumn-plugin-hello".to_owned())
            })
            .collect();
        // GET and the HEAD it also serves.
        assert_eq!(sandboxed.len(), 2, "{sandboxed:?}");
        assert!(sandboxed.iter().all(|route| route.path == "/hello/greet"));
        assert_eq!(sandboxed[0].method, "GET");
        assert_eq!(sandboxed[1].method, "HEAD");
    }

    #[test]
    fn a_mounted_sandboxed_plugin_passes_the_conformance_checks() {
        let builder = crate::app::app().plugin(hello_plugin());
        let routes = builder.plugin_route_infos().expect("route manifest");
        let report = crate::plugin_conformance::run_conformance(
            &crate::plugin_conformance::ConformanceConfig::new("autumn-plugin-hello")
                .prefix("/hello"),
            &routes,
        );
        assert!(report.passed(), "{}", report.to_text_report());
    }
}
