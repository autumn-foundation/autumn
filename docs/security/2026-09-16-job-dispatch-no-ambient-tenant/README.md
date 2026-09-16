# 2026-09-16 — Background job dispatch carries no ambient tenant (negative result)

## 🎯 Surface

`autumn_web::job` (`autumn/src/job.rs`, `autumn/src/scheduler.rs`,
`autumn-macros/src/job.rs`) × `autumn_web::tenancy::CURRENT_TENANT`
(`autumn/src/tenancy.rs`). Entry point investigated: an `#[job(...)]`
handler enqueued via `SomeJob::enqueue(...)` from inside a tenant-scoped
request, then executed through `TestApp::perform_enqueued_jobs()` — the
framework's own documented tool for asserting "the same handler the
[in-process worker] runtime would invoke."

## 🕵️ Threat model (hypothesis)

> Against an app that follows Autumn's documented job pattern
> (`docs/guide/jobs.md`: define a `#[job]` fn, register it with
> `AppBuilder::jobs(jobs![...])`, call `SomeJob::enqueue(args)` from a
> request handler) in a multi-tenant deployment (`[tenancy] enabled = true`,
> `docs/guide/tenant-cells.md`), an attacker who controls what gets enqueued
> as tenant A (e.g. by triggering any ordinary tenant-A action that enqueues
> a job) could get that job's side effects — and, critically, any
> `#[repository(tenant_scoped)]` read or write the job handler performs —
> misattributed to whatever tenant happens to be ambient when a *worker*
> later dispatches it, if the job runtime ever captured or propagated
> `CURRENT_TENANT` across the enqueue → execute boundary. The app author
> would have done nothing wrong: `enqueue()` is called from ordinary,
> tenancy-middleware-scoped request-handling code, exactly as the docs show.

This is ranked attack surface #3 ("Tenancy and sharding") in Warden's
charter, posed almost verbatim: *"does a job inherit the enqueuing request's
tenant or the executing worker's?"* It is also the exact defect shape the
2026-09-14 Keystone memo named across four other subsystems (idempotency,
`#[cached]`, rate-limiting, `plugin_sandbox` KV) — a derived-key or
ambient-context builder that was never revisited when tenancy landed. Jobs
were not one of the four Keystone audited.

## 🧪 Reproduction attempt → negative result

`grep -ni tenant autumn/src/job.rs autumn/src/scheduler.rs
autumn-macros/src/job.rs` returns **zero matches** in all three files: the
job runtime — definition, registration, enqueue, the in-process worker loop,
and the `#[scheduled]` sibling — never reads, sets, or threads
`CURRENT_TENANT` anywhere. That is consistent with either "safe by
omission" (no capture, so nothing to leak) or "silently broken" (no
capture, so nothing stops a stray ambient value from a reused thread/task
from being read instead) — only a live run through the real dispatch path
distinguishes the two.

Added `autumn/tests/integration/job_tenant_scope.rs`,
`job_dispatch_carries_no_ambient_tenant`:

1. Enables header-based tenancy (`x-tenant-id`) on a `TestApp`.
2. Registers a probe job (`tenant_leak_probe`) whose handler reads
   `CURRENT_TENANT.try_with(Clone::clone)` and records what it saw; it
   returns `Err` (naming the leaked value) if it observes *any* tenant, `Ok`
   only if it observes none.
3. POSTs to a route that calls `TenantLeakProbeJob::enqueue(...)` while the
   request is scoped to `tenant-a-sentinel`.
4. Runs `client.perform_enqueued_jobs().await.assert_all_succeeded()` — the
   framework's own sanctioned tool for driving a job through its real
   registered handler — and asserts the probe recorded "no tenant", not
   `tenant-a-sentinel`.

```
cargo test -p autumn-web --test integration_tests --features test-support \
  -- job_tenant_scope --nocapture
```

Result: **pass** — `CURRENT_TENANT` is `None` inside the job handler. See
`after.txt`.

## 🔎 Root cause of the fail-safe behavior

