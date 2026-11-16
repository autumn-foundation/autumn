# 🏛️ Keystone [findings]: `docs/migrations/next.md` is the repo's top merge-conflict file

Date: 2026-11-16. Outcome: findings memo, not an RFC. The fix is a two-way
door, so the team that does it decides the details in its PR.

## 🎯 Scope

The repository's merge record from 2026-09-16 to 2026-11-16. The question:
which files make parallel PRs conflict, and is any of them a cost the repo
built itself, rather than one that comes with the domain?

Method: every two-parent merge commit on the 870 branches still on `origin`
was replayed with `git merge-tree`. A replay that exits 1 had a conflict that
someone fixed by hand, and the replay names the files. See 🔬 Reproduce.

## 📈 Evidence (tier 2: repository record)

| Measure | Value |
| --- | --- |
| Merge commits replayed (since 2026-09-16) | 81 |
| Merges that had a conflict | 49 (60%) |
| … with a conflict in `docs/migrations/next.md` | **20 (41% of conflicted merges)**, on 9 branches |
| … whose **only** conflict was `next.md` | 13 |
| Conflict hunks in `next.md` | 32: 22 `## Breaking changes`, 5 `## Configuration changes`, 4 `## Behavior changes`, 1 `## Plugin authors` |
| Next file by conflicts: `CHANGELOG.md` | 6, all on branches cut before the fragments landed on 2026-09-18 (#2849). None after 2026-09-22. |
| Then `SKILL.md` 4, `actuator.rs` 4, `ci.yml` 3, `deny.toml` 3 | |
| Commits touching `next.md`, last 30 days | 59 of 306 (19%). 49 only add lines. |

Where it sits relative to the rest: `next.md` causes more conflicts than the
next five files put together. Of the four files edited most often in the last
30 days, `ci.yml` (89 commits, 3 conflicts) and `SKILL.md` (70, 4) seldom
conflict. `next.md` (59, 20) does, because each PR adds a section at the same
place.

**This is a lower bound.** It counts only branches that still exist and only
merge commits. A rebased branch and a deleted branch leave no record.

## 💡 Hypothesis (mechanism)

`next.md` is the same pattern `CHANGELOG.md` was before #2849: one shared
file where every PR with a breaking change adds its section at the top of
`## Breaking changes`. Two such PRs open at once always conflict. #2849 split
the changelog note into its own file, and changelog conflicts stopped within
four days. The migration guide was left on the old pattern. A breaking
fragment still has to link to `next.md#anchor`, so every breaking PR edits
both the fragment and the shared guide.

The cost is not in the domain. The guide's content is per change. Only the
file it lives in is shared.

## 🧭 Do nothing / decide later

There were 20 conflicted merges in 61 days, so about 120 a year at today's
rate. Each one is a hand fix to prose plus another full CI run on the branch.
13 of them had no other reason to conflict. The rate is rising: 59 `next.md`
commits in the last 30 days against 43 in the 30 before. Nothing breaks for
users, and the cost lands only on the people writing PRs.

## 🔧 Decision: findings plus a recommendation, decided in a PR

Recommendation: give migration-guide sections the changelog treatment.
Write them as `docs/migrations/next.d/<slug>.md`, one fragment per change,
keyed by the target section. `scripts/check-migration-guides.sh` reads the
assembled view, as `scripts/lib/changelog.sh` does for the changelog. The
release step assembles `next.md` → `<version>.md`, the same place
`scripts/update-changelog.sh` folds the changelog.

- **Door class:** two-way. Reversing it is one release fold plus deleting the
  assembler: **about 0.5 engineer-week**. Building it was about 1
  engineer-week, judging by #2849 (18 files, about 1,000 lines). The
  `next.md#anchor` links in fragments, ADRs and gates need a stable answer, so
  this one may come in a little above that.
- Because it is a two-way door under 2 engineer-weeks, it does not go to
  architecture review. The PR description decides it, including how the
  assembled guide keeps the anchors the links point to.

## ⚖️ Alternatives

1. **Defer / do nothing.** About 120 hand-fixed conflicts a year. That is
   reasonable if breaking-change PRs slow down after 1.0. The rate is going up
   instead.
2. **The boring one:** one file per section (`next/breaking.md`, …). This
   doesn't help, because 22 of 32 hunks are in one section.
3. **A merge driver (`merge=union` in `.gitattributes`) for `next.md`.** This
   is the cheapest fix, at under 1 day. But it applies only where local git
   reads the attribute, not to GitHub's server-side merge. It also merges
   blocks in silence, in whatever order, inside a structured guide. Not
   recommended.

## 🗺️ Path

1. **Abort-safe first step:** add the assembler and the gate's assembled view.
   An empty `next.d/` leaves `next.md` as it is. This ships on its own and
   changes no output.
2. Point CLAUDE.md/AGENTS.md and `changelog.d/README.md` at `next.d/`. Move
   the open sections out of `next.md`.
3. The release checklist assembles the fragments into `<version>.md`.

## 📊 Success metric

- Metric: conflicted merges that touch `docs/migrations/next.md` (or `next.d/`).
- Current: 20 in 61 days (41% of all conflicted merges).
- Target: 0 that come from two PRs adding sections, as the changelog reached
  after #2849.
- Review date: **2027-02-15**. Re-run 🔬 for the window after the change lands.
- Fitness function: the gate that already fails a feature PR editing
  `CHANGELOG.md` gets the same rule for `next.md` once fragments exist.

## Also recorded (negative result)

`.github/workflows/ci.yml` is in 29% of last month's commits (89 of 306).
That looks like a hotspot, but it caused 3 conflicts in 61 days. Do not
propose splitting the workflow file on edit frequency alone: the merge record
shows no conflict cost to remove.

## 🔬 Reproduce

```bash
git fetch origin '+refs/heads/*:refs/remotes/origin/*'
git rev-list --merges --remotes=origin --since=2026-09-16 > merges.txt
: > conf.txt
while read m; do
  p=($(git rev-list --parents -n1 "$m")); [ ${#p[@]} -ne 3 ] && continue
  out=$(git merge-tree --write-tree --name-only --no-messages "${p[1]}" "${p[2]}")
  [ $? -eq 1 ] && echo "$out" | tail -n +2 | grep -v '^$' | sed "s|^|$m |" >> conf.txt
done < merges.txt
awk '{print $2}' conf.txt | sort | uniq -c | sort -rn | head
# only-next.md merges:
awk '{a[$1]=a[$1]" "$2} END{for(k in a) print a[k]}' conf.txt | grep -cx ' docs/migrations/next.md'
# edit frequency, last 30 days:
git log --since=2026-10-16 --format=%H -- docs/migrations/next.md | wc -l
```
