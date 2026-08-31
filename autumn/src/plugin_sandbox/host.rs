//! The sandbox host: a `wasmi` interpreter plus a deny-by-default WASI shim.
//!
//! # The sandbox *is* this file's import list
//!
//! A WebAssembly guest can do exactly two things on its own: compute, and call
//! a function the host gave it. It has no syscalls, no ambient file
//! descriptors and no way to name anything outside its own linear memory. So
//! the entire authority a sandboxed plugin holds is the list of host functions
//! `define_wasi_shim` registers — which is why they are all in one function,
//! in one place, readable top to bottom.
//!
//! Every one of them either serves the request dialogue or says no:
//!
//! | Import | Behaviour |
//! | --- | --- |
//! | `fd_read` (fd 0) | the next bytes of the request frame |
//! | `fd_write` (fd 1) | the response frame, one NDJSON line |
//! | `fd_write` (fd 2) | captured as diagnostics, surfaced in the failure detail |
//! | `random_get` | a fixed-seed PRNG — entropy is not authority, but it is not ambient either |
//! | `clock_time_get` | a fixed instant |
//! | `sched_yield`, `fd_close`, `fd_fdstat_get`, `fd_seek`, `fd_tell` | inert |
//! | `environ_*`, `args_*` | **empty, and each call recorded as a denial** |
//! | every `path_*`, every `fd_*` on an unknown descriptor | **`ENOTCAPABLE`/`EBADF`, recorded** |
//! | every `sock_*`, `poll_oneoff`, `proc_raise` | **`ENOTCAPABLE`, recorded** |
//! | `proc_exit` | ends the guest, never the host |
//!
//! There is no `path_open` that opens, no `fd_prestat_get` that resolves, and
//! no socket that connects. A capability is not "off by default" here — for
//! everything but request handling, there is no implementation to turn on.
//!
//! # The world is closed
//!
//! Anything the module imports that this shim does not define is refused at
//! **load**, before the artifact can run once, and the refusal names the
//! import. That is what makes a bespoke seam — `autumn_db::query`,
//! `env::system` — a non-starter rather than a runtime error a guest could
//! catch and retry.
//!
//! # Everything a guest does wrong is a value, never a process event
//!
//! A trap (what a Rust panic compiles to on wasm), a `proc_exit`, fuel
//! exhaustion, a refused allocation, a malformed frame, a guest that never
//! answers: all of them come back as [`SandboxFailure`] inside an
//! [`SandboxOutcome`]. Nothing in this module can abort, exit or panic the host
//! process.
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

use std::collections::VecDeque;
use std::fmt;

use tokio::sync::Semaphore;
use wasmi::{Caller, Config, Engine, Linker, Module, Store};

use super::artifact::SandboxArtifact;
use super::manifest::{ResourceLimits, SandboxCapability, SandboxManifest};
use super::wire::{GuestFrame, HostFrame, SandboxRequest, SandboxResponse, from_line, to_line};

/// The WASI module name every import in the shim lives under.
const WASI: &str = "wasi_snapshot_preview1";

/// WASI `errno` values this shim returns.
mod errno {
    pub(super) const SUCCESS: i32 = 0;
    /// Bad file descriptor.
    pub(super) const BADF: i32 = 8;
    /// Invalid argument.
    pub(super) const INVAL: i32 = 28;
    /// Seek on a pipe.
    pub(super) const SPIPE: i32 = 70;
    /// Capability insufficient — the sandbox's universal "no".
    pub(super) const NOTCAPABLE: i32 = 76;
}

/// Bytes in one WebAssembly page.
const WASM_PAGE_BYTES: u64 = 64 * 1024;

/// The size of one WASI `iovec` (two `u32`s: pointer and length).
const IOVEC_SIZE: usize = 8;

/// Size of a WASI `fdstat` struct.
const FDSTAT_SIZE: usize = 24;

/// Scratch-buffer size for host-side copies of guest-supplied ranges (64 KiB).
///
/// An in-bounds iovec can legitimately span the guest's whole linear memory,
/// but the host must never mirror one in a single allocation — a few concurrent
/// requests doing so would exhaust host memory that no guest-side limit
/// accounts for. Copies run through a buffer of this size, and the output
/// budget is applied per chunk, so a runaway write fails long before its full
/// length is copied.
const HOST_IO_CHUNK_BYTES: usize = 64 * 1024;

/// Cap on accumulated guest stderr, in bytes (64 KiB).
const STDERR_BUDGET_BYTES: usize = 64 * 1024;

/// Largest `iovec` array the shim will walk in one call.
///
/// Real WASI callers pass a handful; an array of millions is a guest asking the
/// host to do a million times the work of one call. Bounding the array bounds
/// the per-call amplification factor, which the fuel charge below then prices.
const MAX_IOVECS: i32 = 64;

/// The largest number of table elements one instance may hold, across all of
/// its tables.
///
/// A table holds function references, not bytes, but it is still per-instance
/// host storage: at four tables of 65,536 entries each it would be megabytes an
/// instance, multiplied by `max_concurrency`, that
/// [`ResourceLimits::request_footprint_bytes`](crate::plugin_sandbox::ResourceLimits::request_footprint_bytes)
/// never counted. Bounded here to a number small enough that the footprint can
/// carry it as a constant, and generous enough for any real guest's
/// indirect-call table.
pub const MAX_TABLE_ELEMENTS: u32 = 16_384;

/// The most tables one instance may have.
///
/// The store limiter refuses the fifth at instantiation, which is per request,
/// so the count has to be a load-time verdict too — a module with five empty
/// tables clears the *element* ceiling for free. Defined once and read by both,
/// so the number the limiter enforces and the number the loader admits cannot
/// drift apart.
pub const MAX_TABLES: usize = 4;

/// The most globals a module may declare.
///
/// Every instance allocates and initialises its own copy of each, and an
/// instance is per request — so globals are per-request storage and per-request
/// work in exactly the way tables and segments are. The aggregate declared-entry
/// ceiling is far too generous to bound them on its own: a module can sit well
/// under a million total entries and still carry hundreds of thousands of
/// globals, which no fuel charge priced and no footprint counted.
pub const MAX_GLOBALS: usize = 4096;

/// The most bytes a module's code section may occupy.
///
/// The declaration counts bound how many *things* a module names; they say
/// nothing about how much instruction stream sits inside one of them. A single
/// function whose body fills the file allowance declares exactly one entry and
/// still hands `Module::new` tens of megabytes to translate into wasmi's larger
/// internal representation, which is the allocation amplification the file
/// ceiling was never a bound on. Generous against real output — a whole Rust
/// web application compiles to a few megabytes of code — and far under what a
/// 64 MiB artifact can carry.
pub const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;

/// The largest total data + element section a module may carry (16 MiB).
///
/// Every request instantiates a **fresh** module — that is what makes "no state
/// survives a request" true — and instantiation copies the module's data and
/// element segments before the first guest instruction runs. wasmi does not
/// meter that phase, so the only real bound on it is a bound on the segments
/// themselves, checked once at load. A hello-world Rust guest carries tens of
/// kilobytes here; a large one, a few megabytes.
pub const MAX_INIT_SECTION_BYTES: usize = 16 * 1024 * 1024;

/// The largest number of data + element segments a module may declare.
///
/// Bytes alone do not bound the work: a segment costs a bounds check and a
/// copy set-up regardless of its length, so a module of a million empty
/// segments is small on disk and expensive to instantiate. Both are capped
/// because instantiation cost is a function of both.
pub const MAX_INIT_SEGMENTS: usize = 4096;

/// Bytes of host-side copying one unit of fuel buys.
///
/// wasmi meters the guest's own instructions, and a host call costs a handful
/// of units no matter how much the host then copies on the guest's behalf.
/// Without a charge here, a guest could buy gigabytes of `memcpy` for single
/// digits of fuel — a spin *inside the host* rather than inside the
/// interpreter, which the CPU ceiling would never see and no deadline exists to
/// catch. The rate matches the order of wasmi's own pricing for bulk memory
/// operations, so byte-work and instruction-work share one budget and the
/// ceiling means what the manifest says it means.
const BYTES_PER_FUEL: u64 = 64;

/// Characters of stderr echoed into a failure detail.
const STDERR_EXCERPT: usize = 512;

/// Characters of a guest-influenced string kept for a log line or an error.
///
/// Every `String` a [`SandboxFailure`] carries came, directly or by way of an
/// interpreter message quoting one, from an artifact nobody audited. It is
/// evidence, so it has to survive; it is also attacker-controlled text on its
/// way into a log the operator trusts, so it cannot survive intact.
const DETAIL_EXCERPT: usize = 512;

/// Bound and neutralise a string the guest influenced.
///
/// Two hazards, and the fix for one is not the fix for the other. *Length*: a
/// detail can be as large as the stdout budget — megabytes at the maximum
/// response ceiling — and a plugin that fails in a loop writes one per request,
/// which is a log-storage exhaustion with no guest instruction spent on it.
/// *Content*: a newline ends the host's record and starts one the operator did
/// not write, and an ANSI escape repaints records already on screen; either
/// turns "the log says" into something a plugin controls.
///
/// Control characters are escaped rather than dropped, so a detail that was
/// trying to forge a line reads as one that tried. Everything printable —
/// including non-ASCII — is kept: an author debugging a plugin that writes
/// error text in their own language should be able to read it.
fn guest_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(DETAIL_EXCERPT));
    for (kept, ch) in text.chars().enumerate() {
        if kept == DETAIL_EXCERPT {
            out.push_str(" … (truncated)");
            break;
        }
        if ch.is_control() {
            out.extend(ch.escape_debug());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Refuse a request whose body is over the manifest's declared ceiling.
///
/// The ceiling is enforced here, not only in the Axum adapter. The adapter
/// applies it while reading, so an oversized body never gets buffered on that
/// path — but [`SandboxHost::run`] is public, and an embedder calling it
/// directly hands over a `SandboxRequest` that has already been built. Without
/// this the body is cloned into the frame and base64-expanded regardless of the
/// ceiling, and the footprint the manifest advertises — which counts the body
/// at `4 × max_request_body_bytes` — stops bounding anything.
///
/// The encoding price charged below it is a real bound, but only against a
/// manifest whose fuel is small relative to its body ceiling; a generous fuel
/// budget buys an arbitrarily large host-side copy. So the ceiling is checked
/// first, before the request is priced or walked at all.
pub const MAX_REQUEST_METADATA_BYTES: usize = 256 * 1024;

/// Every byte of a request that is not its body, summed.
///
/// One definition, used by both the ceiling below and [`encoding_fuel`], so the
/// bytes that are refused and the bytes that are charged for cannot drift apart
/// as fields are added to the frame.
fn request_metadata_bytes(request: &SandboxRequest) -> usize {
    /// What one `(String, String)` costs before either string holds a byte:
    /// two 24-byte `String` headers in the vector, and the `["",""],` the
    /// serialiser writes around them.
    ///
    /// Counting only the *contents* would leave a list of a million empty pairs
    /// summing to zero — past a byte ceiling for free, then cloned and expanded
    /// into real syntax anyway. That is the same shape as a response frame full
    /// of `["",""]`, and it has to be priced on this side too.
    const ENTRY: usize = 56;

    let pairs = |list: &[(String, String)]| {
        list.iter()
            .map(|(name, value)| name.len().saturating_add(value.len()).saturating_add(ENTRY))
            .fold(0usize, usize::saturating_add)
    };
    request
        .method
        .len()
        .saturating_add(request.path.len())
        .saturating_add(request.query.len())
        .saturating_add(request.route.len())
        .saturating_add(pairs(&request.headers))
        .saturating_add(pairs(&request.path_params))
}

fn refuse_oversized_request(
    request: &SandboxRequest,
    limits: ResourceLimits,
) -> Option<SandboxOutcome> {
    if request.body.len() > limits.max_request_body_bytes {
        return Some(SandboxOutcome::refused(
            SandboxFailure::RequestBudget {
                max: limits.max_request_body_bytes,
                len: request.body.len(),
            },
            0,
        ));
    }
    // The body ceiling is the manifest's; this one is the host's, because no
    // manifest declares a query or header budget. `run` is public and the
    // adapter's own limits do not reach it, so an embedder building a
    // `SandboxRequest` by hand could otherwise hand over a gigabyte of query
    // string: cloned into the frame and serialised into the NDJSON line before
    // the guest starts, against a footprint that budgets for the *body* alone.
    //
    // The encoding charge prices those bytes, but pricing is not a bound — at
    // the manifest's maximum fuel it permits more than a terabyte of them — so
    // the ceiling is what keeps `request_footprint_bytes` honest.
    let metadata = request_metadata_bytes(request);
    (metadata > MAX_REQUEST_METADATA_BYTES).then(|| {
        SandboxOutcome::refused(
            SandboxFailure::RequestMetadataBudget {
                max: MAX_REQUEST_METADATA_BYTES,
                len: metadata,
            },
            0,
        )
    })
}

/// Imports listed on a review surface before it stops enumerating them.
///
/// The names themselves are excerpted where they are rendered, but the *count*
/// is a separate amplification: a legal 64 MiB module can carry millions of
/// tiny import entries, and formatting each into its own `String` turns the
/// artifact into hundreds of megabytes in the process that is trying to refuse
/// it. A review surface needs enough to recognise what a module reaches for,
/// not every repetition of it.
/// The most imports a module may declare.
///
/// A structural ceiling, checked before anything walks the import table. Every
/// import is resolved and retained *per instance*, and an instance is per
/// request, so a module repeating one allowlisted import a million times makes
/// every request pay for a million resolutions and hold the results — work no
/// fuel charge priced and storage `request_footprint_bytes` never counted.
/// Refusing the shape at load is what keeps that footprint the real bound.
///
/// It is checked *first* for a second reason: `forbidden_imports` builds one
/// owned denial per offending import, so an unbounded import table amplifies
/// through the refusal path too — including under `autumn plugin inspect`,
/// which runs it on artifacts nobody has audited. This ceiling does not weaken
/// that gate: a module hiding one forbidden import behind a million decoys is
/// still refused at load, now for its shape rather than its contents.
/// The most entries a module's sections may declare between them.
///
/// Checked from the section headers before the module is compiled, because
/// compilation is itself the allocation that needs bounding. Generous against
/// real output — a large Rust module declares tens of thousands of functions
/// and types — and far below what a 64 MiB file of one-byte declarations can
/// claim.
pub const MAX_DECLARED_ENTRIES: usize = 1_000_000;

pub const MAX_IMPORTS: usize = 1024;

const MAX_REPORTED_IMPORTS: usize = 256;

/// Format a module's imports for review, bounded in number.
///
/// The total is still reported honestly — an operator must not read a truncated
/// list as the whole of what an artifact imports — but only the first
/// [`MAX_REPORTED_IMPORTS`] are allocated. Counting the rest costs nothing: the
/// import table is already parsed, and walking it without formatting allocates
/// nothing per entry.
///
/// This bounds the *review* surface only. The load gate checks every import
/// against the shim's allowlist with no cap, because a module that hides a
/// forbidden import behind a million decoys must still be refused.
fn reported_imports<'a>(imports: impl Iterator<Item = wasmi::ImportType<'a>>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut total: usize = 0;
    for import in imports {
        total = total.saturating_add(1);
        if names.len() < MAX_REPORTED_IMPORTS {
            names.push(format!("{}::{}", import.module(), import.name()));
        }
    }
    if let Some(hidden) = total.checked_sub(names.len()).filter(|more| *more > 0) {
        names.push(format!("… and {hidden} more (truncated)"));
    }
    names
}

/// What encoding one request's frame costs, in fuel.
///
/// Priced off the request's own bytes at [`BYTES_PER_FUEL`], the rate every
/// other host-side copy pays, multiplied by the number of times those bytes are
/// walked on the way to the guest's stdin: cloned into the frame, base64
/// expanded, and serialised into JSON around it. That is the same factor the
/// manifest's footprint check counts those buffers at, so one number describes
/// both the memory and the CPU a request costs before it starts.
fn encoding_fuel(request: &SandboxRequest) -> u64 {
    /// Times the request's bytes are walked building the frame.
    const PASSES: u64 = 4;

    let bytes = request
        .body
        .len()
        .saturating_add(request_metadata_bytes(request));

    u64::try_from(bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(PASSES)
        .checked_div(BYTES_PER_FUEL)
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

/// Largest number of distinct denials recorded for one request.
///
/// The ledger is a diagnostic, and a guest that calls a denied import in a
/// loop must not be able to grow one. Denials are deduplicated by
/// `(capability, operation)` first, so hitting this bound at all means a guest
/// found more distinct refusals than the shim has functions.
const MAX_DENIALS: usize = 64;

// ── Denials ──────────────────────────────────────────────────────────────

/// The class of authority a guest reached for and did not get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DeniedCapability {
    /// Reading, writing, opening or discovering anything on a filesystem.
    Filesystem,
    /// Any outbound socket operation.
    Network,
    /// Environment variables and process arguments.
    Environment,
    /// Signalling, blocking or otherwise steering the host process.
    ProcessControl,
    /// An allocation over the plugin's declared memory ceiling.
    Memory,
    /// A response header a plugin is not allowed to set.
    ResponseHeader,
    /// An import no host function defines. Refused at load.
    UnknownImport,
}

impl DeniedCapability {
    /// Stable lowercase tag used in logs and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::Network => "network",
            Self::Environment => "environment",
            Self::ProcessControl => "process-control",
            Self::Memory => "memory",
            Self::ResponseHeader => "response-header",
            Self::UnknownImport => "unknown-import",
        }
    }
}

