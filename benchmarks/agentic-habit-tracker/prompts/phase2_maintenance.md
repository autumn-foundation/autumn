Prompt-Version: v1

# Phase-2 maintenance brief: Archived habits

This is a **follow-up** brief handed to the agent after the phase-1 app is built
and green. It measures how quickly and cleanly an agent can extend an existing
codebase (maintainability under change), not greenfield build speed.

Work in the same app the phase-1 trial produced. Do not rewrite it — make the
smallest correct change that satisfies the new requirement, keeping the existing
API contract (`SPEC.md` §1-3) fully working.

## The change: soft-delete / hide via an `archived` flag

1. **Data.** Add an `archived` boolean to the habit, defaulting to `false`. Add a
   migration (do not drop or recreate existing data).

2. **New endpoint.** `POST /api/habits/{id}/archive`
   - Sets `archived = true` for the habit.
   - Idempotent: archiving an already-archived habit is not an error.
   - Success: **200** (returning the updated habit) or **204**.
   - Unknown `id`: **404**.

3. **Listing behavior.**
   - `GET /api/habits` (no query) now returns **only non-archived** habits.
   - `GET /api/habits?archived=true` returns **only archived** habits.
   - (`GET /api/habits?archived=false` MAY be treated the same as no query.)

4. **Preserve everything else.**
   - `GET /api/habits/{id}` still works for archived habits (still 200, still
     shows streak + history). Archiving is a hide/soft-delete, not a delete —
     the record and its completions are retained.
   - The HTML view at `GET /` should list non-archived habits (matching the
     default list behavior).
   - All phase-1 endpoints, status codes, and streak semantics remain unchanged.

5. **Tests.** Add tests covering: a newly created habit is not archived; after
   `POST /archive` it disappears from the default list; it appears under
   `?archived=true`; the archive endpoint is idempotent; `GET /api/habits/{id}`
   still returns the archived habit.

## How this phase is scored

Same three axes as phase 1 (`rubric.md`): speed (time + tool calls to green),
correctness (a phase-2 visible + hidden check set for the archive feature), and
maintainability (diff size, whether existing tests still pass, whether the change
follows the codebase's existing conventions rather than bolting on a parallel
path).
