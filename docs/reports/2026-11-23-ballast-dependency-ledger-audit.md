# ⚓ Ballast: dependency ledger audit, 2026-11-23

Tenth Ballast pass, one week after 2026-11-16. This pass's trigger was the
`RUSTSEC-2026-0387` waiver's `review-by` date, due today across root
`deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`, and the CLI scaffold
template. Re-running that trigger turned up a real upstream fix
(`jsonwebtoken` 12.0.0) — but rehearsal showed it is not actually reachable
in this tree, blocked by a separate, previously-undiscovered `rsa`
version/feature conflict with `webauthn-rs`. Installing `cargo-deny`
(previously absent from this environment) to re-run the trigger also
surfaced a second, independent, **new, unwaived, CI-failing** advisory —
`RUSTSEC-2026-0592` in `url` — published 2026-11-20, three days before this
pass. That one had a clean one-line fix and is this pass's headline PR.

## 🎯 Class

**Security response** (primary, landed): `RUSTSEC-2026-0592` (`url`'s
`Url::origin()` recursed once per nested `blob:` prefix with no depth limit,
letting a crafted URL overflow the stack and abort the process) — fixed by
bumping `url` 2.5.8 → 2.5.9 on the root workspace graph and the `fuzz/`
satellite graph, the only two graphs that depend on it. **Policy upkeep**
(documented pin, not landed): `RUSTSEC-2026-0387`'s waiver reconfirmed and
its `reason` text corrected with new rehearsal evidence, review-by moved
2026-11-23 → 2026-12-05; `RUSTSEC-2023-0071`'s waiver text corrected
alongside it (same root cause, discovered as a side effect), review-by left
unchanged (not due). Dependabot queue triage: no action (one Rust crate PR,
one GitHub Actions PR, both routine, left for ordinary review).

## 📈 Evidence

**Trigger check.** `grep -n review-by` across all five policy files found
one date due today: `RUSTSEC-2026-0387`, review-by 2026-11-23, in root
`deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`, and
`autumn-cli/src/templates/deny.toml.tmpl`. `examples/island-flock/deny.toml`
(review-by 2026-12-05) and the two other `RUSTSEC-2026-0440`/`0441`/
`RUSTSEC-2023-0071` waivers (all 2026-12-05) are not due.

**`cargo-deny` was not installed in this environment** — a first for this
ledger (prior passes' environments carried it). Installed
`cargo-deny@0.20.2` via `cargo install --locked` to re-run every check this
pass and every prior pass relied on; this is itself a Tier-1 gap worth
naming, though out of scope to fix today (it's an environment property, not
a repo one — CI pins the same version via `taiki-e/install-action`, unaffected).

**`RUSTSEC-2026-0387` re-check: a fix exists, but it's unreachable.**
Querying the sparse index showed `jsonwebtoken` 12.0.0, published
2026-11-13 (10 days before this pass). Its manifest raises `p256`/`p384`
from `^0.13.2`/`^0.13.0` to `^0.14`, which pulls `elliptic-curve` 0.14.3 and
`crypto-bigint` 0.7.10 — exactly the patched family (`crypto-bigint` fix is
`>=0.7.6`) already used elsewhere in this tree (`autumn-web`'s own `p256`
for Web Push, and `aws-sigv4`). That would have collapsed the last
`crypto-bigint`/`elliptic-curve`/`p256`/`p384`/`ecdsa`/`primeorder` duplicate
pairs in the graph.

It is not adoptable today. `jsonwebtoken` 12.0.0 also raises its own `rsa`
requirement from `^0.9` to `^0.10`. Rehearsal (`cargo update -p jsonwebtoken
-p ed25519-dalek` on this branch, after bumping both in `autumn/Cargo.toml`)
failed resolution:

```
failed to select a version for `rsa`.
    ... required by package `crypto-glue v0.2.2`
    ... which satisfies dependency `crypto-glue = "^0.2.1"` of package `webauthn-rs-core v0.6.2`
    ... which satisfies dependency `webauthn-rs-core = "=0.6.2"` of package `webauthn-rs v0.6.2`
    ... which satisfies dependency `webauthn-rs = "^0.6"` of package `autumn-web`

package `crypto-glue` depends on `rsa` with feature `sha2` but `rsa` does not have that feature.
```

Checked every published `crypto-glue` 0.2.x release (0.2.0, 0.2.1, 0.2.2 —
latest) directly against the sparse index: all three pin
`rsa = "^0.10.0-rc.18", features = ["sha2"]`. Cargo's semver rules let that
range's upper bound match the final `rsa` 0.10.0 release (a prerelease
requirement is allowed to match the release it leads up to), so when
`jsonwebtoken`'s `rsa = "^0.10"` and `crypto-glue`'s
`rsa = "^0.10.0-rc.18"` overlap at `0.10.0`, Cargo tries to unify them into
one node rather than carry two — and the final 0.10.0 release dropped the
`sha2` feature name that only the 0.10.0-rc.* line carried, so unification
fails outright. This is the **same root cause** already named in
`RUSTSEC-2023-0071`'s waiver (`crypto-glue/webauthn-rs ^0.10.0-rc.18`), just
hit from the other direction: that waiver already knew `rsa` 0.10.0 final
existed and blamed two blockers (`jsonwebtoken (^0.9)` and
`crypto-glue/webauthn-rs`). Today's rehearsal retires the first blocker —
`jsonwebtoken` has itself moved to `rsa ^0.10` — leaving `crypto-glue`'s
release-candidate pin as the sole remaining blocker for *both* advisories.
Reverted the `Cargo.toml`/`Cargo.lock` edits after confirming the failure
(`git checkout --`); nothing from this rehearsal ships.