impl fmt::Display for DeniedCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing a guest reached for and was refused.
///
/// Denials are the *observable* half of deny-by-default: without them, a
/// sandbox that silently swallows a `path_open` and one that has a bug look
/// exactly alike from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CapabilityDenial {
    /// Which class of authority was refused.
    pub capability: DeniedCapability,
    /// The guest-visible operation, e.g. `path_open` or `autumn_db::query`.
    pub operation: String,
    /// What the guest was told, for the log.
    pub detail: String,
}

impl fmt::Display for CapabilityDenial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{capability}: {operation} — {detail}",
            capability = self.capability,
            operation = self.operation,
            detail = self.detail
        )
    }
}

// ── Failures ─────────────────────────────────────────────────────────────

/// Why a request produced no answer from the plugin.
///
/// Almost every variant is a *plugin* failure — none of those is a host
/// failure, and none can be anything other than a 5xx on the plugin's own
/// prefix. [`RequestBudget`](Self::RequestBudget) and
/// [`RequestMetadataBudget`](Self::RequestMetadataBudget) are the exceptions:
/// it is the *caller's* request that was refused, before the plugin was asked
/// anything, so they answer 413 the way the ceiling does everywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxFailure {
    /// The module could not be instantiated for this request.
    Instantiation(String),
    /// The guest burned its whole fuel budget without answering.
    FuelExhausted {
        /// The budget it was given.
        budget: u64,
    },
    /// The guest trapped.
    Trap(String),
    /// The guest called `proc_exit`.
    Exited(i32),
    /// The guest returned without answering.
    NoAnswer,
    /// The guest wrote what may well be a complete frame but never ended the
    /// line, so the host never saw one.
    PartialFrame,
    /// The guest wrote something that is not a frame this version knows.
    MalformedFrame(String),
    /// The guest reported its own failure.
    GuestError(String),
    /// The guest answered, but with something HTTP or the manifest refuses.
    ResponseRefused(String),
    /// The guest wrote more than its response ceiling without ending a line.
    OutputBudget {
        /// The ceiling it blew through.
        max: usize,
    },
    /// The plugin already has `max_concurrency` requests executing, so this
    /// one was not started.
    AtCapacity {
        /// The ceiling it ran into.
        max: usize,
    },
    /// The request handed to [`SandboxHost::run`] carried a body over the
    /// manifest's declared ceiling. The guest was never started.
    RequestBudget {
        /// The ceiling it blew through.
        max: usize,
        /// What the caller actually handed over.
        len: usize,
    },
    /// The request handed to [`SandboxHost::run`] carried more metadata — path,
    /// query, route, headers, path parameters — than the host's ceiling allows.
    /// The guest was never started.
    RequestMetadataBudget {
        /// The ceiling it blew through.
        max: usize,
        /// What the caller actually handed over.
        len: usize,
    },
}

impl SandboxFailure {
    /// The status this failure serves on the plugin's prefix.
    ///
    /// A budget exhaustion is a 504: the plugin was given a deadline and missed
    /// it. An oversized request is a 413, the same answer the adapter gives
    /// before it ever gets here — one condition must not have two statuses
    /// depending on which door the request came through. Everything else is a
    /// 502: the plugin answered badly or not at all, which is exactly what a
    /// bad gateway is.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self {
            Self::FuelExhausted { .. } | Self::OutputBudget { .. } => {
                http::StatusCode::GATEWAY_TIMEOUT
            }
            Self::RequestBudget { .. } | Self::RequestMetadataBudget { .. } => {
                http::StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::AtCapacity { .. } => http::StatusCode::SERVICE_UNAVAILABLE,
            _ => http::StatusCode::BAD_GATEWAY,
        }
    }
}

impl fmt::Display for SandboxFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Instantiation(detail) => {
                write!(f, "the plugin could not be instantiated: {detail}")
            }
            Self::FuelExhausted { budget } => write!(
                f,
                "the plugin exhausted its {budget}-unit CPU budget without answering"
            ),
            Self::Trap(detail) => write!(f, "the plugin trapped: {detail}"),
            Self::Exited(code) => write!(f, "the plugin called proc_exit({code})"),
            Self::NoAnswer => write!(f, "the plugin returned without answering"),
            Self::PartialFrame => write!(
                f,
                "the plugin wrote a partial frame with no terminating newline; a frame is one \
                 NDJSON line, so it must end with `\\n` (use `println!`, not `print!`)"
            ),
            Self::MalformedFrame(detail) => {
                write!(f, "the plugin wrote a malformed frame: {detail}")
            }
            Self::GuestError(detail) => write!(f, "the plugin reported a failure: {detail}"),
            Self::ResponseRefused(detail) => {
                write!(f, "the plugin's answer was refused: {detail}")
            }
            Self::OutputBudget { max } => write!(
                f,
                "the plugin wrote more than its {max}-byte response ceiling without ending a frame"
            ),
            Self::AtCapacity { max } => write!(
                f,
                "the plugin already has its {max} permitted requests executing"
            ),
            Self::RequestBudget { max, len } => write!(
                f,
                "the request body is {len} bytes, over the plugin's {max}-byte request ceiling"
            ),
            Self::RequestMetadataBudget { max, len } => write!(
                f,
                "the request's path, query, headers and parameters are {len} bytes, over the \
                 host's {max}-byte ceiling"
            ),
        }
    }
}

impl std::error::Error for SandboxFailure {}

/// Why an artifact could not be loaded at all.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxLoadError {
    /// The manifest handed to [`SandboxHost::from_module`] does not satisfy the
    /// rules parsing enforces — it was built or mutated rather than parsed.
    InvalidManifest(String),
    /// The bytes are not a WebAssembly module this engine can compile.
    Wasm(String),
    /// The module imports something no host function defines.
    ForbiddenImports(Vec<CapabilityDenial>),
    /// The manifest's fuel budget cannot cover what every request must spend
    /// before the guest runs at all, so no route it declares could ever answer.
    FuelBelowFixedCharges {
        /// The budget the manifest declares.
        fuel: u64,
        /// What instantiating this module costs, every request, before `_start`.
        instantiation: u64,
    },
    /// The module's data or element segments would make every request's
    /// instantiation expensive, in host work no fuel budget prices.
    InstantiationTooExpensive {
        /// What was counted.
        what: &'static str,
        /// How many the module carries.
        found: usize,
        /// The ceiling.
        max: usize,
    },
    /// The module exports no `_start` of type `() -> ()`.
    MissingStart,
    /// The module exports no linear memory named `memory`.
    MissingMemory,
    /// The module's *initial* linear memory is already over the manifest's
    /// ceiling, so no request could ever instantiate it.
    MemoryTooLarge {
        /// The module's initial memory, in bytes.
        found: u64,
        /// The manifest's ceiling.
        max: usize,
    },
    /// The engine could not be configured.
    Engine(String),
}

impl fmt::Display for SandboxLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest(detail) => {
                write!(f, "the manifest is not one this host will serve: {detail}")
            }
            Self::Wasm(detail) => write!(f, "the plugin module could not be loaded: {detail}"),
            Self::ForbiddenImports(denials) => write!(
                f,
                "the plugin imports {count} host function(s) the sandbox does not provide, so it \
                 is refused before it runs: {list}",
                count = denials.len(),
                list = denials
                    .iter()
                    .map(|denial| denial.operation.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::FuelBelowFixedCharges {
                fuel,
                instantiation,
            } => write!(
                f,
                "the plugin's {fuel}-unit fuel budget cannot cover the {instantiation} units \
                 instantiating this module costs before `_start` runs, so every request would \
                 exhaust the budget without the guest executing an instruction"
            ),
            Self::InstantiationTooExpensive { what, found, max } => write!(
                f,
                "the plugin declares {found} {what}, over the {max} ceiling: every request \
                 re-instantiates the module, and that work happens before the first guest \
                 instruction — so it is bounded here rather than priced per request"
            ),
            Self::MissingStart => write!(
                f,
                "the plugin exports no `_start` of type `() -> ()`; it must be built as a \
                 wasm32-wasip1 *command*"
            ),
            Self::MissingMemory => write!(
                f,
                "the plugin exports no linear memory named `memory`; every host function reads \
                 and writes through it, so a plugin without one can never answer"
            ),
            Self::MemoryTooLarge { found, max } => write!(
                f,
                "the plugin's initial linear memory is {found} bytes, over the manifest's \
                 {max}-byte ceiling; no request could instantiate it"
            ),
            Self::Engine(detail) => write!(f, "the sandbox engine could not be built: {detail}"),
        }
    }
}

impl std::error::Error for SandboxLoadError {}

// ── The outcome ──────────────────────────────────────────────────────────

/// Everything one request produced: the answer or the failure, plus the
/// evidence.
#[derive(Debug)]
#[non_exhaustive]
pub struct SandboxOutcome {
    /// The plugin's answer, or why there is none.
    pub result: Result<SandboxResponse, SandboxFailure>,
    /// Everything the guest reached for and did not get.
    pub denials: Vec<CapabilityDenial>,
    /// Fuel the guest consumed.
    pub fuel_used: u64,
    /// The high-water mark of the guest's linear memory, in bytes.
    pub peak_memory_bytes: usize,
    /// What the guest wrote to stderr, truncated.
    pub stderr: String,
}

impl SandboxOutcome {
    /// A request refused before the guest produced anything: no denials, no
    /// peak, no stderr, because nothing of the guest's ever ran.
    ///
    /// `fuel_used` is what the *host* had already committed on its behalf — the
    /// whole budget when a charge could not be covered, zero when the request
    /// was turned away before it was priced.
    const fn refused(failure: SandboxFailure, fuel_used: u64) -> Self {
        Self {
            result: Err(failure),
            denials: Vec::new(),
            fuel_used,
            peak_memory_bytes: 0,
            stderr: String::new(),
        }
    }
}

// ── The host ─────────────────────────────────────────────────────────────

/// A compiled sandboxed plugin, ready to serve requests.
///
/// Compilation happens once in [`load`](SandboxHost::load); every call to
/// [`run`](SandboxHost::run) builds a *fresh* store and instance, so no state
/// survives a request and one request's misbehaviour cannot reach the next.
pub struct SandboxHost {
    engine: Engine,
    module: Module,
    /// One permit per concurrently-executing request.
    ///
    /// `SandboxedPlugin::serve` has a semaphore of its own, which it holds
    /// across the body read as well as the run, so the declared footprint
    /// bounds the whole request rather than only the part a guest is running.
    /// This one is narrower and lives here because [`run`](SandboxHost::run)
    /// is public: an embedder calling it directly never passes through
    /// `serve`, and `request_footprint_bytes() × max_concurrency` is the
    /// premise the manifest validator accepts limits on.
    ///
    /// The two never fight. At most `max_concurrency` requests are in `serve`
    /// at once, so they can hold at most that many of these — this semaphore
    /// is only ever the binding constraint on a direct caller, which is the
    /// one it exists for.
    permits: Semaphore,
    /// What one instantiation of this module costs, in fuel. Bounded at load by
    /// [`MAX_INIT_SEGMENTS`] and [`MAX_INIT_SECTION_BYTES`]; charged per request
    /// so the declared CPU ceiling prices it — see [`SandboxHost::run`].
    instantiation_fuel: u64,
    manifest: SandboxManifest,
}

impl fmt::Debug for SandboxHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxHost")
            .field("plugin", &self.manifest.name)
            .field("prefix", &self.manifest.prefix)
            .finish_non_exhaustive()
    }
}

impl SandboxHost {
    /// Compile a verified artifact into a runnable host.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxLoadError`] if the module does not compile, imports
    /// something the sandbox does not provide, or exports no `_start`.
    pub fn load(artifact: &SandboxArtifact) -> Result<Self, SandboxLoadError> {
        Self::from_module(artifact.manifest().clone(), artifact.module())
    }

    /// Compile a module against a manifest.
    ///
    /// Prefer [`load`](Self::load), which also proves the manifest describes
    /// *these* bytes.
    ///
    /// The manifest is re-validated here rather than trusted. Parsing is where
    /// a manifest's rules are enforced, but [`SandboxManifest`]'s fields are
    /// public and this constructor is too, so "it was parsed once" is an
    /// invariant a caller can step around by building or editing one. The
    /// values that matter are the ones something downstream would *panic* on
    /// rather than merely misbehave over — a `max_concurrency` past the
    /// semaphore's ceiling, a route path axum refuses to build — and this crate
    /// does not panic on plugin input.
    ///
    /// # Errors
    ///
    /// See [`load`](Self::load), plus
    /// [`InvalidManifest`](SandboxLoadError::InvalidManifest) if the manifest
    /// does not satisfy the rules parsing enforces.
    pub fn from_module(manifest: SandboxManifest, wasm: &[u8]) -> Result<Self, SandboxLoadError> {
        manifest
            .validate()
            .map_err(|err| SandboxLoadError::InvalidManifest(err.to_string()))?;

        // Before `Module::new`, deliberately: compiling is what allocates a
        // representation of every declaration, so a ceiling checked afterwards
        // is a ceiling checked too late.
        let shape = refuse_unbounded_shape(wasm)?;

        let mut config = Config::default();
        // Fuel metering is what turns "a plugin might loop forever" into "a
        // plugin gets a bounded number of instructions". It must be on before
        // the module is compiled, because the counting is compiled in.
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).map_err(|err| SandboxLoadError::Wasm(err.to_string()))?;

        let import_count = module.imports().count();
        if import_count > MAX_IMPORTS {
            return Err(SandboxLoadError::InstantiationTooExpensive {
                what: "imports",
                found: import_count,
                max: MAX_IMPORTS,
            });
        }

        let forbidden = forbidden_imports(&module);
        if !forbidden.is_empty() {
            return Err(SandboxLoadError::ForbiddenImports(forbidden));
        }

        let (segments, init_bytes) = (shape.segments, shape.init_bytes);
        if segments > MAX_INIT_SEGMENTS {
            return Err(SandboxLoadError::InstantiationTooExpensive {
                what: "data and element segments",
                found: segments,
                max: MAX_INIT_SEGMENTS,
            });
        }
        if init_bytes > MAX_INIT_SECTION_BYTES {
            return Err(SandboxLoadError::InstantiationTooExpensive {
                what: "bytes of data and element sections",
                found: init_bytes,
                max: MAX_INIT_SECTION_BYTES,
            });
        }
        // A budget that cannot cover instantiation is a manifest whose every
        // route is already broken: the charge is unavoidable and paid before
        // `_start`, so the guest never executes an instruction. Refusing at
        // load is what makes `autumn plugin inspect` mean something — a passing
        // verdict on an artifact that can only ever answer 504 is worse than no
        // verdict, because an operator installs on the strength of it.
        //
        // Compared against instantiation alone rather than the whole per-request
        // cost: the frame encoding varies with the request, so there is no
        // single number to check it against here, while this charge is fixed by
        // the module and known now.
        let instantiation =
            instantiation_fuel(segments, init_bytes, import_count, shape.global_count);
        if manifest.limits.fuel <= instantiation {
            return Err(SandboxLoadError::FuelBelowFixedCharges {
                fuel: manifest.limits.fuel,
                instantiation,
            });
        }

        // Not merely "some function called `_start`": the host looks it up as
        // `() -> ()`, so a `_start` with parameters or results is a module that
        // loads and then fails on every request.
        let start_is_callable = module.exports().any(|export| {
            export.name() == "_start"
                && export
                    .ty()
                    .func()
                    .is_some_and(|ty| ty.params().is_empty() && ty.results().is_empty())
        });
        if !start_is_callable {
            return Err(SandboxLoadError::MissingStart);
        }

        // Every shim function reads and writes through an export named
        // `memory`; without one they all answer `EINVAL` and the plugin can
        // never serve a request. And a module whose *initial* memory is already
        // over the manifest's ceiling is refused by the limiter at
        // instantiation — per request, as a gateway error, when it could have
        // been said once here.
        let memory = module
            .exports()
            .find(|export| export.name() == "memory")
            .and_then(|export| export.ty().memory().copied());
        let Some(memory) = memory else {
            return Err(SandboxLoadError::MissingMemory);
        };
        let initial_bytes =
            u64::from(u32::from(memory.initial_pages())).saturating_mul(WASM_PAGE_BYTES);
        if initial_bytes > manifest.limits.memory_bytes as u64 {
            return Err(SandboxLoadError::MemoryTooLarge {
                found: initial_bytes,
                max: manifest.limits.memory_bytes,
            });
        }

        // The same argument as the memory ceiling above, for the other
        // per-instance store the limiter guards. `table_growing` refuses a
        // total past `MAX_TABLE_ELEMENTS` at instantiation — which is per
        // request — so a module whose tables are *already* over it at rest
        // loads cleanly and then fails every request it is ever given. Said
        // once here, it is a verdict `autumn plugin inspect` can be trusted on.
        if shape.table_count > MAX_TABLES {
            return Err(SandboxLoadError::InstantiationTooExpensive {
                what: "tables",
                found: shape.table_count,
                max: MAX_TABLES,
            });
        }
        if shape.table_elements > u64::from(MAX_TABLE_ELEMENTS) {
            return Err(SandboxLoadError::InstantiationTooExpensive {
                what: "initial table elements",
                found: usize::try_from(shape.table_elements).unwrap_or(usize::MAX),
                max: MAX_TABLE_ELEMENTS as usize,
            });
        }

