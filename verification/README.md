# Verus specifications

This directory contains small mathematical shadows of critical runtime state.
They are intentionally separate from the Cargo workspace because Verus uses an
extended Rust dialect. Verify the tenant arena spine with:

```sh
verus verification/tenant_arena.rs
```

Verify the lease-lock fencing model (issue #3053, ADR 0015) with the command
below. It proves that tokens are unique and strictly increasing per lock name,
and that a stale write is rejected. It does not prove that holders never
overlap; the fencing token makes overlap safe.

```sh
verus verification/lease_fencing.rs
```

The runtime correspondence and boundary are recorded in ADR 0012; executable
tests remain authoritative for unmodeled allocator, HTTP, and concurrency glue.
