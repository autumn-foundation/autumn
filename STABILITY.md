# Autumn Stability Policy

This document is Autumn's commitment to its users: an explicit contract that
describes what is stable, what is not, and how we plan to evolve the
framework without destroying the applications that depend on it.

> **Status:** Autumn is pre-`1.0` (current release series: `0.x`). The
> guarantees below describe the policy that will become binding at the `1.0`
> release. The `0.x` releases follow the same policy *in spirit*, but Cargo
> treats every `0.x.y → 0.(x+1).0` bump as breaking, so we use those
> intermediate bumps to iterate toward the stable surface without pretending
> the contract is already final.

- [Versioning (SemVer)](#versioning-semver)
- [The Public API Surface](#the-public-api-surface)
- [What counts as a breaking change](#what-counts-as-a-breaking-change)
- [What does *not* count as a breaking change](#what-does-not-count-as-a-breaking-change)
- [Minimum Supported Rust Version (MSRV) policy](#minimum-supported-rust-version-msrv-policy)
- [Dependencies and re-exports](#dependencies-and-re-exports)
- [Feature flags](#feature-flags)
- [Deprecation process](#deprecation-process)
- [Migration guides](#migration-guides)
- [Pre-1.0 notes](#pre-10-notes)

## Versioning (SemVer)

Starting with `1.0.0`, Autumn follows
[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html) applied to
the public API defined in this document.

Given a version `MAJOR.MINOR.PATCH`:

- **MAJOR** (`1.0.0 → 2.0.0`) — we *may* make breaking changes to the public
  API. Every major release ships with a
  [migration guide](docs/migrations/) describing what changed and how to
  update.
- **MINOR** (`1.0.0 → 1.1.0`) — backwards-compatible feature additions. New
  modules, new methods, new configuration keys. Existing code that compiles
  against `1.0.0` will continue to compile against any `1.x.y`.
- **PATCH** (`1.0.0 → 1.0.1`) — backwards-compatible bug fixes, performance
  improvements, and documentation updates. No behavior changes other than
  fixing clearly incorrect behavior.

The concrete definition of "breaking" matches the Rust API guidelines and the
[Cargo SemVer compatibility reference](https://doc.rust-lang.org/cargo/reference/semver.html).

## The Public API Surface

**Stable (covered by SemVer):**

- All items reachable from `autumn_web` that are:
  - not marked `#[doc(hidden)]`,
  - not inside a module named `__private`, `internal`, or similar, and
  - not documented as "unstable", "experimental", or "subject to change".
- The procedural macros re-exported from `autumn-macros` (`#[get]`,
  `#[post]`, `#[put]`, `#[delete]`, `#[ws]`, `#[static_get]`,
  `#[autumn_web::main]`, `routes![]`, `static_routes![]`, `tasks![]`,
  `#[secured]`, `#[model]`, `#[repository]`, `#[service]`, `#[scheduled]`,
  `#[cached]`). Macro *input syntax* is part of the API contract; the
  generated code is not.
- The `AutumnConfig` TOML schema and all `AUTUMN_*` environment variables
  documented in [`autumn/src/config.rs`](autumn/src/config.rs).
- The HTTP surface mounted automatically by [`AppBuilder::run`][]:
  `/health`, `/live`, `/ready`, `/startup`, `/actuator/*`, `/static/**`.
  Their *paths* are stable. Response *shapes* for non-actuator endpoints
  are stable; actuator endpoint payloads follow the actuator docs.
- The CLI commands shipped by `autumn-cli` (`autumn new`, `autumn setup`,
  `autumn dev`, `autumn build`, `autumn migrate`) and their documented
  flags. This includes the `autumn new` starter flags — `--starter`,
  `--list-starters`, `--starter-ref`, and `--yes` — and the
  `autumn-starter.toml` manifest schema documented in
  [`docs/guide/starters.md`](docs/guide/starters.md), which built-in and
  community starters share. Adding new built-in starters or new optional
  manifest fields is additive (non-breaking); removing or renaming a manifest
  field, or changing the substitution tokens, is a breaking change.

**Not stable (explicitly excluded from SemVer):**

- Anything marked `#[doc(hidden)]`. These are implementation details that
  macros or other internals need to reach, but user code must not depend on
  them.
- Anything under `autumn_web::reexports`. The crates re-exported there
  (`axum`, `diesel`, `diesel_async`, `http`, `tokio`, `tokio_util`,
  `tracing`, `validator`, `chrono`) follow *their own* versioning. See
  [Dependencies and re-exports](#dependencies-and-re-exports).
- Exact error messages (`Display` output, log lines, rendered HTML error
  pages). Only *types* and *status codes* are stable.
- Exact generated HTML/JSON byte sequences. We guarantee semantic
  equivalence (e.g. the error body stays a `{ "error": { "status": ..,
  "message": .. } }` shape), not byte-for-byte identity.
- Internals of derive expansions. The *input* syntax of `#[model]`,
  `#[repository]`, etc. is stable; the generated struct/impl names, field
  ordering, or intermediate helper items are not. Treat the macro output
  as opaque.
- Anything marked in its rustdoc as **experimental**, **unstable**, or
  **preview**. Feature flags whose name starts with `unstable-` are always
  excluded.
- Debug output (`Debug` impls). Useful for logs, not parsable.
- Private modules (`pub(crate)`, `pub(super)`) and the `tests` modules.
- **The edge capsule lane (issue #1790).** The `autumn-edge` crate, the
  `#[edge]` / `edge_routes![]` macro surface, `AppBuilder::with_edge_kv`, the
  capsule **wire protocol** (`WIRE_VERSION`, its NDJSON frames) and the
  reference **host API** (`autumn_edge::host`) are experimental and may change
  in any release. The protocol carries a version field precisely so a host and
  an artifact built from different Autumn versions degrade to origin-serving
  instead of guessing.

When in doubt: if `cargo doc --no-deps` doesn't list it, it is not part of
the public API.

[`AppBuilder::run`]: https://docs.rs/autumn-web/latest/autumn_web/app/struct.AppBuilder.html

### The edge capsule's byte-identity claim

The edge lane promises that a request the capsule serves gets the same status,
the same body bytes and the same headers (after the documented
[projection](docs/guide/edge.md#byte-identity-what-is-actually-guaranteed)) as
the origin binary would give — **for the origin binary and the edge artifact of
the same build**. It is a statement about two compilations of one source tree
agreeing with each other, proven per build by the `edge-conformance` CI job.

It is explicitly *not* a promise across versions. Rendered bytes are already
excluded from SemVer above, and that exclusion applies to both lanes equally: a
release may change what a handler renders, so long as it changes the origin and
the capsule together. Rebuild both from the same tree and deploy them together.

## What counts as a breaking change

The following require a major version bump:

- Removing, renaming, or relocating a public item.
- Adding a required method to a public trait, or changing an existing
  signature. (Adding a *provided* method is allowed if it does not make an
  existing impl ambiguous.)
- Changing a function/method signature in a way that rejects previously
  accepted callers (adding a required parameter, tightening a bound,
  changing the return type).
- Adding, removing, or renaming a public struct field on a struct that is
  not `#[non_exhaustive]`.
- Adding a variant to a public enum that is not `#[non_exhaustive]`.
- Removing or renaming an enum variant, even on a `#[non_exhaustive]` enum.
- Removing a feature flag or changing what it enables in a non-additive
  way.
- Removing, renaming, or changing the meaning of an `AutumnConfig` key or
  `AUTUMN_*` environment variable.
- Changing the HTTP method or path of a built-in endpoint
  (e.g. moving `/health` to `/healthz` without aliasing).
- Bumping the MSRV outside the window described in the
  [MSRV policy](#minimum-supported-rust-version-msrv-policy).
- Bumping a major version of a re-exported dependency whose types appear in
  our public API (e.g. `axum::Router` leaking through
  `AppBuilder::router`). These bumps are called out in the migration guide
  of the corresponding major release.

## What does *not* count as a breaking change

- Adding a new public item (module, type, function, method, trait impl
  that does not create coherence conflicts).
- Adding a new variant to a `#[non_exhaustive]` enum.
- Adding a new field to a `#[non_exhaustive]` struct, or a struct whose
  construction is guarded by a constructor (e.g. a builder).
- Adding a new optional configuration key with a sensible default.
- Adding a new feature flag (opt-in).
- Performance improvements that do not change observable behavior.
- Bug fixes, even if they change the observable behavior of clearly
  incorrect previous output (e.g. returning a correct status code instead
  of an incorrect one). The CHANGELOG calls these out under **Fixed**.
- Internal refactors that leave the public API intact.
- Tightening `#[doc(hidden)]` items or removing them entirely.
- Changing log message wording or tracing span names.

## Minimum Supported Rust Version (MSRV) policy

- Autumn declares its MSRV in two places, which must agree:
  1. `rust-version` in [`Cargo.toml`](Cargo.toml).
  2. The `rust-*` badge in [`README.md`](README.md) and the Requirements
     section.
- CI runs a dedicated `MSRV` job (see
  [`.github/workflows/ci.yml`](.github/workflows/ci.yml)) that builds the
  workspace with the declared toolchain. A
  [`scripts/check-msrv.sh`](scripts/check-msrv.sh) check verifies that all
  `rust-version` declarations in the workspace match each other and match
  the MSRV in the CI matrix. If the numbers diverge, CI fails.
- **MSRV bumps are allowed in a MINOR release** once the new MSRV is at
  least 6 months old as a stable Rust release. This matches the policy of
  Tokio, Serde, and most of the Rust ecosystem: we get to use modern
  language features without each bump counting as a breaking change.
- MSRV bumps are always called out in the CHANGELOG under an **MSRV**
  heading for that release.
- A MAJOR release may set any MSRV; the new MSRV is documented in the
  migration guide.
- We never lower the MSRV in a patch release. We *may* raise it in a
  patch release only to fix a soundness or security issue that cannot be
  fixed otherwise; such bumps are vanishingly rare and documented
  explicitly.

## Dependencies and re-exports

Autumn is a framework, not a walled garden. We re-export core building
blocks (Axum, Diesel, Tokio, …) under `autumn_web::reexports` so that users
can opt into the full upstream API without adding a second dependency.

This means our stability is coupled to the upstream crates. Our policy:

- A major bump of a *leaf* dependency (something that does not appear in
  our public API) is a patch or minor release here, not a major bump.
- A major bump of a dependency whose types *do* appear in our public API
  (e.g. upgrading `axum` from `0.8` to `0.9`, or `diesel` from `2` to
  `3`) is a major release of Autumn. The migration guide documents the
  upstream changes users need to be aware of.
- We are explicit about which dependency versions a given Autumn release
  supports. See `[workspace.dependencies]` in
  [`Cargo.toml`](Cargo.toml).
- We do not promise that every compatible upstream patch release will be
  picked up immediately. We do promise to respond to upstream security
  advisories within a reasonable window (ideally within one patch
  release).

## Feature flags

Cargo feature flags are part of the public API:

- Removing a feature flag, or changing what it enables, is a breaking
  change.
- Adding a new feature flag is *not* a breaking change, provided it is
  additive.
- Features named `unstable-*` are explicitly excluded from the stability
  policy. Use them at your own risk.
- The `default` feature set is stable: removing a feature from `default`
  is a breaking change.

### Feature-combination CI gate

Every individual `autumn-web` feature flag is proven to compile in
isolation (with `--no-default-features`) in CI via a `cargo hack
--each-feature` sweep.  A curated set of representative real-world
combinations is also checked on every PR:

| Combination | Rationale |
|---|---|
| `--no-default-features` (no flags) | bare-minimum compile |
| each flag alone | isolation regression guard |
| `db` | db-only API server |
| `mail` | mail without the full default set |
| `storage,db` | file storage backed by database |
| `maud,htmx` | minimal web front-end |
| `telemetry-otlp` | standalone telemetry |

### Unsupported feature combinations (CI excluded)

The following features are **not** checked in CI because their build
requirements make them cost-prohibitive or unsuitable for standard
runners.  They remain available for users with the necessary environment:

| Feature | Reason excluded |
|---|---|
| `managed-pg` | downloads Postgres binaries on first build |
| `managed-pg-bundled` | embeds Postgres binaries (~150 MB) into the executable |
| `system-tests` | requires a headless Chromium browser (`chromiumoxide`) |
| `test-support` | dev-only; pulls `testcontainers` (Docker-dependent) |

## Deprecation process

We prefer a long deprecation ramp over abrupt removal:

1. An item is marked with `#[deprecated]` in a minor release and a
   replacement is documented.
2. The deprecation note stays for at least one full minor cycle, ideally
   longer.
3. The item is removed in the next *major* release.

Deprecations never change behavior — only signal intent.

### Deterministic clock/entropy seam (issue #1797)

`autumn_web::time` gained a monotonic counterpart to the existing wall-clock
seam, all of it additive:

- `time::MonotonicInstant` — an instant on a clock's own monotonic timeline,
  with `saturating_duration_since` / `saturating_add` / `checked_add`.
- `ClockSource::monotonic` — a **defaulted** trait method, so every existing
  `impl ClockSource` keeps compiling and keeps reading the real
  process-monotonic clock. A custom clock whose `now()` is virtual should
  override it.
- `Clock::monotonic` — the request-start instant, snapshotted with the
  extractor.
- `AppState::monotonic` — the live reading.
- `time::monotonic_now` — the real monotonic clock, for code with no
  `ClockSource` in scope.
- `DbState::clock` — a **defaulted** trait method returning the real system
  clock, so an existing `impl DbState` needs no change.

Deprecated in the same change: `scheduler::now_unix_secs` and
`scheduler::now_unix_duration`, superseded by
`time::clock_unix_secs(state.clock())` / `time::clock_unix_duration(state.clock())`.
They follow the ramp above — the warning lands in a minor release and removal is
a major-release event.

### Config-key deprecations

Config key deprecations (TOML schema and `AUTUMN_*` env vars) are tracked in
`DEPRECATED_CONFIG_KEYS` in `autumn/src/config.rs`. Each entry records the
dotted key path, the replacement key path, `since` (the minor version that
introduced the deprecation), and `remove_in` (the first major version that
removes it).

At startup `AutumnConfig::load_with_env` emits one structured `WARN` per
deprecated key that is found in the resolved config (TOML file or environment
variable). The old value is still honored during the deprecation window — only
the signal changes. Use `autumn doctor` to check for deprecated config keys
without starting the full application; the `deprecated_keys` check appears in
plain-text and `--json` output.

A CI guard (`autumn/tests/schema_drift_guard.rs`) enforces that any key
removed from the compiled schema has a corresponding entry in the registry.
Regenerate its snapshot after schema changes:

```
UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p autumn-web schema_keys_snapshot_guard
```

## Migration guides

**Every release with a breaking change ships a migration guide** under
[`docs/migrations/`](docs/migrations/) — pre-`1.0` that means most `0.x`
releases, not just majors. This is enforced, not merely promised:
`scripts/check-migration-guides.sh` fails CI when a `CHANGELOG.md` section
declares a breaking change without a matching `docs/migrations/<version>.md`,
or when a breaking entry does not link its guide (issue #1588). A release
without an upgrade path is treated as a broken build.

The guide is written against the
[migration guide template](docs/migrations/TEMPLATE.md) and covers:

1. The summary and scope of the breaking changes.
2. MSRV delta, if any.
3. A section per breaking change with *before* / *after* code snippets.
4. Compiler-error cheat sheet — "if you see this error, do that".
5. Dependency major bumps carried with the release.
6. Link to the CHANGELOG section for the release.
7. How to verify the upgrade landed, and the recorded guide-only upgrade
   walk-through of an app scaffolded on the previous release.

Draft guides are opened alongside the *first* breaking change of a cycle, as
[`docs/migrations/next.md`](docs/migrations/next.md), and grow with each
subsequent breaking-change PR; the draft is renamed to `<version>.md` at
release time, so the release ships with a complete guide on day one. See
[`docs/migrations/README.md`](docs/migrations/README.md) for the process and
the `**Breaking:**` changelog convention.

## CSV import/export (issue #808)

### SemVer impact

The CSV import/export surface introduced by issue #808 is **gated behind the
`csv` Cargo feature** (`autumn-web = { features = ["csv"] }`) for the first
minor release cycle.  Enabling an opt-in feature is non-breaking per the
feature-flag policy above; callers who do not enable `csv` are unaffected.

Once the feature graduates out of its initial cycle the `csv` feature will
remain (removing it would be a breaking change), but its content may be
stabilised into the `default` feature set.

### New public items (all `#[cfg(feature = "csv")]`)

| Item | Location | Notes |
|------|----------|-------|
| `autumn_web::data::csv::CsvSchema` | trait | Stable input API; generated expansion is not |
| `autumn_web::data::csv::ImportReport` | struct | `#[non_exhaustive]` for forward compat |
| `autumn_web::data::csv::ImportMode` | enum | `#[non_exhaustive]` |
| `autumn_web::data::csv::ImportOptions` | struct | |
| `autumn_web::data::csv::CsvRowError` | struct | |
| `autumn_web::data::csv::ImportRowResult` | enum | |
| `autumn_web::data::csv::export_csv` | free fn | |
| `autumn_web::data::csv::import_csv` | free fn | |

### Admin plugin additions (`autumn-admin-plugin`)

Two new **provided methods** on `AdminModel` (non-breaking per the trait
evolution policy):

- `fn supports_csv_export(&self) -> bool` — defaults to `true`
- `fn csv_export_columns(&self) -> Vec<&'static str>` — defaults to non-hidden, non-password fields
- `fn csv_export_row(&self, columns: &[&str], record: &Value) -> Vec<String>`
- `fn supports_csv_import(&self) -> bool` — defaults to `false`
- `fn import_csv_row(…) -> AdminFuture<AdminImportRowResult>` — defaults to `Skipped`

Two new HTTP routes (non-breaking; additive):

- `GET /admin/{slug}/export.csv`
- `GET /admin/{slug}/import` (import form)
- `POST /admin/{slug}/import` (multipart upload)

### CLI additions (`autumn-cli`)

New `autumn data` subcommand (non-breaking; additive):

- `autumn data export <model> [--out <file>] [--where <expr>]`
- `autumn data import <model> --in <file> [--dry-run] [--upsert-by <col>]`

### PII redaction strategy

Override `csv_export_columns` to omit sensitive column names from the header
row, or override `csv_export_row` to return `"[REDACTED]"` for a column's
value while keeping the column in the header.  Fields declared with
`AdminFieldKind::Password` are **always excluded** from the default column
list; fields declared `AdminFieldKind::Hidden` are also excluded.

### Transactional batching strategy

`import_csv` processes rows one at a time via the caller-supplied `handler`
closure.  To batch within a database transaction, wrap the handler in a
transaction opened before the call and committed (or rolled back) after.
The `batch_size` knob in `ImportOptions` signals the intended chunk size to
the caller but does not enforce it — the framework does not hold a connection
open across the call.

### Custom column override

To add a computed column (e.g. a joined display value from a related table):

```rust
fn csv_export_columns(&self) -> Vec<&'static str> {
    vec!["id", "title", "author_name"]   // "author_name" is not a real DB column
}

fn csv_export_row(&self, columns: &[&str], record: &Value) -> Vec<String> {
    columns.iter().map(|col| match *col {
        "author_name" => lookup_author_display(record),
        _ => record.get(*col).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }).unwrap_or_default(),
    }).collect()
}
```

## Team membership (issue #1261)

### SemVer impact

This is additive: a new opt-in `autumn generate teams` CLI subcommand plus a
new `examples/teams` reference application. **No public API in the
`autumn`/`autumn-web` or `autumn-macros` crates changed** — every capability
`teams` uses already shipped and is already stable: `#[repository(...,
tenant_scoped)]` (issue #695), the session `"role"` key convention
(`#[secured("...")]`/`PolicyContext::has_role`, issue #496), and the Mail
stack's `#[mailer]`/`#[mailer_preview]`. No new Cargo feature gate was
needed for this, since nothing in the library crates' public API changed —
`autumn-cli` alone gained the new subcommand.

### New public items

| Item | Location | Notes |
|------|----------|-------|
| `autumn generate teams` | `autumn-cli` subcommand | No name argument — always emits the fixed `Organization`/`Membership`/`Invitation` set |
| `autumn destroy teams` | `autumn-cli` subcommand | Reverses a matching `generate teams` (issue #1048's destroy convention) |

No new public Rust API in `autumn-web`/`autumn-macros`: the generated
`src/teams/` module is ordinary, freely-editable application code composed
entirely from already-stable primitives, not a new library surface.

### What it generates

| File | Purpose |
|------|---------|
| `src/teams/models.rs` | `Organization`, `Membership`, `Invitation` `#[model]` structs |
| `src/teams/role.rs` | `Role` enum (`Owner`/`Admin`/`Member`), `require_role`, `establish_org_session` |
| `src/teams/repositories.rs` | `#[repository]` traits, `Membership`/`Invitation` `tenant_scoped` |
| `src/teams/mailers/invitation_mailer.rs` | `InvitationMailer` (`#[mailer]`) |
| `src/teams/routes/{organizations,invitations,members}.rs` | Route handlers, plus the `provision_default_organization` signup-integration helper |
| `migrations/<timestamp>_create_teams/` | `organizations`/`memberships`/`invitations` tables |
| `src/main.rs` (modified) | `mod teams;` + routes wired into `routes![...]` |
| `Cargo.toml` (modified) | `"mail"` feature enabled on `autumn-web` |

See `docs/generate-teams.md` for the two-line auth-integration seam this
generator relies on instead of generating its own login/signup.

## SSG manifest `Content-Type` (issue #1832)

### SemVer impact

**Breaking**, and deliberately taken pre-1.0 to close the hole permanently.
`static_gen::ManifestEntry` gained a public `content_type` field, and both it
and `static_gen::StaticManifest` became `#[non_exhaustive]` — so an existing
`ManifestEntry { file, revalidate }` or `StaticManifest { .. }` literal stops
compiling (E0063/E0639), as does an exhaustive destructuring pattern such as
`let ManifestEntry { file, revalidate } = entry;` (E0638). Sealing them is the
point: the manifest format is
expected to keep growing, and after this release a new field is additive rather
than breaking. Only code that reads or writes `dist/manifest.json` itself is
affected; ordinary `#[static_get]` applications are not. See
[`docs/migrations/next.md`](docs/migrations/next.md).

The JSON format itself is compatible in both directions. `content_type` is
`#[serde(default, skip_serializing_if = "Option::is_none")]`, so a new runtime
reads an old manifest (the field defaults to absent, and the pre-#1832
derivation runs unchanged), and an old runtime reads a new one (no
`deny_unknown_fields`, so the extra key is ignored).

`revalidate` gained `#[serde(default)]` — a hand-written entry may now omit it —
but deliberately **not** `skip_serializing_if`, so what `autumn build` writes
keeps the shape it has always had. An older Autumn runtime would read either
form (serde's derive maps a missing `Option` field to `None` even without
`#[serde(default)]`), so this is not a compatibility fix; it is a decision to
leave the generated format unchanged for anything else that reads
`dist/manifest.json` — a rollback, a rolling deploy sharing one `dist/` volume,
or external tooling with a stricter reader.

### New public items

| Item | Location | Notes |
|------|----------|-------|
| `ManifestEntry::new` / `with_revalidate` / `with_content_type` | `autumn_web::static_gen` | The construction path that survives future fields |
| `ManifestEntry::content_type` | `autumn_web::static_gen` | `Option<String>`; `None` means "nothing recorded", not "unknown type" |
| `StaticManifest::new` | `autumn_web::static_gen` | Stamps `generated_at` (Unix-epoch seconds as a decimal string) and `autumn_version` |
| `StaticManifest::with_generated_at` | `autumn_web::static_gen` | Pins the build timestamp `new` stamped, for reproducible builds |
| `StaticFileLayer::resolve_entry` → `ResolvedStatic` | `autumn_web::static_gen` | Returns the file path plus the ready-to-serve `Content-Type`; `resolve` remains the file-path-only shorthand |
| `resolved_content_type` | `autumn_web::static_gen` | The decision function, public so an app serving `dist/` itself can match Autumn's behaviour exactly |

`ResolvedStatic` is `#[non_exhaustive]` from the start, and has no public
constructor — it is a return type, not something downstream code builds.

`resolved_content_type` returns an `http::HeaderValue`, so `http` is now part of
`autumn-web`'s public API surface here; it is re-exported as
`autumn_web::reexports::http` (`autumn_web::http` is the HTTP *client* module).

## Pre-1.0 notes

Until Autumn reaches `1.0.0`:

- Every minor (`0.x.0 → 0.(x+1).0`) release *may* contain breaking
  changes. Cargo's SemVer rules already treat these as breaking, and we
  use them to iterate on the public surface before it is frozen.
- We still keep a CHANGELOG with **Breaking Changes** callouts for every
  `0.x` bump so users know what to look out for.
- The guarantees above (MSRV handling, non-exhaustive markers, re-export
  policy, feature flags) are already honored. The only difference is that
  the API itself is allowed to move.
- Reaching `1.0.0` is a decision, not a calendar event: we will declare
  1.0 when the surface described above has been stable across a couple of
  `0.x` cycles without user-facing churn.