        Ok(Self {
            engine,
            module,
            permits: Semaphore::new(manifest.limits.max_concurrency),
            instantiation_fuel: instantiation,
            manifest,
        })
    }

    /// The manifest this host enforces.
    #[must_use]
    pub const fn manifest(&self) -> &SandboxManifest {
        &self.manifest
    }

    /// Every import a module declares, as `module::name`, without loading it.
    ///
    /// The review surface for an artifact the sandbox *refuses*: what it wanted
    /// is the whole reason it was refused, so a consent screen must be able to
    /// show it.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxLoadError::Wasm`] if the bytes are not a module.
    pub fn imports_of(wasm: &[u8]) -> Result<Vec<String>, SandboxLoadError> {
        // `inspect` calls this on artifacts nobody has audited, and on the ones
        // the sandbox is about to refuse — so the review surface must not be
        // the way an artifact exhausts the process reviewing it.
        refuse_unbounded_shape(wasm)?;
        let engine = Engine::default();
        let module =
            Module::new(&engine, wasm).map_err(|err| SandboxLoadError::Wasm(err.to_string()))?;
        Ok(reported_imports(module.imports()))
    }

    /// What one request pays, in fuel, just to instantiate this module.
    ///
    /// Bounded at load; charged per request. Surfaced so a packaging tool can
    /// show an author what their module costs before it costs anyone else.
    #[must_use]
    pub const fn instantiation_fuel(&self) -> u64 {
        self.instantiation_fuel
    }

    /// Every import the module declares, as `module::name`, for review.
    #[must_use]
    pub fn imports(&self) -> Vec<String> {
        reported_imports(self.module.imports())
    }

    /// Serve one request.
    ///
    /// This is synchronous and CPU-bound by design — it is an interpreter loop.
    /// Callers on an async runtime must dispatch it to a blocking worker; the
    /// [`plugin`](super::plugin) module does exactly that.
    ///
    /// Never panics and never fails: a plugin that misbehaves in any way
    /// produces an [`SandboxOutcome`] whose `result` is `Err`.
    #[must_use]
    pub fn run(&self, request: &SandboxRequest) -> SandboxOutcome {
        let limits = self.manifest.limits;
        // Bounds where the response may redirect a client; see `sanitize`.
        let prefix = self.manifest.prefix.as_str();

        if let Some(refusal) = refuse_oversized_request(request, limits) {
            return refusal;
        }

        // Admission, before a single buffer is built. `serve` holds a permit of
        // its own across the whole request, so this is never what stops an
        // ordinary HTTP request — it is here for the embedder who calls this
        // public method directly and would otherwise start as many instances
        // as it liked, against a footprint the manifest validator accepted on
        // the premise that `max_concurrency` bounds them.
        //
        // Held for the body of the run: dropping the guard on the way out is
        // what makes the permit mean "executing now".
        let Ok(_permit) = self.permits.try_acquire() else {
            return SandboxOutcome::refused(
                SandboxFailure::AtCapacity {
                    max: limits.max_concurrency,
                },
                0,
            );
        };

        // Building the guest's stdin is host work proportional to the request:
        // the body is cloned into the frame and base64-expanded into the NDJSON
        // line, all of it before a single guest instruction runs. Unpriced,
        // that is megabytes of host CPU per request for a manifest that
        // declares a large body ceiling and almost no fuel — and a client can
        // repeat it for as long as it likes.
        //
        // So it is charged at the same rate as every other host-side copy, and
        // charged *before* the copies happen: a budget that cannot cover the
        // encoding refuses without doing it. The guest then starts on what is
        // left, which is the same arrangement instantiation already has.
        let encoding = encoding_fuel(request);
        let Some(after_encoding) = limits.fuel.checked_sub(encoding) else {
            return SandboxOutcome::refused(
                SandboxFailure::FuelExhausted {
                    budget: limits.fuel,
                },
                limits.fuel,
            );
        };

        let granted: Vec<SandboxCapability> = self.manifest.capabilities.clone();
        let frame = HostFrame::request(request, &granted);

        let line = match to_line(&frame) {
            Ok(line) => line,
            Err(err) => {
                // The host could not encode its own request. Report it as a
                // plugin-prefix failure rather than propagating: the rest of
                // the application is unaffected either way.
                return SandboxOutcome::refused(
                    SandboxFailure::Instantiation(guest_text(&err.to_string())),
                    0,
                );
            }
        };
        // The frame's bytes are *moved* into the queue rather than copied into
        // it: `VecDeque::from(Vec<u8>)` reuses the allocation, and the `String`
        // is gone afterwards. For a plugin with a large body ceiling that is a
        // whole base64-expanded copy of the request that no longer exists at the
        // same time as the others.
        let mut state = HostState::new(self.manifest.name.clone(), limits, line.as_bytes());
        state.stdin = VecDeque::from(line.into_bytes());

        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limiter);
        // Set before instantiation, so the budget is in place the moment the
        // first guest instruction runs.
        if let Err(err) = store.set_fuel(after_encoding) {
            return SandboxOutcome::refused(
                SandboxFailure::Instantiation(guest_text(&err.to_string())),
                0,
            );
        }

        // wasmi does not meter instantiation: every request copies the module's
        // data and element segments before `_start` runs. Two things bound it,
        // and it needs both. The *ceiling* is at load — a module whose segments
        // are structurally expensive is refused outright, because a per-request
        // charge cannot stop work that has already been admitted. The *price* is
        // here, so the work that is admitted still comes out of the budget the
        // manifest declared rather than being free.
        let Some(left) = after_encoding.checked_sub(self.instantiation_fuel) else {
            let failure = SandboxFailure::FuelExhausted {
                budget: limits.fuel,
            };
            return finish(store, limits, prefix, Err(failure));
        };
        if let Err(err) = store.set_fuel(left) {
            return SandboxOutcome::refused(
                SandboxFailure::Instantiation(guest_text(&err.to_string())),
                0,
            );
        }

        let mut linker = <Linker<HostState>>::new(&self.engine);
        if let Err(err) = define_wasi_shim(&mut linker) {
            return SandboxOutcome::refused(
                SandboxFailure::Instantiation(guest_text(&err.to_string())),
                0,
            );
        }

        let started = linker
            .instantiate(&mut store, &self.module)
            .and_then(|pre| pre.start(&mut store));
        let instance = match started {
            Ok(instance) => instance,
            Err(err) => {
                return finish(
                    store,
                    limits,
                    prefix,
                    Err(instantiation_failure(&err, limits)),
                );
            }
        };

        let Ok(start) = instance.get_typed_func::<(), ()>(&store, "_start") else {
            // `from_module` already refused a module without `_start`; reaching
            // here would mean the export changed shape, which is still the
            // plugin's problem and not the host's.
            return finish(
                store,
                limits,
                &self.manifest.prefix,
                Err(SandboxFailure::NoAnswer),
            );
        };

        let trap = start.call(&mut store, ()).err();
        let partial = !store.data().stdout_line.is_empty();
        let result = match (store.data().answer.clone(), trap) {
            // A guest that answered and *then* trapped still answered: the
            // first frame is the answer, and everything after it — including
            // the host's own `AnswerComplete` unwind — is noise after the fact.
            (Some(answer), _) => answer,
            (None, Some(err)) => Err(guest_failure(&err, limits)),
            (None, None) if partial => Err(SandboxFailure::PartialFrame),
            (None, None) => Err(SandboxFailure::NoAnswer),
        };
        finish(store, limits, prefix, result)
    }
}

/// Drain the store into an outcome, applying the response sanitation and the
/// size ceiling that only the host can enforce.
fn finish(
    store: Store<HostState>,
    limits: ResourceLimits,
    // The plugin's declared prefix, which bounds where its response may
    // redirect a client. See `SandboxResponse::sanitize`.
    prefix: &str,
    result: Result<SandboxResponse, SandboxFailure>,
) -> SandboxOutcome {
    let fuel_used = store
        .get_fuel()
        .map_or(limits.fuel, |left| limits.fuel.saturating_sub(left));
    let mut state = store.into_data();
    let peak_memory_bytes = state.limiter.peak;
    if state.limiter.refusals > 0 {
        let detail = format!(
            "{count} allocation(s) over the plugin's {max}-byte memory ceiling were refused",
            count = state.limiter.refusals,
            max = limits.memory_bytes,
        );
        state.deny(DeniedCapability::Memory, "memory.grow", &detail);
    }

    let result = match result {
        Ok(response) => {
            let (response, denied) = response.sanitize(prefix);
            for name in denied {
                // The name is the guest's, so it is as long and as hostile as
                // the guest cares to make it — bounded only by the stdout line
                // ceiling. `deny` clones it into the ledger and logs it, and a
                // name is not validated before it gets here (that it is invalid
                // is often *why* it was denied), so it can carry newlines and
                // terminal escapes too. Same treatment as every other
                // guest-influenced string that reaches a log.
                state.deny(
                    DeniedCapability::ResponseHeader,
                    &guest_text(&name),
                    "a sandboxed plugin may not set this response header",
                );
            }
            response.refused_content_type().map_or_else(
                || {
                    response
                        .validate()
                        .and_then(|()| response.check_size(limits.max_response_bytes))
                        .map_or_else(
                            |err| {
                                Err(SandboxFailure::ResponseRefused(guest_text(
                                    &err.to_string(),
                                )))
                            },
                            |()| Ok(response.clone()),
                        )
                },
                |essence| {
                    // `refused_content_type` returns everything before the
                    // first `;`, so a guest that writes no parameter at all
                    // hands back its whole header value — bounded whether it
                    // likes it or not by the stdout ceiling, which is
                    // megabytes. Both strings built here are logged (the
                    // denial by `deny`, the failure by `serve`), so the
                    // guest's text gets the same cap and control-escaping
                    // every other guest-influenced string gets. The branch
                    // beside this one already did; this one did not.
                    let essence = guest_text(&essence);
                    let detail = format!(
                        "a sandboxed plugin may not serve `{essence}`: a document or a script \
                         from the host's own origin would carry the host's authority"
                    );
                    state.deny(DeniedCapability::ResponseHeader, "content-type", &detail);
                    Err(SandboxFailure::ResponseRefused(
                        super::wire::WireError::UnsupportedContentType(essence).to_string(),
                    ))
                },
            )
        }
        Err(failure) => Err(failure),
    };

    let stderr = state.stderr_excerpt();
    SandboxOutcome {
        result,
        denials: state.denials,
        fuel_used,
        peak_memory_bytes,
        stderr,
    }
}

fn instantiation_failure(err: &wasmi::Error, limits: ResourceLimits) -> SandboxFailure {
    if err.as_trap_code() == Some(wasmi::core::TrapCode::OutOfFuel) {
        SandboxFailure::FuelExhausted {
            budget: limits.fuel,
        }
    } else {
        SandboxFailure::Instantiation(guest_text(&err.to_string()))
    }
}

fn guest_failure(err: &wasmi::Error, limits: ResourceLimits) -> SandboxFailure {
    if let Some(code) = err.i32_exit_status() {
        return SandboxFailure::Exited(code);
    }
    if err.as_trap_code() == Some(wasmi::core::TrapCode::OutOfFuel) {
        return SandboxFailure::FuelExhausted {
            budget: limits.fuel,
        };
    }
    if err.downcast_ref::<OutputBudgetExhausted>().is_some() {
        return SandboxFailure::OutputBudget {
            max: limits.max_response_bytes,
        };
    }
    SandboxFailure::Trap(guest_text(&err.to_string()))
}

/// What a module's section headers declare, without compiling it.
///
/// Walks the top-level sections directly. wasmi's public API does not expose
/// segments, and these are the numbers that have to be bounded at load: the
/// alternative is discovering them per request, in host work no budget prices.
///
/// Returns `None` for bytes that are not a well-formed section stream. This now
/// runs *before* wasmi compiles the module, so malformed bytes reach it — and
/// refusing them is the right answer.
#[derive(Debug, Clone, Copy)]
struct ModuleShape {
    /// Data and element segments, which every instantiation copies.
    segments: usize,
    /// Bytes those sections hold.
    init_bytes: usize,
    /// Entries every section *declares*, summed.
    ///
    /// Read from each section's leading count, so this costs one LEB128 per
    /// section rather than a walk over the entries themselves — the point is to
    /// know the shape before anything allocates per entry.
    declared_entries: usize,
    /// Bytes of instruction stream in the code section.
    code_bytes: usize,
    /// How many globals the module declares.
    global_count: usize,
    /// How many tables the module declares.
    ///
    /// Sizes and count are separate ceilings: five empty tables cost no
    /// elements at all and still exceed what the store will build.
    table_count: usize,
    /// Elements the module's own tables declare as their *initial* size.
    ///
    /// The limiter enforces `MAX_TABLE_ELEMENTS` at instantiation, which is
    /// per request; knowing the declared total at load is what turns "every
    /// request fails" into "this artifact does not load".
    table_elements: u64,
}

fn module_shape(wasm: &[u8]) -> Option<ModuleShape> {
    /// Read an unsigned LEB128 at `at`, returning the value and the next offset.
    fn leb128(wasm: &[u8], at: usize) -> Option<(usize, usize)> {
        let mut value: usize = 0;
        let mut shift: u32 = 0;
        let mut cursor = at;
        loop {
            let byte = *wasm.get(cursor)?;
            cursor = cursor.checked_add(1)?;
            value = value.checked_add(usize::from(byte & 0x7f).checked_shl(shift)?)?;
            if byte & 0x80 == 0 {
                return Some((value, cursor));
            }
            shift = shift.checked_add(7)?;
            if shift > 63 {
                return None;
            }
        }
    }

    /// Section ids for the element and data sections.
    const ELEMENT_SECTION: u8 = 9;
    const DATA_SECTION: u8 = 11;
    /// The table section, whose entries carry the initial sizes the limiter
    /// will be asked to admit.
    const TABLE_SECTION: u8 = 4;
    /// The global section, whose entries each become per-instance storage.
    const GLOBAL_SECTION: u8 = 6;
    /// The code section, whose *size* is the instruction volume the compiler
    /// walks — a thing no entry count reveals.
    const CODE_SECTION: u8 = 10;
    /// The custom section is the one whose payload is not a counted vector.
    const CUSTOM_SECTION: u8 = 0;

    let mut cursor = 8usize; // magic + version
    let mut segments = 0usize;
    let mut bytes = 0usize;
    let mut declared_entries = 0usize;
    let mut table_elements = 0u64;
    let mut table_count = 0usize;
    let mut global_count = 0usize;
    let mut code_bytes = 0usize;
    while cursor < wasm.len() {
        let id = *wasm.get(cursor)?;
        let (size, after_size) = leb128(wasm, cursor.checked_add(1)?)?;
        let end = after_size.checked_add(size)?;
        if end > wasm.len() {
            return None;
        }
        if id != CUSTOM_SECTION {
            let (count, _) = leb128(wasm, after_size)?;
            declared_entries = declared_entries.saturating_add(count);
        }
        if id == ELEMENT_SECTION || id == DATA_SECTION {
            let (count, _) = leb128(wasm, after_size)?;
            segments = segments.saturating_add(count);
            bytes = bytes.saturating_add(size);
        }
        if id == CODE_SECTION {
            code_bytes = code_bytes.saturating_add(size);
        }
        if id == GLOBAL_SECTION {
            let (count, _) = leb128(wasm, after_size)?;
            global_count = global_count.saturating_add(count);
        }
        if id == TABLE_SECTION {
            // `vec(tabletype)`, and a `tabletype` is a reftype byte then
            // `limits`: a flag, the minimum, and a maximum when the flag says
            // so. Only the minimum matters here — it is what the instance
            // allocates before the guest runs.
            let (count, mut at) = leb128(wasm, after_size)?;
            table_count = table_count.saturating_add(count);
            for _ in 0..count {
                at = at.checked_add(1)?; // reftype
                let flag = *wasm.get(at)?;
                at = at.checked_add(1)?;
                let (minimum, next) = leb128(wasm, at)?;
                at = next;
                table_elements = table_elements.saturating_add(minimum as u64);
                if flag == 0x01 {
                    let (_, after_max) = leb128(wasm, at)?;
                    at = after_max;
                }
                if at > end {
                    return None;
                }
            }
        }
        cursor = end;
    }
    Some(ModuleShape {
        segments,
        init_bytes: bytes,
        declared_entries,
        code_bytes,
        global_count,
        table_count,
        table_elements,
    })
}

