# ⚓ Ballast: first dependency ledger audit, 2026-09-07

First Ballast pass in this repo (no prior run found: no
`docs/reports/*ballast*`, no `Ballast`-grep hit in `git log --all`, no
`chore(deps)`-style commit ever merged here). The dependency harness itself —
`deny.toml`, `deny-sqlite.toml`, `scripts/check-advisories.sh`, the `autumn
doctor` `dependencies` check from #1633 — already exists and is unusually
mature. This pass started as the ledger audit that harness enables, and found
a real, evidenced gap in the harness's own coverage; the shipped result is a
**Harness PR** that closes it, not a dependency bump.

## 🎯 Class

Policy / Harness. Target: `scripts/check-advisories.sh`'s audit scope, plus
two new satellite `deny.toml` files (`fuzz/deny.toml`,
`examples/island-flock/deny.toml`).

## 📈 Evidence

**How the gap was found.** `git push` to this repo's remote reported, from
GitHub itself: *"GitHub found 15 vulnerabilities on
autumn-foundation/autumn's default branch (2 high, 9 moderate, 4 low)."* That
number does not square with `./scripts/check-advisories.sh` passing clean —
GitHub's dependency graph scans **every `Cargo.lock` in the repo**, not just
the one `cargo deny` is pointed at. `find . -name Cargo.lock -not -path
"*/target/*"` turns up three:

```
./Cargo.lock                        — the root workspace, audited (deny.toml + deny-sqlite.toml)
./fuzz/Cargo.lock                   — NOT audited anywhere
./examples/island-flock/Cargo.lock  — NOT audited anywhere
```

Both are deliberately their own workspace roots — `Cargo.toml`'s
`[workspace] exclude = ["fuzz", "examples/island-flock"]`, each with its own
`[workspace]` table and comment explaining why (`fuzz` needs a nightly/ASAN
build cargo-fuzz owns; `island-flock` only builds for
`wasm32-unknown-unknown`) — so nothing in the existing `deny.toml`,
`deny-sqlite.toml`, or the scaffold audit ever resolves their graphs. That
exclusion is the right call for the *build*; it was never meant to exempt
either tree from the *advisory* gate, and nothing in `scripts/
check-advisories.sh` or `ci.yml` says so — they are just silently absent.
Both are real, shipped surface, not dead code:

- **`fuzz/`** is compiled and executed by `.github/workflows/fuzz.yml` on
  every push/PR to `trunk`/`trunk-dev` (7 targets: idempotency, routing,
  headers, session, body, dns, sandbox) — a production CI dependency graph
  with zero supply-chain gating.
- **`examples/island-flock/`** is never built in CI (`build-island.sh` is a
  documented local/manual step), but its compiled output — 3 files,
  `examples/flock/static/islands/{autumn_island_flock.js, ..._bg.wasm,
  flock-boot.js}` — **is committed to the repo** and served by the `flock`
  example, which the main workspace does build and test. Whatever advisory
  state that manual build had at commit time is what real browsers execute
  today.

**Auditing both graphs for the first time**, using the same `cargo-deny
0.20.2` binary CI pins (installed here from its GitHub release, since neither
`cargo-deny` nor `cargo-audit` was preinstalled in this sandbox):

**`fuzz/`** — two real findings, both fixed in this PR:

1. `fuzz/Cargo.lock` was **502 lines stale** against `fuzz/Cargo.toml`
   (`cargo fetch --locked` refused to run: "cannot update the lock file...
   because --locked was passed"). Last touched at commit `f29d4b4`
   ("Run untrusted plugins in a capability-sandboxed WASM runtime", #1609) —
   every dependency `autumn-web` gained since then (this crate pulls
   `autumn-web` as a path dependency with `features = ["inbound-mail",
   "multipart", "openapi", "acme", "plugin-sandbox"]`) was undeclared in the
   lockfile. **Fixed**: regenerated via `cargo fetch` (network), then
   rehearsed — `RUSTFLAGS="--cfg fuzzing" cargo +nightly check --workspace`
   from `fuzz/` compiles clean (the `fuzzing` cfg is what `cargo fuzz` itself
   sets; a plain `cargo check` doesn't, so it's needed to reach
   `autumn::__fuzz`, the `#[cfg(fuzzing)]`-gated module the fuzz targets
   import). The stale lockfile also carried a **yanked** `chacha20 0.10.1`
   (via `rand 0.10.2 -> postgres-protocol -> tokio-postgres -> autumn-web`);
   `cargo update -p chacha20` moved it to `0.10.2`, matching what the main
   workspace already resolved to.
2. With the refreshed lockfile, `cargo deny check advisories` finds
   **RUSTSEC-2023-0071** (the `rsa` "Marvin Attack" timing sidechannel) —
   same crate, same `jsonwebtoken -> autumn-web` ingress the root `deny.toml`
   already carries a waiver for. Confirmed none of the 7 fuzz targets touch
   JWT (`grep -ln jwt fuzz_targets/*.rs` — empty). **Waived** in the new
   `fuzz/deny.toml` with the same reasoning as the root policy, review-by
   2026-10-01 to match.

**`examples/island-flock/`** — two `unmaintained` findings, both triaged
with a real reachability call, neither with a safe upgrade available:

1. **RUSTSEC-2024-0370** (`proc-macro-error`, via `yew-macro`). `cargo tree
   -i proc-macro-error` marks it `(proc-macro)` — it runs only on the host
   compiler during the build and cannot be linked into the
   `wasm32-unknown-unknown` output. **Confirmed unreachable.**
2. **RUSTSEC-2025-0141** (`bincode` 1.3.3, "unmaintained" after a maintainer
   harassment/doxxing incident — no CVE, the team calls 1.3.3 complete).
   Arrives via `gloo-worker -> gloo -> prokio -> yew`, i.e. `yew`'s own
   internal runtime plumbing, not anything `island-flock`'s source touches
   (`grep -rn gloo_worker src/` is empty). `cargo tree -i bincode` roots at
   `yew` itself. **Reachability left undetermined, not claimed unreachable**:
   confirming that plumbing is dead-code-eliminated from the actual compiled
   `.wasm` would need a symbol-table inspection of the built artifact, not
   done this pass. Waived with that caveat stated plainly and a revisit
   trigger tied to the crate's next rebuild, rather than a confident claim
   this pass didn't earn.

Both licenses were checked too, deliberately **not gated yet**:
`cargo deny check licenses` on `fuzz/`'s graph fails on `libfuzzer-sys`,
which carries `(MIT OR Apache-2.0) AND NCSA` — an unconditional (not
OR-satisfied) NCSA component, a license class not on the root allow-list and
new to this tree. Accepting a new license class is explicitly an "ask
before" decision, not something this pass should decide unilaterally by
adding NCSA to an allow-list. `fuzz/deny.toml` and `examples/island-flock/
deny.toml` therefore each gate only `advisories` and `sources` (both
independently confirmed clean for both graphs), with the license question
left open and stated in both files' headers and here.

**The rest of the ledger** — re-verified clean, unrelated to the gap above:

- **Root workspace advisories** — 0 unwaived. The 3 existing `deny.toml`
  ignores (`RUSTSEC-2023-0071` rsa, `RUSTSEC-2024-0384` instant,
  `RUSTSEC-2026-0253` lru) were independently re-checked against current
  upstream state rather than taken on faith: `rsa` max stable is still
  0.9.10 (no fix), `instant` max stable is still 0.1.13 (unmaintained since
  2024, no fix), and `aws-sdk-s3` max stable is now 1.145.0 but still
  incompatible with this workspace's `rust-version = 1.88.0` floor — the
  existing `>=1.122, <1.123` pin in `autumn-storage-s3`/`autumn-media-plugin`
  is still the last MSRV-compatible release. All three share a 2026-10-01
  review-by date, 24 days out — not yet due.
- **Licenses/sources, root + SQLite graphs** — clean
  (`cargo deny check licenses sources`, both configs). One non-obvious pass
  worth recording: 18 raw SPDX identifiers appear in the tree via `cargo deny
  list`, but only 17 are on the allow-list — the 18th, `LGPL-2.1-or-later`,
  belongs solely to `r-efi`'s `MIT OR Apache-2.0 OR LGPL-2.1-or-later`
  expression and is satisfied by the `MIT`/`Apache-2.0` branch. Correct
  cargo-deny behavior, not a policy gap; recorded so a future pass doesn't
  re-flag it.
- **Wildcards / unpinned git refs** — zero, anywhere (`grep -rn 'version =
  "\*"' --include=Cargo.toml .` and the equivalent for `git = "` both empty
  across the whole repo, main workspace and both satellites).
- **Duplicate versions** — 73 crate names duplicated in the root audited
  graph (`cargo deny check bans`), dominated by a RustCrypto 0.9/0.10 split
  and per-target `windows-sys` shims. Matches `deny.toml`'s own existing
  rationale ("pervasive and cosmetic... RustCrypto old/new, windows-sys
  target shims") verbatim. No collapse here clears a ≥3-node subtree without
  picking a side in an unrelated upstream major-version split — left as-is,
  a forcing-fact question for a future `Upgrade`-class PR.
- **Scheduled batch** — `cargo update --dry-run --workspace --verbose` locks
  **0 packages** in the root graph; every dependency already sits at the
  newest version its manifest constraint permits (the ~121 "behind latest"
  entries are each blocked by an upstream pre-release pin or this
  workspace's own MSRV floor). A batch PR opened today would carry an empty
  root-lockfile diff.
- **Pain ledger** — no prior Ballast run and no prior dependency-only commit
  exist anywhere in this repo's history.

**Graph facts** (root workspace, `deny.toml`'s own audited feature graph):
743 unique crate@version nodes, 665 unique crate names, 131 direct
(non-workspace) dependencies across the 28 workspace members.

## 💡 Mechanism / forcing fact

Two independent, mechanically-verified facts, both about coverage rather
than any single dependency:

1. GitHub's own dependency graph — a signal external to this repo's own
   tooling — reported vulnerabilities this repo's advisory gate structurally
   cannot see, because two real, CI-relevant dependency graphs live outside
   every config path `scripts/check-advisories.sh` walks.
2. Once audited, both graphs had real, non-trivial findings: a lockfile 502
   lines stale carrying a yanked crate, and two `unmaintained` advisories
   that would fail the same `unmaintained = "all"` policy the root graph
   holds itself to.

That is a forcing fact for closing the gap now, not filing it as a future
recommendation: "the harness is the deliverable" applies exactly here, and
per the impact floor, a **supply-chain fact fixed** (a stale/yanked lockfile
repaired, two previously-unaudited graphs brought under the same gate the
rest of the repo already trusts) clears it on its own.

## 🔧 Change

- `fuzz/Cargo.lock` — regenerated (502 insertions / 6 deletions), no longer
  resolves the yanked `chacha20 0.10.1`.
- `fuzz/deny.toml` — new. Advisories + sources only (see licenses note
  above); one reasoned, review-dated waiver (`RUSTSEC-2023-0071`, mirroring
  the root policy).
- `examples/island-flock/deny.toml` — new. Advisories + sources only; two
  reasoned, review-dated waivers (`RUSTSEC-2024-0370` unreachable,
  `RUSTSEC-2025-0141` undetermined).
- `scripts/check-advisories.sh` — new `audit_satellite_graphs` step (plus
  the matching `cargo fetch --locked` for each satellite manifest before the
  offline checks run), wired into `run_gate`; header comment updated from
  "three graphs" to "five graphs" with the two new ones documented inline.
  Runs automatically in CI — `ci.yml`'s `supply-chain` job already calls
  this script with no changes needed there.
- `deny.toml` — one-line pointer added to the header noting the two
  satellite configs exist and where.
- `CHANGELOG.md` — `Unreleased`/`Added` entry summarizing the gap and the fix
  for anyone tracking CI/dev-tooling changes.

Not shipped, and deliberately left for a human decision rather than decided
here: gating **licenses** on either satellite graph, because doing so today
would require accepting `libfuzzer-sys`'s NCSA license component into the
tree — a new license class, which this charter's "ask before" list puts
outside this pass's authority to decide unilaterally.

## 📊 Measurement

| Check | Before | After |
| --- | --- | --- |
| Graphs audited by `scripts/check-advisories.sh` | 3 | 5 |
| `fuzz/` advisory coverage | none (no config referenced this graph) | `cargo deny check advisories sources` — clean, 1 reasoned waiver |
| `examples/island-flock/` advisory coverage | none | clean, 2 reasoned waivers |
| `fuzz/Cargo.lock` staleness | 502 lines behind `fuzz/Cargo.toml` | current |
| Yanked crates in `fuzz/`'s graph | 1 (`chacha20 0.10.1`) | 0 |
| Unmaintained/unsound advisories in `island-flock`'s graph | 2, unaudited | 2, triaged (1 confirmed unreachable, 1 undetermined) |
| Root workspace: unwaived advisories / licenses / sources | 0 / clean / clean | unchanged (re-verified, not touched) |
| `fuzz` rehearsal | — | `RUSTFLAGS="--cfg fuzzing" cargo +nightly check --workspace` from `fuzz/`: clean |
| `island-flock` rehearsal | — | `cargo check --target wasm32-unknown-unknown` from `examples/island-flock/`: clean |
| Full gate | `./scripts/check-advisories.sh`: OK (3 graphs) | `./scripts/check-advisories.sh`: OK (5 graphs) |
| Self-test | `./scripts/check-advisories.sh --self-test`: OK | unchanged, still OK (root + scaffold policies; not extended to the two new satellite configs this pass) |

## 🔬 Reproduce

```
cargo fetch --locked
(cd fuzz && cargo fetch --locked)
(cd examples/island-flock && cargo fetch --locked)
./scripts/check-advisories.sh                 # now audits all 5 graphs
./scripts/check-advisories.sh --self-test

# per-graph, standalone
(cd fuzz && cargo deny --offline check advisories sources)
(cd examples/island-flock && cargo deny --offline check advisories sources)

# rehearsals
(cd fuzz && RUSTFLAGS="--cfg fuzzing" cargo +nightly check --workspace)
(cd examples/island-flock && cargo check --target wasm32-unknown-unknown)

# root-graph re-verification (unchanged by this PR)
cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources
cargo deny check bans
cargo deny list --format json
cargo update --dry-run --workspace --verbose
```

## Follow-ups for the next pass

1. Revisit all advisory ignores sharing the **2026-10-01** review-by date
   (root `deny.toml`'s three, plus `fuzz/deny.toml`'s one) then; re-check
   upstream `rsa`, `instant`, and `aws-sdk-s3`/`lru` status.
2. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` waiver is honest
   about being undetermined, not unreachable — the next time `island-flock`'s
   wasm output is rebuilt (`build-island.sh`) is the natural point to inspect
   the artifact's retained symbols and firm up that verdict.
3. A human decision is needed on whether to accept NCSA (via `libfuzzer-sys`)
   as a new allowed license class for `fuzz/`'s graph — until then, `fuzz/`
   and `island-flock`'s `deny.toml`s stay advisories+sources only.
4. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires were still not run this pass —
   this pass's scope was the coverage-gap fix plus the advisory/license/
   source/duplicate/pin sweep.
