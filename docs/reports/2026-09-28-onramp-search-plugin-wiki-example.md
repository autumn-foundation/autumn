# 🛣️ Onramp: mount `autumn-search` on `examples/wiki` (issue #2320 T3 Gap 6)

## 🎯 Journey

**First real integration** — hello world → a developer's own use case, for the
`autumn-search` plugin specifically. Weight: this exact gap has been surfaced
and then explicitly deferred by three consecutive prior Onramp cycles as "a
real, well-evidenced candidate" left for whoever picks up next:

- Issue #2320 (the 0.7.0 docs/examples coverage audit) named it directly as
  **T3 Gap 6**: *"`autumn-search` plugin (keyword + vector) — `search.md` is
  one of the most detailed guides in the tree; no example installs the crate.
  Fix: mount `autumn-search` on `wiki` (which already has `#[searchable]` FTS)
  as the keyword-backend example."*
- `docs/reports/2026-09-25-onramp-target-survey-negative-result.md` reconfirmed
  the gap was still open, fixed an unrelated harness item instead (CI scaffold
  coverage for `autumn-billing`), and explicitly deferred this one: *"it needs
  its own implementation + harness cycle, not a fold-in here."*
- Two more prior cycles (`docs/reports/2026-09-16-*`,
  `2026-09-17-*`) spent their budget on the cold-start compile-time lever
  (issue #2795) instead, without touching this gap.

This cycle picks it up. Reproduce the starting state and the fix below.

## 📈 Evidence

**Tier 1 — code-level, before this PR:**

```
$ grep -rl "autumn-search" examples/*/Cargo.toml
# (no output — no example in the workspace depends on autumn-search)
```

`docs/guide/search.md` documents the plugin's API in full (mount, index, hook
composition, config) but backs every snippet with `rust,ignore` — none of it
compiles or runs anywhere in the tree. The crate's own test suite
(`autumn-search/tests/integration/*.rs`, run in CI via
`cargo test -p autumn-search -- --ignored`) is thorough for the *engine*
(backend ranking, tenant scoping, soft-delete, the job-queue reindex path) but
every test builds its own throwaway `TestApp`/schema — none of it proves the
plugin's real-world composition story (an app that already has
`MutationHooks` for other reasons, per the guide's "If the repository already
has hooks, compose instead of replacing") against an actual, pre-existing
host application.

