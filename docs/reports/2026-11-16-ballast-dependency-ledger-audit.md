# ⚓ Ballast: dependency ledger audit, 2026-11-16

Ninth Ballast pass, one week after 2026-11-09. This pass's headline: the
`RUSTSEC-2026-0459` waiver's `review-by` fell on today's date, and re-running
the trigger this time found the fix had actually shipped — `diesel` 2.4.0
(2026-11-13) raised its own `libsqlite3-sys` cap from `<0.39` to `<0.41`, and
a matching `diesel-async` 0.10.0 (`~2.4.0`) exists to go with it. That is the
rare case where a waived advisory stops being a chore and becomes a same-day
fix: rehearsed, confirmed clear on all three in-repo graphs (root,
`deny-sqlite.toml`, `fuzz/`), and landed. A second, independent trigger also
fired: `examples/island-flock/deny.toml` carried two `review-by: 2026-11-13`
dates — 3 days overdue and never touched in eight prior passes (open
follow-up #2 since 2026-09-07). Both reconfirmed unchanged on re-check.

## 🎯 Class

**Security response** (primary): `RUSTSEC-2026-0459` (bundled SQLite before
3.54.0 can return uninitialized page-cache memory when a database file is
opened next to a foreign `-wal` of a different page size) — fixed by
bumping `diesel` 2.3.14 → 2.4.0, `diesel-async` 0.9.2 → 0.10.0, and
`libsqlite3-sys` 0.38.2 → 0.40.0 across the root workspace and the `fuzz/`
satellite graph. Secondary: **policy** upkeep — refreshed the two overdue
`examples/island-flock/deny.toml` waivers (`RUSTSEC-2024-0370`,
`RUSTSEC-2025-0141`), reconfirmed unchanged; adjusted the generated-app
`autumn-cli/src/templates/deny.toml.tmpl` waiver's reasoning to the new,
more precise state (fix landed upstream and in-repo, not yet in a released
`autumn-web`); Dependabot queue triage (no action — one trivial GH Actions
PR, not a Rust dependency); new finding filed, not acted on (see
Follow-ups).

## 📈 Evidence

**Trigger check.** `grep -n review-by` across `deny.toml`,
`deny-sqlite.toml`, `fuzz/deny.toml`, `autumn-cli/src/templates/deny.toml.tmpl`,
and `examples/island-flock/deny.toml` found two dates due or overdue as of
2026-11-16:

| Advisory | Waiver(s) | Old review-by | Status |
| --- | --- | --- | --- |
| RUSTSEC-2026-0459 | root `deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`, CLI template | 2026-11-16 | due today |
| RUSTSEC-2024-0370, RUSTSEC-2025-0141 | `examples/island-flock/deny.toml` | 2026-11-13 | 3 days overdue |

**RUSTSEC-2026-0459 — the trigger found a real fix.** The old waiver text
said `diesel` 2.3.14 (then latest) capped `libsqlite3-sys` below the fixed
0.39.0. Querying the sparse registry index (`https://index.crates.io/di/es/diesel`)
showed `diesel` 2.4.0, published 2026-11-13 — three days before this pass.
Its manifest raises the `libsqlite3-sys` requirement from `>=0.17.2, <0.39.0`
to `>=0.17.2, <0.41.0` (confirmed via the index, and independently via
`diesel`'s own `CHANGELOG.md` `[2.4.0]` "Changed" section: "Extended
libsqlite3-sys support to include 0.40"). `libsqlite3-sys` 0.39.0/0.39.1/0.40.0
all exist upstream.

The first rehearsal attempt (`cargo update -p diesel --precise 2.4.0`)
failed: `diesel-async` 0.9.2 pins `diesel = "~2.3.9"` (tilde — excludes
2.4.0), so the real blocker was one level up the stack, not in `diesel`
itself. Checking `diesel-async`'s own published versions showed 0.10.0
exists and pins `diesel = "~2.4.0"` — a version released to go with
`diesel` 2.4.0. Bumping both together resolved cleanly.

A second-order check: `autumn-web`'s own `Cargo.toml` additionally pins
`libsqlite3-sys = { version = "0.38", ... }` directly (not only transitively
through `diesel`), with an inline comment ("Held at 0.38: 0.39 exists, but
diesel 2.3 needs `<0.39`.") that was itself a dated trigger nobody had
scheduled — this pass found and fixed it as a side effect of fixing the
`diesel`/`diesel-async` pair.

**Compatibility read before rehearsing.** `diesel` 2.4.0's changelog
"Changed" section was read in full for anything our usage touches: MSRV
raised to 1.88.0 (this repo pins 1.97.0 — clear); `SqliteConnection::establish`
now disables the two-argument `fts3_tokenizer()` and
`set_fts3_tokenizer_enabled` became `unsafe fn` (grepped the tree for
`fts3_tokenizer`/`set_fts3_tokenizer_enabled` — zero hits, unused); `Bpchar`
became a distinct SQL type instead of a hidden `Varchar` alias (autumn-cli's
own schema generator already routes fixed-length `char(N)`/`bpchar` columns
to its own `Opaque` type rather than diesel's typed `Varchar`/`Bpchar` —
`autumn-cli/src/schema/introspect.rs`'s
`build_table_preserves_length_limited_char_columns_as_opaque` test documents
this — so the distinction doesn't reach generated code); custom
`ValidGrouping<GB>` impls on subselect expressions now need to be generic
over `GB` (grepped for `ValidGrouping` — zero custom impls in the tree, only
derives). No forcing-fact mismatch found.

**Rehearsal.** On this branch (working tree, reversible):

1. `Cargo.toml`: `diesel-async` `"0.9"` → `"0.10"`; `libsqlite3-sys` `"0.38"`
   → `"0.40"` (with the stale inline comment replaced).
2. The same `diesel-async` bump applied to the 12 workspace-member example
   crates that pin it directly (`teams`, `bookmarks-sharded`, `todo-app`,
   `reddit-clone`, `bookmarks-distributed`, `wiki`, `blog`, `bookmarks`,
   `cms`, `escape-hatches`, `saas`, `react-graphql`) — needed because
   they're one Cargo.lock resolve with the root crate; leaving even one at
   `"0.9"` would have produced two diesel-async major-lines side by side
   instead of a clean dedup.
3. `cargo update -p diesel -p diesel-async -p libsqlite3-sys` (root),
   `cargo update -p libsqlite3-sys -p diesel -p diesel-async -p
   diesel_migrations` (`fuzz/`, its own workspace/lockfile).
4. `cargo deny --offline check advisories` on root, `deny-sqlite.toml`, and
   `fuzz/deny.toml`: **RUSTSEC-2026-0459 no longer matches any crate in any
   of the three graphs** (`advisory-not-detected` warning, confirming the
   ignore entry is now dead weight, not that the check is broken).
5. `cargo build -p autumn-web --no-default-features --features
   sqlite,test-support`: clean, 2m14s, exit 0.
6. `cargo test -p autumn-web --features "repl,sqlite" --lib repl::`: 30
   passed, 0 failed (the CI-named sqlite repl suite).
7. `cargo test -p autumn-web --features "sqlite,mail" --lib
   db::sqlite_types::` and `cargo check -p todo-app` (a representative
   example on the bumped `diesel-async` + Postgres path): queued, see
   Measurement.
8. `examples/island-flock`: `cargo fetch` then `cargo deny --offline check
   advisories sources`: clean — confirms the two reconfirmed-unchanged
   waivers there still correctly match (not stale in the other direction).

Working tree is the PR itself (not reverted) because the rehearsal cleared
the floor — see Measurement.

**`examples/island-flock`'s two advisories — reconfirmed unchanged, dates
only.** `RUSTSEC-2024-0370` (`proc-macro-error`, unmaintained): sparse index
confirms `yew` latest is still 0.23.0 and `yew-macro` 0.23.0 still requires
`proc-macro-error = "^1"` — no fix upstream. `RUSTSEC-2025-0141` (`bincode`,
unmaintained): `gloo-worker` has in fact shipped 0.7.0, which drops the
`bincode` dependency entirely — a real upstream change — but `yew` 0.23.0
(still latest) requires `gloo = "^0.11"`, which resolves to `gloo-worker`
0.5.0, not 0.7.0. The fix exists in the ecosystem; `yew` itself hasn't moved
to take it. Both waivers' `reason` text and `review-by` updated to record
this (2026-12-05), rather than just re-stamping the old text with a new
date — the `gloo-worker` 0.7.0 fact is new information worth keeping for
whoever checks this next.

**Dependabot queue.** `list_pull_requests` (open, this repo): one bot PR,
`#3681` (`taiki-e/install-action` 2.89.0→2.90.3, opened 2026-11-09). A
GitHub Actions tool-installer bump, not a Rust dependency in this ledger's
graph — zero-risk, routine, left for ordinary review rather than a Ballast
action. `#3620` (`actions/setup-node`, blocked last pass by an unrelated
Docker test failure) is gone from the open list entirely — resolved by
someone else between passes, not re-investigated here.

**GitHub-native Dependabot alert count**: still unreadable — `gh api
repos/.../dependabot/alerts` returns `403 Resource not accessible by
integration` through this session's token, same as every prior pass.
**Sixth pass flagging this gap**, no number to report.

**License and source checks**: clean on root, `deny-sqlite.toml`, and
`examples/island-flock` (licenses not gated there by design, sources
checked).

## 💡 Mechanism / forcing fact

A `review-by` date passing is this project's own contract to re-run the
check, not a courtesy reminder, and today it paid off twice over. For
`RUSTSEC-2026-0459`, the forcing fact is textbook security-response: a
reachable fix that didn't exist a week ago now does (`diesel` 2.4.0 three
days old), so it ships now at the smallest viable bump rather than riding a
future batch. For the `island-flock` advisories, the forcing fact check
came back negative — `gloo-worker` moved, but the blocking dependency
(`yew`) didn't — so the correct action is the documented-pin renewal, not a
bump.

## 🔧 Change

- `Cargo.toml`: `diesel-async` `0.9` → `0.10`; `libsqlite3-sys` `0.38` →
  `0.40` (stale "held because diesel 2.3 needs <0.39" comment replaced with
  the fix record).
- 12 example `Cargo.toml`s: `diesel-async` `0.9` → `0.10` (version pin only,
  no code changes — none use diesel-async APIs touched by the 0.9→0.10
  changelog).
- `Cargo.lock`, `fuzz/Cargo.lock`: updated via `cargo update` (see
  Reproduce). `diesel_derives`, `diesel_migrations`,
  `diesel_table_macro_syntax`, `dsl_auto_type`, `migrations_internals`,
  `migrations_macros` moved with `diesel`/`diesel-async` as one family;
  `diesel_attribute_parser` and `sqlstate` are new transitive additions from
  that same family (diesel 2.4.0 split them out); `darling`/`toml`/`winnow`
  old copies dropped as a side effect (see Measurement).
- `deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`: removed the now-dead
  `RUSTSEC-2026-0459` ignore entry (confirmed via `cargo deny` that the
  advisory no longer matches any crate in any of the three graphs).
- `autumn-cli/src/templates/deny.toml.tmpl`: **kept** the
  `RUSTSEC-2026-0459` waiver (freshly-generated apps still depend on
  whatever `autumn-web` is currently published on crates.io, which has not
  shipped this fix — CLAUDE.md: no version bump without the user asking for
  one) but rewrote its `reason`/comment to state the corrected, narrower
  fact: the fix exists upstream and is live in this repo's unreleased root
  and `fuzz/` graphs, and the waiver's real expiry is "next `autumn-web`
  release," not "diesel accepts 0.39" (diesel already does). `review-by`
  moved 2026-11-16 → 2026-12-05.
- `examples/island-flock/deny.toml`: `RUSTSEC-2024-0370` and
  `RUSTSEC-2025-0141` `reason`/comments rewritten with the `gloo-worker`
  0.7.0 finding and `review-by` moved 2026-11-13 → 2026-12-05.
- `changelog.d/sqlite-cve-2026-0459-diesel-2-4.md`: new fragment under
  `### Security` — this is user-visible (an app that pulls a new
  `autumn-web` release picks up the fixed SQLite).

## 📊 Measurement

| Check | 2026-11-09 | 2026-11-16 (before) | 2026-11-16 (after) |
| --- | --- | --- | --- |
| `cargo deny --offline check advisories` (root) | ok, zero warnings | ok (0459 waived) | **ok, zero warnings** — 0459 ignore removed, nothing else matches |
| `cargo deny --offline --config deny-sqlite.toml check advisories` | ok, zero warnings | ok (0459 waived) | **ok, zero warnings** (0441 "advisory-not-detected" on this graph is pre-existing/unrelated — this feature set never pulls `loom`/`wasmtimer`) |
| `cargo deny --offline check advisories` (`fuzz/`) | not separately re-run | ok (0459 waived) | **ok, zero warnings** — 0459 ignore removed |
| `diesel` / `diesel-async` / `libsqlite3-sys` | 2.3.14 / 0.9.2 / 0.38.2 | 2.3.14 / 0.9.2 / 0.38.2 | **2.4.0 / 0.10.0 / 0.40.0** |
| RUSTSEC-2026-0459 | waived, review-by 2026-11-16 | waived, due today | **closed** (not waived — fixed) |
| RUSTSEC-2024-0370 / 0141 (`island-flock`) | not tracked | waived, 3 days overdue | reconfirmed unchanged, review-by 2026-12-05 |
| Root `Cargo.lock` package count | not recorded | 1005 | **1001** |
| Root duplicate-named crates (`^name = ` entries with >1 version) | not recorded this way | 82 | **79** (3 groups fully collapsed: `winnow`, `toml`, `toml_datetime`; `darling`/`darling_core`/`darling_macro` each dropped one of four coexisting versions) |
| `cargo deny check bans` duplicate-version warnings (root) | 67 | not re-measured pre-bump (see note) | **63** |
| `autumn-web` sqlite build (`--no-default-features --features sqlite,test-support`) | not run | — | **clean, 2m14s** |
| `autumn-web` sqlite repl tests (`--features repl,sqlite --lib repl::`) | not run | — | **30 passed, 0 failed** |
| `autumn-web` `db::sqlite_types::` tests | not run | — | **7 passed, 0 failed** |
| `todo-app` check (representative `diesel-async` 0.10 Postgres consumer) | not run | — | **clean, 4m23s** (`diesel_migrations` 2.4.0, `diesel-async` 0.10.0) |
| Open Dependabot PRs (Rust/Actions) | 1 (`#3620`, blocked) | 1 (`#3681`, GH Action only) | 1 (unchanged — routine, not a Rust dep) |

Note: the pre-bump `cargo deny check bans` count for this exact day wasn't
independently re-measured against this pass's own unmodified `Cargo.lock`
(doing so safely would have meant reverting `Cargo.toml`/`Cargo.lock` to
disk while a background `cargo build` against the new lockfile was still
running — judged too risky to the in-flight rehearsal and aborted after
starting). The 82/79 duplicate-named-crate comparison above is the safe
substitute: it diffs `git show HEAD:Cargo.lock` against the working tree
directly, no file-swap required, and is exact.

All four rehearsal builds/tests are clean. Nothing in this pass surfaced a
failure, so the bump ships as a normal security-response PR rather than a
documented pin.

## 🔬 Reproduce

```
cargo install --locked cargo-deny@0.20.2
cargo update -p diesel -p diesel-async -p libsqlite3-sys
(cd fuzz && cargo update -p libsqlite3-sys -p diesel -p diesel-async -p diesel_migrations)
cargo fetch --locked
cargo deny fetch db
cargo deny --offline check advisories
cargo deny --offline --config deny-sqlite.toml check advisories
(cd fuzz && cargo deny --offline check advisories)
(cd examples/island-flock && cargo fetch && cargo deny --offline check advisories sources)
cargo deny --offline check bans

cargo build -p autumn-web --no-default-features --features sqlite,test-support
cargo test -p autumn-web --features "repl,sqlite" --lib repl::
cargo test -p autumn-web --features "sqlite,mail" --lib db::sqlite_types::
cargo check -p todo-app

# Upstream version checks (sparse index, no registry auth needed)
curl -sS https://index.crates.io/di/es/diesel | tail -5            # latest diesel
curl -sS https://index.crates.io/di/es/diesel-async | tail -5      # latest diesel-async
curl -sS https://index.crates.io/li/bs/libsqlite3-sys | tail -5    # latest libsqlite3-sys
curl -sS https://index.crates.io/ye/w-/yew-macro | tail -3         # yew-macro's proc-macro-error req
curl -sS https://index.crates.io/gl/oo/gloo-worker | tail -3       # gloo-worker's bincode req
curl -sS https://raw.githubusercontent.com/diesel-rs/diesel/v2.4.0/CHANGELOG.md

# Dependabot queue
# (GitHub MCP) list_pull_requests(owner=autumn-foundation, repo=autumn, state=open)
gh api repos/autumn-foundation/autumn/dependabot/alerts
```

## Follow-ups still open

1. **Cost attribution and usage/unused-feature audit on the root graph's
   heaviest hires**: still not run — **tenth pass now** flagging this as
   the oldest open item (first flagged 2026-09-07). This genuinely needs a
   pass where it's the only focus instead of continuing to lose its budget
   to whatever trigger fired that week.
2. **`fuzz/` and `examples/island-flock` scheduled-batch ownership**:
   partially addressed this pass (both graphs got real attention — a
   version bump for `fuzz/`, a waiver refresh for `island-flock`) but
   neither has a standing cadence of its own; they're still only touched
   when something else drags Ballast into them.
3. **GitHub-native Dependabot alert count/breakdown**: still not readable
   (`dependabot/alerts` → 403) — sixth pass flagging the gap.
4. **New finding, not yet acted on: `benchmarks/runtime/autumn/` is a fourth
   satellite graph with no audit coverage at all.** It declares its own
   empty `[workspace]` (standalone, like `fuzz/` and `island-flock`), has no
   committed `Cargo.lock`, and pins `diesel-async = "0.9"` directly — unlike
   `fuzz/` and `island-flock`, it isn't named anywhere in
   `scripts/check-advisories.sh`'s five-graph header comment, isn't built
   by any CI workflow found this pass, and wasn't bumped in this PR (out of
   scope: no lockfile to rehearse against, and it's a benchmark harness, not
   shipped code). Needs its own decision: commit a `Cargo.lock` and bring it
   under the advisory gate, or document why a benchmark-only crate with no
   lockfile is deliberately outside the ledger.
5. **`autumn-cli/src/templates/deny.toml.tmpl`'s `RUSTSEC-2026-0459` waiver
   now has a precise, mechanical unblock condition**: it clears on its own
   the day a new `autumn-web` version ships (no further investigation
   needed — just confirm the release notes mention the diesel bump, or
   re-check that the published crate's own `Cargo.toml` requirement moved).
   Flagging so a future pass checks this mechanically rather than
   re-deriving it.
