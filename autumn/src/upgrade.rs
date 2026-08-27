//! In-place upgrades: swap a running app to a new binary without dropping
//! connections or in-memory state (issue #1674).
//!
//! A conventional "zero-downtime" deploy is *process replacement*: the old
//! binary drains and dies, and every byte of in-memory state — sessions,
//! caches, counters — is rebuilt cold in a new process. This module makes the
//! running process itself the unit of upgrade. On `SIGUSR2` an Autumn app:
//!
//! 1. snapshots the block of typed state the app designated as *live state*,
//!    freezing it so no write can be silently lost from here on;
//! 2. forks and execs the new binary, handing it the **already-bound listening
//!    socket** — so not one connection is refused across the cutover;
//! 3. waits for the successor to signal readiness (aborting the whole upgrade,
//!    and unfreezing, if the successor dies or times out);
//! 4. drains its own in-flight requests and exits.
//!
//! The successor adopts the listener, decodes the snapshot, and — when the
//! state shape changed between the two builds — runs a migration whose
//! totality the *compiler* proved: see [`state_migration!`](crate::state_migration).
//!
//! # Designating state
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::upgrade::LiveState;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize, Default)]
//! struct Stats { hits: u64 }
//!
//! impl LiveState for Stats {
//!     const VERSION: u32 = 1;
//! }
//!
//! # #[get("/")] async fn index() -> &'static str { "ok" }
//! # #[autumn_web::main]
//! # async fn main() {
//! autumn_web::app()
//!     .routes(routes![index])
//!     .with_live_state(Stats::default())
//!     .run()
//!     .await;
//! # }
//! ```
//!
//! # Platform
//!
//! Linux/Unix only, and only for a TCP listener: a Unix-socket or TLS listener
//! cannot be handed off in this slice and an upgrade over one is refused with
//! an error rather than silently degraded. On non-Unix targets the signal
//! watcher is not installed at all.
//!
//! See `docs/guide/hot-upgrades.md`.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Environment variable naming the file descriptor the successor finds its
/// inherited listening socket on. Set by the predecessor; never set by hand.
pub const LISTEN_FD_ENV: &str = "AUTUMN_UPGRADE_LISTEN_FD";

/// Environment variable naming the file holding the serialized live-state
/// snapshot the successor should adopt.
pub const STATE_FILE_ENV: &str = "AUTUMN_UPGRADE_STATE_FILE";

/// Environment variable naming the file the successor writes once it is
/// serving, which is what releases the predecessor to drain.
pub const READY_FILE_ENV: &str = "AUTUMN_UPGRADE_READY_FILE";

/// Environment variable carrying the upgrade generation (0 for a cold start,
/// incremented on every in-place hop).
pub const GENERATION_ENV: &str = "AUTUMN_UPGRADE_GENERATION";

/// Environment variable carrying the predecessor's pid, for log correlation.
pub const PREDECESSOR_PID_ENV: &str = "AUTUMN_UPGRADE_PREDECESSOR_PID";

/// Optional operator override: the binary an upgrade should exec. Defaults to
/// the path this process was started from.
pub const BINARY_ENV: &str = "AUTUMN_UPGRADE_BINARY";

/// Optional operator override: the directory the per-upgrade handoff directory
/// is created under. Defaults to the system temp directory.
pub const DIR_ENV: &str = "AUTUMN_UPGRADE_DIR";

/// The descriptor the predecessor places the listening socket on: the
/// successor's **stdin**.
///
/// `inetd` has handed servers their socket this way for decades, and it is the
/// one descriptor a successor can turn back into a listener without `unsafe`
/// (this workspace forbids it) — `std::io::stdin().as_fd()` is a
/// `BorrowedFd`, and `OwnedFd` converts into a `TcpListener` safely. The
/// successor re-points its own stdin at `/dev/null` immediately after adopting,
/// so nothing it later spawns inherits the listening socket.
pub const INHERITED_LISTENER_FD: i32 = 0;

/// A block of typed in-memory application state designated to survive an
/// in-place upgrade.
///
/// [`VERSION`](Self::VERSION) identifies the *shape*: bump it in the same
/// commit that changes the fields, and give the new build a
/// [`state_migration!`](crate::state_migration) from the old shape. A successor that finds a snapshot
/// it can neither decode nor migrate refuses to start, which aborts the
/// upgrade and leaves the predecessor serving.
///
/// # A live-state block must be plain data
///
/// **No interior mutability.** A block that carries an `AtomicU64`, a `Mutex`,
/// a `Cell`/`RefCell`, or anything else mutable through a shared reference can
/// be mutated through [`LiveStateHandle::read`], which hands out `&T` — and
/// that path cannot be refused the way [`write`](LiveStateHandle::write) is.
/// A mutation made that way during a handover is exactly the silent loss this
/// module exists to prevent: the predecessor's snapshot was already taken, or
/// the successor's upgrade can still be abandoned.
///
/// Use plain fields and mutate them through
/// [`write`](LiveStateHandle::write), which is refused (never lost) for the
/// moment a handover is in flight. The handle already provides the
/// synchronisation an `Atomic*`/`Mutex` field would be reaching for.
///
/// This is a contract rather than a compiler-checked bound because stable Rust
/// has no way to name "contains no `UnsafeCell`" — that is `Freeze`, still
/// unstable. Add `T: Freeze` here when it lands and the contract becomes a
/// build error.
pub trait LiveState: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Wire version of this state shape.
    const VERSION: u32;
}

/// A total mapping from an older live-state shape to this one.
///
/// Write it with [`state_migration!`](crate::state_migration), which makes a missing field mapping (or
/// a missing enum variant) a compile error rather than a silent data loss.
pub trait MigrateFrom<Old: LiveState>: LiveState + Sized {
    /// Carry `old` forward into this shape.
    fn migrate_from(old: Old) -> Self;
}

/// Returned by [`LiveStateHandle::write`] while an in-place upgrade is in
/// flight — from either side of it.
///
/// On the **predecessor**, from the snapshot onward: the successor owns the
/// future of this state, so a write here would be thrown away when this
/// process exits. On the **successor**, until it has finished starting up: it
/// is already accepting on the shared socket, but the upgrade can still be
/// abandoned (a failing startup hook), and a write acknowledged now would die
/// with it while the predecessor resumes from the snapshot.
///
/// Either way the write is refused rather than lost: return a retryable `503`
/// from the handler. Only one of the two processes is ever writable, so a
/// retry lands somewhere that keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "live state is frozen: an in-place upgrade is in flight, so this process is not the one \
     that would keep the write"
)]
pub struct LiveStateFrozen;

/// Shared, freezable handle to a designated block of live state.
#[derive(Debug)]
pub struct LiveStateHandle<T> {
    inner: Arc<std::sync::RwLock<T>>,
    frozen: Arc<AtomicBool>,
}

impl<T> Clone for LiveStateHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            frozen: Arc::clone(&self.frozen),
        }
    }
}

