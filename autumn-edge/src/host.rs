//! The reference edge host: a `wasmi` interpreter plus a hand-written WASI
//! shim, wired to the NDJSON dialogue over in-memory pipes.
//!
//! This is what makes AC-2 provable. The conformance test runs the *same*
//! request corpus through the native router and through a real
//! `wasm32-wasip1` artifact loaded here, then compares. No CDN account, no
//! vendor SDK, no runtime to install — `wasmi` is a pure-Rust interpreter, so
//! the whole thing is a dev-dependency.
//!
//! It is also a worked specification: a CDN shim only has to provide what this
//! host provides.
//!
//! # Deliberately deterministic, deliberately tiny
//!
//! The shim implements exactly enough WASI for a capsule and no more:
//!
//! | Import | Behaviour |
//! | --- | --- |
//! | `fd_read` (stdin) | the next bytes of the host→guest frame stream |
//! | `fd_write` (stdout) | line-buffered; each line is a guest frame |
//! | `fd_write` (stderr) | captured, surfaced in a fallthrough detail |
//! | `random_get` | a fixed-seed PRNG — same bytes every run |
//! | `clock_time_get` | always `0` |
//! | `environ_*`, `args_*` | empty |
//! | `proc_exit`, `sched_yield`, `fd_close`, `fd_fdstat_get`, `fd_seek` | inert |
//!
//! There is **no** `path_open`, no `fd_prestat_dir_name` that resolves, and no
//! socket import. A capsule therefore has no ambient filesystem and no
//! network: the only way out is the dialogue. The conformance test asserts the
//! artifact's import list against an allowlist so that stays true.
//!
//! Fixing the clock and the entropy is what turns "byte-identical" from a hope
//! into a property: a capsule that reads the time will read the same time
//! twice, and the conformance run's self-equality check catches the handler
//! that managed to be nondeterministic anyway.
//!
//! # Everything a capsule does wrong is a fallthrough
//!
//! A trap (which is what a Rust panic compiles to on wasm), a malformed frame,
//! a capsule that exits without answering — all of them come back as
//! `Ok(EdgeOutcome::Fallthrough { reason: CapsuleError, .. })`, because from
//! the request's point of view they are the same event: the edge could not
//! answer, so the origin must.

use std::collections::VecDeque;
use std::fmt;

use wasmi::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::kv::EdgeKv;
use crate::route::EdgeCapability;
use crate::wire::{
    EdgeOutcome, EdgeRequest, FallthroughReason, GuestFrame, HostFrame, from_line, to_line,
};

/// WASI `errno` values this shim returns.
mod errno {
    pub(super) const SUCCESS: i32 = 0;
    pub(super) const BADF: i32 = 8;
    pub(super) const INVAL: i32 = 28;
    /// Out of space: a stdout line outgrew [`super::STDOUT_LINE_BUDGET_BYTES`].
    pub(super) const NOSPC: i32 = 51;
    /// Seek on a pipe.
    pub(super) const SPIPE: i32 = 70;
}

/// The size of one WASI `iovec` (two `u32`s: pointer and length).
const IOVEC_SIZE: usize = 8;

/// Size of a WASI `fdstat` struct.
const FDSTAT_SIZE: usize = 24;

/// Bytes of captured stderr echoed into a fallthrough detail.
const STDERR_EXCERPT: usize = 512;

/// Something that went wrong on the *host* side. A misbehaving capsule is not
/// one of these — that is an [`EdgeOutcome::Fallthrough`].
#[derive(Debug)]
#[non_exhaustive]
pub enum EdgeHostError {
    /// The bytes are not a valid WebAssembly module.
    Load(String),
    /// The host could not set up or start the instance.
    Host(String),
}

impl fmt::Display for EdgeHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(detail) => write!(f, "could not load the edge artifact: {detail}"),
            Self::Host(detail) => write!(f, "edge host failure: {detail}"),
        }
    }
}

impl std::error::Error for EdgeHostError {}

/// Fuel budget for one request's guest execution (roughly one unit per
/// instruction).
///
/// Sized for headroom, not precision: rendering a page costs on the order of
/// millions of instructions, so a billion-unit budget is ~100× slack for any
/// honest handler while still bounding a runaway loop to seconds instead of
/// forever. Exhaustion is reported as a [`FallthroughReason::CapsuleError`]
/// naming the budget, so the origin serves the request.
pub const FUEL_BUDGET: u64 = 1_000_000_000;

