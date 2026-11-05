# ADR 0038: Cache CI Builds per Compile With kache

- Status: Accepted
- Date: 2026-11-04
- Deciders: Autumn maintainers
- Tags: ci, build-time

## Context

Every Rust job in `.github/workflows/` restored and saved its `target/`
directory with `Swatinem/rust-cache`. That cache has two blind spots for this
workspace:

- It removes the workspace crates' artifacts before saving. Every job
  therefore recompiled autumn-web, the macro crates and every other workspace
  member, even on a run where none of them changed.
- It keys one whole target directory. A job that builds a different feature
  set restores only what that set has in common with the saved directory.

[kache](https://github.com/kunobi-ninja/kache) is a `RUSTC_WRAPPER` that
caches each compile by the hash of its inputs (sources, flags, rustc). A crate
whose inputs did not change is restored, workspace crate or not.

The trial ran in #3612 (run 41853082676). It timed `cargo test -p autumn-web
--lib --no-run` on `ubuntu-latest`, with a cold seed job per cache followed by
fresh runners restoring it. Each figure is the cache-action step plus the
build step.

| Case | rust-cache | kache |
| --- | --- | --- |
| Cold build, then save | 272 s | 288 s |
| Nothing changed | 162 s | 59 s |
| `autumn/src/lib.rs` edited | 156 s | 127 s |
| `markdown` feature set on the same cache | 149 s | 146 s |

Restoring the 1.9 GiB store took 9–13 s, close to rust-cache's 8–11 s. In the
"nothing changed" case, the remaining kache misses were build-script runs,
mostly aws-lc's C build, which the action only caches with `cache-c-cpp`. In
the trunk history at the time, about a quarter of commits changed no Rust or
Cargo file, and about half changed autumn-web or autumn-macros source.

## Decision

- Every workflow job that used `Swatinem/rust-cache` uses the local composite
  action `.github/actions/kache` instead. It is the one place the repo
  configures `kunobi-ninja/kache-action`: the pinned action commit, kache
  v2.0.0, `cache-c-cpp` (except on Windows), the Windows cache directory, and
  no PR comments.
- The primary store is an S3-compatible bucket (S3 or R2). Every job, branch
  and PR reads one content-addressed store, so a crate compiled anywhere is a
  hit everywhere with the same inputs, and there is no per-repository quota.
  - Only a run on a protected ref (`github.ref_protected`) writes: a push to
    `trunk` or `trunk-dev`, or a scheduled run. Its jobs get the read-write
    token, and the store grows with every push. `trunk-dev` must be a
    protected branch, or nothing is ever written.
  - Every other run, PRs included, gets a read-only token and passes
    `save-cache: false`, which sets `KACHE_REMOTE_READONLY` for the daemon.
  - A composite action cannot read `secrets` or `vars`, so each workflow maps
    them into its env as `AUTUMN_KACHE_S3_*`. The bucket, endpoint and region
    are repository variables or secrets; a variable wins when both are set.
    The two tokens are repository secrets. A run with the keys but no bucket
    falls back to the GitHub cache with a warning annotation.
  - `publish-gate.yml` does not map them and stays on the GitHub cache. Its
    jobs prepare releases, which should not depend on a store other runs
    write to.
- Without S3 settings, or with no read key (a fork PR gets no secrets), the
  composite falls back to the GitHub Actions cache. Its key is
  `<prefix>-<kache version>-<os>-<arch>-<Cargo.lock hash>`, and the action
  never re-saves a key that exists. The prefix is the workflow, the job id, an
  optional `key`, and the exact rustc (release and commit). Jobs that run the
  identical build share an entry with `shared-key`; restore-only jobs pass
  `save: false`.
- Jobs that time compiles (the dev-loop, dev-loop scaling and cold-start
  measurements) blank `RUSTC_WRAPPER` and unset the CMake launchers in the
  timed step, so they time the plain toolchain.
- Dependabot's GitHub Actions updates also scan `.github/actions/*`, so the
  pinned action commit inside the composite is updated with the rest.

```mermaid
flowchart LR
  A[checkout + toolchain] --> B[.github/actions/kache]
  B -->|S3 settings and a read key| S[S3 store<br/>shared by every job]
  B -->|otherwise| G[GitHub Actions cache<br/>prefix: workflow, job, key, rustc]
  S --> D[cargo build/test<br/>RUSTC_WRAPPER=kache]
  G --> D
  D -->|protected ref: write token| U[upload new artifacts]
  D -->|PR or other ref| R[read only]
```

## Consequences

- A run that leaves a crate's inputs unchanged restores that crate, workspace
  crates included. The generated-app builds in `generator-conformance.yml`
  also go through kache, which rust-cache never covered.
- The cargo registry and `$CARGO_HOME/bin` are no longer cached. Crate
  downloads add a few seconds per job. Tools come from their pinned installs,
  which the cargo-deny gates already required.
- A different feature set still recompiles autumn-web, whose key includes its
  features. Per-lane keys stay.
- On the GitHub cache fallback, the first job to save a key fixes its
  contents until `Cargo.lock`, rustc or kache changes. The S3 store has no
  such limit: every protected push adds what it compiled.
- C/C++ objects are cached on Linux and macOS only. On Windows,
  `cache-c-cpp` would replace MSVC `cl.exe` with `clang-cl`, which changes the
  C toolchain the build uses.
- The S3 store needs its own expiry. A lifecycle rule on the bucket that
  deletes objects not read for some weeks keeps it bounded.
- The write token is mapped in each workflow's own env, and a
  `pull_request` run uses the PR's copy of the workflow files. A PR from a
  branch in this repository could therefore change the mapping and receive
  the write token, then upload altered artifacts that protected runs would
  restore. Fork PRs receive no secrets at all. Closing this for same-repo
  PRs means moving the write token into a GitHub environment whose
  deployment branches are limited to protected branches, and naming that
  environment on the jobs that write.
- Scaffolded apps (`autumn-cli/src/templates/.github/workflows/*.tmpl`) still
  use rust-cache. What a generated app's CI uses is a separate decision for
  its users.

## Alternatives

- **Keep rust-cache.** It is simpler, but it rebuilds every workspace crate on
  every run.
- **sccache.** Also a per-compile `RUSTC_WRAPPER` cache. kache's GitHub action
  stores into the Actions cache with no external service, and its restores
  hardlink into `target/` instead of copying.
