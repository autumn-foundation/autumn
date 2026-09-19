//! Deterministic, injectable randomness (entropy).
//!
//! Autumn exposes an [`Rng`] extractor so handlers — and framework internals
//! that mint identifiers — draw random bytes and UUIDs through the framework's
//! injected [`Entropy`] source instead of calling [`uuid::Uuid::new_v4`] or the
//! OS RNG directly. This is the exact mirror of the [`crate::time::Clock`]
//! extractor / [`crate::time::ClockSource`] seam for wall-clock time:
//!
//! - **Production** uses [`OsEntropy`] (the silent default), which reads the
//!   operating-system CSPRNG on every draw — indistinguishable from calling
//!   `getrandom` directly.
//! - **Simulation / tests** inject a [`SeededEntropy`] (a `ChaCha8Rng` seeded
//!   from a single `u64`) via [`crate::state::AppState::with_entropy`], so the
//!   same seed replays the same byte-for-byte identifier stream on every
//!   machine and every run. The seeded simulation handle is
//!   [`crate::sim::SimRng`], reachable from a `#[sim_test]` through
//!   [`crate::sim::Sim`].
//!
//! # Quick example
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::entropy::Rng;
//!
//! #[get("/token")]
//! async fn token(rng: Rng) -> String {
//!     // A fresh v4 UUID drawn through the injected entropy source.
//!     rng.uuid_v4().to_string()
//! }
//! ```

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::sync::{Arc, Mutex};

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use uuid::Uuid;

// ── Entropy source trait ──────────────────────────────────────────────────────

/// Source of random bytes used internally by the framework and by the [`Rng`]
/// extractor.
///
/// Production apps see [`OsEntropy`] (the silent default). Simulation tests swap
/// it out for a [`SeededEntropy`] via
/// [`crate::state::AppState::with_entropy`], making every framework-minted
/// identifier reproducible under a fixed seed.
///
/// Implementations must be cheap to call through a shared reference: the
/// framework holds the source as `Arc<dyn Entropy>` and draws from it
/// concurrently, so stateful generators (like [`SeededEntropy`]) use interior
/// mutability. This mirrors [`crate::time::ClockSource`], whose `now(&self)` is
/// likewise a shared-reference read.
///
/// The [`uuid_v4`](Entropy::uuid_v4) / [`uuid_v7`](Entropy::uuid_v7) helpers are
/// provided methods built on [`fill_bytes`](Entropy::fill_bytes), so every
/// source generates RFC 4122 UUIDs identically — a custom source only implements
/// the two byte-drawing methods.
pub trait Entropy: std::fmt::Debug + Send + Sync + 'static {
    /// Draw the next random `u64`.
    fn next_u64(&self) -> u64;

    /// Fill `dest` with random bytes.
    fn fill_bytes(&self, dest: &mut [u8]);

    /// Draw a random version-4 (fully random) [`Uuid`] from this source.
    ///
    /// Deterministic under a [`SeededEntropy`]: the same seed and the same
    /// number of prior draws always yield the same UUID.
    #[must_use]
    fn uuid_v4(&self) -> Uuid {
        let mut bytes = [0u8; 16];
        self.fill_bytes(&mut bytes);
        uuid_v4_from_bytes(bytes)
    }

    /// Draw a version-7 (time-ordered) [`Uuid`] whose 48-bit timestamp is
    /// `unix_millis` and whose remaining 74 bits are drawn from this source.
    ///
    /// The random portion is deterministic under a [`SeededEntropy`]; overall
    /// determinism additionally requires a deterministic `unix_millis` (e.g.
    /// sourced from the simulation clock). Callers with only wall-clock time
    /// available get a monotonic-ish but non-reproducible timestamp prefix.
    #[must_use]
    fn uuid_v7(&self, unix_millis: u64) -> Uuid {
        let mut rand_bytes = [0u8; 10];
        self.fill_bytes(&mut rand_bytes);
        uuid_v7_from_parts(unix_millis, rand_bytes)
    }
}

