# ⚓ Ballast: dependency ledger audit, 2026-11-02

Eighth Ballast pass. Two things made this one different from the last seven:
the Dependabot queue, 9 PRs deep and un-triageable on 2026-10-26, is now
**empty** — a single consolidated PR (#3471, merged 2026-10-27, not a Ballast
PR) rolled almost all of it in with real rehearsal and a documented major-bump
deferral. And this pass finally ran the cost-attribution baseline that's been
the oldest open item on the ledger for seven passes running. Net result: no
hygiene PR was needed for the queue (there is no queue), so this pass's actual
work is a **documented-pin refresh** — three advisory waivers hit their
`review-by` date today and got re-verified with sharper evidence than they
shipped with — plus a small Dependabot config fix, plus the ledger report.

## 🎯 Class

Policy: scheduled `review-by` reconfirmation for three advisory waivers
(RUSTSEC-2023-0071, RUSTSEC-2024-0384, RUSTSEC-2026-0253) across `deny.toml`,
`deny-sqlite.toml`, `fuzz/deny.toml`, and the `autumn new` scaffold template —
plus a one-line fix to `.github/dependabot.yml`'s broken label references.
No dependency version changed. No PR opened against a (nonexistent)
Dependabot queue.

## 📈 Evidence

**The Dependabot queue is empty for the first time in eight passes.** `gh api
search/issues -f q='repo:autumn-foundation/autumn is:open is:pr
author:app/dependabot'` → 0. All 9 PRs tracked on 2026-10-26 (#3150, #3142,
#3144, #3145, #3147, #3353, #3354, #3355, #3356) closed within a day of that
report, each with a bot comment: "Looks like `<dep>` is up-to-date now, so
this is no longer needed." Tracing the actual version bumps (shallow clone
only had 50 commits of history on session start; deepened with `git fetch
--deepen=200` before trusting any diff) found the cause: **#3471**,
"dependency refresh for 0.9.0 (all open Dependabot bumps) + drop
unsafe-libyaml waiver," merged 2026-10-27, authored outside this routine.
It is a model execution of the scheduled-batch class — worth recording
because it closes four of last pass's five follow-ups at once:

