//! Per-tenant in-process memory accounting cells.
//!
//! A [`TenantCell`] gives each resolved tenant its own byte-accounting boundary
//! with a soft memory quota and an owned scratch buffer. Allocations that flow
//! through the cell's API are tracked; when the cell is evicted and dropped,
//! Rust's ownership rules deterministically reclaim its tracked footprint.
//!
//! The guarantee is scoped to *tracked* bytes — allocations made through the
//! cell's API — not a tenant's true process resident set size. Work a handler
//! performs outside the cell (e.g. a bare `Box::new`) is invisible to the
//! counter by design.
//!
//! # Accounting model
//!
//! [`TenantCell::tracked_bytes`] is a deterministic accounting of the
//! allocations made *through* the cell, and covers exactly three things: (a)
//! each live [`Charge`]'s declared bytes, (b) the allocation *capacity* of every
//! stored scratch key `String` and value `Vec<u8>`, and (c) a fixed
//! [`SCRATCH_ENTRY_OVERHEAD`](TenantCell::scratch_entry_overhead) per scratch
//! entry (covering the map's per-entry
//! `String`/`Vec` headers and an amortized bucket slot, so the *count* of tiny
//! entries is bounded against the quota). This per-entry overhead is charged
//! against a **high-water mark** of the live scratch-entry count rather than the
//! instantaneous count: it is *not* released when an individual entry is removed
//! — [`HashMap`] does not shrink its bucket array on `remove`, so the enlarged
//! bucket allocation stays resident — and it is reclaimed only when the whole
//! cell is dropped/evicted (which drops the map, freeing the buckets).
//! Re-inserting keys within a prior peak therefore adds no new overhead. It is
//! explicitly **not** a measurement of the tenant's true process RSS:
//! allocator-internal fragmentation, size-class rounding, and any allocation a
//! handler makes outside the cell's API are out of scope by design. This is a
//! safe-Rust accounting cell, not a bounding allocator.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// The safe, allocator-backed region for the allocation classes Autumn can
/// enforce: owned byte buffers and UTF-8 strings created by this API.
///
/// This is deliberately not a global allocator.  Ordinary `Vec`, `String`,
/// `Box`, third-party, and allocator-internal allocations remain outside this
/// cooperative tracked-memory boundary.
#[derive(Clone, Debug)]
pub struct TenantArena {
    cell: TenantCell,
}

impl TenantArena {
    /// Allocate a zero-filled byte buffer owned by this tenant region.
    ///
    /// Reservation happens before allocation. Quota exhaustion therefore does
    /// not allocate and does not mutate accounting.
    ///
    /// # Errors
    ///
    /// Returns [`TenantAllocationError::Quota`] when the finite quota would be
    /// exceeded, or [`TenantAllocationError::Allocator`] if the process
    /// allocator rejects the reservation.
    pub fn try_bytes(&self, len: usize) -> Result<TenantBytes, TenantAllocationError> {
        let charge = self.cell.try_charge(len)?;
        let mut value = Vec::new();
        value
            .try_reserve_exact(len)
            .map_err(|error| TenantAllocationError::Allocator {
                requested: len,
                message: error.to_string(),
            })?;
        let excess_charge = (value.capacity() > len)
            .then(|| self.cell.try_charge(value.capacity() - len))
            .transpose()?;
        value.resize(len, 0);
        Ok(TenantBytes {
            value,
            charge,
            excess_charge,
        })
    }

    /// Copy `value` into an owned UTF-8 allocation in this tenant region.
    ///
    /// # Errors
    ///
    /// Returns [`TenantAllocationError::Quota`] when the finite quota would be
    /// exceeded, or [`TenantAllocationError::Allocator`] if the process
    /// allocator rejects the reservation.
    pub fn try_string(&self, value: &str) -> Result<TenantString, TenantAllocationError> {
        let charge = self.cell.try_charge(value.len())?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|error| TenantAllocationError::Allocator {
                requested: value.len(),
                message: error.to_string(),
            })?;
        let excess_charge = (owned.capacity() > value.len())
            .then(|| self.cell.try_charge(owned.capacity() - value.len()))
            .transpose()?;
        owned.push_str(value);
        Ok(TenantString {
            value: owned,
            charge,
            excess_charge,
        })
    }

    /// Tenant accounting domain backing this arena.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        self.cell.tenant_id()
    }
}

/// A byte allocation whose ownership and accounting cannot be separated.
#[derive(Debug)]
pub struct TenantBytes {
    value: Vec<u8>,
    charge: Charge,
    excess_charge: Option<Charge>,
}