impl<T> LiveStateHandle<T> {
    /// Wrap `value` in a fresh handle.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::with_frozen(value, false)
    }

    /// Wrap `value` in a handle that refuses writes until it is unfrozen.
    ///
    /// How a successor installs state it adopted: it starts accepting the
    /// moment it takes the socket, but the upgrade is not yet irreversible, so
    /// until it signals readiness a write here could still be discarded by an
    /// abandoned upgrade. Refusing is what makes that impossible.
    #[must_use]
    pub(crate) fn new_frozen(value: T) -> Self {
        Self::with_frozen(value, true)
    }

    fn with_frozen(value: T, frozen: bool) -> Self {
        Self {
            inner: Arc::new(std::sync::RwLock::new(value)),
            frozen: Arc::new(AtomicBool::new(frozen)),
        }
    }

    /// Read the state under a shared lock.
    ///
    /// Reads keep working after a snapshot: the process is still serving while
    /// it drains, it just no longer owns the state's future.
    ///
    /// `f` receives `&T`, so this is the one path a freeze cannot guard: a
    /// block carrying interior mutability (`AtomicU64`, `Mutex`, `Cell`, …)
    /// could be *mutated* from here, and that mutation would be lost by a
    /// handover exactly as an unrefused write would be. Live state must be
    /// plain data — see [`LiveState`].
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        let guard = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&guard)
    }

    /// Mutate the state under an exclusive lock.
    ///
    /// # Errors
    ///
    /// [`LiveStateFrozen`] once the state has been snapshotted for an in-place
    /// upgrade: this process is draining and the write would be lost.
    pub fn write<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R, LiveStateFrozen> {
        // The freeze flag is both set and read under the *write* lock, so a
        // write can never interleave with the snapshot that supersedes it:
        // either it lands before the snapshot and is carried, or it is refused.
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.frozen.load(Ordering::Acquire) {
            return Err(LiveStateFrozen);
        }
        Ok(f(&mut guard))
    }

    /// Whether the state has been snapshotted and is refusing writes.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// Freeze the state and serialize it, all under one exclusive lock.
    ///
    /// Unfreezes again if serialization fails: a state that cannot be carried
    /// must not also be a state that cannot be written.
    fn freeze_and_serialize(&self) -> Result<serde_json::Value, serde_json::Error>
    where
        T: Serialize,
    {
        let guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.frozen.store(true, Ordering::Release);
        serde_json::to_value(&*guard).inspect_err(|_| self.frozen.store(false, Ordering::Release))
    }

    /// Freeze this block as an in-place upgrade would, so an app can test the
    /// [`LiveStateFrozen`] branch its handlers are expected to have.
    ///
    /// Only useful in tests: the framework freezes and unfreezes the real thing
    /// around a handover. Nothing unfreezes this one but
    /// [`unfreeze_for_test`](Self::unfreeze_for_test).
    pub fn freeze_for_test(&self) {
        self.frozen.store(true, Ordering::Release);
    }

    /// Undo [`freeze_for_test`](Self::freeze_for_test).
    pub fn unfreeze_for_test(&self) {
        self.unfreeze();
    }

    /// Release the freeze (an aborted upgrade, or a failed snapshot).
    fn unfreeze(&self) {
        self.frozen.store(false, Ordering::Release);
    }
}

/// The serialized form of a live-state block as it crosses the process
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StateEnvelope {
    /// Upgrade generation of the *successor* this snapshot was written for.
    pub generation: u64,
    /// [`LiveState::VERSION`] of the shape `payload` was serialized from.
    pub version: u32,
    /// Rust type name of the shape, for diagnostics only.
    pub type_name: String,
    /// The serialized state itself.
    pub payload: serde_json::Value,
}

/// Why a successor could not adopt the state its predecessor handed over.
#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    /// The snapshot could not be read from disk.
    #[error("could not read the carried live-state snapshot at {path}: {source}")]
    Read {
        /// Snapshot path taken from [`STATE_FILE_ENV`].
        path: String,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// The snapshot envelope itself was not decodable.
    #[error("the carried live-state snapshot is not a valid envelope: {0}")]
    Envelope(String),
    /// The snapshot's shape version is neither this build's version nor the
    /// one version this build knows how to migrate from.
    #[error(
        "the carried live-state snapshot is version {found}, but this build understands version \
         {expected}{migratable}; refusing to start with state this build cannot account for"
    )]
    VersionMismatch {
        /// Version found in the envelope.
        found: u32,
        /// Version this build's live-state type declares.
        expected: u32,
        /// `" (or 1, via the registered migration)"`, or empty when no
        /// migration is registered.
        migratable: String,
    },
    /// The payload did not deserialize into the shape its version named.
    #[error("the carried live-state snapshot (version {version}) could not be decoded: {source}")]
    Decode {
        /// Version that was attempted.
        version: u32,
        /// Underlying serde error.
        source: serde_json::Error,
    },
}

/// Why an in-place upgrade could not be started or completed.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    /// The bound listener cannot be handed to a successor.
    #[error(
        "this listener cannot be handed to a successor ({0}); in-place upgrade is supported for \
         plain TCP listeners only in this release"
    )]
    UnsupportedListener(&'static str),
    /// Something else about this process cannot be handed to a successor.
    #[error("this process cannot be upgraded in place: {0}")]
    Unsupported(&'static str),
    /// The binary to exec could not be resolved.
    #[error("could not resolve the binary to upgrade to: {0}")]
    Binary(String),
    /// Snapshotting the designated live state failed.
    #[error("could not snapshot the designated live state: {0}")]
    Snapshot(String),
    /// An IO step of the handoff failed.
    #[error("in-place upgrade handoff failed: {0}")]
    Io(#[from] std::io::Error),
    /// The successor exited before signalling readiness.
    #[error("the successor exited before signalling readiness ({0}); keeping the old build")]
    SuccessorExited(String),
    /// The successor never signalled readiness in time.
    #[error("the successor did not signal readiness within {0:?}; keeping the old build")]
    ReadyTimeout(std::time::Duration),
}

/// Type-erased registration of the app's designated live-state block.
///
/// Installed into the [`AppState`](crate::AppState) extension map by
/// [`AppBuilder::with_live_state`](crate::app::AppBuilder::with_live_state) so
/// the upgrade path can snapshot the block without knowing its type.
pub(crate) struct LiveStateRegistry {
    freeze_and_snapshot: Box<dyn Fn(u64) -> Result<StateEnvelope, UpgradeError> + Send + Sync>,
    unfreeze: Box<dyn Fn() + Send + Sync>,
    type_name: &'static str,
}

impl std::fmt::Debug for LiveStateRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStateRegistry")
            .field("type_name", &self.type_name)
            .finish_non_exhaustive()
    }
}

impl LiveStateRegistry {
    /// Register `handle` as the app's designated live-state block.
    #[must_use]
    pub(crate) fn new<T: LiveState>(handle: &LiveStateHandle<T>) -> Self {
        let for_snapshot = handle.clone();
        let for_unfreeze = handle.clone();
        Self {
            freeze_and_snapshot: Box::new(move |generation| {
                for_snapshot
                    .freeze_and_serialize()
                    .map(|payload| StateEnvelope {
                        generation,
                        version: T::VERSION,
                        type_name: std::any::type_name::<T>().to_owned(),
                        payload,
                    })
                    .map_err(|error| UpgradeError::Snapshot(error.to_string()))
            }),
            unfreeze: Box::new(move || for_unfreeze.unfreeze()),
            type_name: std::any::type_name::<T>(),
        }
    }

    /// Freeze the block against further writes and serialize it for `generation`.
    ///
    /// # Errors
    ///
    /// [`UpgradeError::Snapshot`] if the state does not serialize.
    pub(crate) fn freeze_and_snapshot(
        &self,
        generation: u64,
    ) -> Result<StateEnvelope, UpgradeError> {
        (self.freeze_and_snapshot)(generation)
    }