/// Build a version-4 [`Uuid`] from 16 raw bytes by stamping the RFC 4122
/// version (4) and variant bits. Shared by [`Entropy::uuid_v4`] and
/// [`crate::sim::SimRng::uuid_v4`] so both paths produce identical UUIDs.
#[must_use]
pub(crate) const fn uuid_v4_from_bytes(mut bytes: [u8; 16]) -> Uuid {
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant
    Uuid::from_bytes(bytes)
}

/// Build a version-7 [`Uuid`] from a 48-bit millisecond timestamp and 10 random
/// bytes, stamping the RFC 4122 version (7) and variant bits. Shared by
/// [`Entropy::uuid_v7`] and [`crate::sim::SimRng::uuid_v7`].
#[must_use]
pub(crate) fn uuid_v7_from_parts(unix_millis: u64, rand_bytes: [u8; 10]) -> Uuid {
    let mut bytes = [0u8; 16];
    let ts = unix_millis.to_be_bytes();
    // 48-bit big-endian timestamp occupies bytes 0..6.
    bytes[0..6].copy_from_slice(&ts[2..8]);
    bytes[6..16].copy_from_slice(&rand_bytes);
    bytes[6] = (bytes[6] & 0x0F) | 0x70; // version 7
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant
    Uuid::from_bytes(bytes)
}

/// Derive a stable [`Uuid`] from a `seed` and a `purpose_tag` namespace,
/// **independently of any draw stream** (issue #1797, seed-derived ids).
///
/// The same `(seed, purpose_tag)` pair always yields the same UUID, regardless
/// of how many other values have been drawn from any RNG — it is a pure
/// function of its inputs, so it never perturbs (and is never perturbed by) the
/// main deterministic draw sequence. This makes it ideal for byte-reproducible
/// multi-tenant fixtures: `derive(seed, "tenant:acme")` is a stable id for
/// "acme" under that seed.
///
/// Mechanism: a stable, platform-independent FNV-1a 64-bit hash of the seed
/// bytes followed by the tag bytes yields a sub-seed; a `ChaCha8Rng` seeded from
/// that sub-seed fills 16 bytes, which are stamped as a version-4 (RFC 4122)
/// UUID. Version 4 is used so a derived id is indistinguishable in shape from a
/// normally-drawn one; its reproducibility comes purely from the derivation, not
/// from any embedded structure.
#[must_use]
pub(crate) fn derive_uuid_from(seed: u64, purpose_tag: &[u8]) -> Uuid {
    // FNV-1a 64-bit over seed bytes then tag bytes. Chosen for a fully stable,
    // dependency-free, cross-platform hash (unlike std's DefaultHasher, whose
    // output is not guaranteed stable across Rust versions).
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in seed.to_le_bytes().iter().chain(purpose_tag) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let mut rng = ChaCha8Rng::seed_from_u64(hash);
    let mut bytes = [0u8; 16];
    rng.fill_bytes(&mut bytes);
    uuid_v4_from_bytes(bytes)
}

/// A shared entropy handle is itself an entropy source.
///
/// The [`ClockSource`](crate::time::ClockSource) sibling of this impl exists
/// for the same reason: a capsule replay hands its already-`Arc`ed source
/// straight to `TestApp::with_entropy`, with no forwarding newtype in between.
impl Entropy for std::sync::Arc<dyn Entropy> {
    fn next_u64(&self) -> u64 {
        (**self).next_u64()
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        (**self).fill_bytes(dest);
    }

    // The *provided* methods are forwarded too, not left to the default. A
    // source that overrides `uuid_v4`/`uuid_v7` would otherwise have its
    // override silently dropped the moment it was handed through an `Arc`,
    // which is precisely how this impl is used.
    fn uuid_v4(&self) -> Uuid {
        (**self).uuid_v4()
    }

    fn uuid_v7(&self, unix_millis: u64) -> Uuid {
        (**self).uuid_v7(unix_millis)
    }
}

// ── OS (real) entropy ─────────────────────────────────────────────────────────