impl TenantBytes {
    /// Borrow the allocation.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.value
    }
    /// Mutably borrow the fixed-size allocation.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.value
    }
    /// Number of quota bytes owned by this allocation.
    #[must_use]
    pub fn tracked_bytes(&self) -> usize {
        self.charge.bytes() + self.excess_charge.as_ref().map_or(0, Charge::bytes)
    }
}

/// A UTF-8 allocation whose ownership and accounting cannot be separated.
#[derive(Debug)]
pub struct TenantString {
    value: String,
    charge: Charge,
    excess_charge: Option<Charge>,
}

impl TenantString {
    /// Borrow the allocation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
    /// Number of quota bytes owned by this allocation.
    #[must_use]
    pub fn tracked_bytes(&self) -> usize {
        self.charge.bytes() + self.excess_charge.as_ref().map_or(0, Charge::bytes)
    }
}

/// Failure to reserve tenant quota or obtain memory from the process allocator.
#[derive(Debug)]
pub enum TenantAllocationError {
    /// The tenant's finite cooperative quota was exhausted.
    Quota(QuotaExceeded),
    /// The system allocator rejected a reservation after quota was reserved.
    Allocator { requested: usize, message: String },
}

impl From<QuotaExceeded> for TenantAllocationError {
    fn from(value: QuotaExceeded) -> Self {
        Self::Quota(value)
    }
}

impl fmt::Display for TenantAllocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quota(error) => error.fmt(f),
            Self::Allocator { requested, message } => write!(
                f,
                "allocator rejected tenant scratch reservation of {requested} bytes: {message}"
            ),
        }
    }
}

impl std::error::Error for TenantAllocationError {}

/// Fixed bytes charged per scratch entry to cover the map's per-entry overhead:
/// the `String` and `Vec` structs stored inline in the bucket array plus an
/// amortized bucket slot / control byte. Charging this bounds the *number* of
/// scratch entries against the quota, so a tenant storing many tiny entries
/// cannot amplify its footprint past the configured cap via map growth.
const SCRATCH_ENTRY_OVERHEAD: usize = std::mem::size_of::<(String, Vec<u8>)>() + 16;

/// Error returned when a charge would exceed a tenant's soft memory quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaExceeded {
    /// The tenant whose quota was exceeded.
    pub tenant_id: String,
    /// Bytes the caller attempted to charge.
    pub requested: usize,
    /// Bytes already tracked for the tenant when the request was made.
    pub in_use: usize,
    /// The tenant's soft quota, in bytes.
    pub quota: usize,
}

impl fmt::Display for QuotaExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tenant '{}' memory quota exceeded: requested {} bytes, {} in use, quota {} bytes",
            self.tenant_id, self.requested, self.in_use, self.quota
        )
    }
}

// `QuotaExceeded` is a `std::error::Error`, so it converts into
// [`crate::AutumnError`] via the crate's blanket `From<E: Error>` impl, which
// special-cases it to HTTP 503 Service Unavailable. Handlers that allocate
// through a cell can therefore propagate a quota breach with `?`.
impl std::error::Error for QuotaExceeded {}

/// The tenant's scratch map plus the high-water mark used to charge per-entry
/// overhead. Both fields are guarded together by a single [`Mutex`] so the peak
/// is only ever mutated while the map is locked.
#[derive(Debug, Default)]
struct ScratchState {
    /// Owned per-tenant scratch buffer. Dropped with the cell.
    map: HashMap<String, Vec<u8>>,
    /// High-water mark of the number of live scratch entries. Monotonic for the
    /// life of the cell: it only ever grows (never lowered on removal), because
    /// `HashMap` does not shrink its bucket array when entries are removed, so
    /// the per-entry bucket overhead stays resident until the map is dropped.
    peak_entries: usize,
}

