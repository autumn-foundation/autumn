# ⛏️ Prospect: has ADR 0013's revisit trigger fired? (prior-art close: no, 0 of 2 triggers vs ≥1 line)

## 🎯 Question

**Falsifiable:** as of 2026-10-19, has either revisit trigger in
`docs/adr/0013-defer-deny-config-sync-fitness-function.md` fired?
1. The 2026-10-01 review-by date passed with `deny.toml` and `deny-sqlite.toml`'s shared
   `[advisories].ignore` entries left inconsistent.
2. A third cargo-deny config was added for another mutually-exclusive backend graph.

**Decision fed:** whether Keystone should reopen the deferral and build the sync fitness
function. **Decider:** Keystone / repo maintainer. Fired → reopen; not fired → deferral stands.

## ⚖️ Pre-registration

The criteria are ADR 0013's own "Trigger to revisit" text, committed 2026-09-07, long before
this check. No apparatus was built, so there is no separate plan commit: this is a read-only
record check and the ADR is the contract. Pursue (reopen) = either trigger true. Kill
(deferral stands) = both false. Time box: one session.

## 🔍 Prior art

ADR 0013 itself, `deny.toml`, `deny-sqlite.toml`, `fuzz/deny.toml`,
`examples/island-flock/deny.toml`, `scripts/check-advisories.sh`, and the 2026-10-05 Ballast
ledger audit. No assay-ledger entry covers this question.

## 🧪 Apparatus

None built. One throwaway `tomllib` comparison of the parsed `[advisories].ignore`,
`[licenses].allow` and `[sources]` tables across the three configs (run from the scratchpad,
not committed). **Stubs/limits:** the checkout is shallow (50 commits), so file creation dates
for `fuzz/deny.toml` and `island-flock/deny.toml` cannot be read from history. ADR 0013 itself
already lists both as existing and outside the two configs on 2026-09-07, so they predate the
ADR.

## 📊 Assay

| Check | Result |
|---|---|
| `[advisories].ignore` deny.toml vs deny-sqlite.toml (id and reason, parsed) | identical |
| `[licenses].allow` deny.toml vs deny-sqlite.toml | identical |
| `[sources]` deny.toml vs deny-sqlite.toml | identical |
| Shared waivers now | 4 (ADR counted 3; `RUSTSEC-2024-0449` was added to both) |
| Review dates | all three originals reconfirmed 2026-10-05 and moved to review-by 2026-11-02; `-0449` is 2026-11-12, in both files |
| Third cargo-deny config? | `fuzz/deny.toml` and `examples/island-flock/deny.toml` exist, but they are satellite workspaces with their own lockfiles. `fuzz/deny.toml` gates advisories and sources only, by documented design (NCSA license question). They are not mutually-exclusive backend graphs of the main workspace. |

Trigger 1: **false**. The date passed and the files were updated together, so they are
consistent. Trigger 2: **false**. The satellite configs differ from the root policy by
documented design, and they were already named in the ADR's own evidence item 5.

## 🏁 Verdict

**Kill (deferral stands), prior-art close.** 0 of 2 triggers fired against a line of ≥1. The
manual process held through its first dated test. No fitness function needed.

Caveats a quoter should keep: this is one data point of the human process working, not proof
it will keep working. The next dated test is **review-by 2026-11-02** (three shared entries),
then 2026-11-12. Revisit if either is missed in only one file. The ADR's own triggers
already encode that, so no new trigger is needed.

## 💰 Cost to productionize

N/A (not a pursue verdict).

## 🔬 Reproduce

```bash
python3 -I - <<'PY'
import tomllib
d={n:tomllib.load(open(n,'rb')) for n in ['deny.toml','deny-sqlite.toml']}
for s,k in [('advisories','ignore'),('licenses','allow'),('sources',None)]:
    g=lambda n:(d[n][s][k] if k else d[n][s])
    print(s, g('deny.toml')==g('deny-sqlite.toml'))
PY
```
Run from the repo root at trunk-dev `603b1be`.