/// Real operating-system entropy implementation of [`Entropy`].
///
/// This is the default when no seeded source is configured. It reads the OS
/// CSPRNG via `getrandom` on every draw and carries zero reproducibility — the
/// silent production behavior, matching a direct [`uuid::Uuid::new_v4`] call.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn next_u64(&self) -> u64 {
        let mut buf = [0u8; 8];
        getrandom::getrandom(&mut buf).expect("OS RNG must not fail on supported platforms");
        u64::from_le_bytes(buf)
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        getrandom::getrandom(dest).expect("OS RNG must not fail on supported platforms");
    }
}

// ── Seeded (deterministic) entropy ─────────────────────────────────────────────

/// A deterministic [`Entropy`] source seeded from a single `u64`.
///
/// Wraps a `ChaCha8Rng` behind a [`Mutex`] so it satisfies the
/// shared-reference [`Entropy`] contract while remaining fully reproducible: the
/// same seed and the same sequence of draws always produce the same bytes. This
/// is the source a simulation injects via
/// [`crate::state::AppState::with_entropy`]; it shares its seeding with
/// [`crate::sim::SimRng`] so a `Sim` handle and the `AppState` it mounts agree.
#[derive(Debug)]
pub struct SeededEntropy {
    seed: u64,
    inner: Mutex<ChaCha8Rng>,
}

impl SeededEntropy {
    /// Seed a fresh deterministic entropy source from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            inner: Mutex::new(ChaCha8Rng::seed_from_u64(seed)),
        }
    }

    /// Derive a stable [`Uuid`] from this source's seed and a `purpose_tag`
    /// namespace, **independently of the draw stream** (seed-derived ids).
    ///
    /// The same seed and `purpose_tag` always yield the same UUID no matter how
    /// many bytes have been drawn, so it is safe for byte-reproducible
    /// multi-tenant fixtures without disturbing the deterministic id stream. See
    /// `derive_uuid_from` for the mechanism and the version bits it sets.
    #[must_use]
    pub fn derive_uuid(&self, purpose_tag: impl AsRef<[u8]>) -> Uuid {
        derive_uuid_from(self.seed, purpose_tag.as_ref())
    }

    /// Seed a fresh deterministic entropy source from `seed`, boxed as a shared
    /// [`Entropy`] handle ready to hand to
    /// [`crate::state::AppState::with_entropy`].
    #[must_use]
    pub fn shared(seed: u64) -> Arc<dyn Entropy> {
        Arc::new(Self::new(seed))
    }

    fn with_inner<R>(&self, f: impl FnOnce(&mut ChaCha8Rng) -> R) -> R {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

impl Entropy for SeededEntropy {
    fn next_u64(&self) -> u64 {
        self.with_inner(RngCore::next_u64)
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        self.with_inner(|rng| rng.fill_bytes(dest));
    }
}

// ── Extractor ─────────────────────────────────────────────────────────────────

/// Axum extractor that resolves the framework's injected [`Entropy`] source.
///
/// Use as a handler argument to draw random bytes or UUIDs through the injected
/// source instead of calling [`uuid::Uuid::new_v4`] or the OS RNG directly. This
/// lets simulation tests make handler-minted identifiers reproducible via
/// [`crate::state::AppState::with_entropy`], exactly as the
/// [`crate::time::Clock`] extractor lets tests control time.
///
/// Unlike [`Clock`](crate::time::Clock), which snapshots the current instant at
/// extraction time, `Rng` holds a live handle to the source — an RNG is stateful
/// and each draw advances it.
///
/// ```rust,ignore
/// use autumn_web::entropy::Rng;
///
/// async fn handler(rng: Rng) -> String {
///     rng.uuid_v4().to_string()
/// }
/// ```
#[derive(Clone)]
pub struct Rng(Arc<dyn Entropy>);

impl Rng {
    /// Wrap an [`Entropy`] handle as an [`Rng`] (framework / test helper).
    #[must_use]
    pub fn from_source(source: Arc<dyn Entropy>) -> Self {
        Self(source)
    }

    /// Borrow the underlying [`Entropy`] source.
    #[must_use]
    pub fn source(&self) -> &dyn Entropy {
        self.0.as_ref()
    }

    /// Draw the next random `u64` from the injected source.
    #[must_use]
    pub fn next_u64(&self) -> u64 {
        self.0.next_u64()
    }

    /// Fill `dest` with random bytes from the injected source.
    pub fn fill_bytes(&self, dest: &mut [u8]) {
        self.0.fill_bytes(dest);
    }

    /// Draw a random version-4 [`Uuid`] from the injected source.
    #[must_use]
    pub fn uuid_v4(&self) -> Uuid {
        self.0.uuid_v4()
    }

    /// Draw a version-7 [`Uuid`] with the given millisecond timestamp from the
    /// injected source.
    #[must_use]
    pub fn uuid_v7(&self, unix_millis: u64) -> Uuid {
        self.0.uuid_v7(unix_millis)
    }
}

impl std::fmt::Debug for Rng {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rng").finish_non_exhaustive()
    }
}

