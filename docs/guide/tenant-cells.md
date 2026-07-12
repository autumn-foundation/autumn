# Per-Tenant Memory Cells

Row-level tenancy scopes a tenant's *rows*; per-tenant memory cells bound a
tenant's *in-process memory*. On top of the existing tenancy story, each
resolved tenant gets a `TenantCell` — a byte-accounting boundary with a soft
quota and an owned scratch buffer — created lazily on the first request that
touches tenant memory. Allocations that flow through the cell are tracked
against the tenant's quota; when the cell is evicted and dropped, Rust's
ownership rules deterministically reclaim its tracked footprint.

This is orthogonal to sharding. Sharding decides *which database* a tenant's
rows live on; cells decide *how much process memory* a single tenant may hold
before its own requests start failing. One noisy tenant can no longer allocate
its way into every other tenant's latency.

The whole cell is pure, safe Rust: it holds under the workspace-wide
`#![forbid(unsafe_code)]`. It is an accounting cell, not a bounding allocator —
see [The accounting guarantee](#the-accounting-guarantee) for exactly what that
buys you.

## Configuration

```toml
[tenancy]
enabled = true
quota_bytes = 1048576   # 1 MiB soft quota per tenant; 0 (the default) disables the quota
```

`quota_bytes` is the soft per-tenant quota applied to every cell the registry
mints. `0` — the default — disables the quota entirely: charges always succeed
and nothing is capped, while `tracked_bytes()` still accounts what flows
through the cell.

Environment overrides:

| Variable | Field |
|----------|-------|
| `AUTUMN_TENANCY__QUOTA_BYTES` | `tenancy.quota_bytes` |

Among the `[tenancy]` fields, the environment currently overrides only
`quota_bytes` (full tenancy-section env coverage is tracked in
[#1793](https://github.com/madmax983/autumn/issues/1793)).

## Using the cell in a handler

Reach for the current request's cell with `current_tenant_cell()`. It returns
`Some(Arc<TenantCell>)` when tenancy is enabled and a tenant is bound to the
request, and `None` otherwise (so a route that runs outside a tenant context
degrades gracefully). Calling it is what *materializes* the cell for the
request — routes that never call it create no cell.

`try_charge(n)` reserves `n` bytes against the quota and hands back a `Charge`
RAII guard. The bytes stay tracked for exactly as long as you hold the guard:
drop it (or let it fall off the end of the handler) and they are released
immediately. If the charge would exceed the quota it returns `QuotaExceeded`,
which converts into `AutumnError` as an HTTP **503 Service Unavailable** — so a
handler returning `AutumnResult` can just use `?`:

```rust
use autumn_web::prelude::*;
use autumn_web::tenant_cell::current_tenant_cell;

#[post("/reports")]
async fn build_report() -> AutumnResult<String> {
    // Account for a large working buffer we are about to build for this tenant.
    // Over-quota tenants get a 503 here via `?`; everyone else proceeds.
    let _charge = match current_tenant_cell() {
        Some(cell) => Some(cell.try_charge(512 * 1024)?),
        None => None, // tenancy disabled / no tenant bound: nothing to account
    };

    let report = expensive_render().await?;
    Ok(report)
    // `_charge` drops here, releasing the 512 KiB back to the cell and the
    // process-wide gauge.
}
```

The cell also owns a per-tenant **scratch buffer** — a keyed byte store whose
contents count against the same quota. Use it for state you want to keep
resident across a request (or hand between request phases) while staying
honest about its cost:

```rust
use autumn_web::prelude::*;
use autumn_web::tenant_cell::current_tenant_cell;

#[post("/session/scratch")]
async fn stash() -> AutumnResult<&'static str> {
    if let Some(cell) = current_tenant_cell() {
        // Charged: key capacity + value capacity + a fixed per-entry overhead.
        // Returns 503 (via `?`) if it would push the tenant over quota.
        cell.scratch_insert("draft", b"partial work".to_vec())?;

        if let Some(bytes) = cell.scratch_get("draft") {
            // ... use the stored bytes ...
            let _ = bytes;
        }

        // Removing frees only the key + value capacity back to the quota; the
        // fixed per-entry overhead is retained (against the entry high-water
        // mark) until the cell is evicted.
        let _ = cell.scratch_remove("draft");
    }
    Ok("ok")
}
```

## Quota isolation

A quota breach is scoped to the tenant that hit it. When a tenant is over
quota, only *its* over-budget request fails — with a 503 — and every other
tenant has its own independent counter and is completely unaffected: a whale
exhausting its cell degrades only its own traffic, not the process.

Where in the request that 503 lands depends on the API. `try_charge(n)?`
reserves *before* you allocate: the quota is checked up front, so an over-quota
tenant is rejected without ever building the buffer. `scratch_insert(key,
value)` instead takes an already-built `Vec`, so the value exists in memory
before the check — the quota rejects it before it is stored in and counted
against the cell, bounding what the cell *retains*, but it does not prevent the
caller's transient allocation. That is the same tracked-bytes, not-RSS boundary
as [The accounting guarantee](#the-accounting-guarantee) below: the cell bounds
the memory it owns, not every byte a handler touches on the way there.

## Eviction and lifecycle

Cells live in a process-wide `TenantCellRegistry` shared by every clone of the
app state. Two properties matter operationally:

- **Lazy creation.** Binding a request to a tenant does not allocate a cell;
  the first call to `current_tenant_cell()` (internally a registry
  `get_or_create`) materializes it. Routes that never touch tenant memory leave
  the registry untouched.
- **Deterministic eviction.** `TenantCellRegistry::evict(tenant_id)` removes
  the tenant's cell from the registry and returns it; once that handle and any
  outstanding request references drop, the cell's owned memory (scratch buffer
  and all) is reclaimed, and its tracked bytes leave the process-wide gauge.
  This is ordinary Rust `Drop`, not a background sweep — reclaim is immediate
  and predictable.

Eviction is safe to do mid-request. Each in-flight request caches the cell it
first materialized, so evicting a tenant while one of its requests is running
does **not** reset that request's state or hand it a fresh empty cell: the
running request keeps its stable cell to completion, and the eviction takes
effect for subsequent requests. The registry also exposes `len()`,
`is_empty()`, and `total_tracked_bytes()` for observability.

## The accounting guarantee

`tracked_bytes()` is a deterministic accounting of the allocations made
*through* the cell, and it covers exactly three things:

- each live `Charge`'s declared bytes,
- the allocation **capacity** of every stored scratch key `String` and value
  `Vec<u8>` (capacity, not length — a `Vec` with large spare capacity is
  charged for what it keeps resident), and
- a fixed per-entry overhead, exposed as
  `TenantCell::scratch_entry_overhead()`, charged against the **high-water mark**
  of live scratch entries (not the current count), so that a tenant storing many
  tiny entries cannot amplify its footprint past the cap via map growth. Because a
  `HashMap` never shrinks its bucket array on removal, this overhead is *retained*
  after a `scratch_remove` (which frees only the removed key and value capacity)
  and is reclaimed only when the cell is dropped on eviction. So `tracked_bytes()`
  can stay elevated after you insert then remove scratch entries, and re-inserting
  within the prior peak adds no new overhead (churn-safe).

Everything tracked is deterministically reclaimed when the cell is dropped on
eviction. What it is **not**: a measurement of the tenant's true process RSS.
Allocator-internal fragmentation, size-class rounding, and any allocation a
handler makes *outside* the cell's API (a bare `Box::new`, a `Vec` you build
and never charge) are invisible to the counter by design. Charge through the
cell for the memory you want bounded; the guarantee is that those tracked bytes
are counted honestly and released deterministically.

## Limitations and roadmap

- **Runtime-mutable quota** — the quota is fixed for a cell's lifetime today;
  changing it at runtime is deferred to
  [#1783](https://github.com/madmax983/autumn/issues/1783).
- **Bounded / idle registry eviction** — the registry does not yet evict idle
  tenants automatically; call `evict` yourself. Tracked in
  [#1792](https://github.com/madmax983/autumn/issues/1792).
- **Full tenancy-section env overrides** — only `quota_bytes` is currently
  overridable via the environment; the rest of `[tenancy]` is tracked in
  [#1793](https://github.com/madmax983/autumn/issues/1793).
