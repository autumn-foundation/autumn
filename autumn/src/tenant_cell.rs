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
//! [`SCRATCH_ENTRY_OVERHEAD`] per scratch entry (covering the map's per-entry
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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
    /// Soft quota in bytes; `0` means unlimited.
    quota_bytes: usize,
    /// Bytes currently tracked for this tenant.
    tracked_bytes: AtomicUsize,
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
        if self.quota_bytes == 0 {
            self.tracked_bytes.fetch_add(n, Ordering::Relaxed);
            self.global_tracked.fetch_add(n, Ordering::Relaxed);
            return Ok(());
        }
        let mut current = self.tracked_bytes.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(n);
            if next > self.quota_bytes {
                return Err(QuotaExceeded {
                    tenant_id: self.tenant_id.clone(),
                    requested: n,
                    in_use: current,
                    quota: self.quota_bytes,
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
                quota_bytes,
                tracked_bytes: AtomicUsize::new(0),
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
        self.inner.quota_bytes
    }

    /// Bytes currently tracked for this tenant.
    #[must_use]
    pub fn tracked_bytes(&self) -> usize {
        self.inner.tracked_bytes.load(Ordering::Relaxed)
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
    /// released on removal). The fixed [`SCRATCH_ENTRY_OVERHEAD`] for the map
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
    /// [`SCRATCH_ENTRY_OVERHEAD`]: [`HashMap`] does not shrink its bucket array
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
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                cells: RwLock::new(HashMap::new()),
                global_tracked: Arc::new(AtomicUsize::new(0)),
            }),
        }
    }

    /// Fetch the cell for `tenant_id`, if one is resident.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn get(&self, tenant_id: &str) -> Option<Arc<TenantCell>> {
        self.inner
            .cells
            .read()
            .expect("tenant cell registry lock poisoned")
            .get(tenant_id)
            .cloned()
    }

    /// Fetch the cell for `tenant_id`, creating it with `quota_bytes` if absent.
    /// Atomic: concurrent first requests for the same tenant share one cell.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn get_or_create(&self, tenant_id: &str, quota_bytes: usize) -> Arc<TenantCell> {
        if let Some(cell) = self.get(tenant_id) {
            return cell;
        }
        let mut cells = self
            .inner
            .cells
            .write()
            .expect("tenant cell registry lock poisoned");
        if let Some(cell) = cells.get(tenant_id) {
            return Arc::clone(cell);
        }
        let cell = Arc::new(TenantCell::new(
            tenant_id.to_string(),
            quota_bytes,
            Arc::clone(&self.inner.global_tracked),
        ));
        cells.insert(tenant_id.to_string(), Arc::clone(&cell));
        cell
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