**`RUSTSEC-2026-0592` — new, reachable, trivial fix.** Running
`cargo deny --offline check advisories` on the (unmodified) root graph to
establish this pass's baseline failed outright — not on the trigger
advisory, but on a different one:

```
error[vulnerability]: Stack overflow in `Url::origin()` on nested `blob:` URLs
    ID: RUSTSEC-2026-0592
    url v2.5.8
```

Published 2026-11-20 (3 days before this pass; the prior pass, 2026-11-16,
predates it, so this is not a gap in that pass — the advisory did not exist
yet). `Url::origin()` on a `blob:` URL recursed into itself once per nested
`blob:` prefix with no depth limit; a URL with 10,000 such prefixes (50,020
bytes) overflows a 2 MiB thread stack, aborting the process —
unconditionally, past `catch_unwind()`. Grepped the tree for `.origin()`
calls to check reachability before treating this as a chore: this codebase
calls it directly in `autumn/src/http_client.rs` (comparing origins across a
redirect — the server being fetched controls the redirect target),
`autumn/src/aeo/web_bot_auth.rs` (serializing the request origin for a
signed header), `autumn/src/aeo/documents.rs` (same-origin check against a
parsed document reference), `autumn/src/push/store.rs` and
`autumn/src/push/service.rs` (Web Push subscription endpoint origins — the
endpoint URL is supplied by whatever push service the browser chose), and
`autumn-cli/src/generate/tauri.rs`. The `http_client.rs` and push call sites
in particular sit on externally-influenced input (a redirect `Location`
header, a push subscription endpoint), so this is a **reachable** advisory,
not a chore — verified by code inspection, not assumed from the CVSS text.

Fix: `url` 2.5.9 (published 2026-11-20, same day as the advisory). Checked
the dependents' own version requirements first: all four manifests that pin
`url` directly (`autumn/Cargo.toml`, `autumn-cli/Cargo.toml`,
`examples/todo-app/Cargo.toml` at `"2.5.8"`, `examples/reddit-clone/Cargo.toml`
at `"2"`) already allow 2.5.9 under default caret semantics — no manifest
*requirement* change was strictly needed, `cargo update -p url` alone
resolves it. Bumped the three exact-pinned manifests' floors to `"2.5.9"`
anyway, so a lockfile regenerated from scratch also lands on the patched
floor instead of merely being allowed to.

**Rehearsal.**

1. `cargo update -p url --precise 2.5.9` (root): updates `url` 2.5.8 → 2.5.9
   and `form_urlencoded` 1.2.2 → 1.2.3 (its own dependency bump). One
   incidental, harmless side effect: `cssparser-macros` 0.7.1's `syn`
   dependency resolved to 2.0.119 instead of 3.0.10 — both versions already
   coexisted in the graph before this change (confirmed: `syn` 1.0.109,
   2.0.119, and 3.0.10 are present in `Cargo.lock` both before and after),
   so this just moved one consumer onto an already-shared version; it adds
   no new duplicate.
