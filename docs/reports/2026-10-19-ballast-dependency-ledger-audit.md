# ⚓ Ballast: dependency ledger audit, 2026-10-19

Sixth Ballast pass, and the first gap in the weekly cadence: the prior pass
(`docs/reports/2026-10-05-ballast-dependency-ledger-audit.md`) ran
2026-10-05, so this pass covers a two-week window rather than one. That gap
matters for this report's graph-fact numbers (see Measurement) — two weeks
of unrelated feature work (`aeo`, `mcp` progressive discovery, `fleet` CLI,
`posture/ceiling`, and more) moved the dependency graph by more than any
single week's drift would, and this pass does not attempt to re-derive the
prior reports' exact counting methodology closely enough to produce a
trustworthy week-over-week delta. That is logged as a follow-up, not papered
over with a number that looks precise but isn't comparable.

This pass found and fixed a real bug in the audit harness itself (the
gate silently never ran outside CI), then re-ran the full harness clean,
then triaged the Dependabot queue, which had regrown from 0 (last pass's
first-ever empty queue) to 12 open PRs.

## 🎯 Class

Policy / harness fix, primary: `scripts/check-advisories.sh`. Secondary
(acted on directly, not staged as a PR): Dependabot queue triage — 3 merges,
1 close-as-superseded, 1 rebase request, 6 left open with no forcing fact.

## 📈 Evidence

**The harness bug.** Running `./scripts/check-advisories.sh` in this
session failed immediately on the first generated-app flavor:

```
=== generated app advisories (autumn new <default>) ===
[exited with code 1, no further output]
```

Reproduced manually: `audit_generated_apps()` pins each scaffolded app's
toolchain via `rustup toolchain install --no-update --profile minimal
<version>` when `rustup` is on `PATH`. `--no-update` is not a real `rustup`
flag:

```
$ rustup toolchain install --no-update --profile minimal 1.88.0
error: unexpected argument '--no-update' found
  tip: a similar argument exists: '--no-self-update'
```

