//! The sandbox host: a `wasmi` interpreter plus a deny-by-default WASI shim.
//!
//! # The sandbox *is* this file's import list
//!
//! A WebAssembly guest can do exactly two things on its own: compute, and call
//! a function the host gave it. It has no syscalls, no ambient file
//! descriptors and no way to name anything outside its own linear memory. So
//! the entire authority a sandboxed plugin holds is the list of host functions
//! [`define_wasi_shim`] registers — which is why they are all in one function,
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
/// Every variant is a *plugin* failure. None of them is a host failure, and
/// none of them can be anything other than a 5xx on the plugin's own prefix.
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
}

impl SandboxFailure {
    /// The status this failure serves on the plugin's prefix.
    ///
    /// A budget exhaustion is a 504: the plugin was given a deadline and missed
    /// it. Everything else is a 502: the plugin answered badly or not at all,
    /// which is exactly what a bad gateway is.
    #[must_use]
    pub const fn status(&self) -> http::StatusCode {
        match self {
            Self::FuelExhausted { .. } | Self::OutputBudget { .. } => {
                http::StatusCode::GATEWAY_TIMEOUT
            }
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
        }
    }
}

impl std::error::Error for SandboxFailure {}

/// Why an artifact could not be loaded at all.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxLoadError {
    /// The bytes are not a WebAssembly module this engine can compile.
    Wasm(String),
    /// The module imports something no host function defines.
    ForbiddenImports(Vec<CapabilityDenial>),
    /// The module exports no `_start`.
    MissingStart,
    /// The engine could not be configured.
    Engine(String),
}

