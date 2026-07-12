# Habit Tracker — Application Spec & API Contract

This document is the single source of truth for the benchmark. Every framework
implementation MUST satisfy the JSON API contract below **exactly**, because the
acceptance tests are framework-agnostic: they talk to a running HTTP server over
HTTP and make no assumptions about the language, framework, or ORM used.

## 1. What the app is

A small "habit tracker" web application. A user can:

- Create named habits (with an optional description).
- List, view, update, and delete habits.
- Mark a habit as completed on a given calendar day.
- See a per-habit **current streak** (consecutive days completed) and the full
  completion **history**.
- View a server-rendered HTML page listing the habits.

The app MUST persist data in a database (not just in-memory), expose the JSON
API described below, render at least one server-side HTML view, validate input,
and ship with tests, seed/demo data, and a `run.sh` launcher.

## 2. HTTP / JSON API contract

- Base path: `/api`.
- All request and response bodies are JSON (`Content-Type: application/json`),
  **except** the HTML view at `GET /`.
- IDs are stable for the lifetime of a record. They MAY be integers or strings;
  tests treat the returned `id` opaquely and echo it back in subsequent URLs.
- Timestamps (`created_at`) are ISO-8601 strings.
- Calendar dates are ISO `YYYY-MM-DD` strings (no time component).

### 2.1 Habit object

```json
{
  "id": 1,
  "name": "Meditate",
  "description": "10 minutes every morning",
  "created_at": "2026-07-12T08:00:00Z"
}
```

`description` MAY be `null` when omitted at creation.

### 2.2 Endpoints

#### `POST /api/habits`

Create a habit.

- Request body: `{"name": string, "description": string?}`
- Success: **201 Created** with the created habit object (must include `id`,
  `name`, `description`, `created_at`).
- Validation: an empty or missing `name` MUST return **422** (400 also
  accepted). No habit is created in that case.

#### `GET /api/habits`

List habits.

- Success: **200 OK** with a JSON **array** of habit objects.
- Ordering is unspecified; tests do not rely on it.
- (Phase 2 only) supports an `?archived=true` query parameter — see §4.

#### `GET /api/habits/{id}`

Fetch a single habit, enriched with streak + history.

- Success: **200 OK** with the habit object plus two extra fields:
  ```json
  {
    "id": 1,
    "name": "Meditate",
    "description": "10 minutes every morning",
    "created_at": "2026-07-12T08:00:00Z",
    "current_streak": 3,
    "history": ["2026-07-12", "2026-07-11", "2026-07-10"]
  }
  ```
- `current_streak`: integer, computed per §3.
- `history`: array of ISO `YYYY-MM-DD` date strings, **sorted descending**
  (most recent first), one entry per completed day, no duplicates.
- Unknown `id`: **404 Not Found**.

#### `PUT /api/habits/{id}`

Update a habit's `name` and/or `description`.

- Request body: `{"name": string, "description": string?}`
- Success: **200 OK** with the updated habit object.
- Unknown `id`: **404 Not Found**.
- Empty `name`: **422** (400 accepted).

#### `DELETE /api/habits/{id}`

Delete a habit and all of its completions (cascade).

- Success: **204 No Content** (empty body).
- After deletion, `GET /api/habits/{id}` MUST return **404**.
- Deleting an unknown `id` SHOULD return **404** (204 is tolerated by tests).

#### `POST /api/habits/{id}/complete`

Mark the habit as completed for a calendar day.

- Request body: `{"date": "YYYY-MM-DD"?}` — `date` is **optional**; when omitted
  it defaults to **today** (server date).
- Success: **201 Created** with `{"date": "YYYY-MM-DD", "current_streak": int}`
  where `date` is the completed day and `current_streak` is recomputed per §3
  **as of the completed date is NOT used** — the streak returned here is the
  current streak as of today (server date), consistent with `GET`.
- Duplicate: completing the **same habit** for the **same date** twice MUST
  return **409 Conflict**. This holds whether the two calls use an explicit
  `date` or the default (today) — they collide if they resolve to the same day.
- Validation: a malformed/invalid `date` string (e.g. `"2026-13-40"`,
  `"not-a-date"`) MUST return **422** (400 accepted). No completion is recorded.
- Unknown habit `id`: **404 Not Found**.
- **Backdated completions MUST be supported.** Tests deliberately record
  completions for past dates, in arbitrary insertion order, so that streak
  behavior can be verified deterministically without depending on the real
  calendar.

#### `GET /` (HTML view)

- Success: **200 OK** with `Content-Type: text/html` (charset suffix allowed).
- The body is a server-rendered HTML page listing the current habits. Minimal
  markup is fine; tests only assert status 200 and a `text/html` content type.

## 3. Streak semantics (precise)

This is the part hidden tests probe hardest. Implement it exactly.

- A **completion** is a `(habit_id, date)` pair. At most one completion exists
  per habit per calendar day (enforced by the 409 rule).
- `current_streak` = the number of consecutive calendar days, counting
  **backward from a reference "as-of" date**, on which the habit was completed,
  stopping at the first missed day.
- The reference "as-of" date for `GET /api/habits/{id}` (and for the
  `current_streak` returned by the complete endpoint) is **today** (the server's
  current date).
- A streak is only "current" if it includes **today OR yesterday**:
  - If there is a completion for today, count today, then yesterday, then the day
    before, … until a day with no completion.
  - If there is no completion for today but there is one for yesterday, start
    counting at yesterday and walk backward.
  - If the most recent completion is **older than yesterday** (i.e. two or more
    days ago with nothing since), `current_streak = 0`.
  - If there are no completions at all, `current_streak = 0`.

### Worked examples (let `T` = today)

| Completions present                     | current_streak |
|-----------------------------------------|----------------|
| `T`, `T-1`, `T-2`                       | 3              |
| `T-1`, `T-2`, `T-3` (nothing on `T`)    | 3              |
| `T-1` only                              | 1              |
| `T-2` only                              | 0              |
| `T`, `T-2`, `T-3` (gap at `T-1`)        | 1              |
| `T-2`, `T-3`, `T-4` (skip `T-1`, `T`)   | 0              |
| none                                    | 0              |

Note the second row: because the most recent completion (`T-1`) is yesterday,
the streak is "current" and counts the full consecutive run `T-1, T-2, T-3`.

- `history` = every completed date for the habit, de-duplicated, formatted as
  ISO `YYYY-MM-DD`, sorted **descending** (most recent first). Independent of the
  streak's "as-of" cutoff — it lists all completions, not just the current run.

## 4. Phase 2 maintenance change (archived habits)

Not part of the initial build. Introduced as a follow-up maintenance brief (see
`prompts/phase2_maintenance.md`):

- Add an `archived` boolean to the habit (default `false`).
- Add `POST /api/habits/{id}/archive` → archives the habit (idempotent; returns
  the updated habit or 200/204).
- `GET /api/habits` excludes archived habits by default.
- `GET /api/habits?archived=true` returns **only** archived habits.
- Archiving is a soft-delete/hide: the record and its completions are retained.

## 5. Runtime requirements

- Persistence via a real database.
- At least one server-rendered HTML view (`GET /`).
- Input validation for required fields and date formats (§2).
- Automated tests covering core behavior.
- Seed/demo data on first run.
- A `run.sh` at the app root that starts the server listening on `PORT`
  (environment variable, default **8080**), applies migrations, and seeds demo
  data. `run.sh` MUST block while the server runs.