/// Linear-memory ceiling for one request's guest instance, in bytes (256 MiB).
///
/// Fuel bounds computation; this bounds allocation. Without it a single
/// `memory.grow` could demand gigabytes from the host allocator and kill the
/// process instead of producing a fallthrough. When the ceiling is hit the
/// grow fails inside the guest (its allocator then aborts, which traps) or
/// instantiation is refused — both surface as a
/// [`FallthroughReason::CapsuleError`], never as a dead host.
pub const MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// Cap on the guest's buffered (newline-less) stdout, in bytes (64 MiB).
///
/// Stdout is the wire: complete NDJSON lines are consumed and cleared as they
/// arrive, so this bounds only a line that never ends — a guest streaming
/// bytes without `\n` would otherwise grow a host-owned buffer without limit,
/// unmetered by fuel (host-side copies burn no guest fuel) or the linear-memory
/// ceiling (which bounds each chunk, not their sum). Sized above any honest
/// frame: a response with a 16 MiB body is ~22 MiB base64'd. Exceeding it
/// fails the guest's `fd_write` with `ENOSPC`, which ends as a
/// [`FallthroughReason::CapsuleError`].
pub const STDOUT_LINE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Cap on accumulated guest stderr, in bytes (1 MiB).
///
/// Stderr is diagnostics: only [`STDERR_EXCERPT`] characters ever reach a
/// fallthrough detail, so bytes past this cap are silently dropped rather
/// than failing the write — a panicking guest should never be punished for
/// being chatty while it dies.
pub const STDERR_BUDGET_BYTES: usize = 1024 * 1024;

/// A compiled edge capsule, ready to serve requests.
///
/// Compilation happens once in [`from_bytes`](EdgeArtifact::from_bytes);
/// [`run`](EdgeArtifact::run) instantiates a *fresh* store per request. That
/// is the honest model for an edge deployment — no state survives a request —
/// and it is also what makes the determinism check meaningful.
pub struct EdgeArtifact {
    engine: Engine,
    module: Module,
}

impl fmt::Debug for EdgeArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EdgeArtifact")
            .field("imports", &self.imports())
            .finish_non_exhaustive()
    }
}

impl EdgeArtifact {
    /// Load a `wasm32-wasip1` capsule.
    ///
    /// # Errors
    ///
    /// Returns [`EdgeHostError::Load`] if the bytes are not a valid module.
    pub fn from_bytes(wasm: &[u8]) -> Result<Self, EdgeHostError> {
        // Fuel metering bounds every guest execution: a capsule that loops
        // forever must become a fallthrough, not a hung host. The counting
        // itself does not change what a well-behaved capsule computes.
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).map_err(|err| EdgeHostError::Load(err.to_string()))?;
        Ok(Self { engine, module })
    }

    /// Every import the module declares, as `module::name`.
    ///
    /// The conformance suite asserts this against an allowlist: an artifact
    /// that imports a socket or a `path_open` is an artifact that could reach
    /// something the edge lane promises it cannot.
    #[must_use]
    pub fn imports(&self) -> Vec<String> {
        self.module
            .imports()
            .map(|import| format!("{}::{}", import.module(), import.name()))
            .collect()
    }

    /// Serve one request.
    ///
    /// `kv` is consulted live, mid-request, exactly as a real host would: the
    /// capsule's `EdgeCache::get` becomes a `kv_get` frame that this function
    /// answers before the capsule can continue.
    ///
    /// Sensitive headers are stripped here, before the request frame is built.
    ///
    /// # Errors
    ///
    /// Returns [`EdgeHostError::Host`] only for host-side setup failures. A
    /// capsule that traps, garbles a frame, or never answers produces
    /// `Ok(Fallthrough(CapsuleError, …))`.
    pub fn run(
        &self,
        request: &EdgeRequest,
        capabilities: &[EdgeCapability],
        kv: &dyn EdgeKv,
    ) -> Result<EdgeOutcome, EdgeHostError> {
        let frame = request.clone().into_host_frame(capabilities);
        let line = to_line(&frame).map_err(|err| EdgeHostError::Host(err.to_string()))?;

        let mut state = HostState::new(kv, capabilities.contains(&EdgeCapability::Kv));
        state.stdin.extend(line.as_bytes());

        let mut store = Store::new(&self.engine, state);
        // Set before instantiation: data-segment init and the start section
        // burn fuel too. Exhaustion at any point traps the guest; the check
        // after `_start` translates it into a deterministic fallthrough.
        store
            .set_fuel(FUEL_BUDGET)
            .map_err(|err| EdgeHostError::Host(format!("could not set the fuel budget: {err}")))?;
        store.limiter(|state| &mut state.limits);
        let mut linker = <Linker<HostState<'_>>>::new(&self.engine);
        define_wasi_shim(&mut linker)?;

        let started = linker
            .instantiate(&mut store, &self.module)
            .and_then(|pre| pre.start(&mut store));
        let instance = match started {
            Ok(instance) => instance,
            Err(err) => {
                return Ok(capsule_error(
                    &store.into_data(),
                    &format!("the capsule could not be instantiated: {err}"),
                ));
            }
        };

        let Ok(start) = instance.get_typed_func::<(), ()>(&store, "_start") else {
            return Ok(capsule_error(
                &store.into_data(),
                "the capsule exports no `_start`; it must be built as a wasm32-wasip1 *command*",
            ));
        };

        let trap = start.call(&mut store, ()).err();
        let fuel_exhausted = trap
            .as_ref()
            .and_then(wasmi::Error::as_trap_code)
            .is_some_and(|code| matches!(code, wasmi::core::TrapCode::OutOfFuel));
        let state = store.into_data();

        if let Some(outcome) = state.terminal.clone() {
            return Ok(outcome);
        }
        if fuel_exhausted {
            return Ok(capsule_error(
                &state,
                &format!(
                    "the capsule exceeded its execution budget of {FUEL_BUDGET} fuel units \
                     without answering — a runaway loop falls through to the origin instead \
                     of hanging the host"
                ),
            ));
        }
        let detail = trap.map_or_else(
            || "the capsule exited without answering".to_owned(),
            |err| format!("the capsule trapped: {err}"),
        );
        Ok(capsule_error(&state, &detail))
    }
}