impl axum::extract::FromRequestParts<crate::state::AppState> for Rng {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &crate::state::AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(state.entropy_arc()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Entropy, OsEntropy, SeededEntropy, uuid_v4_from_bytes, uuid_v7_from_parts};

    #[test]
    fn uuid_v4_sets_version_and_variant_bits() {
        let id = uuid_v4_from_bytes([0xFF; 16]);
        assert_eq!(id.get_version_num(), 4);
        // RFC 4122 variant: high two bits of byte 8 are 0b10.
        assert_eq!(id.as_bytes()[8] & 0xC0, 0x80);
    }

    #[test]
    fn uuid_v7_embeds_timestamp_and_sets_bits() {
        let id = uuid_v7_from_parts(0x0000_0102_0304_0506, [0xAA; 10]);
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(id.as_bytes()[8] & 0xC0, 0x80);
        // 48-bit big-endian timestamp in the first six bytes.
        assert_eq!(&id.as_bytes()[0..6], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    }

    #[test]
    fn seeded_entropy_is_reproducible() {
        let a = SeededEntropy::new(42);
        let b = SeededEntropy::new(42);
        assert_eq!(a.next_u64(), b.next_u64());
        assert_eq!(a.uuid_v4(), b.uuid_v4());
        assert_eq!(a.uuid_v7(1_700_000_000_000), b.uuid_v7(1_700_000_000_000));
    }

    #[test]
    fn seeded_entropy_diverges_by_seed() {
        let a = SeededEntropy::new(1);
        let b = SeededEntropy::new(2);
        assert_ne!(a.uuid_v4(), b.uuid_v4());
    }

    #[test]
    fn derive_uuid_is_stable_tag_sensitive_and_stream_independent() {
        let e = SeededEntropy::new(7);
        // Same tag ⇒ same id.
        assert_eq!(e.derive_uuid("tenant:acme"), e.derive_uuid("tenant:acme"));
        // Different tags ⇒ different ids.
        assert_ne!(e.derive_uuid("tenant:acme"), e.derive_uuid("tenant:beta"));
        // Stable regardless of interleaved main-stream draws (order-independent).
        let before = e.derive_uuid("tenant:acme");
        let _ = e.uuid_v4();
        let _ = e.next_u64();
        assert_eq!(before, e.derive_uuid("tenant:acme"));
        // Reproducible from a fresh source with the same seed.
        assert_eq!(before, SeededEntropy::new(7).derive_uuid("tenant:acme"));
        // Diverges by seed.
        assert_ne!(before, SeededEntropy::new(8).derive_uuid("tenant:acme"));
        // Shaped as a v4 UUID.
        assert_eq!(before.get_version_num(), 4);
    }

    #[test]
    fn os_entropy_draws_distinct_values() {
        let os = OsEntropy;
        // Vanishingly unlikely to collide; guards against a stuck source.
        assert_ne!(os.uuid_v4(), os.uuid_v4());
    }
}