    /// Release the freeze — used when an upgrade aborts and this process
    /// carries on serving.
    pub(crate) fn unfreeze(&self) {
        (self.unfreeze)();
    }

    /// Rust type name of the designated block, for diagnostics.
    #[must_use]
    pub(crate) const fn type_name(&self) -> &'static str {
        self.type_name
    }
}

/// This process's upgrade generation: `0` on a cold start, incremented by each
/// in-place hop.
#[must_use]
pub fn generation() -> u64 {
    std::env::var(GENERATION_ENV)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0)
}

/// Path of the snapshot handed over by a predecessor, if this process was
/// started by an in-place upgrade.
#[must_use]
pub(crate) fn carried_snapshot_path() -> Option<std::path::PathBuf> {
    let raw = std::env::var_os(STATE_FILE_ENV)?;
    if raw.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(raw))
}

/// Read and consume the snapshot at `path`.
///
/// The file is removed once read: it holds application state and must not
/// outlive the handoff.
///
/// # Errors
///
/// [`AdoptError::Read`] or [`AdoptError::Envelope`] if it cannot be read or
/// parsed.
pub(crate) fn read_snapshot(path: &std::path::Path) -> Result<StateEnvelope, AdoptError> {
    let raw = std::fs::read(path).map_err(|source| AdoptError::Read {
        path: path.display().to_string(),
        source,
    })?;
    // Consume it whether or not it parses: it holds application state and has
    // no business outliving this one handoff.
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&raw).map_err(|error| AdoptError::Envelope(error.to_string()))
}

/// Write `envelope` to `path`, owner-only, failing if the path already exists.
///
/// # Errors
///
/// Any IO error from creating or writing the file, including
/// [`AlreadyExists`](std::io::ErrorKind::AlreadyExists) when something is
/// already at `path`.
pub(crate) fn write_snapshot(
    path: &std::path::Path,
    envelope: &StateEnvelope,
) -> Result<(), std::io::Error> {
    let bytes = serde_json::to_vec(envelope).map_err(std::io::Error::other)?;
    write_owner_only(path, &bytes)
}

/// Create `path` owner-only and write `bytes` to it, failing if anything is
/// already there.
///
/// Owner-only from the moment the file exists, never a `create` + later
/// `chmod` window — everything this module writes is application data.
fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Decode `envelope` into `T`, with no migration hop available.
///
/// # Errors
///
/// [`AdoptError::VersionMismatch`] if the snapshot is a different shape
/// version, [`AdoptError::Decode`] if the payload does not fit `T`.
pub(crate) fn decode<T: LiveState>(envelope: &StateEnvelope) -> Result<T, AdoptError> {
    decode_as::<T>(envelope, String::new())
}

/// Shared body of [`decode`] / [`decode_migrating`]; `migratable` describes the
/// extra version a migration would have accepted, for the error message.
fn decode_as<T: LiveState>(envelope: &StateEnvelope, migratable: String) -> Result<T, AdoptError> {
    if envelope.version != T::VERSION {
        return Err(AdoptError::VersionMismatch {
            found: envelope.version,
            expected: T::VERSION,
            migratable,
        });
    }
    decode_payload::<T>(envelope)
}

/// Deserialize the payload as `T`, attributing failures to the *envelope's*
/// declared version rather than to `T`.
fn decode_payload<T: DeserializeOwned>(envelope: &StateEnvelope) -> Result<T, AdoptError> {
    T::deserialize(&envelope.payload).map_err(|source| AdoptError::Decode {
        version: envelope.version,
        source,
    })
}

/// Decode `envelope` into `New`, migrating from `Old` when the snapshot
/// carries the older shape.
///
/// # Errors
///
/// As [`decode`], with `Old::VERSION` additionally accepted.
pub(crate) fn decode_migrating<Old, New>(envelope: &StateEnvelope) -> Result<New, AdoptError>
where
    Old: LiveState,
    New: MigrateFrom<Old>,
{
    // Two shapes that declare the same VERSION are indistinguishable on the
    // wire, so the migration below could never run and the old payload would be
    // handed to the new shape's `Deserialize` — which, with a `#[serde(default)]`
    // or an `Option` field, is exactly the silent loss this module exists to
    // prevent. `state_migration!` refuses this at the migration itself; this
    // catches a hand-written `impl MigrateFrom`, at monomorphization.
    const {
        assert!(
            Old::VERSION != New::VERSION,
            "a live-state migration needs two different LiveState::VERSIONs: bump the new \
             shape's VERSION in the same commit that changes its fields"
        );
    }
    if envelope.version == Old::VERSION {
        return decode_payload::<Old>(envelope).map(New::migrate_from);
    }
    decode_as::<New>(
        envelope,
        format!(
            " (or version {}, via the registered migration)",
            Old::VERSION
        ),
    )
}

// ── The handoff itself (Unix) ────────────────────────────────────────────────
//
// Every step below is written without `unsafe`: the workspace forbids it, and
// the socket crosses the process boundary through `OwnedFd` and the child's
// **stdin** — the same convention inetd has used to hand a server its socket
// for forty years — rather than through a raw descriptor number that only
// `from_raw_fd` could turn back into a listener.

/// A duplicate of the bound listening socket, kept aside so it can be handed
/// to a successor while this process goes on serving through the original.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct HandoffSocket(std::os::fd::OwnedFd);

#[cfg(unix)]
impl HandoffSocket {
    /// Duplicate `listener`'s descriptor for a later handoff.
    ///
    /// # Errors
    ///
    /// Any IO error from `dup`.
    pub(crate) fn from_listener(listener: &tokio::net::TcpListener) -> std::io::Result<Self> {
        use std::os::fd::AsFd as _;
        listener.as_fd().try_clone_to_owned().map(Self)
    }

    /// A fresh duplicate to give one spawn attempt, so a failed upgrade leaves
    /// this handle usable for the next one.
    fn stdio(&self) -> std::io::Result<std::process::Stdio> {
        self.0.try_clone().map(std::process::Stdio::from)
    }
}