/// Shared state owned by a single tenant's cell.
#[derive(Debug)]
struct TenantCellInner {
    tenant_id: String,
    /// Soft quota in bytes; `0` means unlimited. Mutable at runtime via
    /// [`TenantCell::set_quota_bytes`] so a resident cell can pick up a
    /// reconfigured quota without being evicted and rebuilt.
    quota_bytes: AtomicUsize,
    /// Bytes currently tracked for this tenant.
    tracked_bytes: AtomicUsize,
    /// Registry-relative timestamp (milliseconds since the registry's `base`
    /// `Instant`) of this cell's most recent access, used to drive idle-TTL and
    /// least-recently-used eviction. Set by the registry on creation and on
    /// every subsequent lookup.
    last_access: AtomicU64,
    /// Globally-monotonic access sequence number (from the registry's `seq`
    /// counter) of this cell's most recent access. Unlike `last_access`, which
    /// has millisecond resolution and can tie when several tenants are created
    /// in the same tick, `last_access_seq` is strictly increasing and unique
    /// per access, so LRU victim selection has no ties and never evicts the
    /// just-inserted cell. Idle-TTL still uses `last_access` (wall-clock age).
    last_access_seq: AtomicU64,
    /// Owned per-tenant scratch buffer and its entry high-water mark. Dropped
    /// with the cell.
    scratch: Mutex<ScratchState>,
    /// Process-wide tracked-bytes gauge shared with the owning registry.
    global_tracked: Arc<AtomicUsize>,
}

