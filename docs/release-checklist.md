# Autumn Release Checklist

This document is the canonical pre-publish checklist for every Autumn release.
It records the crates we publish, the required publication order, the version
compatibility rule for each crate, and the automated gates that must pass before
a tag triggers a GitHub Release.

See also [`STABILITY.md`](../STABILITY.md) for the full stability policy and
SemVer contract.

---

## Published Crates

| Crate | Directory | Publish Order | Notes |
|---|---|---|---|
| `autumn-macros` | `autumn-macros/` | 1 | No Autumn runtime deps; must publish first. |
| `autumn-web` | `autumn/` | 2 | Depends on `autumn-macros`. |
| `autumn-cli` | `autumn-cli/` | 3 | Independent of `autumn-web` at crate level. |
| `autumn-admin-plugin` | `autumn-admin-plugin/` | 4 | Depends on `autumn-web`. |
| `autumn-storage-s3` | `autumn-storage-s3/` | 4 | Depends on `autumn-web`. |
| `autumn-cache-redis` | `autumn-cache-redis/` | 4 | Depends on `autumn-web`. |
| `autumn-search` | `autumn-search/` | 4 | Depends on `autumn-web`. |

All crates share a single workspace version (`[workspace.package].version` in
`Cargo.toml`). They are always released together at the same version.

### Version Compatibility Rules

- Every crate's `version` field inherits from `[workspace.package].version`.
- Crates that depend on other published Autumn crates pin the **exact workspace
  version** (e.g. `autumn-web = { version = "X.Y.Z", ... }`). A workspace
  version bump must update these pins in lockstep.
- The `[patch.crates-io]` override in the root `Cargo.toml` redirects
  `autumn-web` to the local workspace path during development. **Remove or
  comment this section** if you ever need to test against a published version
  locally.

---

## Autumn Harvest Compatibility Boundary