fn capsule_error(state: &HostState<'_>, detail: &str) -> EdgeOutcome {
    let stderr = state.stderr_excerpt();
    let detail = if stderr.is_empty() {
        detail.to_owned()
    } else {
        format!("{detail} (stderr: {stderr})")
    };
    EdgeOutcome::fallthrough(FallthroughReason::CapsuleError, detail)
}

// ── host state ───────────────────────────────────────────────────────

struct HostState<'kv> {
    /// Bytes the guest has not read yet.
    stdin: VecDeque<u8>,
    /// The partial stdout line being accumulated.
    stdout_line: Vec<u8>,
    /// Everything the guest wrote to stderr.
    stderr: Vec<u8>,
    kv: &'kv dyn EdgeKv,
    /// Whether this host offered the `kv` capability for this request.
    kv_provided: bool,
    terminal: Option<EdgeOutcome>,
    /// Fixed-seed PRNG state, so `random_get` is reproducible.
    random_state: u64,
    /// Resource ceilings ([`MEMORY_BUDGET_BYTES`]) the store's limiter answers
    /// from — wasmi queries it on every `memory.grow` and at instantiation.
    limits: StoreLimits,
}

impl<'kv> HostState<'kv> {
    /// The seed is arbitrary but fixed: the property that matters is that two
    /// runs of the same artifact see the same bytes.
    const RANDOM_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

