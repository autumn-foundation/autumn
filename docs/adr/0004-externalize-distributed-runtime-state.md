# ADR 0004: Externalize Distributed Runtime State

- Status: Accepted (2026-10-07; durable async state elaborated by ADR-0016)
- Date: 2026-04-09
- Deciders: Autumn maintainers
- Tags: state, sessions, distributed-systems, runtime

## Context

Autumn is currently generous with process-local state:

- sessions default to in-memory storage
- scheduled tasks are process-local
- distributed examples rely on application-specific glue for shared state

That is fine for local development and single-instance deployments. It is not a
safe foundation for horizontally scaled services.

The first place this bites is session state. The current default store works
only because a single process happens to remember things. The second bite is
distributed coordination, where application code currently bolts on advisory
locks and `OnceLock`-based side state to get multi-replica behavior.

Autumn needs a clearer model for what kinds of state may remain local and what
must move out of process memory.

## Decision

Autumn will classify runtime state into explicit categories and externalize the
categories that require cross-replica consistency.

## State Categories

### 1. Request-Local State

Examples:

- request extensions
- spans
- extracted auth context

This remains process-local and ephemeral.

### 2. Replica-Local Ephemeral State

Examples:

- in-memory caches used only as opportunistic accelerators
- dev-only session stores
- temporary hot-path memoization

This is allowed, but must be documented as replica-local and not assumed to be
authoritative.

### 3. Shared Mutable Runtime State

Examples:

- sessions
- rate-limit counters
- distributed cache entries
- lease ownership

This must use pluggable external backends in production-safe deployments.

### 4. Durable Async State

Examples:

- queued work
- retries
- workflow history
- timers and dead letters

This belongs to Harvest and its durable storage model, not to ad-hoc in-process
schedulers.

## First Externalization Target

The first mandatory externalization target is session state.

Autumn will:

- keep `MemoryStore` for local development and tests
- add a first production session backend backed by Redis
- make backend choice explicit in configuration
- make production-safe behavior explicit instead of silently using in-memory
  sessions everywhere

## Coordination Strategy

Autumn will treat distributed coordination as a framework concern when it
materially affects correctness.

Near-term coordination may use Postgres-backed leases where that keeps the
system simple and aligns with existing application examples.

Autumn will not pretend a process-local scheduler becomes distributed merely
because multiple replicas exist.

## Consequences

### Positive

- Autumn stops encouraging unsafe default behavior for scaled deployments
- Session continuity becomes compatible with multiple replicas
- Runtime boundaries become easier to explain
- Harvest gets a clearer role as the durable async engine

### Negative

- More configuration and optional dependencies
- More integration tests are required
- Some users will need migration guidance away from implicit in-memory state

### Risks

- Making local development worse by over-externalizing everything
- Adding too many backends too early and diluting maintenance effort
- Hiding distributed consistency trade-offs behind abstractions that look safer
  than they are

## Alternatives Considered

### 1. Keep process-local memory as the universal default

Rejected because it encourages broken assumptions in multi-replica deployments.

### 2. Remove all in-memory implementations

Rejected because that would poison local development and testing ergonomics.

### 3. Solve distributed state only in examples

Rejected because examples are not a reliable framework contract and lead to
copy-pasted folklore.

## Non-Goals

- Supporting every possible cache or session backend in Phase 1
- Replacing Harvest with a generic state abstraction
- Making every local cache magically consistent across replicas

## Follow-On Work

- Add session backend config and Redis-backed session storage
- Add production-safety warnings or guards for in-memory session usage
- Document the boundary between local `#[scheduled]` work and durable Harvest
  workloads (done: ADR-0016 and the jobs guide's "Pick the right tool" table)