2. `cargo update -p url --precise 2.5.9` (`fuzz/`, its own lockfile): same
   bump, `form_urlencoded` 1.2.2 → 1.2.3.
3. `examples/island-flock`: checked — does not depend on `url` at all (no
   match in its `Cargo.lock`). Out of scope.
4. `cargo deny --offline check advisories` on root, `deny-sqlite.toml`, and
   `fuzz/deny.toml`: all three **ok** (root and `fuzz/` newly clean;
   `deny-sqlite.toml` was never affected since it shares the root
   `Cargo.lock`).
5. `cargo build -p autumn-web --no-default-features --features
   jwt,test-support`: clean, 1m40s.
6. `cargo check --workspace --no-default-features --features jwt -p
   autumn-web`: clean.
7. `cargo test -p autumn-web --lib --features
   jwt,test-support,webauthn,push http_client::`: **159 passed, 0 failed**
   (includes the SSRF/redirect-origin-comparison suite,
   `ssrf_redirect_plan_honours_chained_override` among them — the exact code
   path that calls `.origin()` on a redirect target).
8. `cargo test -p autumn-web --lib --features test-support,webauthn,push
   aeo::`: **249 passed, 0 failed**.
9. `cargo test -p autumn-web --lib --features test-support,webauthn,push
   push::`: **131 passed, 0 failed** (includes the VAPID/ES256 JWT suite,
   unaffected since it doesn't touch `jsonwebtoken`).
10. `fuzz/`: `RUSTFLAGS="--cfg fuzzing" cargo check` (the real invocation —
    a bare `cargo check` fails on `#[cfg(fuzzing)]`-gated items regardless of
    this change, confirmed by reproducing that failure and then clearing it
    with the correct flag): clean, 1m36s.

Generated-app graph (`scripts/check-advisories.sh`'s `audit_generated_apps`)
was not re-run this pass: it resolves a fresh `Cargo.lock` from crates.io on
every invocation, so any newly generated app already lands on `url` ≥2.5.9
by ordinary resolution regardless of today's manifest edits — unlike the
2026-11-16 `diesel` pass, nothing in the scaffold path pins `url` below the
fix, so there is nothing for that sweep to newly exercise.

**Dependabot queue.** Two open PRs: `#3748` (`smop` 0.3.2 → 0.4.0, the
`xtask` dependency CLAUDE.md names as the project's deliberately
dependency-light tool runner) and `#3744` (`taiki-e/install-action`
2.89.0 → 2.90.3, a GitHub Actions tool installer, not a Rust dependency in
this ledger's graph). Both routine, zero-risk version bumps on dependencies
with no history of breaking this repo — left for ordinary review rather
than a Ballast action, consistent with "one-PR-per-bump bot spam rides
ordinary review, not a Ballast merge."

**GitHub-native Dependabot alert count**: not attempted this pass (no
`gh` CLI / direct API access in this environment's tool set); carrying
forward the prior passes' note that this has been unreadable via `gh api`
in past environments too.

## 💡 Mechanism / forcing fact

`RUSTSEC-2026-0592` is textbook security-response: a new advisory, a
reachable call path (origin comparisons on externally-influenced URLs), and
a same-day upstream patch release — ships now, at the smallest possible
bump (a patch version already inside every dependent's existing semver
range). `RUSTSEC-2026-0387`'s forcing-fact check came back negative a
second time: a fix release now exists, but rehearsal proved it is not
reachable in this tree, for a *new* reason (the `rsa` conflict) rather than
the old one (`jsonwebtoken` still on `p256` 0.13) — so the correct action is
a documented-pin renewal with the new evidence recorded, not a bump.

## 🔧 Change

- `Cargo.lock`: `url` 2.5.8 → 2.5.9, `form_urlencoded` 1.2.2 → 1.2.3;
  `cssparser-macros`'s `syn` dependency edge shifted from the 3.0.10 node to
  the already-present 2.0.119 node (incidental resolver rebalancing, no new
  duplicate — see Measurement).
- `fuzz/Cargo.lock`: same `url`/`form_urlencoded` bump on the satellite
  graph.
- `autumn/Cargo.toml`, `autumn-cli/Cargo.toml`, `examples/todo-app/Cargo.toml`:
  `url = "2.5.8"` → `"2.5.9"` (floor raised to match; not strictly required
  for resolution, done so a from-scratch lockfile also lands on the patched
  floor).
- `deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`,
  `autumn-cli/src/templates/deny.toml.tmpl`: `RUSTSEC-2026-0387` waiver
  `reason`/comment rewritten with this pass's rehearsal evidence (the
  `jsonwebtoken` 12.0.0 fix and the `crypto-glue` `rsa` conflict that blocks
  it), review-by moved 2026-11-23 → 2026-12-05 (aligned with
  `RUSTSEC-2023-0071`'s date, since both now share the same blocker and
  should be re-checked together). `RUSTSEC-2023-0071` waiver `reason` text
  also corrected in the same four files — it no longer blames
  `jsonwebtoken (^0.9)` (true before this pass, false after: `jsonwebtoken`
  12.0.0 has itself moved to `rsa ^0.10`) — review-by left unchanged
  (2026-12-05, not due).
- `changelog.d/rustsec-2026-0592-url-stack-overflow.md`: new fragment under
  `### Security` — user-visible (an app that pulls a new `autumn-web`
  release picks up the fixed `url`).

## 📊 Measurement

| Check | 2026-11-16 | 2026-11-23 (before) | 2026-11-23 (after) |
| --- | --- | --- | --- |
| `cargo deny --offline check advisories` (root) | ok, zero warnings | **FAILED** — RUSTSEC-2026-0592 (url 2.5.8), unwaived | **ok, zero warnings** |
| `cargo deny --offline --config deny-sqlite.toml check advisories` | ok, zero warnings | ok (shares root `Cargo.lock`'s advisory state via the url advisory too — see note) | **ok, zero warnings** |
| `cargo deny --offline check advisories` (`fuzz/`) | ok, zero warnings | **FAILED** — RUSTSEC-2026-0592 (url 2.5.8) | **ok, zero warnings** |
| `url` / `form_urlencoded` (root + `fuzz/`) | 2.5.8 / 1.2.2 | 2.5.8 / 1.2.2 | **2.5.9 / 1.2.3** |
| RUSTSEC-2026-0592 | did not exist | unwaived, failing CI | **closed** (fixed, not waived) |
| RUSTSEC-2026-0387 | waived, review-by 2026-11-23 | due today | reconfirmed unreachable (new reason: `rsa`/`crypto-glue` conflict, not `jsonwebtoken`'s own `p256` version); review-by 2026-12-05 |
| RUSTSEC-2023-0071 | waived, review-by 2026-12-05 | not due | reason text corrected (one of its two named blockers cleared); review-by unchanged, 2026-12-05 |
| Root `Cargo.lock` package count | 1001 | 1002 | **1002** (unchanged — in-place version swap, no crate added/removed) |
| Root duplicate-named crates (`^name = ` entries with >1 version) | 79 | 83 | **83** (unchanged — see `cssparser-macros`/`syn` note below) |
| `fuzz/Cargo.lock` package count | not tracked | 493 | **493** (unchanged) |
| `fuzz/Cargo.lock` duplicate-named crates | not tracked | 40 | **40** (unchanged) |
| `autumn-web` build (`--no-default-features --features jwt,test-support`) | n/a (different feature set) | not run | **clean, 1m40s** |
| `autumn-web` check (`--features jwt`, workspace) | not run | — | **clean** |
| `http_client::` tests (`jwt,test-support,webauthn,push`) | not run | — | **159 passed, 0 failed** |
| `aeo::` tests (`test-support,webauthn,push`) | not run | — | **249 passed, 0 failed** |
| `push::` tests (`test-support,webauthn,push`) | not run | — | **131 passed, 0 failed** |
| `fuzz/` check (`RUSTFLAGS="--cfg fuzzing"`) | not run this way | — | **clean, 1m36s** |
| Open Dependabot PRs (Rust/Actions) | 1 (`#3681`, GH Action) | 2 (`#3748` Rust `smop`, `#3744` GH Action) | 2 (unchanged — both routine, left for ordinary review) |

Note: the 83→83 duplicate count is unchanged in total, but its composition
shifted by one edge (`cssparser-macros` off `syn` 3.0.10, onto the
already-present `syn` 2.0.119) as a side effect of `cargo update -p url`
re-resolving part of the graph — not a separate action taken this pass, and
not a new duplicate (all three `syn` majors were already present before and
after). Root package count went 1001→1002 between the 2026-11-16 and
2026-11-23 "before" columns from unrelated changes landed on `trunk-dev`
during the week (the 0.11.0 release and others); this pass's own before→after
column (1002→1002) is the one that isolates today's change.

Both rehearsal builds/tests are clean. The security-response half of this
pass ships as a normal PR; the policy half (both waiver text corrections)
rides the same PR since it's pure documentation of evidence gathered while
investigating the shipped fix, not a separate dependency change.

## 🔬 Reproduce

```
cargo install --locked cargo-deny@0.20.2

cargo update -p url --precise 2.5.9
(cd fuzz && cargo update -p url --precise 2.5.9)
cargo fetch --locked
cargo deny fetch db
cargo deny --offline check advisories
cargo deny --offline --config deny-sqlite.toml check advisories
(cd fuzz && cargo deny --offline check advisories)

cargo build -p autumn-web --no-default-features --features jwt,test-support
cargo check --workspace --no-default-features --features jwt -p autumn-web
cargo test -p autumn-web --lib --features "jwt,test-support,webauthn,push" http_client::
cargo test -p autumn-web --lib --features "test-support,webauthn,push" aeo::
cargo test -p autumn-web --lib --features "test-support,webauthn,push" push::
(cd fuzz && RUSTFLAGS="--cfg fuzzing" cargo check)

# The RUSTSEC-2026-0387 rehearsal that failed (not landed; shown for the
# next pass to re-verify without repeating the discovery work):
# Edit autumn/Cargo.toml: jsonwebtoken "11.1.0" -> "12.0.0",
#                         ed25519-dalek "2" -> "3" (drop the removed "std" feature)
cargo update -p jsonwebtoken -p ed25519-dalek   # fails: rsa version conflict

# Upstream version checks (sparse index, no registry auth needed)
curl -sS https://index.crates.io/3/u/url | tail -3               # latest url
curl -sS https://index.crates.io/js/on/jsonwebtoken | tail -3    # latest jsonwebtoken
curl -sS https://index.crates.io/cr/yp/crypto-glue | tail -5     # crypto-glue's rsa pin
curl -sS https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/crypto-bigint/RUSTSEC-2026-0387.md
curl -sS https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/rsa/RUSTSEC-2023-0071.md

# Dependabot queue
# (GitHub MCP) list_pull_requests(owner=autumn-foundation, repo=autumn, state=open)
```

## Follow-ups still open

1. **Cost attribution and usage/unused-feature audit on the root graph's
   heaviest hires**: still not run — **eleventh pass now** flagging this as
   the oldest open item (first flagged 2026-09-07).
2. **`fuzz/` and `examples/island-flock` scheduled-batch ownership**: still
   no standing cadence of their own; `fuzz/` got real attention again this
   pass (a second version bump in two passes), `island-flock` did not
   (nothing in this pass's trigger touched it, and it was independently
   confirmed to have no `url` dependency at all).
3. **GitHub-native Dependabot alert count/breakdown**: still not readable in
   this environment — seventh pass flagging the gap, now also missing
   direct `gh` API access (this environment only exposes the GitHub MCP
   tools and a code-search-only `gh api`).
4. **`RUSTSEC-2026-0387` and `RUSTSEC-2023-0071` now share one, single,
   mechanical unblock condition**: `crypto-glue` (via `webauthn-rs`)
   releasing a version that drops its `rsa = "^0.10.0-rc.18", features =
   ["sha2"]` pin in favor of the final 0.10.0 release. The next pass to hit
   either review-by date needs only to re-check `crypto-glue`'s published
   versions (`curl -sS https://index.crates.io/cr/yp/crypto-glue`) — if a
   release past 0.2.2 drops the RC pin, both advisories likely clear in one
   `jsonwebtoken` + `rsa` + `ed25519-dalek` bump (rehearsal sketch: raise
   `jsonwebtoken` to 12.x, `ed25519-dalek` to 3 matching its new
   `rust_crypto` requirement, and autumn's own direct `rsa` pin to the final
   0.10.0 to avoid a three-way split).
5. **This environment had no `cargo-deny` installed**, unlike every prior
   pass's environment. Installed it ad hoc this pass
   (`cargo install --locked cargo-deny@0.20.2`, matching the version CI
   pins via `taiki-e/install-action`). Not a repo issue — CI's own
   `supply-chain` job is unaffected — but worth noting in case it recurs:
   a Ballast pass that skips this step because the binary silently "isn't
   found" rather than erroring loudly would silently skip every advisory
   check in this report.