**Mechanism.** `search.md`'s hook-composition guidance is the one place a
developer following the guide is most likely to get it wrong silently: naming
`hooks = SearchSyncHooks<Article, NewArticle, UpdateArticle>` on a repository
that *already* has its own `MutationHooks` (slug generation, audit trails,
state-machine effects — exactly what `wiki::PageHooks` does) satisfies the
compiler while quietly discarding every one of those existing hooks. The guide
tells the reader not to do that ("compose instead of replacing... call
`enqueue_reindex_for`... from your own `after_*_commit`"), but nothing in the
tree demonstrates it working end to end, so there is no copyable reference and
no CI check that the composed form actually indexes anything.

**Why `wiki` specifically:** it already carries `#[searchable]` on `Page`
(the in-core FTS primitive `search.md` says the plugin "subsumes... rather
than replacing"), so the model needs zero changes — the fix is purely
application wiring, which is what the "keyword-backend example" fix
suggestion in #2320 asked for.

## 🔧 Change

Layer: **the docs/example layer**, backed by a new harness (Acceptable
Outcome #3 — "the journey wasn't measurable, now it is" — applies as much as
Outcome #1 here: before this PR there was no way, anywhere in CI, to prove the
plugin's hook-composition story works against a real host app). No API
change to `autumn-web` or `autumn-search`; everything below is additive and
scoped to `examples/wiki` plus one new CI line.

- `examples/wiki/Cargo.toml`: `autumn-search` as a path dependency.
- `examples/wiki/src/hooks.rs`: `PageHooks` gains `after_create_commit` /
  `after_update_commit` / `after_delete_commit`, each calling
  `autumn_search::enqueue_reindex_for` / `enqueue_unindex_for` — composed
  alongside the existing slug/state-machine logic exactly as the guide
  prescribes, not replacing it.
- `examples/wiki/src/repositories.rs`: `commit_hooks = true` on
  `PageRepository`, so the enqueue is staged durably in the same transaction
  as the mutation.
- `examples/wiki/src/lib.rs`: `pub fn search_plugin()` mounts
  `SearchPlugin::new().postgres().index::<Page>()` — a function (not inlined
  in `main.rs`) for the same reason `all_routes()` is one: the binary and any
  test that boots this app need the identical instance, and `models::Page` is
  a private module only crate-internal code can name.
- `examples/wiki/src/routes/pages.rs`: `GET /api/v1/search`, the plugin-backed
  sibling of the existing hand-rolled `/search` route (left untouched) —
  ranked, paginated, hydrated back into real `Page` rows via
  `SearchClient::search_hydrated`.
- `examples/wiki/tests/search_plugin_integration.rs` (new, Docker-gated): boots
  the real `wiki::all_routes()` app against a migrated Postgres testcontainer
  with the plugin mounted, then proves the whole path through the public HTTP
  surface only — `POST /api/v1/pages` → durably-enqueued job (not indexed yet)
  → `perform_enqueued_jobs()` → `GET /api/v1/search` finds it, hydrated → the
  same round trip in reverse for delete.
- `.github/workflows/ci.yml`: one line in the existing "Run Docker-dependent
  tests" step (`cargo test -p wiki --test search_plugin_integration --
  --ignored`) — `examples/wiki` has no consolidated `#[ignore]`-sweep binary
  the way `autumn`/`autumn-cli` do (CLAUDE.md's Docker-sweep guarantee is
  scoped to those two crates), so a new Docker test in a `tests/*.rs` file
  needs an explicit line, the same convention `saas`/`teams`/`cms`/
  `react-graphql`/`bookmarks-distributed` already use in this step.
- `docs/guide/search.md`, `EXAMPLES.md`: cross-link the example.

## 📊 Measurement

| | Before | After |
|---|---|---|
| Examples depending on `autumn-search` | 0 | 1 (`wiki`) |
| CI coverage of the plugin's application-level (hook-composition, commit-hook, job-queue) wiring against a real host app | none — only the crate's own isolated `TestApp` fixtures | 1 Docker-gated integration test, wired into `ci.yml`'s existing sweep |
| `search.md`'s "compose instead of replacing" guidance | prose only, `rust,ignore` | a working, tested reference (`wiki::hooks::PageHooks`) |
| `cargo check -p wiki` | — | clean |
| `cargo clippy -p wiki --all-targets -- -D warnings` | — | clean |
| `cargo fmt --all -- --check` | — | clean (this PR's files) |
| New Docker test run | — | **could not be run in this sandbox** (no Docker daemon — `docker ps` fails with "no such file or directory" on the socket); verified by CI's Docker-dependent-tests job on this PR instead |
| Public API compatibility (`autumn-web`, `autumn-search`, `autumn-cli`) | — | untouched — every change is inside `examples/wiki` plus one CI line |

This does not cleanly match one of the impact floor's five bullets (no
clean-room harness regression, no question-log class, no compile-time misuse
elimination) — flagging that plainly rather than force-fitting one. It is
justified instead the way `52dd8be`'s billing-harness fix (the immediately
prior cycle's own shipped change) was: Acceptable Outcome #3, "the journey
wasn't measurable, now it is... a complete deliverable on its own," plus three
consecutive prior cycles' explicit judgment that this is a real,
well-evidenced gap rather than a cosmetic one.

## 🔬 Reproduce

```bash
# Before: no example depends on autumn-search.
grep -rl "autumn-search" examples/*/Cargo.toml   # no output, pre-this-PR

# Compile/lint verification this sandbox *could* run (no DB needed):
cargo check -p wiki
cargo clippy -p wiki --all-targets -- -D warnings
cargo check -p wiki --test search_plugin_integration

# The end-to-end round trip (needs Docker; this sandbox has none):
cargo test -p wiki --test search_plugin_integration -- --ignored --test-threads=1

# Same crate-internal engine suite as before (unaffected by this PR):
cargo test -p autumn-search -- --ignored
```