impl fmt::Display for SandboxLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::MissingStart => write!(
                f,
                "the plugin exports no `_start`; it must be built as a wasm32-wasip1 *command*"
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

// ── The host ─────────────────────────────────────────────────────────────

/// A compiled sandboxed plugin, ready to serve requests.
///
/// Compilation happens once in [`load`](SandboxHost::load); every call to
/// [`run`](SandboxHost::run) builds a *fresh* store and instance, so no state
/// survives a request and one request's misbehaviour cannot reach the next.
pub struct SandboxHost {
    engine: Engine,
    module: Module,
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

    /// Compile a module against an already-validated manifest.
    ///
    /// Prefer [`load`](Self::load), which also proves the manifest describes
    /// *these* bytes.
    ///
    /// # Errors
    ///
    /// See [`load`](Self::load).
    pub fn from_module(manifest: SandboxManifest, wasm: &[u8]) -> Result<Self, SandboxLoadError> {
        let mut config = Config::default();
        // Fuel metering is what turns "a plugin might loop forever" into "a
        // plugin gets a bounded number of instructions". It must be on before
        // the module is compiled, because the counting is compiled in.
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).map_err(|err| SandboxLoadError::Wasm(err.to_string()))?;

        let forbidden = forbidden_imports(&module);
        if !forbidden.is_empty() {
            return Err(SandboxLoadError::ForbiddenImports(forbidden));
        }
        if !module
            .exports()
            .any(|export| export.name() == "_start" && export.ty().func().is_some())
        {
            return Err(SandboxLoadError::MissingStart);
        }

        Ok(Self {
            engine,
            module,
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
        let engine = Engine::default();
        let module =
            Module::new(&engine, wasm).map_err(|err| SandboxLoadError::Wasm(err.to_string()))?;
        Ok(module
            .imports()
            .map(|import| format!("{}::{}", import.module(), import.name()))
            .collect())
    }

    /// Every import the module declares, as `module::name`, for review.
    #[must_use]
    pub fn imports(&self) -> Vec<String> {
        self.module
            .imports()
            .map(|import| format!("{}::{}", import.module(), import.name()))
            .collect()
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
        let granted: Vec<SandboxCapability> = self.manifest.capabilities.clone();
        let frame = HostFrame::request(request, &granted);

        let line = match to_line(&frame) {
            Ok(line) => line,
            Err(err) => {
                // The host could not encode its own request. Report it as a
                // plugin-prefix failure rather than propagating: the rest of
                // the application is unaffected either way.
                return SandboxOutcome {
                    result: Err(SandboxFailure::Instantiation(err.to_string())),
                    denials: Vec::new(),
                    fuel_used: 0,
                    peak_memory_bytes: 0,
                    stderr: String::new(),
                };
            }
        };
        let mut state = HostState::new(self.manifest.name.clone(), limits, line.as_bytes());
        state.stdin.extend(line.as_bytes());

        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limiter);
        // Set before instantiation, so the budget is in place the moment the
        // first guest instruction runs. wasmi does not meter instantiation
        // itself, which is why the module's data and element segments are
        // bounded at load instead — by `MAX_MODULE_BYTES` on the container.
        if let Err(err) = store.set_fuel(limits.fuel) {
            return SandboxOutcome {
                result: Err(SandboxFailure::Instantiation(err.to_string())),
                denials: Vec::new(),
                fuel_used: 0,
                peak_memory_bytes: 0,
                stderr: String::new(),
            };
        }

        let mut linker = <Linker<HostState>>::new(&self.engine);
        if let Err(err) = define_wasi_shim(&mut linker) {
            return SandboxOutcome {
                result: Err(SandboxFailure::Instantiation(err.to_string())),
                denials: Vec::new(),
                fuel_used: 0,
                peak_memory_bytes: 0,
                stderr: String::new(),
            };
        }

        let started = linker
            .instantiate(&mut store, &self.module)
            .and_then(|pre| pre.start(&mut store));
        let instance = match started {
            Ok(instance) => instance,
            Err(err) => return finish(store, limits, Err(instantiation_failure(&err, limits))),
        };

        let Ok(start) = instance.get_typed_func::<(), ()>(&store, "_start") else {
            // `from_module` already refused a module without `_start`; reaching
            // here would mean the export changed shape, which is still the
            // plugin's problem and not the host's.
            return finish(store, limits, Err(SandboxFailure::NoAnswer));
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
        finish(store, limits, result)
    }
}

/// Drain the store into an outcome, applying the response sanitation and the
/// size ceiling that only the host can enforce.
fn finish(
    store: Store<HostState>,
    limits: ResourceLimits,
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
            let (response, denied) = response.sanitize();
            for name in denied {
                state.deny(
                    DeniedCapability::ResponseHeader,
                    &name,
                    "a sandboxed plugin may not set this response header",
                );
            }
            if let Some(essence) = response.refused_content_type() {
                let detail = format!(
                    "a sandboxed plugin may not serve `{essence}`: a document or a script from \
                     the host's own origin would carry the host's authority"
                );
                state.deny(DeniedCapability::ResponseHeader, "content-type", &detail);
                Err(SandboxFailure::ResponseRefused(
                    super::wire::WireError::UnsupportedContentType(essence).to_string(),
                ))
            } else {
                response
                    .validate()
                    .and_then(|()| response.check_size(limits.max_response_bytes))
                    .map_or_else(
                        |err| Err(SandboxFailure::ResponseRefused(err.to_string())),
                        |()| Ok(response),
                    )
            }
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
        SandboxFailure::Instantiation(err.to_string())
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
    SandboxFailure::Trap(err.to_string())
}

/// Imports the shim does not define, as denials.
fn forbidden_imports(module: &Module) -> Vec<CapabilityDenial> {
    module
        .imports()
        .filter(|import| import.module() != WASI || !is_shim_function(import.name()))
        .map(|import| CapabilityDenial {
            capability: DeniedCapability::UnknownImport,
            operation: format!("{}::{}", import.module(), import.name()),
            detail: "the sandbox defines no such host function, so the plugin is refused before \
                     it runs"
                .to_owned(),
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
        _current: u32,
        desired: u32,
        _maximum: Option<u32>,
    ) -> Result<bool, wasmi::errors::TableError> {
        // Tables hold function references, not bytes; a small ceiling is plenty
        // for a plugin and keeps an indirect-call table from being a second,
        // unmetered allocation channel.
        Ok(desired <= 65_536)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        4
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
            Ok(GuestFrame::Error { detail }) => Err(SandboxFailure::GuestError(detail)),
            Err(err) => Err(SandboxFailure::MalformedFrame(err.to_string())),
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
                let line = String::from_utf8_lossy(&line).into_owned();
                self.on_guest_line(&line);
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
    fn stderr_is_full(&self) -> bool {
        self.stderr.len() >= STDERR_BUDGET_BYTES
    }

    fn stderr_excerpt(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let trimmed = text.trim();
        match trimmed.char_indices().nth(STDERR_EXCERPT) {
            Some((index, _)) => trimmed.get(..index).unwrap_or_default().to_owned(),
            None => trimmed.to_owned(),
        }
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
/// Each one either serves the request dialogue or answers with something inert
/// and fixed. Nothing here reaches outside the guest.
const SERVED_IMPORTS: &[&str] = &[
    "args_get",
    "args_sizes_get",
    "clock_res_get",
    "clock_time_get",
    "environ_get",
    "environ_sizes_get",
    "fd_close",
    "fd_fdstat_get",
    "fd_read",
    "fd_seek",
    "fd_tell",
    "fd_write",
    "proc_exit",
    "random_get",
    "sched_yield",
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

/// Whether the shim defines a WASI function of this name.
///
/// The load-time gate and the shim read the same two tables, so an import that
/// links at runtime is exactly one the gate admits — they cannot drift apart.
fn is_shim_function(name: &str) -> bool {
    SERVED_IMPORTS.contains(&name) || DENIED_IMPORTS.iter().any(|(known, ..)| *known == name)
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
    fn a_response_splitting_header_is_refused() {
        let outcome = host(guests::SPLIT_RESPONSE).run(&get("/hello/greet"));
        let failure = outcome.result.expect_err("must not answer");
        assert!(
            matches!(failure, SandboxFailure::ResponseRefused(_)),
            "{failure}"
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
    fn the_load_gate_admits_exactly_what_the_shim_defines() {
        for name in SERVED_IMPORTS {
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