/// Refuse a module whose declared shape would cost more to *compile* than the
/// file's own size suggests.
///
/// `Module::new` is the first thing that touches an unaudited artifact's bytes,
/// and it builds an in-memory representation of every type, function, export
/// and global before any ceiling here has run. A legal 64 MiB file of tiny
/// declarations expands many times over in that representation, so the file
/// bound is not a bound on what compiling it costs. The section headers carry
/// the counts, so the shape is knowable *first* — one LEB128 per section, no
/// per-entry work — which is what makes refusing it cheap enough to do before
/// wasmi ever sees the bytes.
fn refuse_unbounded_shape(wasm: &[u8]) -> Result<ModuleShape, SandboxLoadError> {
    let Some(shape) = module_shape(wasm) else {
        return Err(SandboxLoadError::Wasm(
            "the module's section stream could not be walked".to_owned(),
        ));
    };
    if shape.code_bytes > MAX_CODE_BYTES {
        return Err(SandboxLoadError::InstantiationTooExpensive {
            what: "code section bytes",
            found: shape.code_bytes,
            max: MAX_CODE_BYTES,
        });
    }
    if shape.global_count > MAX_GLOBALS {
        return Err(SandboxLoadError::InstantiationTooExpensive {
            what: "globals",
            found: shape.global_count,
            max: MAX_GLOBALS,
        });
    }
    if shape.declared_entries > MAX_DECLARED_ENTRIES {
        return Err(SandboxLoadError::InstantiationTooExpensive {
            what: "declared section entries",
            found: shape.declared_entries,
            max: MAX_DECLARED_ENTRIES,
        });
    }
    Ok(shape)
}

/// Imports the shim will not satisfy — by name **or** by type — as denials.
///
/// Checking the type here rather than letting `Linker::instantiate` discover it
/// is what keeps `autumn plugin package` honest: a module importing an
/// allowlisted name with the wrong signature would otherwise pass packaging and
/// inspection, then fail on every request as a gateway error nobody can explain
/// from the outside.
fn forbidden_imports(module: &Module) -> Vec<CapabilityDenial> {
    module
        .imports()
        .filter_map(|import| {
            let operation = format!("{}::{}", import.module(), import.name());
            if import.module() != WASI {
                return Some((operation, "the sandbox defines no such host module"));
            }
            let Some(declared) = import.ty().func() else {
                return Some((
                    operation,
                    "the sandbox provides host functions only, not memories, tables or globals",
                ));
            };
            let Some((params, results)) = shim_signature(import.name()) else {
                return Some((operation, "the sandbox defines no such host function"));
            };
            match signature_type(params, results) {
                Some(expected) if expected == *declared => None,
                _ => Some((
                    operation,
                    "the sandbox defines this host function with a different signature, so it \
                     could never link",
                )),
            }
        })
        .map(|(operation, why)| CapabilityDenial {
            capability: DeniedCapability::UnknownImport,
            operation,
            detail: format!("{why}, so the plugin is refused before it runs"),
        })
        .take(MAX_DENIALS)
        .collect()
}

/// A guest blew its output budget. Carried as a host error so the trap it
/// causes can be told apart from any other trap.
#[derive(Debug)]
struct OutputBudgetExhausted;

/// The guest answered, so there is nothing left for it to do. Carried as a host
/// error to unwind the interpreter at the frame rather than letting a guest
/// hold a permit and a blocking worker for its whole budget after the exchange
/// is over.
#[derive(Debug)]
struct AnswerComplete;

impl fmt::Display for AnswerComplete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the plugin answered")
    }
}

impl wasmi::core::HostError for AnswerComplete {}

impl fmt::Display for OutputBudgetExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the plugin exceeded its response ceiling")
    }
}

impl wasmi::core::HostError for OutputBudgetExhausted {}

// ── Host state ───────────────────────────────────────────────────────────

/// The guest's memory ceiling, and the evidence that it was applied.
#[derive(Debug)]
struct MemoryLimiter {
    max: usize,
    peak: usize,
    refusals: usize,
    /// Elements held across every table of this instance, so the ceiling is on
    /// the instance rather than on each table separately.
    table_elements: u32,
}

impl wasmi::ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmi::errors::MemoryError> {
        if desired > self.max {
            self.refusals = self.refusals.saturating_add(1);
            // `Ok(false)` makes the guest's `memory.grow` return -1, which is a
            // legal outcome a well-written allocator handles. Returning `Err`
            // would trap instead — a harsher answer that tells a hostile guest
            // less and an honest one nothing useful.
            return Ok(false);
        }
        self.peak = self.peak.max(desired);
        Ok(true)
    }

    fn table_growing(
        &mut self,
        current: u32,
        desired: u32,
        _maximum: Option<u32>,
    ) -> Result<bool, wasmi::errors::TableError> {
        // Across the instance, not per table: four tables each under a per-table
        // ceiling would still be four times that ceiling of host storage, which
        // is exactly the accounting hole this closes.
        let total = self
            .table_elements
            .saturating_sub(current)
            .saturating_add(desired);
        if total > MAX_TABLE_ELEMENTS {
            self.refusals = self.refusals.saturating_add(1);
            return Ok(false);
        }
        self.table_elements = total;
        Ok(true)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        1
    }
}

struct HostState {
    /// The plugin's name, so a denial line names who was refused. Carried here
    /// rather than logged by the caller so the refusal is logged exactly once,
    /// at the point it happens, whoever is driving the host.
    plugin: String,
    /// Bytes of the request frame the guest has not read yet.
    stdin: VecDeque<u8>,
    /// The partial stdout line being accumulated.
    stdout_line: Vec<u8>,
    /// Everything the guest wrote to stderr, bounded.
    stderr: Vec<u8>,
    /// The first terminal frame the guest produced.
    answer: Option<Result<SandboxResponse, SandboxFailure>>,
    /// Everything the guest reached for and did not get.
    denials: Vec<CapabilityDenial>,
    /// Request-seeded PRNG state, so `random_get` is deterministic without
    /// being a published constant.
    random_state: u64,
    limiter: MemoryLimiter,
    limits: ResourceLimits,
}

impl HostState {
    /// The starting point the request is mixed into. Arbitrary but fixed.
    const RANDOM_SEED: u64 = 0x2545_F491_4F6C_DD1D;

    /// Derive this request's PRNG seed from the request itself.
    ///
    /// A single fixed seed would make `random_get` a *constant* across every
    /// request to every deployment of an artifact — anyone holding the same
    /// bytes could predict every value a guest ever derived from it. Mixing the
    /// request in keeps the property that actually matters (the same request
    /// twice produces the same bytes, so an author can reproduce a bug from the
    /// request alone) without publishing the stream.
    ///
    /// This is **not** cryptographic entropy, and the sandbox offers none: a
    /// guest holds no capability that would make a secret useful to it.
    fn seed_from(frame: &[u8]) -> u64 {
        let mut seed = Self::RANDOM_SEED;
        for byte in frame {
            seed = seed.rotate_left(7) ^ u64::from(*byte);
            seed = seed.wrapping_mul(0x0000_0100_0000_01B3);
        }
        seed
    }

    fn new(plugin: String, limits: ResourceLimits, frame: &[u8]) -> Self {
        Self {
            plugin,
            stdin: VecDeque::new(),
            stdout_line: Vec::new(),
            stderr: Vec::new(),
            answer: None,
            denials: Vec::new(),
            random_state: Self::seed_from(frame),
            limiter: MemoryLimiter {
                max: limits.memory_bytes,
                peak: 0,
                refusals: 0,
                table_elements: 0,
            },
            limits,
        }
    }

    /// Record one refusal, deduplicated by `(capability, operation)`.
    ///
    /// Deduplication is what keeps a guest that calls `path_open` in a loop
    /// from turning the ledger into its own memory-exhaustion channel, while
    /// still recording the fact that it tried.
    fn deny(&mut self, capability: DeniedCapability, operation: &str, detail: &str) {
        let already = self
            .denials
            .iter()
            .any(|denial| denial.capability == capability && denial.operation == operation);
        if already || self.denials.len() >= MAX_DENIALS {
            return;
        }
        let denial = CapabilityDenial {
            capability,
            operation: operation.to_owned(),
            detail: detail.to_owned(),
        };
        tracing::warn!(
            plugin = self.plugin,
            capability = capability.as_str(),
            operation,
            detail,
            "sandboxed plugin was denied a capability it reached for"
        );
        self.denials.push(denial);
    }

    /// Handle one complete line the guest wrote to stdout.
    fn on_guest_line(&mut self, line: &str) {
        if self.answer.is_some() {
            // Already answered. Anything after is a guest that does not respect
            // the protocol; ignoring it is what stops a second frame from
            // overwriting a good first one.
            return;
        }
        self.answer = Some(match from_line::<GuestFrame>(line) {
            Ok(GuestFrame::Response(response)) => Ok(response),
            Ok(GuestFrame::Error { detail }) => {
                Err(SandboxFailure::GuestError(guest_text(&detail)))
            }
            Err(err) => Err(SandboxFailure::MalformedFrame(guest_text(&err.to_string()))),
        });
    }

    /// Buffer stdout bytes, consuming each completed NDJSON line.
    ///
    /// Returns `false` — the guest's write must fail — once the pending line
    /// outgrows what a legal response could possibly be.
    #[must_use]
    fn write_stdout(&mut self, bytes: &[u8]) -> bool {
        // Base64 inflates a body by 4/3, and the frame carries headers and JSON
        // punctuation around it, so the pending-line budget is the declared
        // response ceiling doubled plus slack. A frame that cannot fit under
        // this could never pass `check_size` anyway.
        let budget = self
            .limits
            .max_response_bytes
            .saturating_mul(2)
            .saturating_add(4096);
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.stdout_line);
                // Strictly, not lossily. `from_utf8_lossy` turns each invalid
                // byte into a three-byte replacement character while the
                // original is still alive, so a guest that filled its whole
                // stdout budget with invalid bytes would make the host hold
                // four times that budget — memory no manifest accounted for.
                // A frame is JSON, so it is UTF-8 or it is not a frame.
                match String::from_utf8(line) {
                    Ok(line) => self.on_guest_line(&line),
                    Err(_) => {
                        self.answer = Some(Err(SandboxFailure::MalformedFrame(
                            "the frame is not valid UTF-8".to_owned(),
                        )));
                    }
                }
            } else {
                if self.stdout_line.len() >= budget {
                    return false;
                }
                self.stdout_line.push(*byte);
            }
        }
        true
    }

    /// Buffer stderr bytes, silently dropping everything past the budget — a
    /// guest dying loudly should not be punished for being chatty.
    fn write_stderr(&mut self, bytes: &[u8]) {
        let room = STDERR_BUDGET_BYTES.saturating_sub(self.stderr.len());
        if let Some(kept) = bytes.get(..bytes.len().min(room)) {
            self.stderr.extend_from_slice(kept);
        }
    }

    /// Whether every further stderr byte would be discarded.
    const fn stderr_is_full(&self) -> bool {
        self.stderr.len() >= STDERR_BUDGET_BYTES
    }

    fn stderr_excerpt(&self) -> String {
        // Bounded *and* neutralised: truncation stops a flood, but a forged
        // record fits comfortably inside 512 characters.
        let text = String::from_utf8_lossy(&self.stderr);
        let trimmed = text.trim();
        let kept = match trimmed.char_indices().nth(STDERR_EXCERPT) {
            Some((index, _)) => trimmed.get(..index).unwrap_or_default(),
            None => trimmed,
        };
        guest_text(kept)
    }

    /// `SplitMix64`: tiny, deterministic, and good enough for a shim whose
    /// entire job is to be reproducible.
    const fn next_random_byte(&mut self) -> u8 {
        self.random_state = self.random_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.random_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "taking the low byte of the mixed state is the point"
        )]
        {
            (z ^ (z >> 31)) as u8
        }
    }

    /// Pop up to `len` bytes of the pending request frame.
    fn take_stdin(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len.min(self.stdin.len()));
        while out.len() < len {
            match self.stdin.pop_front() {
                Some(byte) => out.push(byte),
                None => break,
            }
        }
        out
    }
}

// ── The WASI shim ────────────────────────────────────────────────────────

type Shim = Linker<HostState>;

fn memory_of(caller: &Caller<'_, HostState>) -> Option<wasmi::Memory> {
    caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
}

fn read_u32(caller: &Caller<'_, HostState>, memory: wasmi::Memory, at: usize) -> Option<u32> {
    let mut buffer = [0u8; 4];
    memory.read(caller, at, &mut buffer).ok()?;
    Some(u32::from_le_bytes(buffer))
}

fn write_u32(
    caller: &mut Caller<'_, HostState>,
    memory: wasmi::Memory,
    at: usize,
    value: u32,
) -> Option<()> {
    memory.write(caller, at, &value.to_le_bytes()).ok()
}

/// Read one `iovec` at `index` of the array starting at `base`.
fn iovec(
    caller: &Caller<'_, HostState>,
    memory: wasmi::Memory,
    base: i32,
    index: i32,
) -> Option<(usize, usize)> {
    let base = usize::try_from(base).ok()?;
    let offset = usize::try_from(index).ok()?.checked_mul(IOVEC_SIZE)?;
    let at = base.checked_add(offset)?;
    let pointer = read_u32(caller, memory, at)?;
    let length = read_u32(caller, memory, at.checked_add(4)?)?;
    Some((
        usize::try_from(pointer).ok()?,
        usize::try_from(length).ok()?,
    ))
}

/// The WASI functions the shim implements itself, rather than refusing.
///
/// Name, parameter signature, and result signature — `i` for `i32`, `l` for
/// `i64`, empty for none. The signatures are here so a module can be
/// **type**-checked at load and not merely name-checked: an import whose type
/// disagrees with the shim links nowhere, and finding that out per request (as
/// a 502 the operator cannot explain) instead of at `autumn plugin package` is
/// exactly the failure this lane exists to move earlier.
const SERVED_IMPORTS: &[(&str, &str, &str)] = &[
    ("args_get", "ii", "i"),
    ("args_sizes_get", "ii", "i"),
    ("clock_res_get", "ii", "i"),
    ("clock_time_get", "ili", "i"),
    ("environ_get", "ii", "i"),
    ("environ_sizes_get", "ii", "i"),
    ("fd_close", "i", "i"),
    ("fd_fdstat_get", "ii", "i"),
    ("fd_read", "iiii", "i"),
    ("fd_seek", "ilii", "i"),
    ("fd_tell", "ii", "i"),
    ("fd_write", "iiii", "i"),
    ("proc_exit", "i", ""),
    ("random_get", "ii", "i"),
    ("sched_yield", "", "i"),
];

/// The WASI functions the shim answers with a refusal: name, the capability
/// class it belongs to, the detail an operator reads, and its signature.
///
/// The signature is `i` for `i32` and `l` for `i64`, in WASI order, and every
/// one of these returns an `errno`. `wasmi` matches host functions by
/// signature, so a wrong descriptor here would make an honest guest fail to
/// instantiate rather than fail to escape — which is why
/// `every_refusal_stub_matches_the_wasi_signature_it_stands_in_for` builds a
/// module importing all of them and proves they link.
const DENIED_IMPORTS: &[(&str, DeniedCapability, &str, &str)] = &[
    // There is no filesystem.
    ("fd_advise", DeniedCapability::Filesystem, FS_DETAIL, "illi"),
    (
        "fd_allocate",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "ill",
    ),
    ("fd_datasync", DeniedCapability::Filesystem, FS_DETAIL, "i"),
    (
        "fd_fdstat_set_flags",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "ii",
    ),
    (
        "fd_fdstat_set_rights",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "ill",
    ),
    (
        "fd_filestat_get",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "ii",
    ),
    (
        "fd_filestat_set_size",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "il",
    ),
    (
        "fd_filestat_set_times",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "illi",
    ),
    ("fd_pread", DeniedCapability::Filesystem, FS_DETAIL, "iiili"),
    (
        "fd_prestat_dir_name",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iii",
    ),
    (
        "fd_prestat_get",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "ii",
    ),
    (
        "fd_pwrite",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiili",
    ),
    (
        "fd_readdir",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiili",
    ),
    ("fd_renumber", DeniedCapability::Filesystem, FS_DETAIL, "ii"),
    ("fd_sync", DeniedCapability::Filesystem, FS_DETAIL, "i"),
    (
        "path_create_directory",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iii",
    ),
    (
        "path_filestat_get",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiii",
    ),
    (
        "path_filestat_set_times",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiilli",
    ),
    (
        "path_link",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiiiii",
    ),
    (
        "path_open",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiiillii",
    ),
    (
        "path_readlink",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiiii",
    ),
    (
        "path_remove_directory",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iii",
    ),
    (
        "path_rename",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiiii",
    ),
    (
        "path_symlink",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iiiii",
    ),
    (
        "path_unlink_file",
        DeniedCapability::Filesystem,
        FS_DETAIL,
        "iii",
    ),
    // There is no network.
    ("sock_accept", DeniedCapability::Network, NET_DETAIL, "iii"),
    ("sock_recv", DeniedCapability::Network, NET_DETAIL, "iiiiii"),
    ("sock_send", DeniedCapability::Network, NET_DETAIL, "iiiii"),
    ("sock_shutdown", DeniedCapability::Network, NET_DETAIL, "ii"),
    // The host process is not the guest's to steer.
    (
        "poll_oneoff",
        DeniedCapability::ProcessControl,
        "a sandboxed plugin may not block the host on external events",
        "iiii",
    ),
    (
        "proc_raise",
        DeniedCapability::ProcessControl,
        "a sandboxed plugin may not signal the host process",
        "i",
    ),
];

const FS_DETAIL: &str = "a sandboxed plugin has no filesystem";
const NET_DETAIL: &str = "a sandboxed plugin has no outbound network";

