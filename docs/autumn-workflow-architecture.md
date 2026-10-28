# Autumn Harvest

Autumn Harvest is the durable workflow engine for Autumn. It lives in its own
repository and release train:
[`autumn-foundation/autumn-harvest`](https://github.com/autumn-foundation/autumn-harvest).

This page used to hold the March 2026 design draft for Harvest. That draft
described Harvest's crates as new members of this workspace, which they never
became, and it drifted from what Harvest actually ships. Harvest's
architecture, roadmap and topology decisions are maintained in the Harvest
repository. This page is only a pointer.

## When to reach for Harvest

Use Autumn's built-in background work first:

- `#[scheduled]` for recurring work on a cron or fixed interval.
- `#[job]` for one unit of work, now or later (`enqueue`, `enqueue_in`,
  `enqueue_at`, `enqueue_in_tx`), retried from the top on failure. The durable
  backends deliver it at least once. See the [jobs guide](guide/jobs.md) for
  each backend's guarantee.

Reach for Harvest when the framework must remember *where inside the work* it
got to: multi-step workflows with per-step history, durable timers in the
middle of a workflow, signals, queries, child workflows, sagas with
compensation, or DAGs. Autumn core deliberately does not ship these.
[ADR-0016](adr/0016-durable-workflows-live-in-harvest.md) records that
decision and the boundary test.

## How it plugs in

Add `autumn-harvest-plugin` to an Autumn app and register `HarvestPlugin` on
the app builder. When an app write and a workflow start must commit together,
use the plugin's transactional outbox
(`autumn_harvest_plugin::outbox::enqueue_workflow_start_outbox`) on the same
connection as the app transaction, the way `enqueue_in_tx` works for jobs.

Harvest depends on `autumn-web`. `autumn-web`, its examples and its tests do
not depend on Harvest, so the two release independently.

## Where to read more

- [Harvest README](https://github.com/autumn-foundation/autumn-harvest#readme):
  quick start, plugin wiring, crate map.
- [Harvest architecture](https://github.com/autumn-foundation/autumn-harvest/blob/HEAD/docs/architecture.md)
- [Harvest ADRs](https://github.com/autumn-foundation/autumn-harvest/tree/HEAD/docs/adr)
- [ADR-0016: Durable workflows live in Harvest; jobs stay lightweight](adr/0016-durable-workflows-live-in-harvest.md)
