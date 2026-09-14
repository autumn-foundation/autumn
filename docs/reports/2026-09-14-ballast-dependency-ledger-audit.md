# ⚓ Ballast: weekly dependency ledger audit, 2026-09-14

Second Ballast pass in this repo, one week after the first
(`docs/reports/2026-09-07-ballast-dependency-ledger-audit.md`, merged as
#2612). That pass built the harness — five audited graphs (root workspace,
SQLite backend, scaffold day-one, `fuzz/`, `examples/island-flock/`) and left
four follow-ups. This pass reruns the harness end to end, re-verifies every
follow-up, and reports what changed. **No ledger change clears the impact
floor this pass** — report only, per "Acceptable outcomes" #2.

## 🎯 Class

Ledger report. No dependency added, removed, bumped, or repinned.

## 📈 Evidence

**Harness re-run, all five graphs, `cargo-deny 0.20.2`** (the exact version
`ci.yml`'s `supply-chain` job pins; installed from the pinned GitHub release
for this pass, matching CI's own `taiki-e/install-action` source):

```
./scripts/check-advisories.sh                 → OK, 0 unwaived advisories, all 5 graphs
./scripts/check-advisories.sh --self-test     → OK, RUSTSEC-2020-0071 blocked then waived, all 4 policies
cargo deny check licenses sources             → licenses ok, sources ok  (root graph)
cargo deny --config deny-sqlite.toml check licenses sources → licenses ok, sources ok  (SQLite graph)
cargo deny check bans                         → bans ok (warn-level; not CI-gated, see below)
```

Identical outcome to last week's pass on every check. Reachability status of
the three existing root-graph waivers, independently re-checked against
current upstream state (not taken on faith from the pin comment):

| RUSTSEC id | Crate | Last week | This week | Changed? |
| --- | --- | --- | --- | --- |
| RUSTSEC-2023-0071 | `rsa` | max stable 0.9.10, no fix | max stable still 0.9.10 (`0.10.0-rc.18` exists but is a release candidate, not stable) | No |
| RUSTSEC-2024-0384 | `instant` | max 0.1.13, unmaintained | max still 0.1.13, unmaintained | No |
| RUSTSEC-2026-0253 | `lru` (via `aws-sdk-s3`) | pinned `aws-sdk-s3` 1.122.0; 1.123.0+ needs `rust-version` 1.91.0/1.94.1 > our 1.88.0 floor | confirmed via `cargo info aws-sdk-s3@1.123.0`/`@1.146.1`: still 1.91.0 / 1.94.1 respectively; 1.122.0 is still the newest MSRV-compatible release | No |

No code touching `autumn-storage-s3`/`autumn-media-plugin`'s S3-cache path
(the reachability argument for the `lru` waiver: "cache keys are `String`,
`pop()` is never called") landed since last week
(`git log --since=2026-09-08 -- autumn-storage-s3/ autumn-media-plugin/`:
empty). All three waivers' **review-by 2026-10-01** stands, 17 days out — not
due this pass.

**Graph facts, root workspace** (via `cargo deny list --format json`, same
methodology as last week's baseline):

| Metric | 2026-09-07 | 2026-09-14 | Δ |
| --- | --- | --- | --- |
| Unique crate@version nodes | 743 | 791 | +48 |
| Unique crate names | 665 | 704 | +39 |
| Direct (non-dev) deps referenced by workspace members | 131 | 134 | +3 |
| Workspace members | 28 | 33 | +5 |
| Duplicate crate names (`cargo deny check bans`, warn-level) | 73 | 76 | +3 |

The growth tracks real feature landings this week, not scope creep of the
audit itself: `git log` shows mTLS client-cert auth (#1640/#2703),
build-checked service-to-service typed contracts (#1755/#2729), and new
workspace members for billing/search/dunning work. None of it introduced an
advisory, a new license class, or a wildcard/git-ref pin (re-checked, see
below) — the ledger got heavier by ~5% but stayed clean.

**Duplicate-version list re-examined** (76 names, up from 73): still
dominated by the two categories `deny.toml`'s own comment names — the
RustCrypto 0.9/0.10-era split (`aes`, `base16ct`, `block-buffer`, `cipher`,
`const-oid`, `crypto-bigint`, `crypto-common`, `der`, `digest`, `ecdsa`,
`elliptic-curve`, `ff`, `group`, `hmac`, `inout`, `md-5`, `p256`, `pkcs8`,
`rfc6979`, `sec1`, `sha2`, `signature`, `spki` — 23 names) and per-target
`windows-sys`/`windows_*` shims (8 names) — but a **third, uncounted category**
is now large enough to name explicitly: general ecosystem major-version splits
unrelated to either (`base64`, `bitflags`, `darling`/`darling_core`/
`darling_macro`, `getrandom`, `hashbrown`, `http`, `http-body`, `lru`,
`parking_lot`/`parking_lot_core`, `phf`/`phf_shared`, `rand`/`rand_chacha`/
`rand_core`, `reqwest`, `strum`/`strum_macros`, `syn`, `thiserror`/
`thiserror-impl`, `toml`/`toml_datetime`, `tower-http`, `tungstenite`,
`winnow`, and others — 45 names). This category was already the majority of
the 73 last week too; the header comment's "dominated by RustCrypto and
windows-sys" undersells it. Not a new finding this pass and not actioned
(`multiple-versions = "warn"`, deliberately not CI-gated, per `deny.toml`'s
own rationale) — recorded so a future pass doesn't have to re-derive the
breakdown, and flagged as a candidate line to tighten in `deny.toml`'s own
comment the next time that file is touched for an unrelated reason.

**Scheduled batch — corrected finding, all three graphs, 0 packages move**:

```
cargo update --dry-run --workspace                              (root)      → Locking 0 packages
(cd fuzz && cargo update --dry-run --workspace)                             → Locking 0 packages
(cd examples/island-flock && cargo update --dry-run --workspace)           → Locking 0 packages
```

Worth recording precisely because this pass initially got a **false positive**
on the two satellite graphs: `cargo update --dry-run --verbose` (no
`--workspace` flag) against `fuzz/` and `island-flock/` reported ~80 and ~29
would-be package changes respectively, with no "requires Rust X" annotation —
looking exactly like real batch material. Re-running with `--workspace`
(matching the flag the root-graph check has always used, and matching what an
actual `cargo update --workspace` does) collapsed both to **0 packages,
"Locking 0 packages to latest Rust 1.88.0 [resp. 1.94.1] compatible
versions."** The bare `--dry-run` invocation, without `--workspace`, does not
apply the same rust-version-aware resolution the scoped/real command does, and
so previews upgrades neither `cargo update --workspace` nor a real
unattended run would ever select. Recorded here so the next pass runs the
satellite dry-runs with `--workspace` from the start rather than re-deriving
this. **Net result: a scheduled batch opened today would carry an empty
lockfile diff on all three graphs** (root, `fuzz/`, `island-flock/`) — same
conclusion as last week for the root graph, now confirmed for the two
satellites too.

**Supply-chain facts, re-verified**: zero wildcard version ranges and zero
unpinned git refs anywhere in the tree (`grep -rn 'version = "\*"'` /
equivalent `git = "` scan, main workspace + both satellites — both empty,
unchanged from last week).

**Pain ledger**: still no dependency-only commit or reverted upgrade anywhere
in this repo's history besides the two Ballast passes themselves
(`git log --all --oneline | grep -i ballast` → 1 hit, this pass's predecessor).

## 💡 Mechanism / forcing fact

None. Every Tier-1 check reruns clean, every existing waiver's underlying
fact is unchanged, and the corrected scheduled-batch dry-run confirms zero
lockfile movement is available anywhere in the tree. Per the charter, "staying
current" and "it's been a week" are not forcing facts — a report, not a bump,
is the correct outcome when nothing crosses the floor.

## 🔧 Change

None to the dependency graph. This report is the only artifact.

## 📊 Measurement

| Check | 2026-09-07 | 2026-09-14 |
| --- | --- | --- |
| Unwaived advisories, all 5 graphs | 0 | 0 |
| Advisory gate self-test (4 policies) | OK | OK |
| Root/SQLite licenses + sources | clean | clean |
| Crate@version nodes / names / direct deps (root) | 743 / 665 / 131 | 791 / 704 / 134 |
| Workspace members | 28 | 33 |
| Duplicate crate names (warn-level) | 73 | 76 |
| Scheduled batch, root graph | 0 packages | 0 packages |
| Scheduled batch, `fuzz/` graph | not checked | 0 packages (confirmed with `--workspace`) |
| Scheduled batch, `island-flock/` graph | not checked | 0 packages (confirmed with `--workspace`) |
| Wildcard ranges / unpinned git refs | 0 / 0 | 0 / 0 |
| Existing waivers still valid on re-check | 3/3 | 3/3 |

## 🔬 Reproduce

```
cargo fetch --locked
(cd fuzz && cargo fetch --locked)
(cd examples/island-flock && cargo fetch --locked)
./scripts/check-advisories.sh
./scripts/check-advisories.sh --self-test

cargo deny check licenses sources
cargo deny --config deny-sqlite.toml check licenses sources
cargo deny check bans
cargo deny list --format json

# scheduled-batch check — use --workspace on every graph, satellites included
cargo update --dry-run --workspace
(cd fuzz && cargo update --dry-run --workspace)
(cd examples/island-flock && cargo update --dry-run --workspace)

# waiver spot-checks
cargo info rsa
cargo info instant
cargo info aws-sdk-s3@1.123.0
```

## Follow-ups still open (unchanged from last week, not actioned this pass)

1. Waivers sharing the **2026-10-01** review-by date (root `deny.toml`'s
   three, plus `fuzz/deny.toml`'s `RUSTSEC-2023-0071`): re-checked this pass
   and all still hold; not yet due. Revisit properly at that date.
2. `examples/island-flock/deny.toml`'s `RUSTSEC-2025-0141` (`bincode`,
   `yew`-internal, "undetermined" not "unreachable") is still unresolved —
   `build-island.sh` has not rerun since last week
   (`git log --since=2026-09-08 -- examples/island-flock examples/flock/
   static/islands`: empty besides an unrelated `deny.toml` rewrite), so
   there has been no natural point to inspect the compiled `.wasm`'s
   retained symbols. Still open.
3. The NCSA-via-`libfuzzer-sys` license-class decision for `fuzz/deny.toml`
   is still an open human "ask before" question — no decision recorded
   anywhere in `CHANGELOG.md`, `deny.toml`, or `fuzz/deny.toml` since last
   week. Both satellite `deny.toml`s remain advisories+sources only.
4. Cost attribution (`cargo build --timings`) and a usage/unused-feature
   audit on the root graph's heaviest hires: still not run. Deferred again
   this pass — a full instrumented workspace build is expensive relative to
   this pass's finding (nothing else moved), and is better spent once there
   is a specific candidate hire to attribute cost to rather than as a
   blanket sweep.
