# Hidden tests (overview)

A hidden acceptance suite runs against the same HTTP contract as the visible
tests. Its exact input values are intentionally not published here — this file
only describes the *categories* it probes so implementers know what correctness
means without being able to hard-code answers. All checks are computed relative
to the server's current date and use **backdated** completions, so they are
deterministic on any calendar day.

The hidden suite probes:

1. **Streak reset after a gap.** A run of consecutive completions that ends
   before the current window does not count toward the current streak.

2. **Staleness cutoff.** When the most recent completion is older than
   yesterday, the current streak is zero — a streak is only "current" if it
   reaches today or yesterday (see `SPEC.md` §3).

3. **Out-of-order backdated completions.** Completions inserted in arbitrary
   chronological order (e.g. today before earlier days) must still yield the
   correct streak — the calculation depends on the set of completed dates, not
   insertion order.

4. **Duplicate prevention across explicit vs default date.** Completing with the
   default (today) date and then again with an explicit date that resolves to
   the same day (or vice versa) is a conflict.

5. **"Yesterday counts."** A single completion on the day before today still
   produces a current streak (it is within the current window).

6. **Timezone / date-boundary correctness.** Dates are treated as whole calendar
   days; the current-window logic is consistent around the today/yesterday
   boundary.

7. **Delete cascade.** Deleting a habit removes it and its completions; the
   habit is subsequently not found.

8. **Malformed-date validation.** Structurally invalid date strings are rejected
   with a 4xx before any completion is recorded.

These are enforced by `hidden_test.py`, which is not provided to the building
agent during a trial.

## Phase-2 hidden checks (archived habits)

A separate hidden suite (`phase2/archive_hidden_test.py`) probes the phase-2
archive feature, again without publishing exact values. At a high level it checks
that a newly created habit defaults to active, that archiving is idempotent, that
the archived-vs-active listings stay consistent, and that streak computation
still works correctly for an archived habit. Like the phase-1 hidden suite it is
not shown to the building agent during a trial.