impl TenantCellInner {
    /// Reserve `n` bytes against the quota, updating both the per-tenant and
    /// process-wide gauges. Fails without mutating state if it would exceed the
    /// quota.
    fn reserve(&self, n: usize) -> Result<(), QuotaExceeded> {
        // Reload the quota on entry (and on each CAS retry below) so a runtime
        // change via `set_quota_bytes` is honored by the very next reservation.
        if self.quota_bytes.load(Ordering::Relaxed) == 0 {
            self.tracked_bytes.fetch_add(n, Ordering::Relaxed);
            self.global_tracked.fetch_add(n, Ordering::Relaxed);
            return Ok(());
        }
        let mut current = self.tracked_bytes.load(Ordering::Relaxed);
        loop {
            // Reload the quota each iteration and re-apply the `0 == unlimited`
            // rule *before* the over-quota check: a concurrent `set_quota_bytes`
            // can flip a finite quota to unlimited between the entry check above
            // and a later CAS retry, and once unlimited every positive `next`
            // must be accepted rather than compared against `0`.
            let quota = self.quota_bytes.load(Ordering::Relaxed);
            if quota == 0 {
                self.tracked_bytes.fetch_add(n, Ordering::Relaxed);
                self.global_tracked.fetch_add(n, Ordering::Relaxed);
                return Ok(());
            }
            let next = current.saturating_add(n);
            if next > quota {
                return Err(QuotaExceeded {
                    tenant_id: self.tenant_id.clone(),
                    requested: n,
                    in_use: current,
                    quota,
                });
            }
            match self.tracked_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.global_tracked.fetch_add(n, Ordering::Relaxed);
                    return Ok(());
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Release `n` bytes, clamping at zero so double-release can never underflow.
    fn release(&self, n: usize) {
        let mut current = self.tracked_bytes.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(n);
            match self.tracked_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.global_tracked
                        .fetch_sub(current - next, Ordering::Relaxed);
                    return;
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for TenantCellInner {
    fn drop(&mut self) {
        // Deterministically reclaim any bytes still tracked (e.g. scratch state)
        // from the process-wide gauge when the last reference to the cell drops.
        let remaining = *self.tracked_bytes.get_mut();
        if remaining > 0 {
            self.global_tracked.fetch_sub(remaining, Ordering::Relaxed);
        }
    }
}

/// An RAII handle for bytes charged to a [`TenantCell`]. Dropping it immediately
/// releases those bytes back to the cell (and the process-wide gauge).
#[must_use = "dropping the Charge immediately releases its bytes"]
pub struct Charge {
    inner: Arc<TenantCellInner>,
    bytes: usize,
}

impl Charge {
    /// The number of bytes this charge holds.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl fmt::Debug for Charge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Charge")
            .field("tenant_id", &self.inner.tenant_id)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.inner.release(self.bytes);
    }
}

/// A per-tenant memory accounting boundary: a byte counter, a soft quota, and an
/// owned scratch buffer. Cheap to clone (reference-counted).
#[derive(Clone, Debug)]
pub struct TenantCell {
    inner: Arc<TenantCellInner>,
}

impl TenantCell {
    fn new(
        tenant_id: impl Into<String>,
        quota_bytes: usize,
        global_tracked: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            inner: Arc::new(TenantCellInner {
                tenant_id: tenant_id.into(),
                quota_bytes: AtomicUsize::new(quota_bytes),
                tracked_bytes: AtomicUsize::new(0),
                // The owning registry stamps the real access time immediately
                // after construction; 0 is a placeholder until then.
                last_access: AtomicU64::new(0),
                // Likewise a placeholder until the registry's first `touch`
                // stamps a real, strictly-increasing sequence number.
                last_access_seq: AtomicU64::new(0),
                scratch: Mutex::new(ScratchState::default()),
                global_tracked,
            }),
        }
    }

    /// The tenant this cell belongs to.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.inner.tenant_id
    }

    /// The soft quota in bytes (`0` means unlimited).
    #[must_use]
    pub fn quota_bytes(&self) -> usize {
        self.inner.quota_bytes.load(Ordering::Relaxed)
    }

    /// Update the soft quota in bytes (`0` means unlimited). The new value takes
    /// effect on the next [`try_charge`](Self::try_charge)/`scratch_insert`
    /// reservation; already-tracked bytes are never retroactively rejected.
    pub fn set_quota_bytes(&self, quota_bytes: usize) {
        self.inner.quota_bytes.store(quota_bytes, Ordering::Relaxed);
    }

    /// Record a registry-relative access timestamp (milliseconds since the
    /// registry's base `Instant`) together with a globally-monotonic access
    /// sequence number. Called by the registry on every lookup to drive
    /// idle-TTL (via the millisecond timestamp) and least-recently-used (via the
    /// strictly-increasing sequence) eviction. Both are relaxed atomic
    /// `fetch_max` updates, so this is safe to call through a shared
    /// `&TenantCell` while the registry's read guard is still held.
    ///
    /// The stores are monotonic (`fetch_max`): a stale `now_millis` captured
    /// before a lock wait, or an out-of-order concurrent `touch`, can never
    /// regress a cell's recorded access time or sequence below a fresher value
    /// already stamped by another accessor. This keeps a just-accessed tenant
    /// from being seen as idle (and evicted, letting a duplicate cell form)
    /// because of a lagging timestamp.
    pub fn touch(&self, now_millis: u64, seq: u64) {
        self.inner
            .last_access
            .fetch_max(now_millis, Ordering::Relaxed);
        self.inner.last_access_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// The registry-relative timestamp of this cell's most recent access.
    #[must_use]
    pub fn last_access_millis(&self) -> u64 {
        self.inner.last_access.load(Ordering::Relaxed)
    }

    /// The globally-monotonic access sequence number of this cell's most recent
    /// access. Strictly increases with every `touch`, so it breaks the
    /// millisecond ties `last_access_millis` can produce when multiple tenants
    /// are accessed in the same tick — used to pick the LRU eviction victim.
    #[must_use]
    pub fn last_access_seq(&self) -> u64 {
        self.inner.last_access_seq.load(Ordering::Relaxed)
    }

    /// Bytes currently tracked for this tenant.
    #[must_use]
    pub fn tracked_bytes(&self) -> usize {
        self.inner.tracked_bytes.load(Ordering::Relaxed)
    }

    /// Return the safe region used for supported tenant-owned scratch buffers.
    #[must_use]
    pub fn arena(&self) -> TenantArena {
        TenantArena { cell: self.clone() }
    }

    /// The fixed per-entry overhead (bytes) charged against the quota for each
    /// stored scratch entry, in addition to the key and value capacities.
    #[must_use]
    pub const fn scratch_entry_overhead() -> usize {
        SCRATCH_ENTRY_OVERHEAD
    }

    /// Charge `bytes` against the quota, returning an RAII [`Charge`] that
    /// releases them on drop. Fails with [`QuotaExceeded`] (→ HTTP 503) if the
    /// charge would exceed the quota, leaving the counter unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`QuotaExceeded`] if the charge would exceed the tenant's quota.
    pub fn try_charge(&self, bytes: usize) -> Result<Charge, QuotaExceeded> {
        self.inner.reserve(bytes)?;
        Ok(Charge {
            inner: Arc::clone(&self.inner),
            bytes,
        })
    }

    /// Store `value` in the tenant's scratch buffer under `key`, charging only
    /// the *net* byte delta against the quota when replacing an existing entry.
    ///
    /// Accounting covers both the stored `String` key and the value `Vec`
    /// allocation *capacity* (the bytes the cell actually owns), not their
    /// lengths, so a large unique/user-derived key or a `Vec` with large spare
    /// capacity is charged for the whole allocation it keeps resident.
    ///
    /// The key's capacity is charged only when a *new* key is inserted (and
    /// released on removal). The fixed
    /// [`SCRATCH_ENTRY_OVERHEAD`](Self::scratch_entry_overhead) for the map
    /// slot and per-entry headers is charged against a *high-water mark* of the
    /// live scratch-entry count: inserting a new key that pushes the count above
    /// the prior peak charges one overhead, but re-inserting within the prior
    /// peak charges none, and the overhead is retained (not released) on removal
    /// because [`HashMap`] keeps its enlarged bucket array. Replacing an existing
    /// key leaves the stored key untouched —
    /// [`HashMap::insert`](std::collections::HashMap::insert) keeps the original
    /// key and only swaps the value — so a replace charges just the
    /// value-capacity delta (`new_cap - old_cap`), releasing the difference when
    /// the value shrinks. A same-size or shrinking replace can therefore never
    /// transiently overshoot the quota and spuriously fail.
    ///
    /// # Errors
    ///
    /// Returns [`QuotaExceeded`] if the net growth would exceed the tenant's
    /// quota; the scratch buffer is left unchanged.
    ///
    /// # Panics
    ///
    /// Panics if the tenant cell's scratch lock is poisoned.
    pub fn scratch_insert(
        &self,
        key: impl Into<String>,
        value: Vec<u8>,
    ) -> Result<(), QuotaExceeded> {
        let key = key.into();
        let new_val_cap = value.capacity();
        let mut scratch = self
            .inner
            .scratch
            .lock()
            .expect("tenant cell scratch lock poisoned");
        let state = &mut *scratch;
        if let Some(old_val_cap) = state.map.get(&key).map(Vec::capacity) {
            // Key already present: `insert` keeps the stored key and swaps the
            // value, so only the value-capacity delta is charged. The freshly
            // built `key` String is dropped. The key, overhead, and peak are all
            // unaffected.
            if new_val_cap > old_val_cap {
                self.inner.reserve(new_val_cap - old_val_cap)?;
            } else if new_val_cap < old_val_cap {
                self.inner.release(old_val_cap - new_val_cap);
            }
            state.map.insert(key, value);
        } else {
            // Genuinely new key: charge the key allocation and the value. Charge
            // the fixed per-entry overhead only for the portion of the new live
            // count that exceeds the high-water mark, so re-inserting within the
            // prior peak (after removals) adds no overhead — the bucket slot it
            // reuses was already charged and is still resident.
            let new_len = state.map.len() + 1;
            let overhead_delta = new_len
                .saturating_sub(state.peak_entries)
                .saturating_mul(SCRATCH_ENTRY_OVERHEAD);
            // Reserve before inserting or bumping the peak, so a quota failure
            // returns without any untracked map growth.
            self.inner
                .reserve(key.capacity() + new_val_cap + overhead_delta)?;
            state.map.insert(key, value);
            if new_len > state.peak_entries {
                state.peak_entries = new_len;
            }
        }
        drop(scratch);
        Ok(())
    }

    /// Fetch a clone of the scratch value for `key`, if present.
    ///
    /// # Panics
    ///
    /// Panics if the tenant cell's scratch lock is poisoned.
    #[must_use]
    pub fn scratch_get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner
            .scratch
            .lock()
            .expect("tenant cell scratch lock poisoned")
            .map
            .get(key)
            .cloned()
    }

    /// Remove the scratch value for `key`, releasing its bytes.
    ///
    /// Releases the stored `String` key and the value `Vec`'s full allocation
    /// *capacity* (the bytes the cell owned). It does **not** release the fixed
    /// [`SCRATCH_ENTRY_OVERHEAD`](Self::scratch_entry_overhead): [`HashMap`] does
    /// not shrink its bucket array
    /// on removal, so the bucket slot this entry occupied stays resident and is
    /// charged against the entry high-water mark until the whole cell is dropped
    /// (see [`scratch_insert`](Self::scratch_insert)).
    ///
    /// # Panics
    ///
    /// Panics if the tenant cell's scratch lock is poisoned.
    #[must_use = "the removed scratch value is returned; bind it or `let _ =` it"]
    pub fn scratch_remove(&self, key: &str) -> Option<Vec<u8>> {
        // `remove_entry` recovers the stored key too, so its allocation is
        // released alongside the value's. The per-entry overhead is retained:
        // the bucket slot survives the removal, and `peak_entries` is not
        // lowered.
        let (removed_key, removed_val) = {
            let mut scratch = self
                .inner
                .scratch
                .lock()
                .expect("tenant cell scratch lock poisoned");
            scratch.map.remove_entry(key)
        }?;
        self.inner
            .release(removed_key.capacity() + removed_val.capacity());
        Some(removed_val)
    }
}

/// A process-wide registry of [`TenantCell`]s keyed by tenant id.
///
/// Stored in [`crate::AppState`]'s extension map, so every clone of the app
/// state shares one registry (and therefore one set of cells) for the process
/// lifetime.
#[derive(Clone)]
pub struct TenantCellRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    cells: RwLock<HashMap<String, Arc<TenantCell>>>,
    global_tracked: Arc<AtomicUsize>,
    /// Maximum number of resident cells; least-recently-used cells are evicted
    /// once the count exceeds this. `0` disables the bound (unlimited).
    max_cells: usize,
    /// Evict a cell whose last access is older than this. `None` disables
    /// idle-TTL eviction.
    idle_ttl: Option<Duration>,
    /// Monotonic clock origin for the registry-relative millisecond timestamps
    /// stored on each cell's `last_access`.
    base: Instant,
    /// Globally-monotonic access counter. Every lookup/insert draws a fresh,
    /// strictly-greater value via [`next_seq`](Self::next_seq) and stamps it on
    /// the touched cell's `last_access_seq`, giving LRU eviction a unique,
    /// tie-free ordering so the just-inserted cell is never the victim.
    seq: AtomicU64,
}

