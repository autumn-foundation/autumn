# 🪝 Snag: exploratory QA session — generated account lockout, 2026-09-05

## 🎯 Charter

*Persona × workflow*: an operator relies on the generated `autumn generate
auth` login handler's account-lockout policy
([docs/guide/authentication.md §"Account lockout"](../guide/authentication.md#account-lockout))
to defend against credential stuffing, and a credential-stuffing tool drives
it exactly the way it's designed to be attacked: repeated wrong passwords,
concurrently, against one account. Concerns: threshold/lockout transition
correctness, the non-enumerating "identical response while locked" claim,
successful-login counter reset, cool-off auto-unlock, the `X-Admin-Secret`
unlock escape hatch, and — the payoff of this charter — whether the feature
holds up under **concurrent** requests, since no shipped example in this repo
exercises this feature at all (per the prior `reddit-clone` Snag session,
2026-09-03) and its own generated test suite only checks that the generated
*source text* contains certain substrings/patterns, never that a running
instance behaves correctly under concurrency.

Time-boxed to one sitting (~2 hours, most of it cold `cargo build` time for
a scratch app pointed at this checkout's `autumn-web` via a `path` dependency
— no Docker in this sandbox, so Postgres 16 ran as a native `postgresql`
service, same workaround the prior Snag session used).

## 📌 Environment

- Commit: `66106a5` on `claude/brave-goldberg-9cr2c4` (tip of `trunk-dev` at
  session start)
- Platform: Ubuntu 24.04.4 LTS, `rustc`/`cargo` 1.94.1, PostgreSQL 16.13
  (native service, not a container)
- Fresh scratch app (`autumn new lockout-qa`, then
  `autumn generate auth User`), `autumn-web` pinned to this checkout via
  `path = "/home/user/autumn/autumn"` so the live app runs today's
  `trunk-dev` lockout code, not the published `0.7.0` crate
- `[auth.lockout]` set explicitly per run (`threshold` and `cooloff_secs`
  varied — see each test below); `AUTUMN_ADMIN_SECRET` set for the
  admin-unlock tests, deliberately unset for the "refuses without a secret"
  test
- Driven via `curl` with cookie jars; confirmation links pulled from the
  `log` mail transport's captured output

## 🐛 Bug: concurrent threshold-crossing requests each stamp their own `locked_at` and each log `account_locked` instead of once at the transition — plus a plausible, unverified stale-write bypass

### Charter

Same as above — the credential-stuffing persona, specifically the
concurrent-request case, since stuffing tools routinely fire requests in
parallel for throughput and that is exactly the traffic pattern this feature
exists to detect and log.

### Oracle

[`docs/guide/authentication.md` §"Account lockout"](../guide/authentication.md#account-lockout),
point 2: *"At `threshold`, `locked_at` is stamped **and a `tracing::warn!`
fires** with `event = "account_locked"`, a salted SHA-256 account digest...
and an IP prefix... **correlatable across log lines for incident
response**."* The singular framing ("at threshold... fires") plus the
explicit incident-response-correlation purpose is a documented claim that
this is a one-shot transition event, not a per-request one. The same
generator template also emits this as a table row in the *app's own*
generated docs (`autumn-cli/src/generate/auth.rs`, the
`docs/guide/authentication.md` written into the generated project).

### Repro (10/10 across two shapes)

Minimal shape — 2 truly-concurrent requests, `threshold = 1`:

1. `autumn generate auth User`, set `[auth.lockout] threshold = 1` (any
   `cooloff_secs`, `enabled = true`).
2. Sign up and confirm a fresh account (`email`/`password`).
3. Fire exactly two concurrent `POST /login` requests with a wrong password
   for that account, backgrounded in the same shell so they race:
   ```sh
   ( curl -s -o /dev/null -X POST http://127.0.0.1:3200/login \
       -d "email=$EMAIL&password=wrong1" &
     curl -s -o /dev/null -X POST http://127.0.0.1:3200/login \
       -d "email=$EMAIL&password=wrong2" &
     wait )
   ```
4. Grep the server log for `account_locked`.

**Observed: 2 `account_locked` WARN lines** (one at `failed_attempts: 1`, one
at `failed_attempts: 2`) for a single lock transition, every one of 5/5 runs
against 5 independent fresh accounts.

Scaled shape — realistic burst size, default-shaped threshold: `threshold =
3`, 10 concurrent wrong-password requests against one fresh account produced
**8 duplicate `account_locked` lines** (`failed_attempts: 3, 4, 5, 6, 7, 8,
9, 10`, filtered to this account's own `account_id_digest` — the two
requests whose atomic increment landed at 1 or 2 correctly stayed silent,
and every one of the other eight logged its own transition). 1/1 run at
this shape; not independently repeated, but the mechanism (below) is
deterministic given the interleaving, and the 5/5 minimal-shape reruns
confirm it isn't a fluke.

**Correction**: this figure originally read "5 duplicate lines
(`failed_attempts: 6, 7, 8, 9, 10`)" — an artifact of having grepped the
server log with `tail -5` while writing the session up and never going back
to look at the full output. A reviewer (`chatgpt-codex-connector`) flagged
that "6 through 10" (five values) didn't match the accompanying "4th through
10th" claim (seven requests) and asked which was right; re-deriving the
count from the complete log, filtered to this test's specific
`account_id_digest` (a second, unrelated account's single-lock event from
earlier in the same server session shared the log file and was polluting an
unfiltered count), showed the true number is worse than either figure: all
eight requests at or past the threshold logged, not five.

The account's `failed_attempts` counter itself is **not** corrupted by the
race — Postgres's `SET failed_attempts = failed_attempts + 1 RETURNING
failed_attempts` is a real atomic per-row increment, so 10 concurrent
failures still land on exactly 10. The *log event* duplicates, and — see
correction below — so, non-idempotently, does the `locked_at` write.

**Correction (caught by `chatgpt-codex-connector` review on this PR,
verified against the code)**: this report originally described the
duplicate `locked_at` writes as "harmless" and "idempotent... to the same
timestamp." That's wrong. `now` (`chrono::Utc::now().naive_utc()`) is
captured once per *request*, not once per race, and each racing request
that satisfies the threshold guard writes **its own** `now` to `locked_at`
unconditionally — the writes are to the same column, but not to the same
value. Whichever racing `UPDATE` commits last wins, and that request's own
call-time timestamp becomes the account's real cool-off anchor, regardless
of which request actually crossed the threshold first. In this session's
observed runs the racing requests' timestamps were tens to ~100ms apart, so
the practical drift in unlock time is small — but it is a real drift in
actual lockout *behavior* (the true, database-recorded lock/unlock time),
not merely duplicate telemetry describing an otherwise-correct state. See
Impact below.

### Root cause

`autumn-cli/src/generate/auth.rs`, generated `login` handler:

```rust
let now = chrono::Utc::now().naive_utc();          // line 3953, per-request
let mut current_attempts = user.failed_attempts;   // line 3958
let mut current_locked_at = user.locked_at;        // line 3959
// ... atomic increment of failed_attempts in the DB ...
if new_attempts >= lockout_cfg.threshold && current_locked_at.is_none() {  // line 4006
    // unconditionally writes locked_at = Some(now) using THIS request's `now`,
    // then tracing::warn!(event = "account_locked", ...)
}
```

`current_locked_at` is captured **once**, from each request's own initial
`SELECT`, before that request's atomic increment runs. Two (or ten)
concurrent requests against a never-locked account all read `locked_at =
NULL` before any of them writes anything, so **every** request whose own
increment happens to land at-or-past the threshold satisfies
`current_locked_at.is_none()` — there is nothing that re-checks `locked_at`
against the database immediately before deciding to log the transition (and
write the account's own `now` into it). Only the *first* request to cross
the threshold should log and stamp the lock; the guard compares against a
snapshot that predates the whole race.

The generated `reauth` handler has the identical `current_locked_at`
snapshot-then-write structure (`autumn-cli/src/generate/auth.rs:5010-5054`)
and so shares the same non-deterministic `locked_at`-anchor drift — but,
per the second correction below, it does **not** duplicate the
`account_locked` **log event**, because its threshold branch never calls
`tracing::warn!` at all.

### Impact

Moderate, non-security-critical but a real violation of a stated,
incident-response-facing claim: `account_locked` is documented and templated
specifically as a correlatable signal for responders, and any SIEM/alert
rule counting or deduplicating on it will see up to (burst size − threshold
+ 1) total events fired for one lock transition — an over-count of up to
(burst size − threshold) *excess* events beyond the one the docs describe
(that distinction corrected below) — worse the more concurrent an
attacker's tooling is, which is precisely the traffic shape
credential-stuffing tools use. An operator paging off "N `account_locked`
events" gets a number that depends on request interleaving, not on the
number of accounts actually locked.

**Correction (`chatgpt-codex-connector` review, verified by re-deriving the
arithmetic)**: the paragraph above originally called `(burst size −
threshold + 1)` the *over-count*. That value is the **total** number of
events fired (for the 10-request/threshold-3 test: 8 total, at counters 3
through 10); the *excess* over the one legitimate transition event is one
less than that, `(burst size − threshold)` — 7 in that same example, not 8.
Reworded above to state both numbers without conflating them.

This is not *purely* a telemetry-fidelity bug, per the correction above: the
`locked_at` value itself is also decided by whichever racing request's
`UPDATE` commits last, not by whichever request actually crossed the
threshold first, so the account's real recorded lock time — and therefore
its cool-off expiry — carries the same non-determinism, bounded by the width
of the concurrent race window (tens of milliseconds in this session's
tests, potentially more under heavier contention or slower storage). No
duplicate database rows, and in every race this session actually drove
(races on the order of tens to ~100ms wide) the account was still genuinely
locked afterward — the defect *observed* is that *when* it unlocks, and how
many transition events describe it, are non-deterministic under concurrent
load rather than tied to the actual first-crossing request.

**Correction — walking back "no auth bypass" (raised by
`chatgpt-codex-connector` review, mechanism confirmed by re-reading the
code, not independently driven live)**: an unqualified "no auth bypass" is
an overclaim this session didn't earn. The same per-request, unconditionally
written `now` has a sharper failure mode than anchor drift: if any one
racing request stalls (slow scheduling, a slow DB round trip, GC-style
pause — anything longer than `cooloff_secs`) between capturing its `now`
and executing its `locked_at` UPDATE, and that stale write lands *after* a
more recent lock, it overwrites the real lock with an already-expired
timestamp. The very next login then takes the expired-lock branch
(`autumn-cli/src/generate/auth.rs:3964-3972`) and the guarded reset
(`4079-4097`) clears the lock outright — reachable by an ordinary correct
password, no special privilege needed. **Not reproduced live this
session** — doing so needs a way to hold one request's timeline open
between its `now` read and its `UPDATE` (e.g. an instrumented build, or
pool/connection starvation tuned precisely enough to stall one request past
`cooloff_secs` while others complete) that this black-box HTTP-only session
didn't build. Severity is corrected upward accordingly: moderate-to-high
rather than "moderate, non-security-critical" — the log duplication is
cosmetic, but this specific stale-write path is a plausible, unverified
lockout-bypass mechanism, not just a telemetry one. See follow-up charters
for driving it for real.

**Correction (`chatgpt-codex-connector` review, verified by re-reading the
line order)**: this report previously cited this session's own observed
750ms-1s bcrypt latency as evidence the vulnerable window is realistically
exceeded. Checked directly: `verify_password(...).await` (the bcrypt call,
`auth.rs:3941`) completes *before* `now` is captured (`auth.rs:3953`), so
that latency happens before the clock the bypass depends on even starts —
it cannot age the eventual `locked_at` write, and is not evidence for this
specific mechanism. The only interval that matters is between line 3953 and
the `UPDATE` at line 4012, which this session never measured directly (its
own DB round trips there were on the order of single-digit milliseconds).
The mechanism itself is unaffected by this correction — it needs only a
stall of any origin (scheduler contention, a slow DB round trip, a GC-class
pause) exceeding `cooloff_secs` between those two lines — but this session
has not established that such a stall is realistic under normal load, only
that the code permits it if one occurs. Weakens the "plausible... window is
realistic" framing to "mechanism confirmed, real-world likelihood
unmeasured"; the follow-up charter to drive it for real stands unchanged.

### Dedup search

`git log --all --grep`, `grep -rn` for `lockout`/`account_locked` over
`docs/reports/`: one hit,
[`docs/reports/auth-pipeline-security-audit-2026-08.md`](auth-pipeline-security-audit-2026-08.md),
which already covers this exact handler in depth but a **different** race in
the same neighborhood: its **L15** ("Lockout counter collapses concurrent
failures right after cool-off") is about the *reset* branch — concurrent
failures landing right as an existing lock's cool-off expires all
unconditionally write `failed_attempts = 1`, undercounting. That is a
distinct branch (`current_attempts != user.failed_attempts`, i.e. the
cool-off-expired path) from this bug, which reproduces on a **fresh,
never-locked** account via the *normal* atomic-increment branch and is about
the log firing multiple times, not the counter under- or over-counting nor
the account failing to lock. No existing entry covers this shape. L15's fix
sketch (a single guarded atomic UPDATE for the reset branch) does not
by itself fix this one, since this bug lives in the unconditional read of
`current_locked_at` at line 3959, used by *both* branches at line 4006.

Not independently re-driven against `reauth` this session (static read
only, corrected above after review): it shares the snapshot-then-write
structure and so the `locked_at`-anchor drift, but it does **not** share the
`account_locked` log duplication specifically, since its threshold branch
never emits that event. The audit report's "duplicates the lockout block"
language about `reauth` is about **L15** (the cool-off-reset counter
collapse), which `reauth` does genuinely duplicate byte-for-byte — that is
a correct cross-reference for L15, just not for this session's telemetry
finding. See follow-up charters.

### Fix

Out of scope for a same-PR fix under Snag's charter: correct behavior here
is unambiguous (log once, and stamp `locked_at` once, at the actual
transition, using the timestamp of whichever request actually wins it), but
a minimal fix needs a design call this session didn't make — whether to
guard the lock-stamping `UPDATE` on `locked_at IS NULL` and only log (using
the DB's own `now()`, not the request's) when a row is actually affected
(mirroring the pattern the audit report's L15 fix sketch already proposes
for the neighboring branch), and whether to fix `login` and `reauth`
independently or extract the shared lockout-transition logic they both
duplicate — `reauth` needs the `locked_at`-write fix but not a new
`tracing::warn!` call, since it has none today and this report takes no
position on whether it should. Filed as a report per the "ships as a report
when it needs a design decision" rule, not a fix PR.

## 🔬 Coverage record

**Toured, and held up against the documented claims:**

- **Threshold lockout** (`threshold = 3`, sequential, non-concurrent):
  exactly the 3rd of 3 consecutive wrong-password `POST /login`s crossed the
  threshold and stamped `locked_at`; the DB row and the `account_locked` log
  line both agreed. 1/1, plus implicit in every other test below that first
  drives an account into a lock.
- **Non-enumerating identical response while locked, within one locked
  account** (oracle: same doc section, point 3). Re-tested carefully after
  an initial false start (see below): with `cooloff_secs` long enough that
  the lock was still active, a **correct**-password `POST /login` and a
  **wrong**-password one against the same locked account returned
  byte-identical bodies and status (`422`), except for two per-request
  fields inside the dev-only debug error overlay (a freshly-minted `Request
  ID` UUID and the session cookie id — neither account- or
  password-dependent). That overlay is explicitly gated on `is_dev` and has
  its own dedicated framework tests ("must NOT show error details in prod",
  `error_page_filter.rs`), so its presence in a dev-profile response is
  expected and doesn't undermine the production claim. Stripping the dev
  overlay, the two response bodies `diff`ed to nothing.
  - **Correction (caught by `chatgpt-codex-connector` review, verified
    directly against the saved response files)**: this report originally
    attributed the overlay diff to "different source line per branch
    taken," implying the two requests took different code paths. Checked
    directly: both responses' embedded stack traces highlight the *same*
    source line (`auth.rs:1179`, the active-lock check) — they took the
    identical branch, as the claim requires. The actual byte diff is the
    two per-request fields named above, not a branch difference; corrected.
  - **Scope note (also from that review)**: this test shows only that,
    *given* a locked account, a correct and a wrong password are
    indistinguishable — it does not compare that response against an
    ordinary wrong-password response on a **not**-locked account or against
    an unknown-email response, so it doesn't by itself establish the doc's
    fuller claim that "the response does not reveal which accounts exist or
    are locked" to an outside prober. Not driven this session; carried into
    the follow-up charters below rather than assumed to also hold.
  - **False-start correction, left in for honesty**: the first attempt at
    this test used `cooloff_secs = 6` and enough wall-clock time elapsed
    between locking the account and issuing the "correct password" request
    (~12.6s, per the access log) that the cool-off had already expired —
    the `303` success this returned was the *documented* auto-unlock
    behavior (point 4), not a violation. Caught by checking the access-log
    timestamps before writing this up, and re-run with `cooloff_secs = 60`
    and the follow-up request issued ~1.4s after the lock to get a real
    result. Recorded here per the charter's own rule against filing
    unverified "it broke once" observations.
- **Successful login resets the failure counter before threshold** (oracle:
  same doc, point 4, and the generator's own `AC8b` scenario name). 2 wrong
  passwords, then a correct login (succeeded), then 2 more wrong passwords,
  then a correct login again (succeeded) — with `threshold = 3`, a naive
  non-resetting counter would have locked on the 4th cumulative failure.
  1/1.
- **Cool-off auto-unlock and admin unlock, isolated from each other.** First
  pass conflated the two (see false-start above); re-run cleanly: relocked
  the account, confirmed a correct-password login still failed (`422`)
  while still within `cooloff_secs = 60`, called `POST
  /auth/admin/unlock` with the correct `X-Admin-Secret`, and confirmed the
  **same** correct password now succeeded (`303`) within about a second —
  isolating the admin-unlock path from the cool-off timer. 1/1.
- **`X-Admin-Secret` gating** (oracle: same doc, "For operator recovery...").
  With `AUTUMN_ADMIN_SECRET` unset entirely, the unlock endpoint refused
  (`422`) both with an arbitrary header value and with no header at all, and
  the target account's lock was confirmed still in effect afterward. With
  the variable set, a wrong header value also got `422`. 1/1 each.
- **Threshold=0 / `enabled=false` disables lockout**: not driven live this
  session (config-only tour, not exercised against a running app) —
  reasoned from the generator's own `lockout_cfg.enabled &&
  lockout_cfg.threshold > 0` gate rather than observed; **not filed as a
  finding either way**, listed here as an explicit gap rather than an
  implicit pass, per the charter's rule against speculative code-reading
  filings standing in for a driven result.

**Digest (oracle-less or too low-severity to file):**

- The generator's `unlock_account` **rustdoc** comment claims *"Returns the
  same 422 as a wrong-password attempt on both wrong secret and unknown
  email so the response does not reveal which accounts exist or are
  locked"* (`autumn-cli/src/generate/auth.rs`, doc comment above
  `unlock_account`). Live-tested: an unknown email with the *correct*
  `X-Admin-Secret` actually returns **`200`** ("Account Unlocked... has been
  cleared if it existed" — the `UPDATE ... WHERE email = ...` simply affects
  zero rows and the handler doesn't distinguish that from a hit). This does
  **not** break non-enumeration in practice (the response body and status
  are identical regardless of whether the account exists — just `200`
  either way, not `422`), and the customer-facing generated
  `docs/guide/authentication.md` this same generator writes into a new
  project makes no such "422 for unknown email" claim — only the internal
  rustdoc does. A stale/inaccurate doc comment on a still-correctly-behaving
  handler; digest, not a bug, since the only oracle it disagrees with is
  itself.
- Whether `login_verify` (TOTP) and `passkey_login_finish` share this
  session's concurrent-duplicate-log bug, or the audit report's separately
  documented and more serious lockout-bypass gaps (L13c), was not
  re-verified live — `--totp`/`--passkeys` scaffolding wasn't generated this
  session. Carried into follow-ups below rather than assumed.

## Findings summary

- **Bugs filed: 1** — duplicate `account_locked` telemetry, and a
  non-deterministic `locked_at` write, under concurrent threshold-crossing
  requests (moderate-to-high severity — the telemetry duplication is
  confirmed live at 8/10 and 2/2 requests respectively across two repro
  shapes; the same non-idempotent write also has a plausible, code-confirmed
  but not live-driven, lockout-bypass mechanism via a stalled request's
  stale timestamp, see Impact — oracle: docs/guide/authentication.md
  §"Account lockout" point 2).
- **Digest: 1** — stale rustdoc claim on `unlock_account`'s unknown-email
  behavior (cosmetic; the actual behavior is fine).
- **Solid areas**: threshold lockout transition, non-enumerating
  identical-locked-response (dev-overlay caveat noted and explained),
  successful-login counter reset, cool-off auto-unlock, admin-unlock
  correctness and secret-gating, all confirmed by live HTTP traffic against
  a real running instance rather than by reading the generator template.

## Proposed next charters

1. **Fix + regression test for the duplicate-telemetry bug above**, and
   while in that code, re-verify L15 from the prior audit
   (`docs/reports/auth-pipeline-security-audit-2026-08.md`) is still present
   (it is, per this session's read of the current `auth.rs` template) —
   both live in the same handful of lines and a single guarded-atomic-UPDATE
   redesign plausibly fixes both at once.
1a. **Drive the stale-write lockout-bypass mechanism for real** (raised by
   review on this PR, not yet live-tested): construct a way to stall one
   `POST /login` request between its `now` read and its `locked_at` write —
   e.g. an instrumented debug build with an injectable delay, or tuning
   pool/connection contention to stall exactly one request past
   `cooloff_secs` — and confirm whether a subsequent correct-password login
   actually clears an active lock early. This is the highest-priority
   follow-up: if it reproduces, it upgrades from "plausible mechanism" to a
   confirmed lockout bypass.
1b. **Locked-vs-not-locked / locked-vs-unknown-email response comparison**:
   this session only compared correct-vs-wrong password *within* one locked
   account. Compare that locked-account response against an ordinary
   wrong-password response on an unlocked account and against an
   unknown-email response, at both the body and timing level, to actually
   validate the doc's broader "does not reveal which accounts exist or are
   locked" claim rather than the narrower one this session confirmed.
2. **`reauth`, `login_verify` (`--totp`), and `passkey_login_finish`
   (`--passkeys`)** — the audit report already flags `reauth` as duplicating
   the L15 counter-collapse block (confirmed by this session's static read:
   `reauth` shares this session's `locked_at`-anchor-drift bug too, but not
   the `account_locked` log duplication, since it never emits that event)
   and `passkey_login_finish` as skipping the `locked_at` check entirely;
   this session only drove the plain password `/login` path live. A
   follow-up charter should scaffold `--totp --passkeys` and actually drive
   concurrent/cool-off-boundary traffic against those paths rather than
   trusting the static-template cross-reference.
3. **`threshold = 0` / `enabled = false`** — config-driven opt-out, listed
   above as untested rather than assumed-fine; a five-minute live check
   would close this gap.
4. **Multi-account timing/enumeration sweep**: this session's non-enumeration
   check was single-account: it did not compare response *timing* between a
   locked account and an unknown email at scale (the framework's own
   bcrypt-dummy-hash equalization is designed for exactly this, per
   `docs/guide/authentication.md`'s constant-time login section, but this
   charter didn't measure it).