There is no code path to sweep, because there is no capture: `job.rs`'s
`enqueue`/`enqueue_in`/`enqueue_at`/`enqueue_*_after_commit` free functions
never read `crate::tenancy::CURRENT_TENANT` when recording a job, and
`perform_enqueued_jobs` (`autumn/src/test.rs:2994-3018`, mirroring the
in-process worker's own dispatch) calls `(handler)(state, payload).await`
directly with no `CURRENT_TENANT.scope(...)` wrapper. A tokio task-local's
scope is confined to the exact future tree it wraps; a job handler invoked
this way — or, in production, from the worker's own independent poll loop —
was simply never inside the enqueuing request's `CURRENT_TENANT.scope(...)`
future to begin with. There is nothing analogous to the `render_slot` /
`serve` dual-capture bug class this report's sibling
(`docs/security/2026-09-15-render-slot-kv-tenant-capture/`) investigated,
because jobs have no capture site at all, correct or otherwise.

The consequence for app authors: a `#[repository(tenant_scoped)]` derived
query called from inside a job handler with no tenant re-established fails
closed with `"no tenant context was established"` — proven already by
`autumn/tests/integration/tenancy.rs::test_unscoped_query_without_context_fails`
(the repository-macro half of this chain; unchanged and re-verified here,
not re-tested, since this report's job carries no repository call). Chained
with this report's proof that a job never inherits the enqueuing tenant,
the two tests together close the loop: **a job can never silently read or
write another tenant's `tenant_scoped` rows** — it can only error loudly, or
(the documented, correct pattern, per `PostPublicationArgs`-style job args
in `examples/reddit-clone/src/jobs.rs`) explicitly thread a tenant id
through its own args and re-establish scope itself via
`autumn_web::tenancy::with_tenant(id, ...)`.

## 🩹 Fix

None — no bug found. Test-only addition:
`autumn/tests/integration/job_tenant_scope.rs`, registered in
`autumn/tests/integration/mod.rs` (no feature gate needed — matches
`idempotency_tenant_scope.rs` and `rate_limit_tenant_scope.rs`, which also
need no `db`/Docker feature since the assertion never touches a database).

Confirmed the test is not vacuous: temporarily edited
`TestApp::perform_enqueued_jobs` (`autumn/src/test.rs`) to wrap the handler
invocation in `CURRENT_TENANT.scope(Some("fake-leaked-tenant".into()), ...)`
— simulating exactly the leak the hypothesis worried about — and reran the
same test. It failed immediately, on the exact assertion this report is
about, naming the injected value:

```
expected all performed jobs to succeed, but 1 failed:
  - tenant_leak_probe: AutumnError { status: 500, inner: StringError("job dispatch observed ambient tenant \"fake-leaked-tenant\"; CURRENT_TENANT leaked from the enqueuing request into job execution"), ... }
```

See `non-vacuous-check.txt`. The production code was restored immediately
after (verified `git diff -- autumn/src/test.rs` is empty); only the new
test file and the `mod.rs` registration are committed.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo clippy -p autumn-web --test integration_tests --features test-support -- -D warnings` — clean (no new warnings from the added file).
- `cargo test -p autumn-web --test integration_tests --features test-support -- job_tenant_scope --nocapture` — 1/1 pass (`after.txt`).
- `cargo test -p autumn-web --test integration_tests --features test-support -- job_tenant_scope job_recorder_integration --nocapture` — 14/14 pass, no collateral breakage in the sibling job-recorder suite (`full-suite-run.txt`).
- `./scripts/check-panic-gate.sh` — 35/35 self-tests pass, 81 request-path modules gated (unaffected by a test-only change).
- Non-vacuousness check above: the same test fails loudly, naming the leaked value, when a synthetic ambient-tenant leak is injected into the dispatch path (`non-vacuous-check.txt`).
- `./scripts/pre-push-check.sh` — the panic-gate and determinism-gate steps passed (35/35 and 20/20 self-tests); the plugin-surface step needed `git fetch --unshallow` first (this sandbox started from a shallow clone) and then passed clean; its final `cargo test --workspace --no-run` step was killed by the sandbox's own memory limit partway through an unrelated crate (`autumn-macros`, which this diff never touches) under this script's default parallelism — an environment constraint, not a compile error. Re-ran the equivalent compile check with reduced parallelism instead: `CARGO_BUILD_JOBS=2 cargo check --workspace --tests` completed clean, 0 errors, confirming every workspace member (including every test target) still compiles with this change.

## 📡 Blast radius

- Swept every acquisition site across the job/scheduling surface:
  `grep -ni tenant autumn/src/job.rs autumn/src/scheduler.rs
  autumn-macros/src/job.rs` — zero matches in all three, so there is no
  second capture site to check (unlike the `plugin_sandbox` render/serve
  pair, this subsystem has exactly one dispatch path, and it has no
  capture at all).
- `TestApp::perform_enqueued_jobs` documents that it mirrors "the same
  handler the runtime would invoke," and the in-process worker `TestApp`
  starts by default (per that method's own doc note) drains the same
  queue through the same handler table — so this proof extends to the real
  production dispatch path, not only the test helper.
- `#[scheduled]` (`autumn/src/scheduler.rs`) shares the same zero-tenant-
  reference finding; a recurring scheduled task has exactly the same "no
  ambient tenant, fails closed on a `tenant_scoped` repository call" shape
  as an ad-hoc job, consistent with `docs/guide/retention-sweeps.md`'s own
  explicit "sweeps are cross-tenant by design" statement for the
  `retention(...)` sweep task built on the same scheduler.
- Feature-independent: the job runtime's `local`/`postgres`/`redis`/`sqlite`
  backends (`docs/guide/jobs.md`'s backend table) all funnel through the
  same handler-table dispatch this test exercises; none of them thread a
  tenant id through the job envelope today, so none of them have a
  divergent capture to check.
- Did not find a fix to make, so no downstream-version blast radius: no
  released version behaves differently from what this report documents.

## 📜 Compatibility

No behavior change, no `CHANGELOG.md` entry (test-only addition, matching
this repo's convention for negative-result commits — see
`docs/security/2026-09-06-idempotency-token-principal/` and
`docs/security/2026-09-15-render-slot-kv-tenant-capture/`).

## 🗂 Ledger

This directory. `after.txt` has the full passing run, `non-vacuous-check.txt`
has the synthetic-leak run proving the test is load-bearing,
`full-suite-run.txt` has the sibling `job_recorder_integration` suite
running clean alongside it.