impl fmt::Debug for TenantCellRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantCellRegistry")
            .field("cells", &self.len())
            .field("total_tracked_bytes", &self.total_tracked_bytes())
            .finish()
    }
}

impl Default for TenantCellRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TenantCellRegistry {
    /// Create an empty registry with eviction disabled (unlimited resident
    /// cells, no idle-TTL sweep).
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(0, None)
    }

    /// Create an empty registry that evicts cells to stay within the given
    /// limits: at most `max_cells` resident cells (`0` = unbounded, evicting the
    /// least-recently-used above the bound), and evicting any cell idle for
    /// longer than `idle_ttl` (`None` = no idle sweep).
    #[must_use]
    pub fn with_limits(max_cells: usize, idle_ttl: Option<Duration>) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                cells: RwLock::new(HashMap::new()),
                global_tracked: Arc::new(AtomicUsize::new(0)),
                max_cells,
                idle_ttl,
                base: Instant::now(),
                seq: AtomicU64::new(0),
            }),
        }
    }

    /// Registry-relative "now" in milliseconds since `base`.
    fn now_millis(&self) -> u64 {
        u64::try_from(self.inner.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Draw the next globally-monotonic access sequence number. Each call
    /// returns a strictly-greater value, so the most recent access always has
    /// the greatest sequence and LRU victim selection is tie-free.
    fn next_seq(&self) -> u64 {
        self.inner.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Fetch the cell for `tenant_id`, if one is resident.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    // The read guard is deliberately held across the `touch` below: refreshing
    // the access time under the lock is what closes the eviction race, so we
    // opt out of the drop-tightening lint that would shrink the guard's scope.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get(&self, tenant_id: &str) -> Option<Arc<TenantCell>> {
        let guard = self
            .inner
            .cells
            .read()
            .expect("tenant cell registry lock poisoned");
        // Refresh the access time/sequence WHILE the read guard is still held,
        // so a concurrent writer cannot evict this cell in the gap between the
        // lookup and the touch. Capture `now`/`seq` here, under the guard and
        // immediately before the touch, so a stale timestamp taken before a
        // lock wait can never stamp the cell. `touch` is relaxed atomic
        // `fetch_max` stores through the shared `&Arc<TenantCell>`, safe under
        // the read lock.
        if let Some(cell) = guard.get(tenant_id) {
            cell.touch(self.now_millis(), self.next_seq());
            return Some(Arc::clone(cell));
        }
        None
    }

    /// Fetch the cell for `tenant_id`, creating it with `quota_bytes` if absent.
    /// Atomic: concurrent first requests for the same tenant share one cell.
    ///
    /// A *resident* cell refreshes its soft quota from `quota_bytes` on every
    /// call, so the latest configured value is applied without evicting and
    /// rebuilding the cell. In practice this only changes anything once a
    /// config-reload path swaps the resident [`crate::config::AutumnConfig`]
    /// (the middleware passes `config.tenancy.quota_bytes` here); no such
    /// hot-reload path exists today, so the refresh is currently a no-op — the
    /// mechanism is in place for when hot-reload lands.
    ///
    /// Every call also stamps the cell's last-access time and, on a miss, may
    /// evict idle or least-recently-used cells to honor the registry's limits.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn get_or_create(&self, tenant_id: &str, quota_bytes: usize) -> Arc<TenantCell> {
        // Fast path: a resident cell just needs its access time and quota
        // refreshed under the cheaper read lock. Do the touch/quota-refresh
        // WHILE the read guard is still held so a concurrent writer cannot evict
        // this cell in the gap between the lookup and the refresh — `touch` and
        // `set_quota_bytes` are relaxed atomic stores, safe under the read lock.
        {
            let guard = self
                .inner
                .cells
                .read()
                .expect("tenant cell registry lock poisoned");
            if let Some(cell) = guard.get(tenant_id) {
                // Capture `now` here, under the guard and immediately before the
                // touch, so a timestamp taken before a lock wait can never stamp
                // a fresh access time as stale.
                cell.touch(self.now_millis(), self.next_seq());
                cell.set_quota_bytes(quota_bytes);
                return Arc::clone(cell);
            }
        }
        let mut cells = self
            .inner
            .cells
            .write()
            .expect("tenant cell registry lock poisoned");
        // Another writer may have inserted between dropping the read lock and
        // taking the write lock.
        if let Some(cell) = cells.get(tenant_id) {
            let cell = Arc::clone(cell);
            // Fresh `now` under the write guard, right before the touch.
            cell.touch(self.now_millis(), self.next_seq());
            cell.set_quota_bytes(quota_bytes);
            return cell;
        }
        let cell = Arc::new(TenantCell::new(
            tenant_id.to_string(),
            quota_bytes,
            Arc::clone(&self.inner.global_tracked),
        ));
        // Capture `now` under the write guard, immediately before stamping the
        // new cell and sweeping, so the age comparison in `enforce_limits_locked`
        // uses a fresh wall-clock reading rather than one taken before the lock
        // wait.
        let now = self.now_millis();
        // Stamp the new cell's access sequence BEFORE enforcing limits, so it
        // holds the greatest sequence and is never chosen as the LRU victim.
        cell.touch(now, self.next_seq());
        cells.insert(tenant_id.to_string(), Arc::clone(&cell));
        // Enforce eviction limits while we already hold the write lock. The
        // just-touched new cell has the newest access time and the greatest
        // access sequence, so it is never the idle or LRU victim.
        self.enforce_limits_locked(&mut cells, now);
        drop(cells);
        cell
    }

    /// Evict idle and over-capacity cells from an already write-locked map.
    ///
    /// Must be called while holding the `cells` write lock; it never re-locks,
    /// so it is safe to invoke from inside [`get_or_create`]'s miss branch.
    /// Removal is a plain `HashMap::remove`, which drops only the registry's
    /// strong reference — any outstanding `Arc<TenantCell>` (e.g. one held by an
    /// in-flight request) stays valid and reclaims deterministically on drop.
    fn enforce_limits_locked(&self, cells: &mut HashMap<String, Arc<TenantCell>>, now: u64) {
        if let Some(ttl) = self.inner.idle_ttl {
            let ttl_millis = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
            cells.retain(|_, cell| now.saturating_sub(cell.last_access_millis()) <= ttl_millis);
        }
        let max_cells = self.inner.max_cells;
        if max_cells > 0 {
            while cells.len() > max_cells {
                // Evict the least-recently-used cell, chosen by the smallest
                // access *sequence*. The sequence is globally unique and
                // monotonic, so there are no ties (unlike millisecond
                // timestamps) and the just-inserted cell — which drew the
                // greatest sequence above — is never the victim.
                let Some(victim) = cells
                    .iter()
                    .min_by_key(|(_, cell)| cell.last_access_seq())
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                cells.remove(&victim);
            }
        }
    }

    /// Evict `tenant_id`'s cell, removing it from the registry and returning it.
    /// When the returned handle (and any outstanding request references) drop,
    /// the cell's owned memory is deterministically reclaimed.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use = "the evicted cell is returned so it (and its memory) can be dropped"]
    pub fn evict(&self, tenant_id: &str) -> Option<Arc<TenantCell>> {
        self.inner
            .cells
            .write()
            .expect("tenant cell registry lock poisoned")
            .remove(tenant_id)
    }

    /// Evict every resident cell whose most recent access is older than `ttl`
    /// (measured against the registry's current clock), returning the number of
    /// cells removed. A reusable ops/test primitive that applies the same
    /// idle-sweep policy `get_or_create` runs automatically.
    ///
    /// Removal drops only the registry's strong reference; any outstanding
    /// `Arc<TenantCell>` keeps its cell alive and reclaims deterministically on
    /// drop.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn evict_idle_older_than(&self, ttl: Duration) -> usize {
        let ttl_millis = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        let mut cells = self
            .inner
            .cells
            .write()
            .expect("tenant cell registry lock poisoned");
        // Read the clock under the write guard, so a `now` captured before a
        // lock wait cannot age out a cell another thread just touched.
        let now = self.now_millis();
        let before = cells.len();
        cells.retain(|_, cell| now.saturating_sub(cell.last_access_millis()) <= ttl_millis);
        before - cells.len()
    }

    /// The configured maximum number of resident cells (`0` = unbounded).
    #[must_use]
    pub fn max_cells(&self) -> usize {
        self.inner.max_cells
    }

    /// The configured idle-eviction TTL (`None` = disabled).
    #[must_use]
    pub fn idle_ttl(&self) -> Option<Duration> {
        self.inner.idle_ttl
    }

    /// Number of resident cells.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .cells
            .read()
            .expect("tenant cell registry lock poisoned")
            .len()
    }

    /// Whether the registry holds no cells.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes tracked across every resident cell.
    #[must_use]
    pub fn total_tracked_bytes(&self) -> usize {
        self.inner.global_tracked.load(Ordering::Relaxed)
    }
}