/// Adopt the listening socket a predecessor handed over, if this process was
/// started by an in-place upgrade.
///
/// Returns `None` for an ordinary cold start. The descriptor arrives as this
/// process's **stdin**; anything else in [`LISTEN_FD_ENV`] is refused rather
/// than guessed at, and a stdin that is not a socket (someone exported the
/// variable by hand) is refused too.
#[cfg(unix)]
#[must_use]
pub(crate) fn adopt_inherited_listener() -> Option<std::net::TcpListener> {
    use std::os::fd::AsFd as _;

    if !handoff_requested() {
        return None;
    }
    // Adoption consumes the handoff: a second caller must not get a second
    // listener on the same socket.
    if ADOPTION_ATTEMPTED.set(()).is_err() {
        tracing::warn!("the inherited listening socket has already been adopted");
        return None;
    }
    let raw = std::env::var(LISTEN_FD_ENV).unwrap_or_default();
    if raw.trim() != INHERITED_LISTENER_FD.to_string() {
        tracing::error!(
            requested_fd = raw,
            "ignoring an inherited listening socket on an unsupported descriptor: this release \
             receives the socket as the successor's stdin (fd {INHERITED_LISTENER_FD}), so \
             {LISTEN_FD_ENV} must be {INHERITED_LISTENER_FD}"
        );
        return None;
    }
    let fd = match std::io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => fd,
        Err(error) => {
            tracing::error!(error = %error, "could not adopt the inherited listening socket");
            return None;
        }
    };
    let listener = std::net::TcpListener::from(fd);
    // `From<OwnedFd>` does not check what the descriptor is, and neither this
    // process nor the kernel will complain later: an accept loop over a
    // non-listening socket spins on `EINVAL` forever while `/ready` says 200.
    // A listening socket has a local address and *no peer*; a terminal or pipe
    // has neither, and a connected socket has both.
    let Ok(addr) = listener.local_addr() else {
        tracing::error!("{LISTEN_FD_ENV} is set but stdin is not a socket; refusing to adopt it");
        return None;
    };
    // `getpeername` answers only for a *connected* socket, so a duplicate
    // viewed as a `TcpStream` is the cheap discriminator. The duplicate closes
    // with the probe; the listener keeps its own descriptor.
    let connected_peer = listener
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .and_then(|fd| std::net::TcpStream::from(fd).peer_addr().ok());
    if let Some(peer) = connected_peer {
        tracing::error!(
            %peer,
            "{LISTEN_FD_ENV} is set but stdin is a connected socket, not a listener; \
             refusing to adopt it"
        );
        return None;
    }
    if let Err(error) = listener.set_nonblocking(true) {
        tracing::error!(error = %error, "inherited listening socket is unusable");
        return None;
    }
    // The socket is ours now (the `TcpListener` above holds its own descriptor),
    // so re-point stdin at `/dev/null`: otherwise every subprocess this app ever
    // spawns inherits the listening socket on fd 0 by default, and could accept
    // connections on it or keep the port bound after the app exits.
    match std::fs::File::open("/dev/null")
        .and_then(|null| nix::unistd::dup2_stdin(&null).map_err(std::io::Error::from))
    {
        Ok(()) => {}
        Err(error) => {
            tracing::error!(
                error = %error,
                "adopted the inherited listening socket but could not re-point stdin at \
                 /dev/null; refusing to serve rather than leaking the listener into every \
                 subprocess this app spawns"
            );
            return None;
        }
    }
    ADOPTED.set(addr).ok();
    tracing::info!(
        %addr,
        generation = generation(),
        predecessor = std::env::var(PREDECESSOR_PID_ENV).unwrap_or_default(),
        "adopted the listening socket handed over by the previous build"
    );
    Some(listener)
}

/// Whether a predecessor handed this process a listening socket — i.e. whether
/// this process is the successor half of an in-place upgrade.
#[must_use]
pub(crate) fn handoff_requested() -> bool {
    std::env::var_os(LISTEN_FD_ENV).is_some_and(|value| !value.is_empty())
}

/// The address this process adopted from a predecessor, if it adopted one.
#[must_use]
pub(crate) fn adopted_addr() -> Option<std::net::SocketAddr> {
    ADOPTED.get().copied()
}

/// Set once adoption has been attempted, so the handoff is consumed exactly
/// once even if several code paths ask for it.
static ADOPTION_ATTEMPTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Set once a listening socket has actually been adopted.
static ADOPTED: std::sync::OnceLock<std::net::SocketAddr> = std::sync::OnceLock::new();

/// Check that this process really completed the handover it was started for,
/// *before* it releases its predecessor to drain.
///
/// A successor that boots without taking over is the worst outcome available:
/// the predecessor exits believing the service moved, and the address it was
/// serving either disappears or is served by a process with none of the state.
/// Both ways of half-arriving are caught here — a listening socket that was
/// handed over but not adopted (a build that switched to `server.unix_socket`,
/// a descriptor that turned out not to be a listener), and a live-state
/// snapshot that was handed over but never consumed (a build that dropped its
/// `with_live_state(...)` call).
///
/// # Errors
///
/// A description of what was handed over and not taken up. The caller should
/// refuse to start: the predecessor then times out and keeps serving.
pub(crate) fn verify_handover_complete() -> Result<(), String> {
    if handoff_requested() && adopted_addr().is_none() {
        return Err(format!(
            "this build was started by an in-place upgrade and was handed a listening socket, \
             but never adopted it (see the errors above). Serving would abandon the address \
             the previous build is still answering on, so this build refuses to start; the \
             previous build keeps serving. In-place upgrade requires the new build to bind a \
             plain TCP listener ({LISTEN_FD_ENV} was set)"
        ));
    }
    if let Some(path) = carried_snapshot_path()
        && path.exists()
    {
        return Err(format!(
            "this build was handed the previous build's live state ({}) but never adopted it: \
             nothing called AppBuilder::with_live_state(...) or with_live_state_from(...). \
             Starting would silently drop that state, so this build refuses to start and the \
             previous build keeps serving",
            path.display()
        ));
    }
    Ok(())
}

/// Record the path this process was started from, before a deploy can replace
/// the file underneath it.
///
/// Called once at startup. `current_exe` resolves through `/proc/self/exe`,
/// which reports `"… (deleted)"` after the binary is replaced — reading it at
/// upgrade time instead of boot time would resolve to a path that no longer
/// exists.
pub(crate) fn record_startup_exe() {
    if let Ok(path) = std::env::current_exe() {
        let _ = startup_exe_cell().set(path);
    }
}

fn startup_exe_cell() -> &'static std::sync::OnceLock<std::path::PathBuf> {
    static STARTUP_EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    &STARTUP_EXE
}

/// The binary an upgrade would exec: [`BINARY_ENV`] when set, otherwise the
/// path this process was started from.
///
/// # Errors
///
/// [`UpgradeError::Binary`] when neither can be resolved, or when the resolved
/// path no longer names a file — a half-finished deploy must abort *before* a
/// successor is spawned, not after the old build has drained.
pub(crate) fn upgrade_binary() -> Result<std::path::PathBuf, UpgradeError> {
    let configured = std::env::var_os(BINARY_ENV).filter(|value| !value.is_empty());
    let path = match configured {
        Some(value) => std::path::PathBuf::from(value),
        None => startup_exe_cell().get().cloned().map_or_else(
            || std::env::current_exe().map_err(|error| UpgradeError::Binary(error.to_string())),
            Ok,
        )?,
    };
    if !path.is_file() {
        return Err(UpgradeError::Binary(format!(
            "{} is not a file; set {BINARY_ENV} to the new binary, or install it at that path \
             before signalling",
            path.display()
        )));
    }
    Ok(path)
}

/// Tell a waiting predecessor that this process is serving.
///
/// Written to a temporary sibling and renamed into place, so the predecessor —
/// which polls for the path — can never observe a half-written file.
///
/// Returns whether there was a predecessor to tell: `false` on an ordinary
/// cold start, where nothing is waiting.
///
/// # Errors
///
/// Any IO error from writing or renaming the file. The caller must treat this
/// as a failed handover and refuse to go on: readiness that never reached the
/// predecessor means the predecessor will time out and kill this process, so
/// anything this process acknowledged in the meantime would be discarded.
pub(crate) fn publish_upgrade_readiness() -> Result<bool, std::io::Error> {
    let Some(path) = std::env::var_os(READY_FILE_ENV).filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let path = std::path::PathBuf::from(path);
    let mut tmp = path.clone();
    tmp.as_mut_os_string().push(".tmp");
    write_owner_only(&tmp, generation().to_string().as_bytes())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    Ok(true)
}

