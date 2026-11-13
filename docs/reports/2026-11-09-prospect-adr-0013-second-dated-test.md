# ⛏️ Prospect: did ADR 0013's second dated test break the deny.toml sync? (prior-art close: no, 0 of 2 triggers vs ≥1 line)

## 🎯 Question

**Falsifiable:** as of 2026-11-09, has either revisit trigger in
`docs/adr/0013-defer-deny-config-sync-fitness-function.md` fired?
1. A review-by date passed with `deny.toml` and `deny-sqlite.toml` shared
   `[advisories].ignore` entries left inconsistent.
2. A third cargo-deny config was added for another mutually-exclusive backend graph.

**New information since the 2026-10-19 check:** the 2026-11-02 review-by date (the check named it
as the next dated test) and the 2026-11-08 date have both passed. The 10-19 report said to
revisit if either was missed in only one file.

**Decision fed:** whether Keystone builds the sync fitness function. **Decider:** Keystone / maintainer.

## ⚖️ Pre-registration

Criteria are the ADR's own trigger text (2026-09-07) plus the 10-19 report's stated revisit
condition. Pursue = any trigger true; kill = all false. One session, no apparatus plan commit
(read-only record check).

## 🔍 Prior art

`docs/reports/2026-10-19-prospect-deny-config-sync-trigger-check.md` (ledger entry, no trigger
fired), ADR 0013, 2026-10-26 Ballast audit.

## 🧪 Apparatus

One throwaway `tomllib` comparison (snippet under Reproduce). **Stubs/limits:** compares the parsed
tables only; says nothing about whether the advisories themselves were re-reviewed.

## 📊 Assay

| Check | Result |
|---|---|
| `[advisories].ignore`, `[licenses].allow`, `[sources]` parsed equal | all three identical |
| Shared waivers | 4: RUSTSEC-2023-0071, -2026-0387, -2026-0440, -2026-0441 (the three originals of the ADR are down to one; the rest were replaced) |
| Trigger 1 | false: the files are consistent |
| Trigger 2 | false: configs are `deny.toml`, `deny-sqlite.toml`, and the two satellite configs already judged in the 10-19 check; none new |
| Commits touching any deny config since 10-19 | #3550, #3591 (MSRV), #3592; none left the pair inconsistent |

**Side finding, not a trigger:** the waivers are consistent but two are past their stated date,
identically in both files. RUSTSEC-2023-0071 says review-by 2026-11-02 (7 days overdue, reason text
still says "reconfirmed unchanged 2026-10-05"); RUSTSEC-2026-0387 says 2026-11-08 (1 day overdue).
`dependency_audit.rs` requires the string `review-by` in each reason but does not compare the date
to today, so an expired date fails nothing. That is a separate gap from the sync invariant. The
weekly Ballast audit is the natural owner; Keystone may decide whether an expiry check is wanted.

## 🏁 Verdict

**Kill (deferral stands), prior-art close.** 0 of 2 triggers fired against a line of ≥1. The
manual process has now held through two dated tests with the pair in sync. Next dated tests:
2026-12-05 (-0440, -0441).

## 💰 Cost to productionize

N/A (not a pursue verdict).

## 🔬 Reproduce

```bash
python3 -I - <<'P'
import tomllib
a=tomllib.load(open('deny.toml','rb'));b=tomllib.load(open('deny-sqlite.toml','rb'))
print(a['advisories']['ignore']==b['advisories']['ignore'],
      a['licenses']['allow']==b['licenses']['allow'], a['sources']==b['sources'])
P
grep -n "review-by" deny.toml deny-sqlite.toml
```