introduced 4 days before this pass by #3317 ("ci: audit more scaffold
flavor combinations; skip toolchain update", merged 2026-10-15). It never
failed CI: `ci.yml`'s `supply-chain` job installs its toolchain via
`dtolnay/rust-toolchain@1.99.0`, which — confirmed by reading job
[117835759000](https://github.com/autumn-foundation/autumn/actions/runs/39181743342/job/117835759000)'s
logs directly — leaves no `rustup` binary on `PATH`, so the
`command -v rustup` guard that gates this code is false on every CI run and
the broken line is simply never reached there. It reaches it on any machine
where `rustup` genuinely is installed: this session's sandbox, and
presumably any contributor's normal dev box. **The gate's CI-green record
was never evidence this code path worked outside CI** — a latent bug sat in
the harness, undetected, for 4 days (and would have sat indefinitely, since
nothing in CI can ever exercise it).

Fix: `--no-update` → `--no-self-update` (the flag that actually exists and
does what the surrounding comment says it's for — skip rustup's own
self-update during the toolchain install). Verified end to end:

```
./scripts/check-advisories.sh          → advisories ok, all 5 graphs, exit 0
./scripts/check-advisories.sh --self-test → OK, RUSTSEC-2020-0071 blocked
                                             then waived, all 4 policies
cargo deny check licenses sources               → licenses ok, sources ok
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok
cargo deny check bans                           → 91 duplicate-name warnings (warn-level)
```

**The Dependabot queue, regrown from 0 to 12.** Last pass's own queue
clearance (11→0) was the first empty queue in five passes; two weeks later
it had regrown to 12 — 1 the weekly `rust-deps` scheduled batch opened
*today* (2026-10-19, this session's own run date), 7 individual major/minor
bumps from 2026-10-05 that Dependabot's `rust-deps` group skipped (grouping
only covers `minor`/`patch` update-types, per `dependabot.yml`), 1 GitHub
Actions patch bump, 1 npm patch bump in an example frontend, 1 superseded
security PR, and 1 security-relevant minor bump still open from the prior
window. Checked individually (`pull_request_read`), not inferred from the
open-PR count alone:

| PR | Title | Update type | Disposition | Evidence |
| --- | --- | --- | --- | --- |
| #3338 | `rust-deps` group, 14 updates | minor/patch | **Merged** | lockfile-only diff (`syn`, `rand`, `h2`, `hyper`, `libc`, `serde_json`, `tokio-util`, `zeroize`, `want`, `trybuild`, `shuttle*`, `wasmparser` family); CI green on exactly this head (run [39181743342](https://github.com/autumn-foundation/autumn/actions/runs/39181743342), includes `supply-chain` job) |
| #3283 | `taiki-e/install-action` 2.87.20→2.87.28 | patch | **Merged** | CI tooling only, CI green ([38799141703](https://github.com/autumn-foundation/autumn/actions/runs/38799141703)) |
| #3169 | `source-map-js` 1.2.1→1.2.2 (`examples/react-graphql/frontend`) | patch | **Merged** | fixes a real upstream DoS (CVE-2026-93749); CI green; outside the Rust graphs this harness audits, but low-risk and worth closing |
| #3333 | `ammonia` → 4.2.4 (RUSTSEC-2026-0373) | patch, security | **Closed, superseded** | `trunk-dev` already carries this exact fix (`Cargo.toml` pins `ammonia = "4.2.4"`, `changelog.d/ammonia-rustsec-2026-0373.md` already merged) via a different commit — diffed and confirmed byte-identical in effect before closing |
| #3146 | `css-inline` 0.21.2→0.22.0 | minor, security | **Commented, rebase requested** | upstream fix for GHSA-jh54-c7gc-mvcc (unescaped `class` attribute → markup injection); CI green but on a 12-day-stale base; asked `@dependabot rebase` so CI revalidates against current `trunk-dev` |
| #3145 | `wasmi` 0.40.0→2.0.0 | **major** | Left open | 4 consecutive CI failures, confirmed real break (see below), no forcing fact |
| #3147 | `croner` 3.0.1→4.0.1 | **major** | Left open | 3 consecutive CI failures, no forcing fact |
| #3142 | `brotli` 8.0.4→9.0.0 | **major** | Left open | `mergeable_state: unstable`, not re-diagnosed this pass |
| #3143 | `opentelemetry` 0.32.0→0.33.0 | **major** | Left open | `mergeable_state: unstable`, not re-diagnosed this pass |
| #3148 | `opentelemetry_sdk` 0.32.1→0.33.0 | **major** | Left open | `mergeable_state: unstable`, not re-diagnosed this pass |
| #3149 | `opentelemetry-otlp` 0.32.0→0.33.0 | **major** | Left open | `mergeable_state: unstable`, not re-diagnosed this pass |
| #3144 | `tracing-opentelemetry` 0.33.0→0.34.0 | **major** | Left open | `mergeable_state: unstable`, not re-diagnosed this pass |

**`wasmi` 2.0.0, confirmed as real breakage, not flake.** Dependabot
auto-rebased this PR 4 times (2026-10-05 13:34 through 2026-10-05 22:58);
every single run failed CI, identically. Pulled the job log for the latest
failure directly rather than trusting the red X:

```
error[E0599]: no method named `instantiate` found for struct `wasmi::Linker<T>` in the current scope
  --> autumn-edge/src/host.rs:245:14
...
error[E0603]: module `core` is private
   --> ... wasmi::core::TrapCode ...
```

`wasmi` 2.0 is a complete interpreter rewrite per its own migration guide
(new IR, new executor, `Linker` API changes); `autumn-edge/src/host.rs`'s
plugin-sandbox host code uses exactly the APIs that moved. This is real
migration work, not a Ballast-sized change, and carries no forcing fact (no
advisory requires 2.0 — `cargo deny check advisories` is 0-unwaived at
0.40.0 too). `croner` 4.0.1's own changelog documents equivalent breaking
changes (generic `CronDateTime` trait replacing `chrono::TimeZone` binding,
stricter step-syntax validation) consistent with its own 3-for-3 CI failure
record, though this pass did not pull its job log the way it did for
`wasmi`.

**`css-inline` reachability, assessed but not resolved.** `css-inline` is
used in exactly one place in this codebase:
`autumn/src/mail.rs` (the `mail` feature, HTML-email CSS inlining).
GHSA-jh54-c7gc-mvcc's fix is that `class` attribute values weren't
HTML-escaped on output. Whether this is reachable depends on whether any
app built on Autumn ever interpolates caller-controlled data into a `class`
attribute of a template passed through the mailer — not confirmed either
way this pass. Classed as **undetermined**, not **reachable**: the gate's
own three-way split (reachable / unreachable / undetermined) exists exactly
for this case, where "determining it is the task" and that task isn't
finished.

**GitHub-native Dependabot alert count — still 16, unchanged two passes
running.** This pass's own `git push` printed the same **16 vulnerabilities
(4 high, 10 moderate, 2 low)** the 2026-10-05 pass escalated. `cargo deny
check advisories` is still 0-unwaived across all 5 Rust graphs. One
plausible partial explanation surfaced this pass: #3146's `css-inline`
finding is a GHSA, not (yet) a RustSec advisory — `cargo deny` matches
against the RustSec database, so a GHSA-only finding would never show up in
this harness's unwaived-advisory count no matter how current the pinned
version is. Not confirmed as one of the 16 (no tool in this session lists
individual Dependabot alerts — searched again this pass; still nothing),
but it's the first concrete candidate mechanism offered in three passes of
flagging this gap.

**License and source checks**: clean, both graphs (`cargo deny check
licenses sources` on root and `deny-sqlite.toml`).

**Supply-chain facts**: not re-walked exhaustively this pass (budget spent
on the harness bug and the regrown queue); no new wildcard ranges or
unpinned git refs were introduced by anything this pass touched (a
lockfile-only grouped bump, two patch bumps, no manifest edits).

## 💡 Mechanism / forcing fact

**Harness fix**: a dependency-advisory gate that can only be proven to work
*inside* CI, and silently fails everywhere else, is not actually verified —
it's verified-shaped. The charter's VERIFY step treats harness correctness
as the precondition for every other claim in a Ballast report; this pass's
first finding was that the precondition didn't hold, so fixing it came
before anything else.

**Queue triage**: no forcing fact needed for the merges — each is a
routine minor/patch bump (or, for #3169, closes a real CVE) with CI green
on the head actually being merged, which is exactly what "the scheduled
batch" and ordinary hygiene PRs look like. The six left-open majors have
*no* forcing fact (no advisory, no EOL, no named consumer waiting on a new
feature) and real reproduced breakage for two of them, so leaving them
alone is the correct call under "majors ride forcing facts, never fashion"
— closing them with a dependabot ignore is the next correct step, but
scoping the actual `wasmi`/`croner` migrations first is real engineering
work this pass didn't attempt.

## 🔧 Change

- `scripts/check-advisories.sh`: one-line fix, `--no-update` →
  `--no-self-update`.
- Merged #3338, #3283, #3169. Closed #3333 as superseded. Commented and
  requested a Dependabot rebase on #3146. Left #3145, #3147, #3142, #3143,
  #3148, #3149, #3144 open and undecided.
- No dependency added, removed, or bumped by the harness-fix PR itself; the
  three merges above are lockfile/manifest changes already on `trunk-dev`
  via their own PRs, not folded into this one.

## 📊 Measurement

| Check | 2026-10-05 | 2026-10-19 |
| --- | --- | --- |
| `./scripts/check-advisories.sh` (local run, rustup present) | not tested outside CI | **was broken (exit 1); now passes clean, all 5 graphs** |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 90 | 91 |
| Open Dependabot PRs | 0 | 12 → **6** after this pass's triage |
| GitHub-native Dependabot alert count | 16 (4 high, 10 moderate, 2 low) | 16 (4 high, 10 moderate, 2 low) — unchanged |
| Waivers' review-by date (`deny.toml` ×3, `deny-sqlite.toml` ×3, `fuzz/deny.toml` ×1, `island-flock/deny.toml` ×2) | 2026-11-02 | 2026-11-02 — not yet due, not re-dated this pass |

Root-graph node/name/direct-dep counts are **not reported as a delta this
pass** — see the note at the top of this report and follow-up 2 below.

## 🔬 Reproduce

```
cargo install --locked cargo-deny@0.20.2   # matches ci.yml's taiki-e/install-action pin
./scripts/check-advisories.sh
./scripts/check-advisories.sh --self-test
cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources
cargo deny check bans

# wasmi failure reproduction
# (Dependabot's own CI runs: see run 37385797488, job logs for autumn-edge/src/host.rs:245 E0599/E0603)

# Dependabot queue (via GitHub MCP)
# search_pull_requests: repo:autumn-foundation/autumn is:open author:app/dependabot
# pull_request_read get/get_diff on each tracked PR number
# actions_list list_workflow_runs, filtered by each PR's head branch, for CI history
```

## Follow-ups still open

1. **GitHub-native Dependabot alert count: still 16, third pass running
   with no resolution.** This pass offers the first concrete partial
   mechanism (GHSA-only findings, like #3146's css-inline advisory, never
   show up in `cargo deny`'s RustSec-backed count) but does not confirm it
   against the actual 16. Still no tool in this session that lists
   individual alerts. Next pass: check whether a human has triaged this
   before re-deriving from scratch, same as last pass's note — unactioned
   two passes running now.
2. **Root-graph node/name/direct-dep counts need re-deriving carefully next
   pass.** This pass's two-week gap (rather than the usual one week) means
   a naive diff against the 2026-10-05 numbers would compare apples to
   oranges — two weeks of unrelated feature development moved the graph by
   more than drift alone would. Rather than publish a precise-looking
   number that isn't a trustworthy delta, this pass omits it. Next pass
   should re-establish the prior reports' exact counting methodology
   (`cargo tree` scoped to `autumn-web`'s additive-feature set, per
   `deny.toml`'s `[graph]` section) before reporting a comparison.
3. **`wasmi` 2.0.0 and `croner` 4.0.1 migrations**: real engineering work,
   scoped but not attempted this pass. `wasmi`'s break is pinned to exactly
   two call sites in `autumn-edge/src/host.rs` (`Linker::instantiate`,
   `wasmi::core::TrapCode`); `croner`'s is less precisely diagnosed (CI logs
   not pulled this pass, only its own changelog's documented breaking
   changes). Once scoped and migrated, both PRs should close with a
   `dependabot.yml` ignore entry added at the same time, same pattern as
   `matchit`/`dtolnay-rust-toolchain` from the 2026-10-05 pass — until then,
   closing them without a fix just means Dependabot reopens the same PR
   next week.
4. **`css-inline` reachability**: undetermined, not resolved. Determining
   whether any Autumn app's mail template puts caller-controlled data into
   a `class` attribute is the actual task; this pass only identified the
   one call site and the shape of the question.
5. `fuzz/` and `island-flock/` scheduled-batch ownership: still
   unaddressed, unchanged across six passes now.
6. The `cargo deny list` vs `cargo deny check bans` discrepancy (flagged
   2026-09-21): not revisited this pass; no new signal either way.
7. NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`:
   still open, unchanged since the first pass.
8. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run, sixth pass
   running.