/// A lazily-materializing reference to a tenant's cell.
///
/// Binding a handle does NOT create a registry entry; the cell is created on
/// first access, so requests that never touch tenant memory leave the registry
/// untouched. The handle is cheap to clone (a registry `Arc` plus the tenant id
/// and its quota), which lets the tenancy middleware scope it into a task-local
/// without eagerly allocating a cell for every protected request.
#[derive(Clone)]
pub struct TenantCellHandle {
    registry: TenantCellRegistry,
    tenant_id: String,
    quota_bytes: usize,
    /// Per-request cache of the first materialized cell. Wrapped in an `Arc` so
    /// every clone of the same handle (the task-local copy and the copy held by
    /// the streaming body) shares one cache; the middleware builds a fresh
    /// handle per request, so the cache is scoped to a single request.
    cached: Arc<std::sync::OnceLock<Arc<TenantCell>>>,
}

impl fmt::Debug for TenantCellHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The registry is a process-wide shared handle; summarise it by size
        // rather than recursing into every resident cell.
        f.debug_struct("TenantCellHandle")
            .field("tenant_id", &self.tenant_id)
            .field("quota_bytes", &self.quota_bytes)
            .field("registry_cells", &self.registry.len())
            .field("materialized", &self.cached.get().is_some())
            .finish()
    }
}