/// What an upgrade handed over, once the successor is serving.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Handover {
    /// Process id of the successor now sharing the listening socket.
    pub successor_pid: u32,
    /// Generation the successor is running as.
    pub generation: u64,
    /// How long the successor took to come up and signal readiness.
    pub elapsed: std::time::Duration,
}

/// Everything the handoff needs from the running app.
#[cfg(unix)]
pub(crate) struct UpgradePlan<'a> {
    /// Duplicate of the listening socket to hand over.
    pub socket: &'a HandoffSocket,
    /// The app's designated live-state block, if it designated one.
    pub registry: Option<Arc<LiveStateRegistry>>,
    /// How long to wait for the successor to signal readiness.
    pub ready_timeout: std::time::Duration,
    /// Clock the wait is measured on.
    pub clock: Arc<dyn crate::time::ClockSource>,
    /// Entropy the handoff directory's name is drawn from.
    pub entropy: Arc<dyn crate::entropy::Entropy>,
}

/// Hand this process's listening socket and live state to a freshly-execed
/// build, and return once that build is serving.
///
/// Nothing about this process's own serving changes: on success the caller
/// drains and exits; on **any** error the old build carries on, its live state
/// unfrozen, exactly as if no signal had arrived.
#[cfg(unix)]
pub(crate) async fn upgrade_in_place(plan: UpgradePlan<'_>) -> Result<Handover, UpgradeError> {
    let started = plan.clock.monotonic();
    let next_generation = generation().saturating_add(1);
    // Resolved before anything is frozen, so a deploy that has not landed yet
    // costs the running app nothing at all.
    let binary = upgrade_binary()?;

    // Everything one in-flight handoff owns, and the undo for every way it can
    // fail to complete.
    let mut handoff = Handoff {
        dir: create_handoff_dir(next_generation, plan.entropy.as_ref())?,
        child: None,
        registry: plan.registry.clone(),
        completed: false,
    };
    let successor_pid =
        spawn_and_await_successor(&plan, &binary, &mut handoff, next_generation).await?;
    handoff.completed = true;

    Ok(Handover {
        successor_pid,
        generation: next_generation,
        elapsed: plan.clock.monotonic().saturating_duration_since(started),
    })
}

/// The state one in-flight handoff owns, and the undo for every way it can fail
/// to complete — an error, or this task being dropped outright.
///
/// A dropped upgrade is not hypothetical: the watcher gives up on shutdown, so
/// a `SIGTERM` arriving between "successor spawned" and "successor serving"
/// runs exactly this path. Leaving the undo to the error arm alone would orphan
/// a half-booted successor on the port, leave the app's state sitting on disk,
/// and leave the live state frozen for the rest of the drain.
#[cfg(unix)]
struct Handoff {
    /// Private directory holding this handoff's snapshot and ready file.
    dir: std::path::PathBuf,
    /// The successor, from the moment it is spawned.
    child: Option<std::process::Child>,
    /// The designated live state, frozen for the duration.
    registry: Option<Arc<LiveStateRegistry>>,
    /// Set once the successor is serving and the handover is this process's to
    /// walk away from.
    completed: bool,
}