- Ported `getrandom` 0.2→0.4 (`getrandom` → `fill`), `croner` 3→4
  (`sloppy_ranges(true)` to keep 3.x shorthand-step schedules working), and
  `css-inline` 0.21→**0.23.1** (further than the open PR's 0.22 target).
- Rolled the OpenTelemetry trio 0.32→0.33.1 and `tracing-opentelemetry`
  0.33→0.34 together (the three-crate lockstep the individual Dependabot PRs
  couldn't do one at a time).
- **Did not take `wasmi` 0.40→2.0**, with hard evidence, not a vibe: ported it
  first, green on Linux, then Windows CI hit `STATUS_ACCESS_VIOLATION` — a
  bad instance-pointer read inside `Caller::get_export`, reproduced outside
  the workspace with a one-import guest cross-built for
  `x86_64-pc-windows-gnu` under wine, root-caused to wasmi 2.0 miscompiling
  when built at a different opt-level than its caller (this workspace pins
  `[profile.dev.package.wasmi] opt-level = 3`). Undefined behaviour in the
  interpreter that runs untrusted plugins doesn't ship; `wasmi` stays at
  0.40. This is Acceptable Outcome #4 (documented pin, with a migration
  sketch for whoever retries it) executed correctly by someone else's PR.
- Cleared **RUSTSEC-2024-0449** (`unsafe-libyaml`, unmaintained) by swapping
  `serde_yaml` for `serde-saphyr` 1.1 — the newest line that still builds on
  the 1.88 MSRV.
- Duplicate crate-version count (warn-level, `cargo deny check bans`):
  **91 → 87**.

So four of 2026-10-26's five follow-ups (css-inline, wasmi, croner, cost
attribution is the fifth and is handled below) are closed. The CI-health
follow-up (Dependabot's "Test suite" aggregator reporting skipped-shard
failures under the bot's restricted token) is **moot while the queue is
empty**, not fixed — #3471 sidestepped it entirely by porting the code
directly into one PR run under full permissions, rather than merging
Dependabot's own PRs. The next time the queue has anything in it, that CI
signal still needs checking before trusting it.

**Baseline re-run, both graphs, clean:**

```
cargo deny --offline check advisories                                # advisories ok
cargo deny --offline --config deny-sqlite.toml check advisories      # advisories ok
cargo deny --offline check licenses sources                          # licenses ok, sources ok
cargo deny --offline --config deny-sqlite.toml check licenses sources # licenses ok, sources ok
```

Zero warnings on both — no repeat of 2026-10-26's duplicate-ignore-entry
surprise.

**Three `review-by` dates came due today (2026-11-02) across four files.**
`grep -n review-by deny.toml deny-sqlite.toml` surfaced RUSTSEC-2023-0071
(rsa), RUSTSEC-2024-0384 (instant), and RUSTSEC-2026-0253 (lru via
aws-sdk-s3), all dated `2026-11-02`, plus matching copies in `fuzz/deny.toml`
(rsa only) and `autumn-cli/src/templates/deny.toml.tmpl` (rsa + instant). Each
was re-verified against fresh evidence rather than rubber-stamped:

1. **RUSTSEC-2023-0071 (rsa, Marvin Attack timing sidechannel).** The
   advisory DB's own file (`~/.cargo/advisory-dbs/.../rsa/RUSTSEC-2023-0071.md`)
   still carries `patched = []` and an author note confirming rsa 0.9.10 and
   0.10.0-rc.18 are both still affected as of 2026-09-12 — unchanged. What
   changed is the exposure analysis: the existing waiver reason said "RSA-JWT
   path only via jsonwebtoken," but `grep -rn rsa:: autumn/src` turned up a
   **second, undocumented call site**: `autumn/src/inbound_mail.rs`'s
   `verify_rsa_signature()`, which uses `rsa::RsaPublicKey::from_public_key_der`
   and `Pkcs1v15Sign` to check AWS SES/SNS notification signatures on inbound
   mail. Read the function (lines 221-258): it is `public_key.verify(...)` —
   a public-key operation — same as the jsonwebtoken path, which (confirmed by
   grepping for `EncodingKey::from_rsa_pem` / `RsaPrivateKey` — zero hits) is
   also verify-only. The Marvin Attack recovers a private key through its own
   timing sidechannel; with **no RSA private-key operation anywhere in the
   codebase**, there is no oracle to time, which is a stronger and more
   complete verdict than the old text stated, not a different one. Reason
   text and the scaffold's copy both corrected; review-by moved to
   2026-12-02.
2. **RUSTSEC-2024-0384 (`instant`, unmaintained).** Confirmed the ingress
   chain is unchanged (`cargo tree -p autumn-web --all-features --target=all
   -i instant`): `instant → parking_lot 0.11.2 / wasm-timer → reqwest-retry
   0.7.0 → postgresql_archive 0.19.0 → postgresql_embedded 0.19.0 →
   autumn-web` (build-dependency, `managed-pg-bundled` feature only). Checked
   whether a newer `postgresql_embedded` drops the chain: the sparse index
   (`index.crates.io/po/st/postgresql_embedded`) shows the latest is
   **0.20.1**, `rust_version = "1.94.0"` — above this workspace's 1.88.0
   MSRV. The old reason said "≥0.20 needs Rust 1.89+"; the real number is
   higher and the gap widened rather than closed. Review-by moved to
   2026-12-02.
3. **RUSTSEC-2026-0253 (`lru::LruCache::pop()` panic-unsafety via
   aws-sdk-s3).** The deliberately-pinned `aws-sdk-s3 >=1.122, <1.123` (grep
   in `autumn-storage-s3/Cargo.toml` and `autumn-media-plugin/Cargo.toml`
   confirms the pin is still in place) resolves `lru 0.16.4`, pre-fix.
   Walked the full sparse-index version history for `aws-sdk-s3` from 1.122
   to 1.157 (current latest) cross-referencing `rust_version` against the
   `lru` requirement at every version: MSRV rises 1.88.0→1.91.0 at 1.123.0,
   then →1.94.1 at 1.138.0, and **only at 1.144.0** does the `lru`
   requirement move to `^0.18.2` — at the *same* 1.94.1 floor 1.138.0 already
   needed, not a lower one. There is no aws-sdk-s3 release that fixes `lru`
   without also demanding rustc 1.94.1, so this is still a pure MSRV
   trade-off, now pinned to the exact version (1.144.0) and rustc floor
   (1.94.1) that would unblock it, instead of a vague ">1.122 needs Rust
   1.94". Review-by moved to 2026-12-02.

RUSTSEC-2026-0387 (crypto-bigint, review-by 2026-11-08) is not due for
another six days; left untouched rather than reconfirmed early.

**Dependabot config bug, found while reading the closed PRs' comments, not
hunted for.** Every one of #3353's two bot comments included: *"The following
labels could not be found: `automated`. Please create it before Dependabot
can add it to a pull request."* Checked both labels `.github/dependabot.yml`
asks for beyond `dependencies`: `mcp__github__get_label` returns 404 for both
`automated` (cargo + github-actions ecosystems) and `ci` (github-actions
ecosystem only); `dependencies` resolves fine. So **every single Dependabot
PR**, on both ecosystems, has been posting a failed-label-application
comment since whenever these lines were added — pure noise competing for
attention with the real CI-health signal problem documented last pass, and
exactly the kind of thing a bot-fatigue reviewer learns to skip past. I have
no tool to create the missing labels (GitHub-write access in this session is
scoped to the MCP server, which has no label-creation tool, and direct GitHub
API access is out of scope for anything but search) — the admissible,
minimal fix is to stop asking for labels that don't exist.

**Cost attribution — the first baseline, delivered.** `cargo build -p
autumn-web --timings`, clean `target/`, 4 vCPUs (`jobs=4 ncpu=4`, max
concurrency 6), rustc 1.97.0, dev profile, default features:

| Metric | Value |
| --- | --- |
| Total time | **278.0s** (4m 38.0s) |
| Units compiled | 445 (0 fresh, 445 dirty) |

Heaviest hires by self-time (`target/cargo-timings/cargo-timing.html`):

| # | Unit | Total | Frontend | Codegen |
| - | --- | --- | --- | --- |
| 1 | autumn-web v0.9.0 (own code) | 176.9s | 128.3s (73%) | 48.6s (27%) |
| 2 | diesel v2.3.14 | 28.1s | 25.6s (91%) | 2.5s (9%) |
| 3 | autumn-macros-repository v0.9.0 | 17.8s | — | — |
| 4 | autumn-macros v0.9.0 | 8.7s | — | — |
| 5 | tokio v1.54.1 | 8.0s | 5.1s (64%) | 2.8s (36%) |
| 6 | rustls v0.23.46 | 7.9s | 3.9s (49%) | 4.0s (51%) |
| 7 | libsqlite3-sys build script | 7.7s | — | — |
| 8 | reqwest v0.12.28 | 7.3s | 2.5s (34%) | 4.8s (66%) |
| 9 | chrono-tz v0.10.4 | 7.0s | 6.7s (96%) | 0.3s (4%) |
| 10 | syn v2.0.119 | 6.8s | 5.2s (76%) | 1.7s (24%) |
| 11 | tokio-cron-scheduler v0.15.1 | 6.7s | 2.4s (35%) | 4.3s (65%) |
| 12 | regex-automata v0.4.18 | 6.7s | 3.1s (46%) | 3.6s (54%) |
| 13 | **syn v3.0.9** | **6.6s** | 4.6s (70%) | 2.0s (30%) |
| 14 | autumn-macros-model v0.9.0 | 6.3s | — | — |
| 15 | h2 v0.4.20 | 6.0s | 4.3s (71%) | 1.7s (29%) |

autumn-web's own compile unit dominates (64% of total), which is expected and
not actionable by Ballast — that is the framework's own macro-heavy code, not
a hire. Line 13 is the interesting one: **three `syn` major versions
currently resolve in this graph** (1.0.109, 2.0.119, 3.0.9 — `grep -A2 'name
= "syn"' Cargo.lock`). The duplicate isn't caused by our manifest; it's a
single transitive crate making a different choice than the rest of the
ecosystem:

```
cargo tree -p autumn-web --all-features -i syn@3.0.9
```

traces every path to `async-trait v0.1.92`, the *only* crate in the graph
pulling `syn@3.0.9`. The sparse index for `async-trait` shows the version
history: 0.1.89 was the last release on `syn ^2.0.46`; 0.1.90 (yanked) and
0.1.91/0.1.92 (current) moved to `syn ^3`. Every direct dependent of
`async-trait` in this graph (`instant-acme`, `lettre`, `opentelemetry-http`,
`postgresql_archive`, `reqwest-middleware`, `reqwest-retry`,
`reqwest-tracing`, `testcontainers`, `tokio-postgres`, `tonic`) declares a
requirement no tighter than `^0.1.51`, all satisfied by 0.1.89 — checked each
one's own manifest in the sparse index. So `cargo update -p async-trait
--precise 0.1.89` would legally collapse `syn@3.0.9` out of the graph (`proc-macro2`
and `quote` are single-version already, unaffected either way).

**Deliberately not actioned.** Per the hard gate's floor, this does not
clear it on its own: `syn` is *already* a duplicate today because of
1.0.109 vs. 2.0.119 (pre-existing, unrelated to `async-trait`), so dropping
the 3.0.9 copy does not change the duplicate-crate-*name* count (still 87 —
`syn` stays duplicated either way) and the build-time saving is one unit's
6.6s of 278.0s, **2.4%**, under the 10% floor. No advisory, license, or
feature gap is involved. Pinning a dependency backward with no forcing fact
beyond a sub-floor build-time shave is exactly the "churn with a lockfile
diff" the framework warns against. Recorded here as a ranked, ready-to-go
recommendation — reachability and the exact command are both already
done — for whenever it can ride alongside something that does clear the
floor (the next scheduled batch, or a future pass that also collapses the
1.0.109/2.0.119 pair).

## 💡 Mechanism / forcing fact

Three independent `review-by` dates landed on the same day this routine
fires (2026-11-02) — that is what a monthly reconfirmation cadence looks
like when it is honored rather than skipped, not a coincidence to read into.
None had a fix ship; all three got sharper, more precisely-sourced evidence
than they had, and their trigger dates moved forward on schedule. The
dependabot.yml fix has its own forcing fact: a 404 on both referenced labels,
confirmed by the tool, not inferred from a hunch, and a visible cost (a noise
comment on every PR for as long as it's been wrong).

## 🔧 Change

- `deny.toml`, `deny-sqlite.toml`: refreshed the `reason` strings and
  surrounding comments for RUSTSEC-2023-0071, RUSTSEC-2024-0384, and
  RUSTSEC-2026-0253 with the sharper evidence above; `review-by` moved
  2026-11-02 → 2026-12-02 on all three, in both files.
- `autumn-cli/src/templates/deny.toml.tmpl`: same refresh for its copies of
  RUSTSEC-2023-0071 and RUSTSEC-2024-0384 (no `lru` entry ships in the
  scaffold).
- `fuzz/deny.toml`: same refresh for its copy of RUSTSEC-2023-0071.
- `.github/dependabot.yml`: dropped the `automated` label from the `cargo`
  ecosystem's `labels:` list (keeping `dependencies`, which exists) and
  dropped the entire `labels:` block from the `github-actions` ecosystem
  (both `ci` and `automated` are 404s, leaving nothing valid to request).
- No dependency version changed. No changelog fragment — nothing a consumer
  of the framework can observe changed; this is waiver bookkeeping and CI-bot
  config.
- No PR opened against the Dependabot queue — it is empty.

## 📊 Measurement

| Check | 2026-10-26 (after) | 2026-11-02 (before) | 2026-11-02 (after) |
| --- | --- | --- | --- |
| `cargo deny --offline check advisories` (root) | ok, 0 warnings | ok, 0 warnings | ok, 0 warnings |
| `cargo deny --offline --config deny-sqlite.toml check advisories` | ok, 0 warnings | ok, 0 warnings | ok, 0 warnings |
| Root/SQLite licenses + sources | clean | clean | clean |
| Duplicate crate names (warn-level, `cargo deny check bans`) | 91 | 87 (via external #3471) | 87 (unchanged — no version touched this pass) |
| Open Dependabot PRs | 9 | 0 | 0 |
| Unwaived advisories, all graphs | 0 | 0 | 0 |
| `review-by` dates due and unactioned | 3 (due 11-02, not yet due) | 3 due today | **0** |
| Cost-attribution baseline (autumn-web, cold, dev, 4 vCPU) | not run (7 passes) | — | **278.0s / 445 units — first baseline** |
| Dependabot label-config 404s | not checked | 2 (`automated` x2 ecosystems, `ci`) | **0** |

Rehearsal:

```
cargo test -p autumn-cli --bin autumn deps::                                  # 74 passed
cargo test -p autumn-cli --bin autumn the_advisory_policy_resolves_its_flavor_anchors  # 1 passed
cargo test -p autumn-cli --bin autumn doctor::                                # 573 passed
cargo test -p autumn-cli --test cli_tests dependency_audit::                  # 33 passed
./scripts/check-advisories.sh --self-test                                     # OK, all 4 policies
```

All green; none of these tests assert the exact waiver text (checked by
grep before editing — they assert substring presence of the RUSTSEC id and
that a `review-by` marker exists, both still true), so the text refresh is
the only change and all four still pass.

## 🔬 Reproduce

```
cargo install --locked cargo-deny@0.20.2
git fetch --deepen=200 origin trunk-dev   # this session started from a 50-commit shallow clone
cargo fetch --locked
cargo deny fetch db
cargo deny --offline check advisories
cargo deny --offline --config deny-sqlite.toml check advisories
cargo deny --offline check licenses sources
cargo deny --offline --config deny-sqlite.toml check licenses sources
cargo deny check bans
./scripts/check-advisories.sh --self-test

cargo test -p autumn-cli --bin autumn deps::
cargo test -p autumn-cli --bin autumn the_advisory_policy_resolves_its_flavor_anchors
cargo test -p autumn-cli --bin autumn doctor::
cargo test -p autumn-cli --test cli_tests dependency_audit::

# Cost attribution
rm -rf target && cargo build -p autumn-web --timings
# then open target/cargo-timings/cargo-timing.html

# syn duplicate trace (not actioned — see Evidence)
cargo tree -p autumn-web --all-features -i syn@3.0.9
cargo tree -p autumn-web --all-features -i async-trait@0.1.92 --depth 1
curl -sS https://index.crates.io/as/yn/async-trait   # version/syn-req history

# Dependabot label 404s (via GitHub MCP)
# get_label owner=autumn-foundation repo=autumn name=automated  -> 404
# get_label owner=autumn-foundation repo=autumn name=ci         -> 404
# get_label owner=autumn-foundation repo=autumn name=dependencies -> ok
```

## Follow-ups still open

1. **`async-trait` → `syn@3.0.9` duplicate**: reachable, rehearsed-in-evidence
   fix ready (`cargo update -p async-trait --precise 0.1.89`), not actioned —
   below the impact floor alone (2.4% build time, no change to the
   duplicate-name count since `syn` is already duplicated via 1.0.109 vs.
   2.0.119). Good candidate to bundle into the next scheduled batch or a pass
   that also tackles the 1.0.109/2.0.119 pair.
2. **`syn` 1.0.109 vs. 2.0.119** (the pre-existing duplicate, unrelated to
   this pass's finding): not investigated this pass — who pulls 1.0.109 and
   whether it still needs to is open for a future pass.
3. **CI-health for the Dependabot queue**: still unconfirmed, now moot only
   because the queue is empty. The next PR Dependabot opens will re-raise
   last pass's "Test suite" skipped-shard problem; #3471 proved the escape
   hatch (port directly, land as one PR under full permissions) works but
   doesn't fix the underlying token-scoping issue.
4. **`fuzz/` and `examples/island-flock/` scheduled-batch ownership**: eighth
   pass, still unaddressed.
5. **GitHub-native Dependabot alert count moved 18 → 23** (4/11/3 → 5/13/5,
   high/moderate/low), surfaced only as a side-effect message on this pass's
   own `git push` — still no tool in this session lists individual alerts by
   id, the sixth pass flagging that gap. A rise the same week the Dependabot
   *PR* queue went to zero is counterintuitive enough to be worth next pass's
   first look: either GitHub's scanner opened alerts for advisories that
   never got a corresponding PR (a GHSA-only finding, as `css-inline`'s
   GHSA-jh54-c7gc-mvcc was two passes ago), or something in #3471's own
   version moves (e.g. `css-inline` landing on 0.23.1, past what any
   Dependabot PR targeted) introduced new exposure that the RustSec-backed
   `cargo-deny` count (87 unwaived-advisories-ok, unchanged) doesn't
   independently corroborate. Not investigated further this pass — no
   breakdown-by-id tool available — but the direction of the delta is new
   information, not noise, and shouldn't wait six more passes.
6. `cargo-bloat` / binary-size attribution and an unused-feature audit: cost
   attribution via `--timings` is now done (item 6 from last pass); size and
   unused-feature analysis are the next layer of the same oldest-open-item,
   not yet started.