[Autumn Harvest](https://github.com/madmax983/autumn-harvest) is a companion
repository that provides starter templates, the scaffold generator registry, and
generated application CI. It is maintained on its own release train.

**Checks that belong in this repo:**

- Autumn framework crate packaging, docs.rs build, and SemVer gate.
- CLI commands shipped by `autumn-cli`.
- Generated application smoke test (see [Downstream Smoke Test](#downstream-smoke-test)).

**Checks that belong in the Harvest repo:**

- Template rendering correctness and starter project CI.
- Harvest-specific CLI flags and template version pins.
- Integration tests that use the Harvest template registry API.

When an Autumn release changes the generated-app contract (config schema,
generated file structure, CLI flags), open a companion PR in the Harvest repo
before tagging the Autumn release.

---

## Automated Gates (`publish-gate` Workflow)

The `.github/workflows/publish-gate.yml` workflow runs these jobs. Each must
pass before the release is announced.

### 1 · Crate Metadata (`metadata` job)

Script: `scripts/check-crate-metadata.sh`

Fails if any publishable crate is missing:

- `description`, `homepage`, `repository`, `readme`, `license`,
  `keywords`, `categories`, `rust-version`
- The `readme` file referenced in the manifest actually exists on disk.

### 2 · Package Dry-Run (`package` job)

Script: `scripts/check-publish-dry-run.sh`

Runs `cargo package -p <crate> --no-verify --allow-dirty` for every publishable
crate in dependency order. Fails if `cargo` cannot assemble the `.crate` archive
(missing files, bad manifest, workspace-path leakage, etc.).

This check does **not** upload anything to crates.io.

### 3 · Documentation Build (`docs` job)

Script: `scripts/check-docs.sh`

Builds the full workspace documentation with:

```text
RUSTDOCFLAGS="-D warnings -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links"
cargo doc --workspace --all-features --no-deps
```

Fails on any rustdoc warning or broken intra-doc link.

**docs.rs feature posture:** docs.rs builds each crate with the feature set
declared in `[package.metadata.docs.rs]` (if present), or with no extra features
otherwise. We use `--all-features` here to surface problems across the entire
feature matrix. If a feature is incompatible with docs.rs, add a
`[package.metadata.docs.rs]` section to that crate's `Cargo.toml` listing only
the features docs.rs should enable, and update `check-docs.sh` to build that
crate with the restricted set.

### 4 · SemVer Check (`semver` job)

Script: `scripts/check-semver.sh`

Uses [`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks)
to compare the public API surface of each publishable crate against the last
version published on crates.io.

- **Patch / minor releases:** any breaking change fails the gate.
- **Major releases (or breaking pre-1.0 minor):** failures are expected, and
  are cleared with the `skip_semver` `workflow_dispatch` input — not by the
  migration guide. `check-semver.sh` has no knowledge of `docs/migrations/`;
  the guide is enforced separately by the
  [Migration Guide Gate](#migration-guide-gate).

Crates that have never been published are skipped.

### 5 · Release Notes Alignment (`release-notes` job)

Scripts: `scripts/check-release-notes.sh`, `scripts/check-migration-guides.sh`

`check-release-notes.sh` fails if:

- The release tag version does not match `[workspace.package].version` in
  `Cargo.toml`.
- `CHANGELOG.md` has no entry for the current workspace version.
- The release contains breaking changes (a `### Breaking Changes` heading or an
  inline `**Breaking:**` marker in the CHANGELOG entry) but no migration guide
  exists at `docs/migrations/<version>.md`.

`check-migration-guides.sh` is the [Migration Guide
Gate](#migration-guide-gate) below. It also runs on every pull request in the
`lint` job of `ci.yml`, so a breaking change without a guide fails at review
time rather than at tag time.

### 6 · Downstream Smoke Test (`smoke` job)

Defined inline in `publish-gate.yml`.

Creates a temporary directory outside the workspace, generates a minimal Autumn
app skeleton, substitutes the candidate crate set (by path, simulating a crates.io
install), and verifies it compiles. This proves the published `autumn-web` is
usable from a fresh project without workspace path dependencies.

### 7 · Published Quickstart Gate (`quickstart-gate` workflow, post-publish)

Workflow: `.github/workflows/quickstart-gate.yml` · Script: `scripts/check-quickstart.sh`

Runs the README quickstart verbatim against the crates **published on
crates.io** — `cargo install autumn-cli`, `autumn new`, `autumn setup`, build,
serve, first 200 from `GET /`, then the README's scaffold path
(`autumn generate scaffold Post ...` → build → `autumn migrate` → `GET /posts`
responds) — and records the install→first-200 funnel time in the job summary.

Unlike gates 1–6, this one cannot run against the release candidate before
publication: it installs from crates.io, so it can only validate crates that
are actually there. It is therefore a **post-publish, pre-announce** gate:

- [ ] After `cargo publish` completes for the release candidate, trigger the
  `Quickstart Gate` workflow manually (Actions → Quickstart Gate → *Run
  workflow*) with the `cli-version` input set to the candidate version
  (e.g. `0.6.0`), or via the CLI:

  ```bash
  gh workflow run quickstart-gate.yml -f cli-version=X.Y.Z
  ```

- [ ] The dispatched run must be **green against the release candidate**
  before the release is announced. A red run is a release blocker for both
  `autumn-web` and `autumn-cli`: fix (or yank and re-publish), re-dispatch,
  and only announce once the gate passes.
- [ ] Confirm the README quickstart's pinned `cargo install autumn-cli
  --version` matches the version just published — the scheduled/push runs of
  the gate install exactly what the README says, so a stale pin turns the
  gate red for every new user.

The gate also runs on every push to `trunk-dev` and on a daily schedule. Those
runs validate the README against the *currently published* crates (never the
pushed code — the workspace `[patch.crates-io]` override means no other CI job
sees the published `autumn-web`), so a red push run means new users are broken
today, not that the commit is bad.

## Migration Guide Gate

Autumn ships every 2–4 weeks and, pre-1.0, most releases can break existing
apps. **A release with a breaking change does not go out without a migration
guide** — the guide is a gate, not a courtesy (issue #1588). See
[`docs/migrations/README.md`](migrations/README.md) for the process and the
`**Breaking:**` changelog convention.

### Automated

- [ ] `./scripts/check-migration-guides.sh` is green. It fails on an unmarked
  breaking changelog entry, a breaking section with no guide, a breaking entry
  that does not link its guide, and a guide missing a required section or an
  index entry.
- [ ] `./scripts/check-migration-guides.sh --list` shows the breaking-entry
  count for the release being cut, and it matches what you expect to ship.

### Rename the rolling draft

- [ ] `git mv docs/migrations/next.md docs/migrations/X.Y.Z.md`.
- [ ] Fill in the version placeholders in the renamed guide (*At a glance*,
  *Before you start*).
- [ ] Repoint every `docs/migrations/next.md` link in the release's `CHANGELOG.md`
  section to `docs/migrations/X.Y.Z.md`.
- [ ] **Recreate `docs/migrations/next.md`** from
  [`TEMPLATE.md`](migrations/TEMPLATE.md) (banner deleted) so the rolling draft
  always exists. [`docs/migrations/README.md`](migrations/README.md) and
  [`STABILITY.md`](../STABILITY.md) both link it by name, and nothing in this
  repo checks markdown links — a missing `next.md` 404s silently until the next
  breaking PR happens to recreate it.
- [ ] Update the index in [`docs/migrations/README.md`](migrations/README.md):
  add `X.Y.Z.md`, keep `next.md`.

### Guide-only upgrade walk-through (required before `cargo publish`)

The guide is only proven when someone who has not read the diff can follow it.
Perform this against the **previous** release and record the result — the
success metric is under 30 minutes.

- [ ] `cargo install autumn-cli --version <previous-version>`
- [ ] `autumn new upgrade-probe && cd upgrade-probe && autumn setup`
- [ ] Give the app something to break against: `autumn generate scaffold Post
  title:String body:Text published:bool`, `autumn migrate`, `cargo test`, and a
  `GET /posts` that responds. This is the green baseline.
- [ ] Upgrade to the release candidate **following only
  `docs/migrations/X.Y.Z.md`** — no changelog, no source reading, no asking the
  author. If you have to look outside the guide, that is a gap in the guide:
  fix the guide and restart from this step.
- [ ] `cargo check`, `cargo test`, and every step in the guide's *How to verify*
  section pass.
- [ ] Record the outcome in the guide's `### Guide-only upgrade walkthrough`
  section: status, from → to versions, elapsed minutes, and any gap the
  walk-through exposed. `check-migration-guides.sh` requires the `- **Status:**`
  line to be present; a release must not ship it as `pending`.

## Version Alignment

- [ ] `Cargo.toml` workspace `version` and `rust-version` match the README
  requirements and first-run docs.
- [ ] `autumn-web`, `autumn-cli`, and `autumn-macros` publish metadata point at
  the same repository, license, and release line.
- [ ] CHANGELOG entries call out any MSRV change.

## Automated Gates

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo test -p autumn-cli --test cli_tests repo_hygiene` — `repo_hygiene`
  is a module inside the consolidated `cli_tests` binary, not a test target of
  its own, so `--test repo_hygiene` errors with "no test target named".

## First-Run Docs Gate

- [ ] Run the `docs-smoke` procedure in
  [`docs/guide/docs-smoke.md`](guide/docs-smoke.md).
- [ ] Confirm the smoke uses the published `autumn-cli` install path and the
  published `autumn-web` dependency line, with no workspace patches.
- [ ] Treat any failure in the active first-run docs as a release blocker for
  both `autumn-web` and `autumn-cli`.
- [ ] If the smoke is temporarily run before crates.io publication, record the
  workspace-prepublish reason in release notes and rerun the published
  docs-smoke before announcing the release.

---

## Manual Pre-Tag Steps

Before pushing the release tag:

1. **Bump the workspace version** in `Cargo.toml` under `[workspace.package]`.
2. **Update internal version pins** for inter-crate dependencies
   (e.g. `autumn-web = { version = "X.Y.Z", path = "../autumn" }`).
3. **Update `CHANGELOG.md`** — move unreleased items under a `## [X.Y.Z]` heading.
   Every breaking entry carries the `**Breaking:**` marker (or sits under a
   `### Breaking Changes` heading) and links its migration guide.
4. **Complete the [Migration Guide Gate](#migration-guide-gate)** — rename
   `docs/migrations/next.md`, repoint the changelog links, and perform and
   record the guide-only upgrade walk-through.
5. **Run all gate scripts locally** to catch problems before CI sees the tag:
   ```bash
   ./scripts/check-crate-metadata.sh
   ./scripts/check-release-notes.sh
   ./scripts/check-migration-guides.sh
   ./scripts/check-docs.sh
   ./scripts/check-semver.sh   # requires network; skip offline
   ```
6. **Tag and push:**
   ```bash
   git tag v0.5.0
   git push origin v0.5.0
   ```
   The `publish-gate` workflow runs automatically. The `release` workflow runs
   only after `publish-gate` succeeds.
7. **Publish to crates.io** (in dependency order, after the gate passes):
   ```bash
   cargo publish -p autumn-macros
   cargo publish -p autumn-web
   cargo publish -p autumn-cli
   cargo publish -p autumn-admin-plugin
   cargo publish -p autumn-storage-s3
   cargo publish -p autumn-cache-redis
   cargo publish -p autumn-search
   ```
8. **Gate the published quickstart** (see
   [Published Quickstart Gate](#7--published-quickstart-gate-quickstart-gate-workflow-post-publish)):
   ```bash
   gh workflow run quickstart-gate.yml -f cli-version=X.Y.Z
   ```
   The dispatched run must be green before the release is announced.

> Publishing to crates.io is a manual step; no crates.io credentials are stored
> in CI. See the Out of Scope section in [issue #594](https://github.com/autumn-foundation/autumn/issues/594).