/// The signature the shim defines for a WASI function of this name, if any.
///
/// The load-time gate and the shim read the same two tables, so an import that
/// links at runtime is exactly one the gate admits — they cannot drift apart.
fn shim_signature(name: &str) -> Option<(&'static str, &'static str)> {
    if let Some((_, params, results)) = SERVED_IMPORTS.iter().find(|(known, ..)| *known == name) {
        return Some((params, results));
    }
    DENIED_IMPORTS
        .iter()
        .find(|(known, ..)| *known == name)
        .map(|(_, _, _, params)| (*params, "i"))
}

/// Whether the shim defines a WASI function of this name.
///
/// Test-only: the load gate needs the *signature*, not just the name, so it
/// calls [`shim_signature`] directly. This stays as the shape the
/// `the_load_gate_admits_exactly_what_the_shim_defines` invariant is written
/// against.
#[cfg(test)]
fn is_shim_function(name: &str) -> bool {
    shim_signature(name).is_some()
}

/// Build a [`wasmi::FuncType`] from a signature descriptor pair.
fn signature_type(params: &str, results: &str) -> Option<wasmi::FuncType> {
    fn types(descriptor: &str) -> Option<Vec<wasmi::core::ValType>> {
        descriptor
            .chars()
            .map(|ch| match ch {
                'i' => Some(wasmi::core::ValType::I32),
                'l' => Some(wasmi::core::ValType::I64),
                _ => None,
            })
            .collect()
    }
    Some(wasmi::FuncType::new(types(params)?, types(results)?))
}

/// What one instantiation costs in fuel: the bytes copied, plus a unit per
/// segment for the bounds check and copy set-up each one needs regardless of
/// its length, plus a unit per import.
///
/// Imports are counted for the same reason segments are: resolving one is
/// per-instance work the guest does not execute but every request pays for, so
/// leaving it unpriced is a way to buy host CPU with no fuel. `MAX_IMPORTS`
/// bounds the count; this makes the admitted ones cost something.
fn instantiation_fuel(segments: usize, init_bytes: usize, imports: usize, globals: usize) -> u64 {
    let bytes = u64::try_from(init_bytes).unwrap_or(u64::MAX);
    let segments = u64::try_from(segments).unwrap_or(u64::MAX);
    let imports = u64::try_from(imports).unwrap_or(u64::MAX);
    let globals = u64::try_from(globals).unwrap_or(u64::MAX);
    bytes
        .checked_div(BYTES_PER_FUEL)
        .unwrap_or(u64::MAX)
        .saturating_add(segments)
        .saturating_add(imports)
        .saturating_add(globals)
        .saturating_add(1)
}

/// Charge the guest's fuel budget for `bytes` of host-side work.
///
/// Returns the `OutOfFuel` trap when the budget cannot cover it, so a guest
/// that tries to buy unbounded copying ends exactly as a guest that spins does:
/// [`SandboxFailure::FuelExhausted`], and a 504 on its own prefix.
fn charge_bytes(caller: &mut Caller<'_, HostState>, bytes: usize) -> Result<(), wasmi::Error> {
    let units = u64::try_from(bytes)
        .unwrap_or(u64::MAX)
        .checked_div(BYTES_PER_FUEL)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let left = caller.get_fuel()?;
    let remaining = left
        .checked_sub(units)
        .ok_or_else(|| wasmi::Error::from(wasmi::core::TrapCode::OutOfFuel))?;
    caller.set_fuel(remaining)
}

/// Register the guest-visible WASI surface.
///
/// One registration per import, all in one place, because this list **is** the
/// sandbox. Read it top to bottom and you have read every capability a
/// sandboxed plugin holds.
#[allow(
    clippy::too_many_lines,
    reason = "splitting this up would hide the fact that this list IS the sandbox"
)]
fn define_wasi_shim(linker: &mut Shim) -> Result<(), SandboxLoadError> {
    fn engine_error(err: impl fmt::Display) -> SandboxLoadError {
        SandboxLoadError::Engine(err.to_string())
    }

    // ── the dialogue ─────────────────────────────────────────────────

    linker
        .func_wrap(
            WASI,
            "fd_read",
            |mut caller: Caller<'_, HostState>, fd: i32, iovs: i32, iovs_len: i32, nread: i32| {
                if fd != 0 {
                    // The guest was given one descriptor. Anything else is a
                    // reach for a file it was never handed.
                    caller.data_mut().deny(
                        DeniedCapability::Filesystem,
                        "fd_read",
                        "a sandboxed plugin has no descriptors beyond the request dialogue",
                    );
                    return Ok(errno::BADF);
                }
                let Some(memory) = memory_of(&caller) else {
                    return Ok(errno::INVAL);
                };
                if iovs_len > MAX_IOVECS {
                    return Ok(errno::INVAL);
                }

                let mut total: u32 = 0;
                for index in 0..iovs_len {
                    let Some((pointer, length)) = iovec(&caller, memory, iovs, index) else {
                        return Ok(errno::INVAL);
                    };
                    // Bounds-check BEFORE consuming: `take_stdin` pops the bytes,
                    // and a write that then fails would have destroyed part of
                    // the request frame with no way for the guest to recover it.
                    let in_bounds = pointer
                        .checked_add(length)
                        .is_some_and(|end| end <= memory.data_size(&caller));
                    if !in_bounds {
                        return Ok(errno::INVAL);
                    }
                    charge_bytes(&mut caller, length)?;
                    let chunk = caller.data_mut().take_stdin(length);
                    if memory.write(&mut caller, pointer, &chunk).is_err() {
                        return Ok(errno::INVAL);
                    }
                    let Ok(read) = u32::try_from(chunk.len()) else {
                        return Ok(errno::INVAL);
                    };
                    let Some(sum) = total.checked_add(read) else {
                        return Ok(errno::INVAL);
                    };
                    total = sum;
                    if chunk.len() < length {
                        break;
                    }
                }

                let Ok(at) = usize::try_from(nread) else {
                    return Ok(errno::INVAL);
                };
                if write_u32(&mut caller, memory, at, total).is_none() {
                    return Ok(errno::INVAL);
                }
                Ok(errno::SUCCESS)
            },
        )
        .map_err(engine_error)?;

    linker
        .func_wrap(
            WASI,
            "fd_write",
            |mut caller: Caller<'_, HostState>,
             fd: i32,
             iovs: i32,
             iovs_len: i32,
             nwritten: i32|
             -> Result<i32, wasmi::Error> {
                if fd != 1 && fd != 2 {
                    caller.data_mut().deny(
                        DeniedCapability::Filesystem,
                        "fd_write",
                        "a sandboxed plugin may only write its response frame and diagnostics",
                    );
                    return Ok(errno::BADF);
                }
                let Some(memory) = memory_of(&caller) else {
                    return Ok(errno::INVAL);
                };
                if iovs_len > MAX_IOVECS {
                    return Ok(errno::INVAL);
                }

                let mut total: u32 = 0;
                for index in 0..iovs_len {
                    let Some((pointer, length)) = iovec(&caller, memory, iovs, index) else {
                        return Ok(errno::INVAL);
                    };
                    // Bounds-check BEFORE any copy: `length` is guest-chosen,
                    // and a `u32::MAX` iovec must fail rather than start a copy
                    // the host then has to abandon.
                    let in_bounds = pointer
                        .checked_add(length)
                        .is_some_and(|end| end <= memory.data_size(&caller));
                    if !in_bounds {
                        return Ok(errno::INVAL);
                    }
                    let Ok(written) = u32::try_from(length) else {
                        return Ok(errno::INVAL);
                    };
                    let Some(sum) = total.checked_add(written) else {
                        return Ok(errno::INVAL);
                    };
                    total = sum;

                    // Copy through a bounded scratch buffer: an in-bounds iovec
                    // can span the guest's whole memory, and the host must never
                    // mirror it in one allocation.
                    let mut offset = 0usize;
                    while offset < length {
                        // Stderr past its budget is discarded, so copying it is
                        // pure host work with no output. Stop rather than
                        // faithfully copying bytes into the bin.
                        if fd == 2 && caller.data().stderr_is_full() {
                            break;
                        }
                        let take = HOST_IO_CHUNK_BYTES.min(length.saturating_sub(offset));
                        charge_bytes(&mut caller, take)?;
                        let mut scratch = vec![0u8; take];
                        let at = pointer.saturating_add(offset);
                        if memory.read(&caller, at, &mut scratch).is_err() {
                            return Ok(errno::INVAL);
                        }
                        if fd == 1 {
                            if !caller.data_mut().write_stdout(&scratch) {
                                // The response ceiling is a hard stop, not an
                                // errno the guest can ignore and retry: trap so
                                // the request ends here.
                                return Err(wasmi::Error::host(OutputBudgetExhausted));
                            }
                            if caller.data().answer.is_some() {
                                // The exchange is over. A guest that keeps
                                // running would hold a permit and a blocking
                                // worker for its whole budget and then serve the
                                // answer it already had.
                                return Err(wasmi::Error::host(AnswerComplete));
                            }
                        } else {
                            caller.data_mut().write_stderr(&scratch);
                        }
                        offset = offset.saturating_add(take);
                    }
                }

                let Ok(at) = usize::try_from(nwritten) else {
                    return Ok(errno::INVAL);
                };
                if write_u32(&mut caller, memory, at, total).is_none() {
                    return Ok(errno::INVAL);
                }
                Ok(errno::SUCCESS)
            },
        )
        .map_err(engine_error)?;

    // ── inert, because a guest's runtime expects them to exist ────────

    linker
        .func_wrap(WASI, "fd_close", |_: Caller<'_, HostState>, fd: i32| {
            if (0..=2).contains(&fd) {
                errno::SUCCESS
            } else {
                errno::BADF
            }
        })
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "fd_seek",
            |_: Caller<'_, HostState>, _fd: i32, _offset: i64, _whence: i32, _out: i32| {
                // stdio is a pipe. Saying so is more useful than saying no.
                errno::SPIPE
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "fd_tell",
            |_: Caller<'_, HostState>, _fd: i32, _out: i32| errno::SPIPE,
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "fd_fdstat_get",
            |mut caller: Caller<'_, HostState>, fd: i32, out: i32| {
                if !(0..=2).contains(&fd) {
                    caller.data_mut().deny(
                        DeniedCapability::Filesystem,
                        "fd_fdstat_get",
                        "a sandboxed plugin has no descriptors beyond the request dialogue",
                    );
                    return errno::BADF;
                }
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };
                let Ok(at) = usize::try_from(out) else {
                    return errno::INVAL;
                };
                // filetype 2 = character device, no rights, no flags.
                let mut stat = [0u8; FDSTAT_SIZE];
                stat[0] = 2;
                if memory.write(&mut caller, at, &stat).is_err() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(WASI, "sched_yield", |_: Caller<'_, HostState>| {
            errno::SUCCESS
        })
        .map_err(engine_error)?;

    // ── time and entropy: fixed, not ambient ─────────────────────────

    linker
        .func_wrap(
            WASI,
            "clock_time_get",
            |mut caller: Caller<'_, HostState>, _id: i32, _precision: i64, out: i32| {
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };
                let Ok(at) = usize::try_from(out) else {
                    return errno::INVAL;
                };
                // A fixed instant. The host's wall clock is not a capability a
                // plugin was granted, and a plugin that is a function of its
                // request is one an author can reason about.
                if memory.write(&mut caller, at, &0u64.to_le_bytes()).is_err() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "clock_res_get",
            |mut caller: Caller<'_, HostState>, _id: i32, out: i32| {
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };
                let Ok(at) = usize::try_from(out) else {
                    return errno::INVAL;
                };
                if memory.write(&mut caller, at, &1u64.to_le_bytes()).is_err() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "random_get",
            |mut caller: Caller<'_, HostState>, buf: i32, len: i32| -> Result<i32, wasmi::Error> {
                let Some(memory) = memory_of(&caller) else {
                    return Ok(errno::INVAL);
                };
                let (Ok(at), Ok(len)) = (usize::try_from(buf), usize::try_from(len)) else {
                    return Ok(errno::INVAL);
                };
                let in_bounds = at
                    .checked_add(len)
                    .is_some_and(|end| end <= memory.data_size(&caller));
                if !in_bounds {
                    return Ok(errno::INVAL);
                }
                let mut written = 0usize;
                while written < len {
                    let take = HOST_IO_CHUNK_BYTES.min(len.saturating_sub(written));
                    charge_bytes(&mut caller, take)?;
                    let mut scratch = vec![0u8; take];
                    for slot in &mut scratch {
                        *slot = caller.data_mut().next_random_byte();
                    }
                    if memory
                        .write(&mut caller, at.saturating_add(written), &scratch)
                        .is_err()
                    {
                        return Ok(errno::INVAL);
                    }
                    written = written.saturating_add(take);
                }
                Ok(errno::SUCCESS)
            },
        )
        .map_err(engine_error)?;

    // ── the process is not the guest's to steer ──────────────────────

    linker
        .func_wrap(
            WASI,
            "proc_exit",
            |_: Caller<'_, HostState>, code: i32| -> Result<(), wasmi::Error> {
                // Ends the *guest*. The host process is not the guest's to end.
                Err(wasmi::Error::i32_exit(code))
            },
        )
        .map_err(engine_error)?;

    // ── the environment is empty, and asking is recorded ─────────────

    linker
        .func_wrap(
            WASI,
            "environ_sizes_get",
            |mut caller: Caller<'_, HostState>, count: i32, size: i32| {
                caller.data_mut().deny(
                    DeniedCapability::Environment,
                    "environ_sizes_get",
                    "a sandboxed plugin sees an empty environment",
                );
                write_two_zeroes(&mut caller, count, size)
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "environ_get",
            |mut caller: Caller<'_, HostState>, _ptrs: i32, _buf: i32| {
                caller.data_mut().deny(
                    DeniedCapability::Environment,
                    "environ_get",
                    "a sandboxed plugin sees an empty environment",
                );
                // Nothing to write: the environment is empty, so success is the
                // truthful answer and needs no bytes.
                errno::SUCCESS
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "args_sizes_get",
            |mut caller: Caller<'_, HostState>, count: i32, size: i32| {
                caller.data_mut().deny(
                    DeniedCapability::Environment,
                    "args_sizes_get",
                    "a sandboxed plugin sees no process arguments",
                );
                write_two_zeroes(&mut caller, count, size)
            },
        )
        .map_err(engine_error)?;
    linker
        .func_wrap(
            WASI,
            "args_get",
            |mut caller: Caller<'_, HostState>, _ptrs: i32, _buf: i32| {
                caller.data_mut().deny(
                    DeniedCapability::Environment,
                    "args_get",
                    "a sandboxed plugin sees no process arguments",
                );
                errno::SUCCESS
            },
        )
        .map_err(engine_error)?;

    // ── everything else is a refusal ─────────────────────────────────

    for (name, capability, detail, signature) in DENIED_IMPORTS {
        deny(linker, name, *capability, detail, signature)?;
    }

    Ok(())
}

/// Write two zero `u32`s — the "empty list" answer `*_sizes_get` needs.
fn write_two_zeroes(caller: &mut Caller<'_, HostState>, first: i32, second: i32) -> i32 {
    let Some(memory) = memory_of(caller) else {
        return errno::INVAL;
    };
    let (Ok(first), Ok(second)) = (usize::try_from(first), usize::try_from(second)) else {
        return errno::INVAL;
    };
    if write_u32(caller, memory, first, 0).is_none()
        || write_u32(caller, memory, second, 0).is_none()
    {
        return errno::INVAL;
    }
    errno::SUCCESS
}