#[cfg(unix)]
impl Drop for Handoff {
    fn drop(&mut self) {
        // `completed` is this process's *observation* that the successor took
        // over, and it lags the fact by up to one poll interval. A successor
        // that has already published readiness and is still running owns the
        // socket and the state whether or not this process got to notice —
        // so a cancellation inside that window (a `SIGTERM` between the
        // successor's rename and our next poll) must not kill it, and must not
        // hand this process's state back: the successor is writable, and two
        // writable copies of one block is exactly the divergence the freeze
        // exists to prevent.
        let ready_path = self.dir.join("ready");
        let taken_over = self.completed
            || self
                .child
                .as_mut()
                .is_some_and(|child| successor_took_over(&ready_path, child));

        if !taken_over {
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            // The successor never took over, so this process still owns the
            // state's future: let it be written again.
            if let Some(registry) = &self.registry {
                registry.unfreeze();
            }
        }
        // The handoff directory carries application state; it never outlives
        // the handoff, completed or abandoned.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Whether the successor has taken over: it published readiness *and* is still
/// running.
///
/// Both halves matter. Readiness is a file, so it outlives the process that
/// wrote it — a successor that published and then died has not taken over, and
/// this process must resume. A successor that published and is running has,
/// even if this process has not observed it yet.
#[cfg(unix)]
fn successor_took_over(ready_path: &std::path::Path, child: &mut std::process::Child) -> bool {
    ready_path.exists() && matches!(child.try_wait(), Ok(None))
}

/// Snapshot, spawn, and wait — the fallible middle of [`upgrade_in_place`],
/// with everything that needs undoing parked in `handoff`.
#[cfg(unix)]
async fn spawn_and_await_successor(
    plan: &UpgradePlan<'_>,
    binary: &std::path::Path,
    handoff: &mut Handoff,
    next_generation: u64,
) -> Result<u32, UpgradeError> {
    let ready_path = handoff.dir.join("ready");

    // 1. Freeze and snapshot the designated state. From here until the
    //    successor is serving, a write to that block is refused rather than
    //    lost — and if anything below fails, the freeze is lifted again.
    let state_path = match &plan.registry {
        Some(registry) => {
            let envelope = registry.freeze_and_snapshot(next_generation)?;
            let path = handoff.dir.join("state.json");
            write_snapshot(&path, &envelope)?;
            tracing::info!(
                state = registry.type_name(),
                version = envelope.version,
                "in-place upgrade: designated live state snapshotted and frozen"
            );
            Some(path)
        }
        None => None,
    };

    // 2. Exec the new build, handing it the listening socket as its stdin.
    let mut command = std::process::Command::new(binary);
    command.args(std::env::args_os().skip(1));
    command.env(LISTEN_FD_ENV, INHERITED_LISTENER_FD.to_string());
    command.env(GENERATION_ENV, next_generation.to_string());
    command.env(PREDECESSOR_PID_ENV, std::process::id().to_string());
    command.env(READY_FILE_ENV, &ready_path);
    match &state_path {
        Some(path) => command.env(STATE_FILE_ENV, path),
        None => command.env_remove(STATE_FILE_ENV),
    };
    command.stdin(plan.socket.stdio()?);
    let child = handoff.child.insert(command.spawn()?);
    let successor_pid = child.id();
    tracing::info!(
        successor_pid,
        binary = %binary.display(),
        generation = next_generation,
        "in-place upgrade: successor spawned, waiting for it to serve"
    );

    // 3. Wait for the successor to serve — or to die trying. A successor that
    //    cannot boot (a bad binary, a state it cannot account for, a socket it
    //    cannot adopt) is the case this whole handshake exists for: the old
    //    build keeps serving.
    let deadline = plan.clock.monotonic().saturating_add(plan.ready_timeout);
    loop {
        if ready_path.exists() {
            // Readiness is a file, so it outlives the process that wrote it:
            // check the successor is still alive before releasing the one
            // process that is definitely serving.
            return match child.try_wait() {
                Ok(Some(status)) => Err(UpgradeError::SuccessorExited(format!(
                    "{status}, immediately after signalling readiness"
                ))),
                Ok(None) => Ok(successor_pid),
                Err(error) => Err(UpgradeError::Io(error)),
            };
        }
        match child.try_wait() {
            Ok(Some(status)) => return Err(UpgradeError::SuccessorExited(status.to_string())),
            Ok(None) => {}
            Err(error) => return Err(UpgradeError::Io(error)),
        }
        if plan.clock.monotonic() >= deadline {
            // One last look before calling it: the successor may have published
            // readiness in the very tick the budget ran out, and abandoning it
            // then would kill a build that is already serving and writable.
            if successor_took_over(&ready_path, child) {
                return Ok(successor_pid);
            }
            return Err(UpgradeError::ReadyTimeout(plan.ready_timeout));
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// How often the predecessor checks whether its successor is serving.
#[cfg(unix)]
const READY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Where per-upgrade handoff directories are created.
///
/// `AUTUMN_UPGRADE_DIR` wins; otherwise `XDG_RUNTIME_DIR` (a per-user `0700`
/// directory on any modern Linux) is preferred over the shared temp directory,
/// because a directory only this user can write to leaves nothing for a local
/// neighbour to squat on.
#[cfg(unix)]
fn handoff_base_dir() -> std::path::PathBuf {
    if let Some(configured) = std::env::var_os(DIR_ENV).filter(|value| !value.is_empty()) {
        return std::path::PathBuf::from(configured);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        let runtime = std::path::PathBuf::from(runtime);
        if runtime.is_dir() {
            return runtime;
        }
    }
    std::env::temp_dir()
}

/// Create the private directory this handoff's files live in.
///
/// `0700` and created (never opened) by us, so nothing pre-planted by another
/// local user can be read as state or mistaken for a readiness signal.
///
/// The name carries 64 bits from the framework's entropy seam, not just the
/// pid and generation. Those are observable, and a name a local neighbour can
/// predict is a name they can pre-create: sixteen `mkdir`s would otherwise
/// deny this process every future upgrade, since a taken name is (correctly)
/// stepped over rather than opened.
#[cfg(unix)]
fn create_handoff_dir(
    generation: u64,
    entropy: &dyn crate::entropy::Entropy,
) -> Result<std::path::PathBuf, std::io::Error> {
    use std::os::unix::fs::DirBuilderExt as _;

    let base = handoff_base_dir();
    std::fs::create_dir_all(&base)?;
    let pid = std::process::id();
    for _ in 0..16u32 {
        let nonce = entropy.next_u64();
        let dir = base.join(format!("autumn-upgrade-{pid}-{generation}-{nonce:016x}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            // Anything already at this path is not ours: draw another name
            // rather than opening — or clobbering — whatever is there.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not create a private handoff directory under {}",
            base.display()
        ),
    ))
}

/// Why a designated live-state block could not be installed at startup.
///
/// Recorded into the app state rather than exiting on the spot: the
/// initializer that discovers it runs inside the async runtime, and the
/// managed-Postgres child that a bare `process::exit` would orphan can only be
/// stopped from there. [`AppBuilder::run`](crate::app::AppBuilder::run) checks
/// for it immediately afterwards and refuses to start.
#[derive(Debug)]
pub(crate) struct LiveStateInstallFailure(pub(crate) String);

/// Declare a **total** migration from an older live-state shape to the current
/// one, checked by the compiler.
///
/// Two forms — a struct shape, whose every field must be mapped, and an enum
/// shape, whose every variant must be mapped. Both name the old value
/// (`as old` below) so the mapping expressions can read it:
///
/// ```rust
/// use autumn_web::state_migration;
/// use autumn_web::upgrade::LiveState;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct StatsV1 { hits: u64 }
/// #[derive(Serialize, Deserialize)]
/// struct Stats { hits: u64, upgrades: u64 }
///
/// impl LiveState for StatsV1 { const VERSION: u32 = 1; }
/// impl LiveState for Stats { const VERSION: u32 = 2; }
///
/// state_migration! {
///     from StatsV1 as old => Stats {
///         hits: old.hits,
///         upgrades: 1,
///     }
/// }
/// ```
///
/// Omit the `upgrades` line and the build fails with *missing field `upgrades`
/// in initializer* — and there is no `..Default::default()` escape hatch,
/// because the grammar has no rule for a rest pattern. For an enum shape every
/// *variant name* is listed, so a catch-all `_` arm is not expressible either
/// and a forgotten variant is a non-exhaustive `match`:
///
/// ```rust
/// # use autumn_web::state_migration;
/// # use autumn_web::upgrade::LiveState;
/// # use serde::{Deserialize, Serialize};
/// #[derive(Serialize, Deserialize)]
/// enum ModeV1 { Fast, Slow(u8) }
/// #[derive(Serialize, Deserialize)]
/// enum Mode { Fast, Slow { level: u8 } }
/// # impl LiveState for ModeV1 { const VERSION: u32 = 1; }
/// # impl LiveState for Mode { const VERSION: u32 = 2; }
///
/// state_migration! {
///     from ModeV1 as old => Mode {
///         match old {
///             Fast => Mode::Fast,
///             Slow(level) => Mode::Slow { level },
///         }
///     }
/// }
/// ```
///
/// The compiler proves the mapping is *total*, not that it is *right*: a field
/// mapped to a constant compiles. Totality is what stops a shape change from
/// quietly dropping the state an upgrade was supposed to carry.
#[macro_export]
macro_rules! state_migration {
    // Enum shape. Listed first: `match` would otherwise be swallowed by the
    // struct rule's `$field:ident` (an `ident` fragment matches keywords).
    (from $old:ty as $binding:ident => $new:ty {
        match $scrutinee:ident {
            $( $variant:ident
               $( ( $($tuple:tt)* ) )?
               $( { $($named:tt)* } )?
               => $arm:expr ),+ $(,)?
        }
    }) => {
        const _: () = assert!(
            <$old as $crate::upgrade::LiveState>::VERSION
                != <$new as $crate::upgrade::LiveState>::VERSION,
            "a live-state migration needs two different LiveState::VERSIONs: bump the new \
             shape's VERSION in the same commit that changes its fields, or the snapshot \
             cannot be told apart on the wire and this migration would never run"
        );

        impl $crate::upgrade::MigrateFrom<$old> for $new {
            fn migrate_from($binding: $old) -> Self {
                // A type alias, not a path, so the old shape may be any type
                // expression; `Alias::Variant` is a valid pattern.
                #[allow(non_camel_case_types)]
                type __AutumnOldLiveState = $old;
                match $scrutinee {
                    $( __AutumnOldLiveState::$variant
                       $( ( $($tuple)* ) )?
                       $( { $($named)* } )? => $arm ),+
                }
            }
        }
    };
    // Struct shape.
    (from $old:ty as $binding:ident => $new:ty {
        $( $field:ident : $value:expr ),+ $(,)?
    }) => {
        const _: () = assert!(
            <$old as $crate::upgrade::LiveState>::VERSION
                != <$new as $crate::upgrade::LiveState>::VERSION,
            "a live-state migration needs two different LiveState::VERSIONs: bump the new \
             shape's VERSION in the same commit that changes its fields, or the snapshot \
             cannot be told apart on the wire and this migration would never run"
        );

        impl $crate::upgrade::MigrateFrom<$old> for $new {
            fn migrate_from($binding: $old) -> Self {
                Self { $( $field: $value ),+ }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct StatsV1 {
        hits: u64,
        note: String,
    }

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct Stats {
        hits: u64,
        note: String,
        upgrades: u64,
    }

    impl LiveState for StatsV1 {
        const VERSION: u32 = 1;
    }
    impl LiveState for Stats {
        const VERSION: u32 = 2;
    }

    crate::state_migration! {
        from StatsV1 as old => Stats {
            hits: old.hits,
            note: old.note,
            upgrades: 1,
        }
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    enum ModeV1 {
        Fast,
        Slow(u8),
    }

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    enum Mode {
        Fast,
        Slow { level: u8 },
    }

    impl LiveState for ModeV1 {
        const VERSION: u32 = 1;
    }
    impl LiveState for Mode {
        const VERSION: u32 = 2;
    }

    crate::state_migration! {
        from ModeV1 as old => Mode {
            match old {
                Fast => Mode::Fast,
                Slow(level) => Mode::Slow { level },
            }
        }
    }

    fn envelope_of<T: LiveState>(value: &T, generation: u64) -> StateEnvelope {
        StateEnvelope {
            generation,
            version: T::VERSION,
            type_name: std::any::type_name::<T>().to_owned(),
            payload: serde_json::to_value(value).expect("test state serializes"),
        }
    }

    #[test]
    fn a_value_written_before_the_snapshot_is_readable_after_adoption() {
        let handle = LiveStateHandle::new(StatsV1::default());
        handle
            .write(|s| {
                s.hits = 41;
                s.note = "carried".to_owned();
            })
            .expect("state is not frozen before an upgrade");

        let registry = LiveStateRegistry::new(&handle);
        let envelope = registry
            .freeze_and_snapshot(1)
            .expect("designated state snapshots");

        let adopted: StatsV1 = decode(&envelope).expect("same-shape adoption decodes");
        assert_eq!(adopted.hits, 41);
        assert_eq!(adopted.note, "carried");
    }

    #[test]
    fn snapshotting_freezes_writes_so_they_cannot_be_lost_silently() {
        let handle = LiveStateHandle::new(StatsV1::default());
        let registry = LiveStateRegistry::new(&handle);
        assert!(!handle.is_frozen());

        registry.freeze_and_snapshot(1).expect("snapshots");

        assert!(handle.is_frozen());
        assert_eq!(handle.write(|s| s.hits += 1), Err(LiveStateFrozen));
        // Reads keep working: the process is still serving while it drains.
        assert_eq!(handle.read(|s| s.hits), 0);
    }

    #[test]
    fn an_aborted_upgrade_unfreezes_the_state_so_the_old_build_keeps_serving() {
        let handle = LiveStateHandle::new(StatsV1::default());
        let registry = LiveStateRegistry::new(&handle);
        registry.freeze_and_snapshot(1).expect("snapshots");

        registry.unfreeze();

        assert!(!handle.is_frozen());
        assert_eq!(handle.write(|s| s.hits += 1), Ok(()));
    }

    #[test]
    fn a_shape_change_is_carried_across_by_the_registered_migration() {
        let old = StatsV1 {
            hits: 7,
            note: "before".to_owned(),
        };
        let envelope = envelope_of(&old, 1);

        let new: Stats = decode_migrating::<StatsV1, Stats>(&envelope).expect("migrates");

        assert_eq!(
            new,
            Stats {
                hits: 7,
                note: "before".to_owned(),
                upgrades: 1,
            }
        );
    }

    #[test]
    fn the_current_shape_is_adopted_without_a_migration_hop() {
        let current = Stats {
            hits: 3,
            note: "n".to_owned(),
            upgrades: 9,
        };
        let envelope = envelope_of(&current, 4);

        let adopted: Stats = decode_migrating::<StatsV1, Stats>(&envelope).expect("decodes");

        assert_eq!(adopted, current);
    }

    #[test]
    fn an_enum_shape_migration_maps_every_variant() {
        assert_eq!(Mode::migrate_from(ModeV1::Fast), Mode::Fast);
        assert_eq!(Mode::migrate_from(ModeV1::Slow(3)), Mode::Slow { level: 3 });
    }

    #[test]
    fn an_unknown_shape_version_is_refused_rather_than_silently_defaulted() {
        let envelope = StateEnvelope {
            generation: 1,
            version: 99,
            type_name: "Stats".to_owned(),
            payload: serde_json::json!({}),
        };

        let err = decode_migrating::<StatsV1, Stats>(&envelope)
            .expect_err("an unaccountable version must not be adopted");

        assert!(
            matches!(
                err,
                AdoptError::VersionMismatch {
                    found: 99,
                    expected: 2,
                    ..
                }
            ),
            "unexpected error: {err}"
        );
        // The operator is told which versions this build could have taken.
        assert!(err.to_string().contains("version 1"), "{err}");
    }

    #[test]
    fn a_payload_that_does_not_fit_its_declared_shape_is_refused() {
        let envelope = StateEnvelope {
            generation: 1,
            version: 1,
            type_name: "StatsV1".to_owned(),
            payload: serde_json::json!({ "hits": "not-a-number" }),
        };

        let err = decode_migrating::<StatsV1, Stats>(&envelope).expect_err("must not decode");

        assert!(
            matches!(err, AdoptError::Decode { version: 1, .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_snapshot_file_round_trips_and_is_consumed_on_read() {
        let dir = std::env::temp_dir().join(format!(
            "autumn-upgrade-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("state.json");

        let envelope = envelope_of(
            &StatsV1 {
                hits: 5,
                note: "n".to_owned(),
            },
            2,
        );
        write_snapshot(&path, &envelope).expect("writes");

        // Written owner-only: the snapshot is application state on disk.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "snapshot must be owner-only");
        }

        let read = read_snapshot(&path).expect("reads");
        assert_eq!(read.version, 1);
        assert_eq!(read.generation, 2);
        assert!(
            !path.exists(),
            "the snapshot must be consumed, not left holding app state on disk"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_handover_this_build_did_not_complete_is_refused_rather_than_half_taken() {
        // A build handed a listening socket that it never adopted must not
        // release its predecessor: the address the predecessor is still serving
        // would be abandoned. (Adoption is a process-global one-shot, so this
        // test asserts the guard, not the adoption.)
        temp_env::with_var(LISTEN_FD_ENV, Some("0"), || {
            let refusal = verify_handover_complete().expect_err("must refuse");
            assert!(refusal.contains("never adopted it"), "{refusal}");
        });

        // ...and neither must a build that was handed live state and never
        // installed it (an app that dropped its `with_live_state(...)` call).
        let dir = std::env::temp_dir().join(format!(
            "autumn-upgrade-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("state.json");
        write_snapshot(&path, &envelope_of(&StatsV1::default(), 1)).expect("writes");

        temp_env::with_var(STATE_FILE_ENV, Some(&path), || {
            let refusal = verify_handover_complete().expect_err("must refuse");
            assert!(refusal.contains("never adopted it"), "{refusal}");
        });

        // A snapshot that *was* consumed leaves nothing behind, so the same
        // check passes.
        let _ = read_snapshot(&path).expect("reads");
        temp_env::with_var(STATE_FILE_ENV, Some(&path), || {
            verify_handover_complete().expect("a consumed snapshot is a completed handover");
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successor_cannot_acknowledge_a_write_its_upgrade_might_still_discard() {
        // A successor accepts the moment it adopts the socket, but until it
        // signals readiness the upgrade can still be abandoned — at which point
        // the predecessor resumes from the snapshot it took before spawning.
        // A write acknowledged in that window would die with this process, so
        // it is refused, exactly as on the predecessor's side of the handover.
        let handle = LiveStateHandle::new_frozen(StatsV1::default());

        assert!(handle.is_frozen());
        assert_eq!(handle.write(|s| s.hits += 1), Err(LiveStateFrozen));
        // Reads work throughout: the adopted state is complete, it is only its
        // *future* that is not settled yet.
        assert_eq!(handle.read(|s| s.hits), 0);

        // Readiness is what settles it.
        LiveStateRegistry::new(&handle).unfreeze();
        assert_eq!(handle.write(|s| s.hits += 1), Ok(()));
        assert_eq!(handle.read(|s| s.hits), 1);
    }

    #[test]
    fn readiness_that_cannot_be_published_is_an_error_not_a_shrug() {
        // The predecessor is released by this file appearing. If it cannot be
        // written, the predecessor times out and kills this process — so the
        // caller has to know, rather than this logging and carrying on with a
        // handover that will not complete.
        let dir = std::env::temp_dir().join(format!(
            "autumn-upgrade-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // The directory deliberately does not exist.
        temp_env::with_var(READY_FILE_ENV, Some(dir.join("ready")), || {
            publish_upgrade_readiness().expect_err("an unwritable readiness path must be an error");
        });

        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("ready");
        temp_env::with_var(READY_FILE_ENV, Some(&path), || {
            assert!(
                publish_upgrade_readiness().expect("publishes"),
                "a successor with a waiting predecessor reports that it released it"
            );
        });
        assert!(path.exists(), "the predecessor polls for this file");
        assert!(
            !dir.join("ready.tmp").exists(),
            "the file is renamed into place, never left half-written beside it"
        );

        // A cold start has nobody to tell.
        temp_env::with_vars_unset([READY_FILE_ENV], || {
            assert!(
                !publish_upgrade_readiness().expect("a cold start publishes nothing"),
                "a cold start has no predecessor to release"
            );
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a `Handoff` around a live child process, for the drop-guard tests.
    #[cfg(unix)]
    fn handoff_for_test(dir: std::path::PathBuf, registry: Arc<LiveStateRegistry>) -> Handoff {
        // Long enough to outlive the drop under test, short enough that a
        // panic before the explicit kill cannot leave it behind for long.
        let child = std::process::Command::new("sleep")
            .arg("5")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a stand-in successor process");
        Handoff {
            dir,
            child: Some(child),
            registry: Some(registry),
            completed: false,
        }
    }

    /// Whether `pid` names a live process, portably: `/proc` is Linux-only,
    /// and these tests run wherever `cfg(unix)` does.
    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    fn scratch_dir(line: u32) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("autumn-upgrade-test-{}-{line}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    #[cfg(unix)]
    fn an_abandoned_upgrade_kills_a_successor_that_never_took_over() {
        let dir = scratch_dir(line!());
        let handle = LiveStateHandle::new(StatsV1::default());
        let registry = Arc::new(LiveStateRegistry::new(&handle));
        registry.freeze_and_snapshot(1).expect("snapshots");

        let handoff = handoff_for_test(dir.clone(), registry);
        let pid = handoff.child.as_ref().expect("child").id();
        // No readiness file: this successor never took over.
        drop(handoff);

        assert!(
            !handle.is_frozen(),
            "an abandoned upgrade must hand the state back, or the old build serves the rest \
             of its life refusing writes"
        );
        assert!(
            !dir.exists(),
            "the handoff directory must not outlive the handoff"
        );
        assert!(
            !process_is_alive(pid),
            "the half-started successor must not be left holding the port"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_successor_that_already_published_readiness_survives_an_abandoned_wait() {
        // The window: the successor renames its readiness file and becomes
        // writable, and a `SIGTERM` reaches this process before its next poll
        // observes it. The successor owns the socket and the state by then —
        // killing it would discard writes it has already acknowledged, and
        // unfreezing here would leave two writable copies of one block.
        let dir = scratch_dir(line!());
        std::fs::write(dir.join("ready"), "1").expect("the successor published readiness");

        let handle = LiveStateHandle::new(StatsV1::default());
        let registry = Arc::new(LiveStateRegistry::new(&handle));
        registry.freeze_and_snapshot(1).expect("snapshots");

        let handoff = handoff_for_test(dir.clone(), registry);
        let pid = handoff.child.as_ref().expect("child").id();
        drop(handoff);

        assert!(
            handle.is_frozen(),
            "the successor is writable, so this process must stay frozen: two writable copies \
             of one block is the divergence the freeze exists to prevent"
        );
        assert!(
            process_is_alive(pid),
            "a successor that has taken over must not be killed by a wait this process \
             abandoned"
        );
        assert!(
            !dir.exists(),
            "the snapshot has been read and readiness published, so the directory holding \
             application state goes on this path too"
        );

        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }

    #[test]
    fn an_ordinary_cold_start_has_no_handover_to_complete() {
        temp_env::with_vars_unset([LISTEN_FD_ENV, STATE_FILE_ENV], || {
            assert!(!handoff_requested());
            verify_handover_complete().expect("a cold start is not a half-completed handover");
        });
    }

    #[test]
    fn the_binary_to_upgrade_into_must_exist_before_anything_is_frozen() {
        let missing = std::env::temp_dir().join("autumn-upgrade-no-such-binary");
        let _ = std::fs::remove_file(&missing);

        temp_env::with_var(BINARY_ENV, Some(&missing), || {
            let error = upgrade_binary().expect_err("a binary that is not there is not a target");
            assert!(
                matches!(error, UpgradeError::Binary(_)),
                "unexpected error: {error}"
            );
            assert!(error.to_string().contains("is not a file"), "{error}");
        });
    }

    #[test]
    fn the_handoff_directory_is_private_to_this_user() {
        // It holds a serialized copy of the application's state.
        let base = std::env::temp_dir().join(format!(
            "autumn-upgrade-test-base-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);

        let entropy = crate::entropy::SeededEntropy::new(1674);
        let dir = temp_env::with_var(DIR_ENV, Some(&base), || {
            create_handoff_dir(1, &entropy).expect("creates a private directory")
        });

        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(&dir).expect("stat").permissions().mode()
        };
        assert_eq!(mode & 0o777, 0o700, "handoff directory must be owner-only");

        // The name is not predictable from the pid and generation alone: a
        // local neighbour cannot pre-create it to deny every future upgrade.
        let second = temp_env::with_var(DIR_ENV, Some(&base), || {
            create_handoff_dir(1, &entropy).expect("creates another private directory")
        });
        assert_ne!(
            dir, second,
            "two handoffs at the same pid and generation must not collide on a name"
        );
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("directory name");
        assert!(
            !name.ends_with(&format!("-{}-1-0", std::process::id())),
            "the name must not be a bare pid/generation counter: {name}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_snapshot_file_is_an_error_not_an_empty_adoption() {
        let path = std::env::temp_dir().join("autumn-upgrade-test-does-not-exist.json");
        let _ = std::fs::remove_file(&path);

        let err = read_snapshot(&path).expect_err("must not silently succeed");

        assert!(matches!(err, AdoptError::Read { .. }), "{err}");
    }
}