impl TenantCellHandle {
    /// Create a handle for `tenant_id` backed by `registry`, with the soft
    /// `quota_bytes` to apply if and when the cell is materialized. Building the
    /// handle does not touch the registry.
    #[must_use]
    pub fn new(registry: TenantCellRegistry, tenant_id: String, quota_bytes: usize) -> Self {
        Self {
            registry,
            tenant_id,
            quota_bytes,
            cached: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Materialize (get-or-create) the tenant's cell in the registry, creating
    /// the registry entry on first access, and cache it for the rest of the
    /// request.
    ///
    /// The first call does the registry `get_or_create`; every subsequent call
    /// on this handle (or any clone of it — the cache is a shared `Arc`) returns
    /// that same `Arc<TenantCell>`. So even if the tenant is evicted from the
    /// registry mid-request, an in-flight request keeps its cell alive and
    /// stable to completion instead of minting a fresh empty one. Laziness is
    /// preserved: nothing materializes until this is first called.
    #[must_use]
    pub fn cell(&self) -> Arc<TenantCell> {
        self.cached
            .get_or_init(|| {
                self.registry
                    .get_or_create(&self.tenant_id, self.quota_bytes)
            })
            .clone()
    }

    /// The tenant id this handle resolves to.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

tokio::task_local! {
    /// A lazily-materializing [`TenantCellHandle`] for the current request, if
    /// tenancy is enabled and a registry is present. Binding the handle does not
    /// create a cell; the cell is materialized on first access via
    /// [`current_tenant_cell`]. Mirrors [`crate::tenancy::CURRENT_TENANT`].
    pub static CURRENT_TENANT_CELL: Option<TenantCellHandle>;
}

/// Returns the current request's [`TenantCell`], creating it in the registry on
/// first access (lazy). Returns `None` if tenancy is disabled or no handle is
/// bound to the current task.
#[must_use]
pub fn current_tenant_cell() -> Option<Arc<TenantCell>> {
    CURRENT_TENANT_CELL
        .try_with(|h| h.as_ref().map(TenantCellHandle::cell))
        .ok()
        .flatten()
}

/// Return the current request's supported cooperative scratch-allocation
/// region. Unlike a bare heap allocation, values returned by this region own
/// their quota charge for their entire lifetime.
#[must_use]
pub fn current_tenant_arena() -> Option<TenantArena> {
    current_tenant_cell().map(|cell| cell.arena())
}
