# Verus specifications

This directory contains small mathematical shadows of critical runtime state.
They are intentionally separate from the Cargo workspace because Verus uses an
extended Rust dialect. Verify the tenant arena spine with:

```sh
verus verification/tenant_arena.rs
```

The runtime correspondence and boundary are recorded in ADR 0012; executable
tests remain authoritative for unmodeled allocator, HTTP, and concurrency glue.

## Billing plan gate

`billing_gate.rs` models the plan gate selection in
`autumn-billing/src/gate.rs` (issue #3114). Verify it with:

```sh
verus verification/billing_gate.rs
```

The spec proves the selection contract:

- When no entitled view satisfies the rule, the gate denies.
- When at least one view satisfies the rule, the gate selects a view.
- The selected view satisfies the rule and comes first in gate order. Gate
  order is the latest `last_event_at` first, then the lowest `id`.

The model does not include plan lookup, status rules or the grace period.
The integration test `gate_matches_the_any_row_oracle_for_every_pair_of_rows`
covers those parts with the default config. The test
`require_breaks_an_event_time_tie_by_row_id` covers the tie on `id`.
