# ADR 0033: Role Capabilities

- Status: Accepted
- Date: 2026-11-03
- Deciders: Autumn maintainers
- Tags: config, roles, jobs, scheduler, deployment

## Context

Issue [#3542](https://github.com/autumn-foundation/autumn/issues/3542), part
of epic [#3540](https://github.com/autumn-foundation/autumn/issues/3540).

`ProcessRole::runs_workers()` was the gate for each background subsystem:
the `#[job]` workers, the `#[scheduled]` scheduler, the outbox relay, the
commit-hook workers, the SQLite fleet sweeper relay, the restore check and
the retention sweeps. Thus a worker tier pinned to one queue also ran the
scheduler and each relay. No tier could run the scheduler alone. Named roles
(#3543) need one switch for each subsystem.

## Options

1. **One `bool` field for each subsystem** in a `RoleCapabilities` struct,
   plus a `Capability` enum to name one field.
2. **A bitset** (`u16` or the `bitflags` crate).
3. **Keep `runs_workers()`** and add new predicates to `ProcessRole`.

## Decision

Option 1.

- `RoleCapabilities` has the fields `serves_http`, `job_workers`,
  `scheduler`, `outbox_relay`, `commit_hooks`, `fleet_relay`,
  `restore_check` and `retention`. `Capability::ALL` lists them in field
  order.
- The process resolves the set once at boot (`AutumnConfig::capabilities`)
  and stores it in `AppState`. Each framework gate reads its own field.
- `combined`, `web` and `worker` resolve to the same subsystems as before.
- Retention sweeps are scheduled tasks. The boot records their names. The
  `retention` field gates them, and `scheduler` gates all other tasks.
- `/actuator/info` shows the role and the set under `role`.
- `serves_http()` and `runs_workers()` stay on `ProcessRole`. A test fails
  when `autumn/src/app.rs` uses `runs_workers`.

Both types are `#[non_exhaustive]`, so a new subsystem can add a field.

## Consequences

- Named roles (#3543) change only the resolve step.
- A new framework subsystem must name its capability.
- Option 2 is smaller, but its JSON is harder to read. Option 3 makes the
  role enum grow with each subsystem.
