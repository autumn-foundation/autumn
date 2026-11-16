# Pre-registration: RUSTSEC-2026-0459 waiver on its review-by date (2026-11-16)

Committed BEFORE any crates.io lookup or code audit of this assay. Non-production; no apparatus merges.

**Question (two parts, both falsifiable):**
- **Q1 (blocker):** as of 2026-11-16, can the workspace resolve `libsqlite3-sys >= 0.39.0`
  (bundled SQLite >= 3.54.0) with a published diesel release? The waiver says no:
  diesel 2.3.14 requires `libsqlite3-sys < 0.39`, and `links = "sqlite3"` allows one copy.
- **Q2 (exposure):** is the waiver's exposure claim true in code — that no Autumn path opens a
  SQLite main file of outside origin while a `-wal` of outside origin can sit next to it?
  The waiver itself says this "was not audited end to end".

**Decision / decider:** renew the `deny.toml` + `deny-sqlite.toml` ignore with a new review-by,
or take the fix now, or narrow the exposure first. Decider: repo maintainer (Ballast's weekly
audit carries the waiver).

**Prior art:** waiver text and its 2026-11-09 rehearsal (`cargo update -p libsqlite3-sys` fails
on `links`), #3671 which added it; ADR 0013 (both deny files must agree);
2026-11-09 Prospect report (ADR 0013 second dated test). No ledger entry on -0459.
New information: the review-by date is today.

**Lines (fixed now):**
- Q1 pursue ("take the fix"): a published, non-yanked diesel version's sqlite dependency admits
  `libsqlite3-sys` 0.39.x, AND in a scratch copy of the workspace, bumping the workspace pin to
  0.39 plus `cargo update` resolves (`cargo metadata` / `cargo tree -i libsqlite3-sys` succeeds
  showing one 0.39.x). Q1 kill: no such diesel release, or resolution fails.
- Q2 holds ("exposure claim true"): 0 code paths in `autumn/src` or `autumn-cli/src` that open a
  SQLite file whose bytes arrive from outside the process (import, restore, upload, offsite
  pull, seed from archive) without either removing/refusing `-wal`/`-shm`/`-journal` siblings
  first, or opening at a path the process just created. Q2 fails: >= 1 such path.
- Verdict mapping: Q1 pursue -> "take the fix". Q1 kill + Q2 holds -> "renew waiver" (kill for
  bump-now). Q1 kill + Q2 fails -> "renew only with the gap named; fix the open path".

**Conditions:** trunk-dev tip `7e93005`, this sandbox, crates.io index via the proxy.
**Time box:** this session (~2h). **Riskiest assumption:** Q2 — the waiver's own text flags it
unaudited; a code read can falsify it cheaply, Q1 is a lookup.
**Control:** for Q1, the current lockfile resolution (0.38.2) under the same commands.
**Containment:** scratch copy under the session scratchpad; no tree edits besides this file and
the report; no spend.
