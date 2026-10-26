# ⛏️ Prospect: has ADR 0012's revisit trigger fired? (prior-art close: no, 0 of 3 triggers vs ≥1 line)

## 🎯 Question

**Falsifiable:** as of 2026-10-26, has any revisit trigger in
`docs/adr/0012-defer-core-crate-decomposition.md` fired?
1. A second maintainer produces ≥3 merge conflicts on `job.rs`, `config.rs`, `app.rs` or
   `router.rs` within one quarter.
2. CI compile/link resource incidents (OOM kills, linker timeouts) recur ≥3 times in a
   rolling 90 days (window 2026-07-28 → 2026-10-26).
3. A dated product commitment requires publishing one of these subsystems independently.

**Decision fed:** whether Keystone reopens the deferral. **Decider:** Keystone / repo
maintainer. Any trigger fired → reopen; none → deferral stands.

## ⚖️ Pre-registration

The criteria are the ADR's own "Trigger to revisit" text, committed 2026-09-01, long before
this check. No apparatus was built, so there is no separate plan commit; the ADR is the
contract. Pursue (reopen) = any trigger true. Kill (deferral stands) = all false. Counting
rule for trigger 2, fixed before looking at the data: only compile or link steps that were
OOM-killed or hit a linker timeout count. Time box: one session.

## 🔍 Prior art

ADR 0012; full `git log` (1955 commits after `git fetch --unshallow`);
`docs/ci-health/quarantine-ledger.md`; the semaphore CI-health follow-ups; commit bodies of the
candidate incidents. No assay-ledger entry covers ADR 0012.

## 🧪 Apparatus

None built. `git log`/`git show`/`grep` only. **Stubs/limits:** git does not record merge
conflicts, so trigger 1 is judged from author counts, not conflict counts. CI run logs
were not re-fetched; incidents are those recorded in commit messages and the CI-health docs.

## 📊 Assay

| Trigger | Evidence | Result |
|---|---|---|
| 1. Second maintainer, ≥3 conflicts | Since 2026-07-28: 784 commits by the one maintainer, 32 dependabot, 8 claude[bot], 2 by `madmax983`; none of the 2 touch the four files. | false |
| 2. ≥3 compile/link OOM or linker timeouts in 90 days | 0 under the registered counting rule. Three nearby incidents, none qualifying: #3265 (2026-10-13) DST nightly ran out of memory at runtime, a sim leak, fixed in the sim; #2547 (2026-10-21) `deploy_e2e` disk exhaustion, fixed by its own job; #3299 (2026-10-15) Windows LNK4319 is a linker warning, not a timeout or OOM. A macOS job "killed mid-`cargo build`" in the CI-health docs was a workflow cancellation (orphan-process termination), not a resource kill. | false (0 strict; 2 even if the rule is widened to any resource incident) |
| 3. Dated commitment to publish independently | No roadmap or PRD entry found naming one of the four subsystems for independent publication. | false |

Context, not a trigger: the four files grew from 20,487 / 16,819 / 16,563 / 13,072 lines
(ADR, 2026-09-01) to 28,358 / 23,204 / 23,875 / 17,407 now: +38%, +38%, +44%, +33% in
8 weeks. The ADR rated the same-crate split a two-way door needing no review, so this
growth alone does not reopen it.

## 🏁 Verdict

**Kill (deferral stands), prior-art close.** 0 of 3 triggers fired against a line of ≥1.
Caveats a quoter should keep: trigger 2 is 2 away from firing only if its wording is widened
beyond compile/link; trigger 1 is unmeasurable from git alone. The growth rate means the
next check should be quarterly. Keystone may want to add a size-based trigger if the files
keep growing at this rate; that is a Keystone call, not this report's.

## 💰 Cost to productionize

N/A (not a pursue verdict).

## 🔬 Reproduce

```bash
git fetch --unshallow origin
git log --since=2026-07-28 --format='%an' | sort | uniq -c | sort -rn
git log --since=2026-07-28 --format='%h %ad %s' --date=short -i --grep='oom\|out of memory\|linker\|LNK\|no space'
wc -l autumn/src/{job,config,app,router}.rs
```