    fn new(kv: &'kv dyn EdgeKv, kv_provided: bool) -> Self {
        Self {
            stdin: VecDeque::new(),
            stdout_line: Vec::new(),
            stderr: Vec::new(),
            kv,
            kv_provided,
            terminal: None,
            random_state: Self::RANDOM_SEED,
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_BUDGET_BYTES)
                .build(),
        }
    }

    fn feed(&mut self, line: &str) {
        self.stdin.extend(line.as_bytes());
    }

    /// Handle one complete line the guest wrote to stdout.
    fn on_guest_line(&mut self, line: &str) {
        if self.terminal.is_some() {
            // Already answered: anything after is noise from a capsule that
            // does not respect the protocol. Ignore it rather than letting it
            // overwrite a good answer.
            return;
        }

        let frame = match from_line::<GuestFrame>(line) {
            Ok(frame) => frame,
            Err(err) => {
                self.terminal = Some(EdgeOutcome::fallthrough(
                    FallthroughReason::CapsuleError,
                    format!("the capsule emitted a malformed frame: {err}"),
                ));
                return;
            }
        };

        match frame {
            GuestFrame::KvGet { key } => {
                let value = if self.kv_provided {
                    self.kv.get(&key)
                } else {
                    // The capsule asked for a seam this host never offered.
                    // Answer with a miss — a miss is always legal — rather
                    // than deadlocking it.
                    None
                };
                match to_line(&HostFrame::KvValue { value }) {
                    Ok(line) => self.feed(&line),
                    Err(err) => {
                        self.terminal = Some(EdgeOutcome::fallthrough(
                            FallthroughReason::CapsuleError,
                            format!("could not answer a kv_get: {err}"),
                        ));
                    }
                }
            }
            terminal => self.terminal = terminal.into_outcome(),
        }
    }

    /// Buffer stdout bytes, consuming each completed NDJSON line.
    ///
    /// Returns `false` — the guest's write must fail — once a single line
    /// exceeds [`STDOUT_LINE_BUDGET_BYTES`].
    #[must_use]
    fn write_stdout(&mut self, bytes: &[u8]) -> bool {
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.stdout_line);
                let line = String::from_utf8_lossy(&line).into_owned();
                self.on_guest_line(&line);
            } else {
                if self.stdout_line.len() >= STDOUT_LINE_BUDGET_BYTES {
                    return false;
                }
                self.stdout_line.push(*byte);
            }
        }
        true
    }

    /// Buffer stderr bytes, silently dropping everything past
    /// [`STDERR_BUDGET_BYTES`] — the excerpt that reaches a fallthrough detail
    /// is far shorter anyway.
    fn write_stderr(&mut self, bytes: &[u8]) {
        let room = STDERR_BUDGET_BYTES.saturating_sub(self.stderr.len());
        if let Some(kept) = bytes.get(..bytes.len().min(room)) {
            self.stderr.extend_from_slice(kept);
        }
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

    /// Pop up to `len` bytes of pending stdin.
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

// ── the WASI shim ────────────────────────────────────────────────────

type Shim<'kv> = Linker<HostState<'kv>>;

fn memory_of(caller: &Caller<'_, HostState<'_>>) -> Option<wasmi::Memory> {
    caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
}

fn read_u32(caller: &Caller<'_, HostState<'_>>, memory: wasmi::Memory, at: usize) -> Option<u32> {
    let mut buffer = [0u8; 4];
    memory.read(caller, at, &mut buffer).ok()?;
    Some(u32::from_le_bytes(buffer))
}

fn write_u32(
    caller: &mut Caller<'_, HostState<'_>>,
    memory: wasmi::Memory,
    at: usize,
    value: u32,
) -> Option<()> {
    memory.write(caller, at, &value.to_le_bytes()).ok()
}

/// Read one `iovec` at `index` of the array starting at `base`.
fn iovec(
    caller: &Caller<'_, HostState<'_>>,
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

/// Register the guest-visible WASI surface.
///
/// One function per import, all in one place, so the entire ambient authority
/// a capsule has is readable top to bottom.
#[allow(
    clippy::too_many_lines,
    reason = "one registration per WASI import; splitting them up would hide \
              the fact that this list IS the capsule's sandbox"
)]
fn define_wasi_shim(linker: &mut Shim<'_>) -> Result<(), EdgeHostError> {
    const WASI: &str = "wasi_snapshot_preview1";

    fn host_error(err: impl fmt::Display) -> EdgeHostError {
        EdgeHostError::Host(err.to_string())
    }

    linker
        .func_wrap(
            WASI,
            "fd_read",
            |mut caller: Caller<'_, HostState<'_>>,
             fd: i32,
             iovs: i32,
             iovs_len: i32,
             nread: i32| {
                if fd != 0 {
                    return errno::BADF;
                }
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };

                let mut total: u32 = 0;
                for index in 0..iovs_len {
                    let Some((pointer, length)) = iovec(&caller, memory, iovs, index) else {
                        return errno::INVAL;
                    };
                    let chunk = caller.data_mut().take_stdin(length);
                    if memory.write(&mut caller, pointer, &chunk).is_err() {
                        return errno::INVAL;
                    }
                    let Ok(read) = u32::try_from(chunk.len()) else {
                        return errno::INVAL;
                    };
                    let Some(sum) = total.checked_add(read) else {
                        return errno::INVAL;
                    };
                    total = sum;
                    if chunk.len() < length {
                        break;
                    }
                }

                let Ok(at) = usize::try_from(nread) else {
                    return errno::INVAL;
                };
                if write_u32(&mut caller, memory, at, total).is_none() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(host_error)?;

    linker
        .func_wrap(
            WASI,
            "fd_write",
            |mut caller: Caller<'_, HostState<'_>>,
             fd: i32,
             iovs: i32,
             iovs_len: i32,
             nwritten: i32| {
                if fd != 1 && fd != 2 {
                    return errno::BADF;
                }
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };

                let mut total: u32 = 0;
                for index in 0..iovs_len {
                    let Some((pointer, length)) = iovec(&caller, memory, iovs, index) else {
                        return errno::INVAL;
                    };
                    // Bounds-check BEFORE allocating: `length` is guest-chosen,
                    // and a `u32::MAX` iovec must fail with EINVAL, not size a
                    // host allocation. A range beyond the guest's linear
                    // memory can never be read anyway.
                    let in_bounds = pointer
                        .checked_add(length)
                        .is_some_and(|end| end <= memory.data_size(&caller));
                    if !in_bounds {
                        return errno::INVAL;
                    }
                    let mut chunk = vec![0u8; length];
                    if memory.read(&caller, pointer, &mut chunk).is_err() {
                        return errno::INVAL;
                    }
                    let Ok(written) = u32::try_from(length) else {
                        return errno::INVAL;
                    };
                    let Some(sum) = total.checked_add(written) else {
                        return errno::INVAL;
                    };
                    total = sum;

                    let state = caller.data_mut();
                    if fd == 2 {
                        state.write_stderr(&chunk);
                    } else if !state.write_stdout(&chunk) {
                        // A single stdout line has outgrown any honest frame:
                        // fail the write so the guest aborts and the request
                        // falls through, instead of growing host memory.
                        return errno::NOSPC;
                    }
                }

                let Ok(at) = usize::try_from(nwritten) else {
                    return errno::INVAL;
                };
                if write_u32(&mut caller, memory, at, total).is_none() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(host_error)?;

    linker
        .func_wrap(
            WASI,
            "random_get",
            |mut caller: Caller<'_, HostState<'_>>, pointer: i32, len: i32| {
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };
                let (Ok(pointer), Ok(len)) = (usize::try_from(pointer), usize::try_from(len))
                else {
                    return errno::INVAL;
                };
                // Same bounds-before-allocate rule as `fd_write`: the write
                // below would reject an out-of-range destination, but only
                // after this buffer was already sized by the guest's `len`.
                let in_bounds = pointer
                    .checked_add(len)
                    .is_some_and(|end| end <= memory.data_size(&caller));
                if !in_bounds {
                    return errno::INVAL;
                }
                let bytes: Vec<u8> = {
                    let state = caller.data_mut();
                    (0..len).map(|_| state.next_random_byte()).collect()
                };
                if memory.write(&mut caller, pointer, &bytes).is_err() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(host_error)?;

    linker
        .func_wrap(
            WASI,
            "clock_time_get",
            |mut caller: Caller<'_, HostState<'_>>, _clock: i32, _precision: i64, out: i32| {
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };
                let Ok(at) = usize::try_from(out) else {
                    return errno::INVAL;
                };
                // Time stands still at the edge: a capsule that reads the
                // clock reads the same value on every run, so a clock read can
                // never be what makes a conformance run flaky.
                if memory.write(&mut caller, at, &0u64.to_le_bytes()).is_err() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(host_error)?;

    linker
        .func_wrap(
            WASI,
            "environ_get",
            |_caller: Caller<'_, HostState<'_>>, _environ: i32, _buffer: i32| errno::SUCCESS,
        )
        .map_err(host_error)?;
    linker
        .func_wrap(WASI, "environ_sizes_get", write_two_zeroes)
        .map_err(host_error)?;
    linker
        .func_wrap(
            WASI,
            "args_get",
            |_caller: Caller<'_, HostState<'_>>, _argv: i32, _buffer: i32| errno::SUCCESS,
        )
        .map_err(host_error)?;
    linker
        .func_wrap(WASI, "args_sizes_get", write_two_zeroes)
        .map_err(host_error)?;

    linker
        .func_wrap::<_, ()>(
            WASI,
            "proc_exit",
            // Returning instead of trapping: the terminal frame is what
            // decides the outcome, and a capsule that exits after answering
            // has done nothing wrong.
            |_caller: Caller<'_, HostState<'_>>, _code: i32| {},
        )
        .map_err(host_error)?;

    linker
        .func_wrap(WASI, "sched_yield", |_caller: Caller<'_, HostState<'_>>| {
            errno::SUCCESS
        })
        .map_err(host_error)?;
    linker
        .func_wrap(
            WASI,
            "fd_close",
            |_caller: Caller<'_, HostState<'_>>, _fd: i32| errno::SUCCESS,
        )
        .map_err(host_error)?;
    linker
        .func_wrap(
            WASI,
            "fd_seek",
            |_caller: Caller<'_, HostState<'_>>,
             _fd: i32,
             _offset: i64,
             _whence: i32,
             _out: i32| { errno::SPIPE },
        )
        .map_err(host_error)?;

    linker
        .func_wrap(
            WASI,
            "fd_fdstat_get",
            |mut caller: Caller<'_, HostState<'_>>, fd: i32, out: i32| {
                if !(0..=2).contains(&fd) {
                    return errno::BADF;
                }
                let Some(memory) = memory_of(&caller) else {
                    return errno::INVAL;
                };
                let Ok(at) = usize::try_from(out) else {
                    return errno::INVAL;
                };
                // filetype = 2 (character device), no flags, no rights: stdio
                // is a pipe here and nothing more.
                let mut fdstat = [0u8; FDSTAT_SIZE];
                if let Some(filetype) = fdstat.first_mut() {
                    *filetype = 2;
                }
                if memory.write(&mut caller, at, &fdstat).is_err() {
                    return errno::INVAL;
                }
                errno::SUCCESS
            },
        )
        .map_err(host_error)?;

    // No preopened directories, ever: a capsule has no filesystem.
    linker
        .func_wrap(
            WASI,
            "fd_prestat_get",
            |_caller: Caller<'_, HostState<'_>>, _fd: i32, _out: i32| errno::BADF,
        )
        .map_err(host_error)?;
    linker
        .func_wrap(
            WASI,
            "fd_prestat_dir_name",
            |_caller: Caller<'_, HostState<'_>>, _fd: i32, _path: i32, _len: i32| errno::BADF,
        )
        .map_err(host_error)?;

    Ok(())
}

fn write_two_zeroes(mut caller: Caller<'_, HostState<'_>>, first: i32, second: i32) -> i32 {
    let Some(memory) = memory_of(&caller) else {
        return errno::INVAL;
    };
    let (Ok(first), Ok(second)) = (usize::try_from(first), usize::try_from(second)) else {
        return errno::INVAL;
    };
    if write_u32(&mut caller, memory, first, 0).is_none()
        || write_u32(&mut caller, memory, second, 0).is_none()
    {
        return errno::INVAL;
    }
    errno::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::InMemoryEdgeKv;
    use crate::wire::EdgeResponse;

    fn state_with(kv: &dyn EdgeKv, kv_provided: bool) -> HostState<'_> {
        HostState::new(kv, kv_provided)
    }

    #[test]
    fn invalid_wasm_is_a_load_error() {
        let err = EdgeArtifact::from_bytes(b"not wasm at all").expect_err("must reject");
        assert!(matches!(err, EdgeHostError::Load(_)), "{err:?}");
        assert!(err.to_string().contains("edge artifact"), "{err}");
    }

    #[test]
    fn an_empty_module_loads_and_declares_no_imports() {
        // The 8-byte wasm preamble: magic + version. A valid, empty module.
        let wasm = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let artifact = EdgeArtifact::from_bytes(&wasm).expect("valid module");
        assert!(artifact.imports().is_empty());
        assert!(format!("{artifact:?}").contains("EdgeArtifact"));
    }

    #[test]
    fn a_module_without_start_falls_through_rather_than_erroring() {
        let wasm = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let artifact = EdgeArtifact::from_bytes(&wasm).expect("valid module");
        let kv = InMemoryEdgeKv::new();

        let outcome = artifact
            .run(&EdgeRequest::get("/x"), &[], &kv)
            .expect("host setup succeeds");

        let EdgeOutcome::Fallthrough { reason, detail } = outcome else {
            panic!("expected a fallthrough");
        };
        assert_eq!(reason, FallthroughReason::CapsuleError);
        assert!(detail.contains("_start"), "{detail}");
    }

    #[test]
    fn a_runaway_loop_exhausts_its_fuel_and_falls_through() {
        // A hand-assembled module whose `_start` is `loop { br 0 }`: without
        // fuel metering this call would never return. Sections: type
        // `() -> ()`, one function, the `_start` export, and a body of
        // `loop(void) br 0 end end`.
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // preamble
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: () -> ()
            0x03, 0x02, 0x01, 0x00, // function: [type 0]
            0x07, 0x0a, 0x01, 0x06, b'_', b's', b't', b'a', b'r', b't', 0x00,
            0x00, // export "_start" fn 0
            0x0a, 0x09, 0x01, 0x07, 0x00, 0x03, 0x40, 0x0c, 0x00, 0x0b,
            0x0b, // code: loop br 0
        ];
        let artifact = EdgeArtifact::from_bytes(&wasm).expect("valid module");
        let kv = InMemoryEdgeKv::new();

        let outcome = artifact
            .run(&EdgeRequest::get("/spin"), &[], &kv)
            .expect("host setup succeeds");

        let EdgeOutcome::Fallthrough { reason, detail } = outcome else {
            panic!("expected a fallthrough");
        };
        assert_eq!(reason, FallthroughReason::CapsuleError);
        assert!(detail.contains("execution budget"), "{detail}");
    }

    #[test]
    fn stdout_line_budget_fails_the_write_and_stderr_budget_drops_the_excess() {
        let kv = InMemoryEdgeKv::new();
        let mut state = HostState::new(&kv, false);

        // Newline-less stdout accumulates only up to the budget.
        assert!(state.write_stdout(b"honest partial line"));
        state.stdout_line = vec![b'x'; STDOUT_LINE_BUDGET_BYTES];
        assert!(
            !state.write_stdout(b"one byte too many"),
            "a line past the budget must fail the guest's write"
        );
        // A newline still drains the buffered line even at the cap.
        assert!(state.write_stdout(b"\n"));
        assert!(state.stdout_line.is_empty());

        // Stderr past its budget is dropped, never an error.
        state.stderr = vec![b'e'; STDERR_BUDGET_BYTES - 2];
        state.write_stderr(b"abcdef");
        assert_eq!(state.stderr.len(), STDERR_BUDGET_BYTES);
        assert!(state.stderr.ends_with(b"ab"), "kept only what fit");
    }

    #[test]
    fn a_memory_hungry_module_is_refused_instead_of_killing_the_host() {
        // A hand-assembled module declaring a 4097-page (256 MiB + 64 KiB)
        // linear memory — one page over MEMORY_BUDGET_BYTES — with an empty
        // `_start`. The store limiter must refuse it at instantiation and the
        // refusal must surface as a fallthrough, not an allocation-death.
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // preamble
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: () -> ()
            0x03, 0x02, 0x01, 0x00, // function: [type 0]
            0x05, 0x04, 0x01, 0x00, 0x81, 0x20, // memory: min 4097 pages
            0x07, 0x0a, 0x01, 0x06, b'_', b's', b't', b'a', b'r', b't', 0x00,
            0x00, // export "_start" fn 0
            0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // code: empty body
        ];
        let artifact = EdgeArtifact::from_bytes(&wasm).expect("valid module");
        let kv = InMemoryEdgeKv::new();

        let outcome = artifact
            .run(&EdgeRequest::get("/hog"), &[], &kv)
            .expect("host setup succeeds");

        let EdgeOutcome::Fallthrough { reason, detail } = outcome else {
            panic!("expected a fallthrough");
        };
        assert_eq!(reason, FallthroughReason::CapsuleError);
        assert!(detail.contains("instantiated"), "{detail}");
    }

    #[test]
    fn a_response_line_becomes_the_terminal_outcome() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);
        let response = EdgeResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: b"hi".to_vec(),
        };
        let line = to_line(&GuestFrame::Response(response.clone())).expect("line");

        assert!(state.write_stdout(line.as_bytes()));

        assert_eq!(state.terminal, Some(EdgeOutcome::Served(response)));
    }

    #[test]
    fn a_kv_get_line_is_answered_from_the_store() {
        let kv = InMemoryEdgeKv::new().with("greeting", "hello");
        let mut state = state_with(&kv, true);

        assert!(state.write_stdout(b"{\"op\":\"kv_get\",\"key\":\"greeting\"}\n"));

        let answer: String = state.stdin.iter().map(|byte| char::from(*byte)).collect();
        assert_eq!(
            answer.trim_end(),
            r#"{"op":"kv_value","value_b64":"aGVsbG8="}"#
        );
        assert_eq!(state.terminal, None);
    }

    #[test]
    fn a_kv_get_without_the_capability_is_answered_with_a_miss() {
        let kv = InMemoryEdgeKv::new().with("greeting", "hello");
        let mut state = state_with(&kv, false);

        assert!(state.write_stdout(b"{\"op\":\"kv_get\",\"key\":\"greeting\"}\n"));

        let answer: String = state.stdin.iter().map(|byte| char::from(*byte)).collect();
        assert_eq!(answer.trim_end(), r#"{"op":"kv_value","value_b64":null}"#);
    }

    #[test]
    fn a_malformed_guest_line_becomes_a_capsule_error() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);

        assert!(state.write_stdout(b"{ not json at all\n"));

        let Some(EdgeOutcome::Fallthrough { reason, .. }) = state.terminal else {
            panic!("expected a capsule error, got {:?}", state.terminal);
        };
        assert_eq!(reason, FallthroughReason::CapsuleError);
    }

    #[test]
    fn output_after_the_terminal_frame_is_ignored() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);

        assert!(state.write_stdout(
            b"{\"op\":\"fallthrough\",\"reason\":\"unknown_route\",\"detail\":\"a\"}\n",
        ));
        assert!(state.write_stdout(
            b"{\"op\":\"fallthrough\",\"reason\":\"capsule_error\",\"detail\":\"b\"}\n",
        ));

        assert_eq!(
            state
                .terminal
                .and_then(|outcome| outcome.fallthrough_reason()),
            Some(FallthroughReason::UnknownRoute)
        );
    }

    #[test]
    fn partial_lines_accumulate_until_a_newline_arrives() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);

        assert!(state.write_stdout(b"{\"op\":\"fallthrough\",\"reason\":"));
        assert_eq!(state.terminal, None);
        assert!(state.write_stdout(b"\"unknown_route\",\"detail\":\"\"}\n"));

        assert_eq!(
            state
                .terminal
                .and_then(|outcome| outcome.fallthrough_reason()),
            Some(FallthroughReason::UnknownRoute)
        );
    }

    #[test]
    fn stdin_is_drained_in_order_and_never_over_reads() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);
        state.feed("abc\n");

        assert_eq!(state.take_stdin(2), b"ab");
        assert_eq!(state.take_stdin(10), b"c\n");
        assert_eq!(state.take_stdin(4), b"");
    }

    #[test]
    fn random_bytes_are_reproducible_across_states() {
        let kv = InMemoryEdgeKv::new();
        let mut first = state_with(&kv, false);
        let mut second = state_with(&kv, false);

        let left: Vec<u8> = (0..16).map(|_| first.next_random_byte()).collect();
        let right: Vec<u8> = (0..16).map(|_| second.next_random_byte()).collect();

        assert_eq!(left, right);
        // Not a constant stream either — a shim that returned all zeroes would
        // be deterministic and useless.
        assert!(left.windows(2).any(|pair| pair.first() != pair.last()));
    }

    #[test]
    fn the_stderr_excerpt_is_trimmed_and_bounded() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);
        state.stderr.extend_from_slice(b"  panicked at boom  ");
        assert_eq!(state.stderr_excerpt(), "panicked at boom");

        state.stderr = vec![b'x'; STDERR_EXCERPT * 2];
        assert_eq!(state.stderr_excerpt().len(), STDERR_EXCERPT);
    }

    #[test]
    fn a_capsule_error_carries_the_stderr_excerpt() {
        let kv = InMemoryEdgeKv::new();
        let mut state = state_with(&kv, false);
        state.stderr.extend_from_slice(b"panicked at 'boom'");

        let EdgeOutcome::Fallthrough { detail, .. } = capsule_error(&state, "the capsule trapped")
        else {
            panic!("expected a fallthrough");
        };
        assert!(detail.contains("the capsule trapped"), "{detail}");
        assert!(detail.contains("panicked at 'boom'"), "{detail}");
    }
}