/// Register one refusal stub.
///
/// `wasmi` matches host functions by signature, so a stub must have exactly the
/// shape of the WASI function it stands in for — hence the descriptor rather
/// than a hand-written closure per function. The bodies are identical by
/// design: there is nothing to implement, only a refusal to record.
fn deny(
    linker: &mut Shim,
    name: &'static str,
    capability: DeniedCapability,
    detail: &'static str,
    signature: &str,
) -> Result<(), SandboxLoadError> {
    let mut params = Vec::with_capacity(signature.len());
    for ch in signature.chars() {
        params.push(match ch {
            'i' => wasmi::core::ValType::I32,
            'l' => wasmi::core::ValType::I64,
            other => {
                return Err(SandboxLoadError::Engine(format!(
                    "unknown signature character `{other}` for {name}"
                )));
            }
        });
    }
    let ty = wasmi::FuncType::new(params, [wasmi::core::ValType::I32]);
    linker
        .func_new(
            WASI,
            name,
            ty,
            move |mut caller: Caller<'_, HostState>, _args: &[wasmi::Val], results| {
                caller.data_mut().deny(capability, name, detail);
                if let Some(slot) = results.first_mut() {
                    *slot = wasmi::Val::I32(errno::NOTCAPABLE);
                }
                Ok(())
            },
        )
        .map_err(|err| SandboxLoadError::Engine(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::manifest::{ResourceLimits, SandboxManifest};
    use crate::plugin_sandbox::test_guests as guests;

    fn manifest_with(limits: ResourceLimits) -> SandboxManifest {
        let mut manifest = SandboxManifest::parse(&format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"
"#,
            digest = "a".repeat(64)
        ))
        .expect("valid manifest");
        manifest.limits = limits;
        manifest
    }

    fn try_host(wat: &str) -> Result<SandboxHost, SandboxLoadError> {
        try_host_with(wat, ResourceLimits::default())
    }

    fn try_host_with(wat: &str, limits: ResourceLimits) -> Result<SandboxHost, SandboxLoadError> {
        let wasm = wat::parse_str(wat).expect("the fixture is valid WAT");
        SandboxHost::from_module(manifest_with(limits), &wasm)
    }

    fn host(wat: &str) -> SandboxHost {
        try_host(wat).expect("the fixture loads")
    }

    fn request(method: &str, path: &str) -> SandboxRequest {
        SandboxRequest {
            method: method.to_owned(),
            // The declared pattern and the concrete path agree here: the
            // fixtures dispatch on the frame's *content*, so a route that
            // never varied would make every test look like a match.
            route: path.to_owned(),
            path: path.to_owned(),
            query: String::new(),
            path_params: vec![],
            headers: vec![("accept".to_owned(), "text/plain".to_owned())],
            body: vec![],
        }
    }

    fn get(path: &str) -> SandboxRequest {
        request("GET", path)
    }

    fn denied(outcome: &SandboxOutcome, capability: DeniedCapability) -> Vec<String> {
        outcome
            .denials
            .iter()
            .filter(|denial| denial.capability == capability)
            .map(|denial| denial.operation.clone())
            .collect()
    }

    // ── the happy path ───────────────────────────────────────────────

    #[test]
    fn a_well_behaved_guest_answers_the_request() {
        let outcome = host(guests::HELLO).run(&get("/hello/greet"));
        let response = outcome.result.expect("answers");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello from the sandbox");
        assert!(outcome.denials.is_empty(), "{:?}", outcome.denials);
        assert!(outcome.fuel_used > 0);
    }

    #[test]
    fn the_guest_sees_the_request_it_was_sent() {
        let host = host(guests::HELLO);
        assert_eq!(
            host.run(&get("/hello/other"))
                .result
                .expect("answers")
                .status,
            404
        );
        assert_eq!(
            host.run(&request("POST", "/hello/greet"))
                .result
                .expect("answers")
                .status,
            405
        );
    }

    #[test]
    fn each_request_gets_a_fresh_instance() {
        // Nothing a guest does can survive into the next request, which is what
        // makes one request's misbehaviour unable to poison the next.
        let host = host(guests::HELLO);
        let first = host.run(&get("/hello/greet"));
        let second = host.run(&get("/hello/greet"));
        assert_eq!(
            first.result.expect("answers"),
            second.result.expect("answers")
        );
        assert_eq!(first.fuel_used, second.fuel_used);
    }

    // ── resource bounds ──────────────────────────────────────────────

    #[test]
    fn a_guest_that_never_stops_is_stopped() {
        let limits = ResourceLimits {
            fuel: 5_000_000,
            ..ResourceLimits::default()
        };
        let outcome = try_host_with(guests::CPU_SPIN, limits)
            .expect("loads")
            .run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(
            matches!(failure, SandboxFailure::FuelExhausted { .. }),
            "{failure}"
        );
        assert!(failure.status().is_server_error(), "{failure}");
    }

    #[test]
    fn a_guest_that_allocates_without_bound_is_capped() {
        let limits = ResourceLimits {
            fuel: 20_000_000,
            memory_bytes: 1024 * 1024,
            ..ResourceLimits::default()
        };
        let outcome = try_host_with(guests::MEMORY_BOMB, limits)
            .expect("loads")
            .run(&get("/hello/greet"));
        assert!(outcome.result.is_err(), "a memory bomb must not answer");
        assert!(
            outcome.peak_memory_bytes <= 1024 * 1024,
            "peak {} exceeded the ceiling",
            outcome.peak_memory_bytes
        );
        assert!(
            !denied(&outcome, DeniedCapability::Memory).is_empty(),
            "the refused growth must be observable: {:?}",
            outcome.denials
        );
    }

    #[test]
    fn a_guest_that_floods_stdout_without_a_newline_is_cut_off() {
        let limits = ResourceLimits {
            max_response_bytes: 4096,
            fuel: 50_000_000,
            ..ResourceLimits::default()
        };
        let outcome = try_host_with(guests::OUTPUT_FLOOD, limits)
            .expect("loads")
            .run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(
            matches!(failure, SandboxFailure::OutputBudget { .. }),
            "{failure}"
        );
    }

    // ── fault isolation ──────────────────────────────────────────────

    #[test]
    fn host_side_copying_is_charged_against_the_guest_s_fuel() {
        // wasmi meters the guest's instructions, not what the host does on its
        // behalf. Without a charge, a guest buys gigabytes of memcpy for single
        // digits of fuel — a spin inside the host that the CPU ceiling never
        // sees. This guest asks the host to copy 1 MiB in ~50 instructions.
        const COPIED: u64 = 1024 * 1024;
        let bulk = host(guests::STDOUT_BULK).run(&get("/hello/greet"));
        assert!(
            bulk.fuel_used >= COPIED / 64,
            "1 MiB of host-side copying cost only {} fuel units",
            bulk.fuel_used
        );
        // …and the loop itself is nothing, so the charge is the copy.
        let honest = host(guests::HELLO).run(&get("/hello/greet"));
        assert!(
            honest.fuel_used < COPIED / 64,
            "the baseline is not a baseline: {} units",
            honest.fuel_used
        );
    }

    #[test]
    fn instantiating_the_module_is_priced_against_the_budget() {
        // wasmi meters guest instructions, not instantiation: every request
        // copies the module's data and element segments before `_start` runs.
        // The ceiling on that is at load (below); this is the price, so the work
        // that IS admitted still comes out of the declared budget.
        let charged = host(guests::HELLO).instantiation_fuel();
        assert!(charged > 1, "the fixture carries data segments");

        // A budget that cannot even cover instantiation is refused at *load*
        // now: the artifact could never answer a request, so it never mounts.
        let err = try_host_with(
            guests::HELLO,
            ResourceLimits {
                fuel: charged / 2,
                ..ResourceLimits::default()
            },
        )
        .expect_err("a budget below the fixed charge must not produce a host");
        assert!(
            matches!(err, SandboxLoadError::FuelBelowFixedCharges { .. }),
            "{err:?}"
        );

        // The per-request refusal still matters, and is still reached: the load
        // floor compares against instantiation alone, but the frame encoding is
        // subtracted *first*, so a budget that clears the floor can still be
        // spent before `_start`. That is the case no load-time check can catch,
        // and it must still refuse before instantiating rather than after.
        let outcome = try_host_with(
            guests::HELLO,
            ResourceLimits {
                fuel: charged + 1,
                ..ResourceLimits::default()
            },
        )
        .expect("a budget above the fixed charge loads")
        .run(&get("/hello/greet"));
        assert!(
            matches!(outcome.result, Err(SandboxFailure::FuelExhausted { .. })),
            "{:?}",
            outcome.result
        );

        // …and an honest request pays for it out of the same budget.
        let outcome = host(guests::HELLO).run(&get("/hello/greet"));
        assert!(outcome.result.is_ok());
        assert!(
            outcome.fuel_used >= charged,
            "instantiation cost {} of a {charged}-unit module",
            outcome.fuel_used
        );
    }

    #[test]
    fn a_frame_that_is_not_utf8_is_refused_rather_than_expanded() {
        // `from_utf8_lossy` turns each invalid byte into a three-byte
        // replacement while the original is still alive, so a guest filling its
        // whole stdout budget with invalid bytes would make the host hold four
        // times that budget — memory no manifest accounted for.
        let outcome = host(guests::INVALID_UTF8).run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(
            matches!(failure, SandboxFailure::MalformedFrame(ref detail) if detail.contains("UTF-8")),
            "{failure}"
        );
    }

    #[test]
    fn a_module_without_the_memory_the_shim_needs_is_refused_at_load() {
        // Every host function reads and writes through an export named
        // `memory`. Without one they all answer EINVAL, so the plugin loads and
        // then fails every request — which packaging exists to prevent.
        let wat = r#"(module (func (export "_start") (nop)))"#;
        assert!(matches!(
            try_host(wat),
            Err(SandboxLoadError::MissingMemory)
        ));
    }

    #[test]
    fn a_module_whose_initial_memory_exceeds_the_ceiling_is_refused_at_load() {
        // 32 pages = 2 MiB of *initial* memory against a 1 MiB ceiling: the
        // limiter would refuse it at instantiation, per request, as a gateway
        // error — when it can be said once, here.
        let wat = r#"(module (memory (export "memory") 32) (func (export "_start") (nop)))"#;
        let limits = ResourceLimits {
            memory_bytes: 1024 * 1024,
            ..ResourceLimits::default()
        };
        let err = try_host_with(wat, limits).expect_err("must be refused");
        assert!(
            matches!(err, SandboxLoadError::MemoryTooLarge { found, .. } if found == 2 * 1024 * 1024),
            "{err}"
        );
    }

    #[test]
    fn table_growth_past_the_ceiling_is_refused_while_the_guest_runs() {
        // A module already over the ceiling at rest is now refused at load,
        // summed across its tables. The limiter is still what holds the line
        // here, and this is the case it alone can: a guest that starts *under*
        // the ceiling and reaches for more with `table.grow` while running,
        // which no load-time check can see.
        let wat = format!(
            r#"(module
                 (memory (export "memory") 1)
                 (table 1 funcref)
                 (func (export "_start")
                   (drop (table.grow 0 (ref.null func) (i32.const {MAX_TABLE_ELEMENTS})))))"#
        );
        let outcome = host(&wat).run(&get("/hello/greet"));
        assert!(outcome.result.is_err(), "{:?}", outcome.result);
        // Refused, and recorded: an operator has to be able to see that the
        // plugin reached past its ceiling rather than merely that it 5xx'd.
        assert!(
            !denied(&outcome, DeniedCapability::Memory).is_empty(),
            "the refusal was not recorded: {:?}",
            outcome.denials
        );
    }

    #[test]
    fn a_module_that_is_expensive_to_instantiate_is_refused_at_load() {
        // Segment *count* is the sharp edge: each one costs a bounds check and a
        // copy set-up regardless of its length, so a module of many empty
        // segments is small on disk and expensive on every single request. A
        // per-request charge cannot bound work that has already been admitted,
        // so the ceiling is at load.
        use std::fmt::Write as _;

        let mut wat = String::from("(module\n  (memory (export \"memory\") 1)\n");
        for offset in 0..=MAX_INIT_SEGMENTS {
            let _ = writeln!(wat, "  (data (i32.const {offset}) \"x\")");
        }
        wat.push_str("  (func (export \"_start\") (nop))\n)");

        let err = try_host(&wat).expect_err("must be refused");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive { max, .. } if max == MAX_INIT_SEGMENTS
            ),
            "{err}"
        );
        assert!(err.to_string().contains("re-instantiates"), "{err}");
    }

    #[test]
    fn a_guest_that_floods_stderr_runs_out_of_fuel_rather_than_time() {
        let limits = ResourceLimits {
            fuel: 2_000_000,
            ..ResourceLimits::default()
        };
        let outcome = try_host_with(guests::STDERR_FLOOD, limits)
            .expect("loads")
            .run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(
            matches!(failure, SandboxFailure::FuelExhausted { .. }),
            "{failure}"
        );
    }

    #[test]
    fn a_guest_that_answers_and_then_spins_does_not_hold_its_whole_budget() {
        // The exchange is over at the frame. A guest that keeps running would
        // otherwise hold a permit and a blocking worker for its whole budget
        // and then serve the answer it already had.
        let limits = ResourceLimits {
            fuel: 500_000_000,
            ..ResourceLimits::default()
        };
        let outcome = try_host_with(guests::ANSWER_THEN_SPIN, limits)
            .expect("loads")
            .run(&get("/hello/greet"));
        assert_eq!(outcome.result.expect("answers").status, 200);
        assert!(
            outcome.fuel_used < 1_000_000,
            "the answer cost {} fuel; the guest was allowed to keep spinning",
            outcome.fuel_used
        );
    }

    #[test]
    fn a_frame_without_its_newline_says_what_the_author_did() {
        let outcome = host(guests::PARTIAL_FRAME).run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(matches!(failure, SandboxFailure::PartialFrame), "{failure}");
        assert!(failure.to_string().contains("println!"), "{failure}");
    }

    #[test]
    fn a_guest_may_not_forge_the_host_s_own_response_headers() {
        let outcome = host(guests::FORGE_ATTRIBUTION).run(&get("/hello/greet"));
        let denied = denied(&outcome, DeniedCapability::ResponseHeader);
        assert!(
            denied.contains(&"x-autumn-sandboxed".to_owned()),
            "{denied:?}"
        );
        assert!(
            denied.contains(&"x-content-type-options".to_owned()),
            "{denied:?}"
        );
        let response = outcome.result.expect("answers");
        assert!(
            response
                .headers
                .iter()
                .all(|(name, _)| name.starts_with("content-"))
        );
    }

    #[test]
    fn a_document_content_type_is_refused_rather_than_served_from_the_host_s_origin() {
        for essence in [
            "text/html",
            "application/javascript",
            "image/svg+xml",
            "text/css",
        ] {
            let response = SandboxResponse {
                status: 200,
                headers: vec![("content-type".to_owned(), essence.to_owned())],
                body: b"<script>".to_vec(),
            };
            assert_eq!(
                response.refused_content_type().as_deref(),
                Some(essence),
                "{essence} must be refused"
            );
        }
        for essence in ["text/plain; charset=utf-8", "application/json", "image/png"] {
            let response = SandboxResponse {
                status: 200,
                headers: vec![("Content-Type".to_owned(), essence.to_owned())],
                body: vec![],
            };
            assert_eq!(
                response.refused_content_type(),
                None,
                "{essence} must be served"
            );
        }
    }

    #[test]
    fn entropy_is_deterministic_per_request_without_being_a_published_constant() {
        // The guest folds an entropy byte into its status, so this only holds
        // if the host's stream is a function of the request.
        let host = host(guests::ENTROPY);
        let first = host.run(&get("/hello/greet")).result.expect("answers");
        let again = host.run(&get("/hello/greet")).result.expect("answers");
        assert_eq!(first.status, again.status, "the same request must replay");

        let other = host.run(&get("/hello/other")).result.expect("answers");
        assert!(
            (200..=207).contains(&other.status),
            "unexpected status {}",
            other.status
        );
        // Not asserted equal *or* unequal: one byte mod 8 collides one time in
        // eight. What matters is that the seed is not a global constant, which
        // the seed function's own test below pins.
    }

    #[test]
    fn the_entropy_seed_is_a_function_of_the_request() {
        assert_ne!(
            HostState::seed_from(b"one request"),
            HostState::seed_from(b"another request")
        );
        assert_eq!(
            HostState::seed_from(b"one request"),
            HostState::seed_from(b"one request")
        );
    }

    #[test]
    fn a_trap_is_an_error_value_not_a_dead_process() {
        let outcome = host(guests::TRAP).run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(matches!(failure, SandboxFailure::Trap(_)), "{failure}");
    }

    #[test]
    fn proc_exit_does_not_exit_the_host() {
        let outcome = host(guests::EXIT).run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(matches!(failure, SandboxFailure::Exited(3)), "{failure}");
    }

    #[test]
    fn a_guest_that_never_answers_is_a_failure_not_a_hang() {
        let outcome = host(guests::SILENT).run(&get("/hello/greet"));
        assert!(
            matches!(outcome.result, Err(SandboxFailure::NoAnswer)),
            "{:?}",
            outcome.result
        );
    }

    #[test]
    fn a_module_without_a_start_is_refused_at_load() {
        let err = try_host(guests::NO_START).expect_err("must be refused");
        assert!(matches!(err, SandboxLoadError::MissingStart), "{err}");
    }

    // ── deny-by-default ──────────────────────────────────────────────

    #[test]
    fn a_file_read_is_denied_and_logged() {
        let outcome = host(guests::READ_FILE).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::Filesystem),
            vec!["path_open".to_owned()]
        );
        assert_eq!(outcome.result.expect("still answers").status, 200);
    }

    /// A subscriber that records the `capability` / `operation` fields of every
    /// event, so "observable in logs" can be asserted rather than assumed.
    #[derive(Default)]
    struct DenialLog(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for DenialLog {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::TRACE)
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields<'a>(&'a mut Vec<String>);
            impl tracing::field::Visit for Fields<'_> {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                    self.0.push(format!("{}={value:?}", field.name()));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push(format!("{}={value}", field.name()));
                }
            }
            let mut fields = Vec::new();
            event.record(&mut Fields(&mut fields));
            if let Ok(mut recorded) = self.0.lock() {
                recorded.push(format!(
                    "{} [{}]",
                    event.metadata().level(),
                    fields.join(" ")
                ));
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    use rusty_fork::rusty_fork_test;

    rusty_fork_test! {
        #[test]
        fn a_denial_reaches_the_log_and_not_only_the_ledger() {
            // The ledger is what tests read; the log is what an operator reads.
            // If the two could drift, "each denial observable in logs" would be
            // a claim about a field nobody sees.
            //
            // Forked: `tracing`'s callsite-interest cache and max-level hint are
            // process-global, so a sibling test that installs a global
            // subscriber can filter this event out before any thread-local
            // subscriber is consulted — which makes an in-process version of
            // this test pass or fail on test ordering.
            let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            tracing::subscriber::with_default(
                DenialLog(std::sync::Arc::clone(&recorded)),
                || {
                    let _ = host(guests::READ_FILE).run(&get("/hello/greet"));
                },
            );
            let lines = recorded.lock().expect("not poisoned").clone();
            let denial = lines
                .iter()
                .find(|line| line.contains("operation=path_open"))
                .unwrap_or_else(|| panic!("no denial line in {lines:#?}"));
            assert!(denial.starts_with("WARN"), "{denial}");
            assert!(denial.contains("capability=filesystem"), "{denial}");
            assert!(denial.contains("plugin=autumn-plugin-hello"), "{denial}");
            assert!(denial.contains("no filesystem"), "{denial}");
        }
    }

    #[test]
    fn there_are_no_preopened_directories_to_discover() {
        let outcome = host(guests::DISCOVER_PREOPENS).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::Filesystem),
            vec!["fd_prestat_get".to_owned()]
        );
    }

    #[test]
    fn a_stray_descriptor_read_is_denied() {
        let outcome = host(guests::READ_STRAY_FD).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::Filesystem),
            vec!["fd_read".to_owned()]
        );
    }

    #[test]
    fn outbound_network_is_denied() {
        let outcome = host(guests::NETWORK).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::Network),
            vec!["sock_send".to_owned()]
        );
    }

    #[test]
    fn the_environment_is_empty_and_the_attempt_is_logged() {
        let outcome = host(guests::ENVIRONMENT).run(&get("/hello/greet"));
        let ops = denied(&outcome, DeniedCapability::Environment);
        assert_eq!(outcome.result.expect("still answers").status, 200);
        assert!(ops.contains(&"environ_sizes_get".to_owned()), "{ops:?}");
        assert!(ops.contains(&"environ_get".to_owned()), "{ops:?}");
    }

    #[test]
    fn process_arguments_are_empty_and_the_attempt_is_logged() {
        let outcome = host(guests::ARGUMENTS).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::Environment),
            vec!["args_sizes_get".to_owned()]
        );
    }

    #[test]
    fn blocking_the_host_on_a_poll_is_denied() {
        let outcome = host(guests::POLL).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::ProcessControl),
            vec!["poll_oneoff".to_owned()]
        );
    }

    #[test]
    fn entropy_is_answered_deterministically_rather_than_denied() {
        let host = host(guests::ENTROPY);
        let first = host.run(&get("/hello/greet"));
        let second = host.run(&get("/hello/greet"));
        assert!(first.denials.is_empty(), "{:?}", first.denials);
        assert_eq!(
            first.result.expect("answers"),
            second.result.expect("answers")
        );
    }

    #[test]
    fn a_database_seam_the_host_never_defined_is_refused_at_load() {
        let err = try_host(guests::DATABASE).expect_err("must be refused");
        let SandboxLoadError::ForbiddenImports(denials) = err else {
            panic!("expected a forbidden-import refusal, got {err}");
        };
        assert_eq!(denials.len(), 1);
        assert_eq!(denials[0].capability, DeniedCapability::UnknownImport);
        assert!(denials[0].operation.contains("autumn_db"), "{denials:?}");
    }

    #[test]
    fn a_host_escape_from_an_invented_namespace_is_refused_at_load() {
        let err = try_host(guests::HOST_COMMAND).expect_err("must be refused");
        assert!(
            matches!(err, SandboxLoadError::ForbiddenImports(_)),
            "{err}"
        );
    }

    #[test]
    fn a_wasi_function_this_shim_does_not_implement_is_refused_at_load() {
        let err = try_host(guests::UNDEFINED_WASI).expect_err("must be refused");
        let SandboxLoadError::ForbiddenImports(denials) = err else {
            panic!("expected a forbidden-import refusal, got {err}");
        };
        assert!(denials[0].operation.contains("sock_connect"), "{denials:?}");
    }

    // ── the response is not a trusted channel either ─────────────────

    #[test]
    fn a_forged_session_cookie_is_stripped_and_logged() {
        let outcome = host(guests::FORGE_COOKIE).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::ResponseHeader),
            vec!["set-cookie".to_owned()]
        );
        let response = outcome.result.expect("answers");
        assert!(
            !response
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("set-cookie")),
            "{:?}",
            response.headers
        );
    }

    #[test]
    fn a_plugin_cannot_borrow_the_reverse_proxy_s_filesystem() {
        let outcome = host(guests::PROXY_REDIRECT).run(&get("/hello/greet"));
        let denied = denied(&outcome, DeniedCapability::ResponseHeader);
        assert!(
            denied.contains(&"x-accel-redirect".to_owned()),
            "{denied:?}"
        );
        assert!(denied.contains(&"x-sendfile".to_owned()), "{denied:?}");
        let response = outcome.result.expect("answers without them");
        assert!(
            response
                .headers
                .iter()
                .all(|(name, _)| name == "content-type"),
            "{:?}",
            response.headers
        );
    }

    #[test]
    fn a_response_splitting_header_never_reaches_a_response() {
        // Two locks on this door: the header allowlist drops the name before
        // anything looks at its value, and `SandboxResponse::validate` refuses a
        // value carrying CRLF for a name that *is* allowed (asserted directly in
        // `wire`). Here the first lock holds, so the guest is served — minus the
        // header, with the attempt on the record.
        let outcome = host(guests::SPLIT_RESPONSE).run(&get("/hello/greet"));
        assert_eq!(
            denied(&outcome, DeniedCapability::ResponseHeader),
            vec!["x-evil".to_owned()]
        );
        let response = outcome.result.expect("answers without the header");
        assert!(
            response
                .headers
                .iter()
                .all(|(name, _)| name == "content-type"),
            "{:?}",
            response.headers
        );
    }

    #[test]
    fn an_impossible_status_is_refused() {
        let outcome = host(guests::IMPOSSIBLE_STATUS).run(&get("/hello/greet"));
        assert!(
            matches!(outcome.result, Err(SandboxFailure::ResponseRefused(_))),
            "{:?}",
            outcome.result
        );
    }

    #[test]
    fn a_guest_error_detail_can_neither_flood_nor_forge_a_log_line() {
        // A guest's `detail` is attacker-controlled text on its way to a log
        // the operator trusts. Two separate hazards: its *length* (a line can
        // be as large as the stdout budget, and a plugin that fails in a loop
        // writes one per request) and its *content* (a newline starts a record
        // the operator did not write; an ANSI escape repaints one they did).
        let outcome = host(guests::FORGE_LOG).run(&get("/hello/greet"));
        let Err(failure) = outcome.result else {
            panic!("the guest reported a failure");
        };
        let rendered = failure.to_string();
        assert!(
            rendered.len() < 4096,
            "a 2 KiB detail reached the log intact: {} bytes",
            rendered.len()
        );
        assert!(
            !rendered.contains('\n') && !rendered.contains('\r'),
            "the detail can start a log record of its own: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{1b}'),
            "the detail can repaint the operator's terminal: {rendered:?}"
        );
        assert!(
            rendered.contains("forged"),
            "the detail is still legible enough to debug with: {rendered:?}"
        );
    }

    #[test]
    fn a_denied_response_header_name_is_bounded_before_it_is_logged() {
        // The sibling of the content-type cap, on the header *name* rather
        // than its value. A denied name is often denied precisely because it
        // is not a valid header name, so it can carry newlines and escapes as
        // well as megabytes — and `deny` both stores it and logs it.
        let name = "x-".to_owned() + &"y".repeat(20_000);
        let frame = format!(
            r#"{{"op":"response","status":200,"headers":[["content-type","text/plain"],["{name}","v"]],"body_b64":""}}"#
        );
        let len = frame.len() + 1;
        let wat = format!(
            r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 1024) "{escaped}\0a")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 1024))
    (i32.store (i32.const 4) (i32.const {len}))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
            escaped = frame.replace('"', "\\\""),
        );

        let outcome = host(&wat).run(&get("/hello/greet"));
        let logged = outcome
            .denials
            .iter()
            .find(|denial| denial.capability == DeniedCapability::ResponseHeader)
            .expect("the stripped header is recorded as a denial");
        assert!(
            logged.operation.len() < 1_024,
            "the denial carries the guest's whole header name: {} bytes",
            logged.operation.len()
        );
        assert!(
            logged.operation.contains("truncated"),
            "{:?}",
            logged.operation
        );
    }

    #[test]
    fn a_refused_content_type_is_bounded_before_it_is_logged() {
        // `refused_content_type` takes everything before the first `;`, so a
        // guest that writes no parameter hands back its entire header value —
        // capped only by the stdout ceiling, which is megabytes. That string
        // reached both the denial detail and the `ResponseRefused` text, and
        // both are logged, so a guest could flood an operator's log with a
        // header rather than with output.
        let essence = "x".repeat(20_000);
        let frame = format!(
            r#"{{"op":"response","status":200,"headers":[["content-type","{essence}"]],"body_b64":""}}"#
        );
        // The frame is ASCII, so its byte length is its char length; + 1 for
        // the newline that ends it. The WAT literal needs its quotes escaped,
        // which changes the *source* length but not the decoded bytes.
        let len = frame.len() + 1;
        let wat = format!(
            r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 1024) "{escaped}\0a")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 1024))
    (i32.store (i32.const 4) (i32.const {len}))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
            escaped = frame.replace('"', "\\\""),
        );

        let outcome = host(&wat).run(&get("/hello/greet"));
        let Err(SandboxFailure::ResponseRefused(detail)) = outcome.result else {
            panic!(
                "an unsupported content type must be refused: {:?}",
                outcome.result
            );
        };
        assert!(
            detail.len() < 1_024,
            "the failure text carries the guest's whole header: {} bytes",
            detail.len()
        );
        assert!(detail.contains("truncated"), "{detail}");

        let logged = outcome
            .denials
            .iter()
            .find(|denial| denial.capability == DeniedCapability::ResponseHeader)
            .expect("the refusal is recorded as a denial");
        assert!(
            logged.detail.len() < 1_024,
            "the denial detail carries the guest's whole header: {} bytes",
            logged.detail.len()
        );
    }

    #[test]
    fn stderr_is_escaped_as_well_as_bounded() {
        // Same hazard, same answer: stderr was truncated but not neutralised,
        // so a guest could forge a record inside its 512-character excerpt.
        let mut state = HostState::new("hello".to_owned(), ResourceLimits::default(), b"");
        state.write_stderr(b"panicked\n2026-01-01  INFO forged\x1b[2K");
        let excerpt = state.stderr_excerpt();
        assert!(
            !excerpt.contains('\n') && !excerpt.contains('\u{1b}'),
            "{excerpt:?}"
        );
        assert!(excerpt.contains("panicked"), "{excerpt:?}");
    }

    #[test]
    fn a_manifest_mutated_after_validation_is_refused_rather_than_trusted() {
        // `SandboxManifest`'s fields are public and `from_module` is public, so
        // "the manifest was validated when it was parsed" is an invariant a
        // caller can step around without meaning to. The values that matter
        // here are the ones that panic something downstream rather than merely
        // misbehaving: a concurrency past the semaphore's ceiling, and a route
        // path axum refuses to build.
        let wasm = wat::parse_str(guests::HELLO).expect("the fixture is valid WAT");

        let mut manifest = manifest_with(ResourceLimits::default());
        manifest.limits.max_concurrency = usize::MAX;
        let err = SandboxHost::from_module(manifest, &wasm)
            .expect_err("an invalid manifest must not produce a host");
        assert!(matches!(err, SandboxLoadError::InvalidManifest(_)), "{err}");

        let mut manifest = manifest_with(ResourceLimits::default());
        manifest.routes[0].path = "/{".to_owned();
        let err = SandboxHost::from_module(manifest, &wasm)
            .expect_err("an unbuildable route must not produce a host");
        assert!(matches!(err, SandboxLoadError::InvalidManifest(_)), "{err}");
    }

    #[test]
    fn encoding_the_request_frame_is_priced_before_it_is_performed() {
        // The body is cloned into the frame and base64-expanded into the NDJSON
        // line before a single guest instruction runs. With a large body
        // ceiling and a small fuel budget that is megabytes of host CPU outside
        // the declared ceiling, repeatable for as long as a client keeps
        // sending.
        let host = host(guests::HELLO);
        let mut request = get("/hello/greet");
        request.body = vec![b'x'; 200_000];

        // Just above the load-time floor, so the host exists — and far below
        // what encoding this body costs, which is the point.
        let starved = try_host_with(
            guests::HELLO,
            ResourceLimits {
                fuel: host.instantiation_fuel() + 1,
                ..ResourceLimits::default()
            },
        )
        .expect("the fixture loads");
        let outcome = starved.run(&request);
        assert!(
            matches!(outcome.result, Err(SandboxFailure::FuelExhausted { .. })),
            "{:?}",
            outcome.result
        );

        // And an honest request pays for it rather than getting it free.
        let outcome = host.run(&request);
        assert!(
            outcome.fuel_used >= 200_000 / BYTES_PER_FUEL,
            "the encoding was not charged: {} units",
            outcome.fuel_used
        );
    }

    #[test]
    fn a_direct_caller_cannot_start_more_instances_than_the_manifest_allows() {
        // `SandboxedPlugin::serve` has a semaphore of its own, so HTTP traffic
        // is bounded. `run` is public, though, and an embedder calling it
        // directly used to bypass admission entirely — while the manifest
        // validator accepts limits on the premise that
        // `request_footprint_bytes() × max_concurrency` bounds the plugin.
        let host = try_host_with(
            guests::HELLO,
            ResourceLimits {
                max_concurrency: 1,
                ..ResourceLimits::default()
            },
        )
        .expect("the fixture loads");

        // Hold the only permit, exactly as an in-flight request would, rather
        // than racing a real one: the property is the admission, not the race.
        let held = host
            .permits
            .try_acquire()
            .expect("the first permit is free");
        assert_eq!(
            host.run(&get("/hello/greet")).result,
            Err(SandboxFailure::AtCapacity { max: 1 }),
            "a second concurrent run was admitted past max_concurrency = 1"
        );

        // And the permit comes back: the ceiling is on requests executing at
        // once, not a budget the plugin spends down.
        drop(held);
        assert!(
            host.run(&get("/hello/greet")).result.is_ok(),
            "the permit was not returned when the run finished"
        );
    }

    #[test]
    fn a_fuel_budget_that_cannot_reach_start_is_refused_at_load() {
        // `fuel = 1` passes the manifest's own range check, and every request
        // then spends the instantiation charge before `_start` — so every route
        // the manifest declares answers 504, always. `inspect` reporting that
        // artifact as loadable is worse than reporting nothing, because an
        // operator installs on the strength of the verdict.
        let err = try_host_with(
            guests::HELLO,
            ResourceLimits {
                fuel: 1,
                ..ResourceLimits::default()
            },
        )
        .expect_err("a budget below the fixed charges must not produce a host");
        let SandboxLoadError::FuelBelowFixedCharges {
            fuel,
            instantiation,
        } = err
        else {
            panic!("expected the fuel refusal, got {err:?}");
        };
        assert_eq!(fuel, 1);
        assert!(
            instantiation >= 1,
            "the charge must be real to refuse against"
        );

        // And a budget that clears the charge still loads: this is a floor, not
        // a demand for the default.
        assert!(
            try_host_with(
                guests::HELLO,
                ResourceLimits {
                    fuel: instantiation + 1,
                    ..ResourceLimits::default()
                },
            )
            .is_ok(),
            "a budget just above the fixed charge is legal"
        );
    }

    #[test]
    fn the_import_list_a_review_surface_shows_is_bounded() {
        // Each name is excerpted where it is rendered, but the *count* is a
        // separate amplification: a legal module can carry far more imports
        // than a person will read, and formatting each into its own `String`
        // turns the artifact into memory in the process refusing it.
        let imports = (0..MAX_REPORTED_IMPORTS + 50)
            .map(|n| format!(r#"  (import "env" "f{n}" (func))"#))
            .collect::<Vec<_>>()
            .join("\n");
        let wat = format!("(module\n{imports}\n  (func (export \"_start\") (nop)))");
        let wasm = wat::parse_str(&wat).expect("the fixture is valid WAT");

        let listed = SandboxHost::imports_of(&wasm).expect("a module's imports can be read");
        assert!(
            listed.len() <= MAX_REPORTED_IMPORTS + 1,
            "the list is unbounded: {} entries",
            listed.len()
        );
        // The operator must not read a truncated list as the whole of it.
        let last = listed.last().expect("a non-empty list");
        assert!(last.contains("truncated") && last.contains("50"), "{last}");
    }

    #[test]
    fn a_module_declaring_more_imports_than_the_ceiling_is_refused_at_load() {
        // The reporting cap bounds the *review surface*; it does nothing for the
        // runtime path, where every import is resolved and retained per
        // instance — per request. A module repeating one allowlisted import
        // makes every request pay for those resolutions, and neither the fuel
        // charge nor `request_footprint_bytes` knew about them.
        let imports = (0..=MAX_IMPORTS)
            .map(|_| {
                format!(
                    r#"  (import "{WASI}" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let wat = format!(
            "(module\n{imports}\n  (memory (export \"memory\") 1)\n  (func (export \"_start\") (nop)))"
        );
        let wasm = wat::parse_str(&wat).expect("the fixture is valid WAT");

        let err = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm)
            .expect_err("a module past the import ceiling must not load");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive {
                    what: "imports",
                    ..
                }
            ),
            "{err:?}"
        );
        // Every import here is allowlisted, so nothing but the *count* refuses
        // it: the ceiling is structural, not a second spelling of the
        // capability gate.
    }

    #[test]
    fn the_import_ceiling_does_not_weaken_the_forbidden_import_gate() {
        // A module hiding one forbidden import behind a crowd is still refused;
        // the count check running first changes why, never whether.
        let mut lines = (0..=MAX_IMPORTS)
            .map(|_| {
                format!(
                    r#"  (import "{WASI}" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#
                )
            })
            .collect::<Vec<_>>();
        lines.push(r#"  (import "env" "escape" (func))"#.to_owned());
        let wat = format!(
            "(module\n{}\n  (memory (export \"memory\") 1)\n  (func (export \"_start\") (nop)))",
            lines.join("\n")
        );
        let wasm = wat::parse_str(&wat).expect("the fixture is valid WAT");
        assert!(
            SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm).is_err(),
            "a module carrying a forbidden import must never load"
        );
    }

    #[test]
    fn resolving_imports_is_priced_into_the_instantiation_charge() {
        // Per-instance work the guest never executes but every request pays
        // for. Unpriced, it is host CPU bought for free — the same defect the
        // host-side copying charge exists to close.
        let bare = wat::parse_str(
            "(module (memory (export \"memory\") 1) (func (export \"_start\") (nop)))",
        )
        .expect("the fixture is valid WAT");
        let imports = (0..32)
            .map(|_| {
                format!(
                    r#"  (import "{WASI}" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let with_imports = wat::parse_str(format!(
            "(module\n{imports}\n  (memory (export \"memory\") 1)\n  (func (export \"_start\") (nop)))"
        ))
        .expect("the fixture is valid WAT");

        let cheap = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &bare)
            .expect("loads")
            .instantiation_fuel();
        let dear =
            SandboxHost::from_module(manifest_with(ResourceLimits::default()), &with_imports)
                .expect("loads")
                .instantiation_fuel();
        assert_eq!(
            dear,
            cheap + 32,
            "each admitted import must cost a unit: {cheap} then {dear}"
        );
    }

    #[test]
    fn a_metadata_list_of_empty_pairs_is_charged_for_its_structure() {
        // Summing only the contents leaves a million empty pairs weighing
        // nothing: past a byte ceiling for free, then cloned into the frame and
        // expanded into real JSON syntax anyway. The same shape as a response
        // frame full of `["",""]`, and it has to be priced on this side too.
        let host = host(guests::HELLO);
        let mut request = get("/hello/greet");
        request.path_params = vec![(String::new(), String::new()); 100_000];

        let err = host
            .run(&request)
            .result
            .expect_err("a list of empty pairs must not be free");
        assert!(
            matches!(err, SandboxFailure::RequestMetadataBudget { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_module_whose_tables_start_over_the_ceiling_is_refused_at_load() {
        // The limiter enforces the table ceiling at instantiation, which is per
        // request — so a module already over it at rest loaded cleanly and then
        // failed every request. Same defect as a fuel budget below the fixed
        // charges: a passing verdict on an artifact that can never answer.
        let over = MAX_TABLE_ELEMENTS + 1;
        let wasm = wat::parse_str(format!(
            "(module (table {over} funcref) (memory (export \"memory\") 1) (func (export \"_start\") (nop)))"
        ))
        .expect("the fixture is valid WAT");

        let err = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm)
            .expect_err("a module over the table ceiling must not load");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive {
                    what: "initial table elements",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_module_whose_code_section_is_too_large_is_refused_before_compilation() {
        // One function declares one entry however long its body is, so the
        // declaration ceilings never saw this: a single body filling the file
        // allowance still hands the compiler tens of megabytes to translate.
        // The section header carries the size, so the refusal costs one LEB128
        // and never touches the instruction stream.
        let mut wasm = wat::parse_str(
            "(module (memory (export \"memory\") 1) (func (export \"_start\") (nop)))",
        )
        .expect("the fixture is valid WAT");

        // Rewrite the code section's declared size to just over the ceiling
        // rather than building a module that large: the check reads the header,
        // and a fixture of real instructions would cost the test minutes.
        let over = u32::try_from(MAX_CODE_BYTES + 1).expect("fits");
        let mut header = vec![10u8]; // the code section id
        let mut size = over;
        loop {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size != 0 {
                byte |= 0x80;
            }
            header.push(byte);
            if size == 0 {
                break;
            }
        }
        // The bytes have to actually be there: a header claiming a length the
        // file does not carry is refused as malformed, which is a correct
        // refusal but a different one from the ceiling under test.
        header.extend(std::iter::repeat_n(0u8, over as usize));
        wasm.extend_from_slice(&header);

        let err = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm)
            .expect_err("an oversized code section must not reach the compiler");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive {
                    what: "code section bytes",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_module_declaring_more_globals_than_the_ceiling_is_refused_at_load() {
        // Every instance allocates and initialises its own copy of each global,
        // and an instance is per request. The aggregate declared-entry ceiling
        // is far too generous to bound them: a module can sit well under a
        // million total entries and still carry hundreds of thousands of
        // globals, which no fuel charge priced and no footprint counted.
        use std::fmt::Write as _;

        let mut wat = String::from("(module\n  (memory (export \"memory\") 1)\n");
        for _ in 0..=MAX_GLOBALS {
            let _ = writeln!(wat, "  (global i32 (i32.const 0))");
        }
        wat.push_str("  (func (export \"_start\") (nop))\n)");
        let wasm = wat::parse_str(&wat).expect("the fixture is valid WAT");

        let err = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm)
            .expect_err("a module past the globals ceiling must not load");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive {
                    what: "globals",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn globals_are_priced_into_the_instantiation_charge() {
        // Per-instance work the guest never executes but every request pays
        // for — the same reason segments and imports are charged.
        use std::fmt::Write as _;

        let bare = wat::parse_str(
            "(module (memory (export \"memory\") 1) (func (export \"_start\") (nop)))",
        )
        .expect("the fixture is valid WAT");
        let mut wat = String::from("(module\n  (memory (export \"memory\") 1)\n");
        for _ in 0..64 {
            let _ = writeln!(wat, "  (global i32 (i32.const 0))");
        }
        wat.push_str("  (func (export \"_start\") (nop))\n)");
        let with_globals = wat::parse_str(&wat).expect("the fixture is valid WAT");

        let cheap = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &bare)
            .expect("loads")
            .instantiation_fuel();
        let dear =
            SandboxHost::from_module(manifest_with(ResourceLimits::default()), &with_globals)
                .expect("loads")
                .instantiation_fuel();
        assert_eq!(
            dear,
            cheap + 64,
            "each admitted global must cost a unit: {cheap} then {dear}"
        );
    }

    #[test]
    fn a_module_with_more_tables_than_the_store_will_build_is_refused_at_load() {
        // Sizes and count are separate ceilings, and the element sum says
        // nothing about the second: five *empty* tables cost no elements at all
        // and still exceed what the limiter will build, so the artifact loaded,
        // inspected clean, and failed every request at instantiation.
        use std::fmt::Write as _;

        let mut wat = String::from("(module\n  (memory (export \"memory\") 1)\n");
        for _ in 0..=MAX_TABLES {
            let _ = writeln!(wat, "  (table 0 funcref)");
        }
        wat.push_str("  (func (export \"_start\") (nop))\n)");
        let wasm = wat::parse_str(&wat).expect("the fixture is valid WAT");

        let err = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm)
            .expect_err("a module past the table-count ceiling must not load");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive { what: "tables", .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn tables_within_the_ceiling_still_load() {
        // The ceiling is a ceiling, not a ban: a module with real tables must
        // still mount, and the sum is across the module rather than per table.
        let half = MAX_TABLE_ELEMENTS / 4;
        let wasm = wat::parse_str(format!(
            "(module (table {half} funcref) (table {half} funcref) (memory (export \"memory\") 1) (func (export \"_start\") (nop)))"
        ))
        .expect("the fixture is valid WAT");
        assert!(
            SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm).is_ok(),
            "tables under the ceiling must load"
        );
    }

    #[test]
    fn a_module_declaring_more_entries_than_the_ceiling_never_reaches_the_compiler() {
        // `Module::new` builds a representation of every declaration before any
        // later ceiling runs, so the file's size is not a bound on what
        // compiling it costs. The section headers carry the counts, so the
        // shape is knowable first.
        //
        // A header claiming far more entries than the section holds is refused
        // either way — by this ceiling if the claim is large, by wasmi if the
        // bytes do not back it. Both are a refusal before anything allocates
        // per entry.
        let mut wasm = wat::parse_str("(module (func (export \"_start\") (nop)))")
            .expect("the fixture is valid WAT");
        // Rewrite the type section's declared count to something no 64 MiB file
        // could honestly carry.
        let forged = {
            let mut bytes = vec![0x01u8, 0x05]; // type section, 5 bytes
            bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07]); // count: ~2^31
            bytes
        };
        wasm.splice(8..8, forged);

        let err = SandboxHost::from_module(manifest_with(ResourceLimits::default()), &wasm)
            .expect_err("a module past the declared-entry ceiling must not load");
        assert!(
            matches!(
                err,
                SandboxLoadError::InstantiationTooExpensive {
                    what: "declared section entries",
                    ..
                }
            ),
            "{err:?}"
        );

        // And the review surface refuses it too: `inspect` runs this on
        // artifacts nobody has audited, so it must not be the way one exhausts
        // the process reviewing it.
        assert!(
            SandboxHost::imports_of(&wasm).is_err(),
            "the import listing compiled a module the loader would not"
        );
    }

    #[test]
    fn request_metadata_over_the_ceiling_is_refused_before_the_frame_is_built() {
        // The body ceiling was the manifest's and only ever covered the body.
        // Everything else on a `SandboxRequest` — query, headers, path params —
        // is cloned into the frame and serialised into the NDJSON line just the
        // same, and `request_footprint_bytes` budgets for none of it. The
        // encoding charge prices those bytes, but a manifest may declare fuel
        // enough to buy more than a terabyte of them, so pricing is not a bound.
        let host = host(guests::HELLO);
        let mut request = get("/hello/greet");
        request.query = "x=".repeat(MAX_REQUEST_METADATA_BYTES);

        let outcome = host.run(&request);
        let err = outcome
            .result
            .expect_err("oversized metadata must be refused");
        assert!(
            matches!(err, SandboxFailure::RequestMetadataBudget { .. }),
            "{err:?}"
        );
        // The caller's request was refused, not the plugin's answer: 413, the
        // same door the body ceiling answers through.
        assert_eq!(err.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            outcome.fuel_used, 0,
            "nothing was built, so nothing was spent"
        );
    }

    #[test]
    fn the_metadata_ceiling_leaves_an_ordinary_request_alone() {
        // A ceiling that a real request could reach would be a bug of its own:
        // every HTTP server in front of this caps a URI and a header block far
        // below it.
        let host = host(guests::HELLO);
        let mut request = get("/hello/greet");
        request.query = "x=1&".repeat(64);
        request
            .headers
            .push(("x-trace".to_owned(), "a".repeat(1024)));
        assert!(
            host.run(&request).result.is_ok(),
            "an ordinary request must not be caught by the metadata ceiling"
        );
    }

    #[test]
    fn a_body_over_the_ceiling_is_refused_by_run_itself() {
        // The Axum adapter applies the ceiling while reading, so nothing
        // oversized reaches `run` on that path. But `run` is public: an
        // embedder builds the `SandboxRequest` itself, and a manifest with
        // generous fuel would otherwise buy an arbitrarily large host-side
        // copy — cloned into the frame and base64-expanded — of a body the
        // manifest said it would never accept.
        let host = try_host_with(
            guests::HELLO,
            ResourceLimits {
                max_request_body_bytes: 1_024,
                // Deliberately generous — the largest a manifest may declare.
                // The encoding price must not be what saves us here, or the
                // ceiling is decorative on this path.
                fuel: 100_000_000_000,
                ..ResourceLimits::default()
            },
        )
        .expect("the fixture loads");

        let mut request = get("/hello/greet");
        request.body = vec![b'x'; 1_025];
        let outcome = host.run(&request);
        assert_eq!(
            outcome.result,
            Err(SandboxFailure::RequestBudget {
                max: 1_024,
                len: 1_025,
            }),
            "a body over the ceiling must be refused, not encoded"
        );
        // Refused before the request was priced or walked at all.
        assert_eq!(outcome.fuel_used, 0);
        assert_eq!(outcome.peak_memory_bytes, 0);
        // The same answer the adapter gives for the same condition.
        assert_eq!(
            outcome.result.unwrap_err().status(),
            http::StatusCode::PAYLOAD_TOO_LARGE
        );

        // And a body exactly at the ceiling is still served: the check is a
        // ceiling, not an off-by-one that costs the last byte.
        request.body = vec![b'x'; 1_024];
        let outcome = host.run(&request);
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    }

    #[test]
    fn a_malformed_frame_is_refused() {
        let outcome = host(guests::MALFORMED_FRAME).run(&get("/hello/greet"));
        assert!(
            matches!(outcome.result, Err(SandboxFailure::MalformedFrame(_))),
            "{:?}",
            outcome.result
        );
    }

    #[test]
    fn an_op_the_wire_does_not_define_is_refused() {
        let outcome = host(guests::UNKNOWN_OP).run(&get("/hello/greet"));
        assert!(
            matches!(outcome.result, Err(SandboxFailure::MalformedFrame(_))),
            "{:?}",
            outcome.result
        );
    }

    #[test]
    fn the_first_answer_is_the_answer() {
        let outcome = host(guests::DOUBLE_ANSWER).run(&get("/hello/greet"));
        assert_eq!(
            outcome.result.expect("answers").body,
            b"hello from the sandbox"
        );
    }

    #[test]
    fn every_refusal_stub_matches_the_wasi_signature_it_stands_in_for() {
        // A wrong signature would not weaken the sandbox — it would stop an
        // honest guest from linking at all. This builds a module importing
        // every refusal with the shape the table declares and proves it
        // instantiates and runs.
        use std::fmt::Write as _;

        let mut wat = String::from("(module\n");
        for (name, _, _, signature) in DENIED_IMPORTS {
            let params: Vec<&str> = signature
                .chars()
                .map(|ch| if ch == 'l' { "i64" } else { "i32" })
                .collect();
            let _ = writeln!(
                wat,
                "  (import \"wasi_snapshot_preview1\" \"{name}\" (func (param {params}) (result i32)))",
                params = params.join(" ")
            );
        }
        wat.push_str("  (memory (export \"memory\") 1)\n  (func (export \"_start\") (nop))\n)");
        let outcome = host(&wat).run(&get("/hello/greet"));
        assert!(
            matches!(outcome.result, Err(SandboxFailure::NoAnswer)),
            "every stub must link: {:?}",
            outcome.result
        );
    }

    #[test]
    fn an_import_with_the_wrong_signature_is_refused_at_load() {
        // Name-checking alone lets a module through packaging and inspection
        // and then fails it on every request, as a gateway error nobody can
        // explain from outside.
        let wat = r#"(module
             (import "wasi_snapshot_preview1" "fd_write"
               (func (param i32 i32) (result i32)))
             (memory (export "memory") 1)
             (func (export "_start") (nop)))"#;
        let err = try_host(wat).expect_err("must be refused");
        let SandboxLoadError::ForbiddenImports(denials) = err else {
            panic!("expected a forbidden-import refusal, got {err}");
        };
        assert!(denials[0].operation.contains("fd_write"), "{denials:?}");
        assert!(denials[0].detail.contains("signature"), "{denials:?}");
    }

    #[test]
    fn a_non_function_import_is_refused_at_load() {
        let wat = r#"(module
             (import "wasi_snapshot_preview1" "fd_write" (memory 1))
             (memory (export "memory") 1)
             (func (export "_start") (nop)))"#;
        assert!(matches!(
            try_host(wat),
            Err(SandboxLoadError::ForbiddenImports(_))
        ));
    }

    #[test]
    fn a_start_that_takes_arguments_is_not_a_start() {
        // The host looks `_start` up as `() -> ()`, so anything else loads and
        // then fails on every request.
        let wat = r#"(module
             (memory (export "memory") 1)
             (func (export "_start") (param i32) (nop)))"#;
        assert!(matches!(try_host(wat), Err(SandboxLoadError::MissingStart)));

        let wat = r#"(module
             (memory (export "memory") 1)
             (func (export "_start") (result i32) (i32.const 0)))"#;
        assert!(matches!(try_host(wat), Err(SandboxLoadError::MissingStart)));
    }

    #[test]
    fn the_load_gate_admits_exactly_what_the_shim_defines() {
        for (name, ..) in SERVED_IMPORTS {
            assert!(is_shim_function(name), "{name} is served but not admitted");
        }
        for (name, ..) in DENIED_IMPORTS {
            assert!(is_shim_function(name), "{name} is refused but not admitted");
        }
        for name in ["sock_connect", "path_open_v2", "fd_write2"] {
            assert!(!is_shim_function(name), "{name} must not be admitted");
        }
    }

    #[test]
    fn the_import_list_is_readable_for_review() {
        let imports = host(guests::HELLO).imports();
        assert!(
            imports.contains(&"wasi_snapshot_preview1::fd_read".to_owned()),
            "{imports:?}"
        );
    }
}
