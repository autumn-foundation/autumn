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

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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

/// Shared state owned by a single tenant's cell.
#[derive(Debug)]
struct TenantCellInner {
    tenant_id: String,
    /// Soft quota in bytes; `0` means unlimited.
    quota_bytes: usize,
    /// Bytes currently tracked for this tenant.
    tracked_bytes: AtomicUsize,
    /// Owned per-tenant scratch buffer. Dropped with the cell.
    scratch: Mutex<HashMap<String, Vec<u8>>>,
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
                scratch: Mutex::new(HashMap::new()),
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
    /// Replacing a key reserves at most `new_len - old_len` bytes (releasing the
    /// difference when the value shrinks), so a same-size or shrinking replace
    /// can never transiently overshoot the quota and spuriously fail.
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
        let new_len = value.len();
        let mut scratch = self
            .inner
            .scratch
            .lock()
            .expect("tenant cell scratch lock poisoned");
        let old_len = scratch.get(&key).map_or(0, Vec::len);
        if new_len > old_len {
            self.inner.reserve(new_len - old_len)?;
        } else if new_len < old_len {
            self.inner.release(old_len - new_len);
        }
        scratch.insert(key, value);
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
            .get(key)
            .cloned()
    }

    /// Remove the scratch value for `key`, releasing its bytes.
    ///
    /// # Panics
    ///
    /// Panics if the tenant cell's scratch lock is poisoned.
    #[must_use = "the removed scratch value is returned; bind it or `let _ =` it"]
    pub fn scratch_remove(&self, key: &str) -> Option<Vec<u8>> {
        let removed = {
            let mut scratch = self
                .inner
                .scratch
                .lock()
                .expect("tenant cell scratch lock poisoned");
            scratch.remove(key)
        };
        let removed = removed?;
        self.inner.release(removed.len());
        Some(removed)
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

tokio::task_local! {
    /// The [`TenantCell`] bound to the current request, if tenancy is enabled and
    /// a registry is present. Mirrors [`crate::tenancy::CURRENT_TENANT`].
    pub static CURRENT_TENANT_CELL: Option<Arc<TenantCell>>;
}

/// Returns the [`TenantCell`] bound to the current request, if any.
#[must_use]
pub fn current_tenant_cell() -> Option<Arc<TenantCell>> {
    CURRENT_TENANT_CELL.try_with(Clone::clone).ok().flatten()
}
