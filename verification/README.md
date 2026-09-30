# Verus specifications

This directory contains small mathematical shadows of critical runtime state.
They are intentionally separate from the Cargo workspace because Verus uses an
extended Rust dialect. Verify the tenant arena spine with:

```sh
verus verification/tenant_arena.rs
```

The runtime correspondence and boundary are recorded in ADR 0012; executable
tests remain authoritative for unmodeled allocator, HTTP, and concurrency glue.
