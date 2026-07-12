Prompt-Version: v1

# Build brief: Habit Tracker (framework-agnostic core)

You are being timed and scored while you build a small **habit tracker** web
application. Build it end to end, get it running, and make the acceptance tests
pass. Work inside the run directory you were given (e.g. `runs/<run-id>/app`).

## What to build

A habit tracker with a database, a JSON API, at least one server-rendered HTML
view, input validation, tests, seed data, and a launcher script.

Users can create habits, list/view/update/delete them, mark a habit complete on
a given day, and see a per-habit **current streak** plus completion history.

## Hard requirements

1. **Persistence.** Store data in a real database (the framework's standard
   database + migration story). No in-memory-only storage.
2. **JSON API — exact contract.** Implement the endpoints in `SPEC.md` **exactly**
   (status codes, shapes, validation, the 409-on-duplicate rule, backdated
   completions). Read `SPEC.md` in full — the acceptance tests hit these routes
   over HTTP and assert the documented status codes and JSON shapes. In
   particular:
   - `POST /api/habits`, `GET /api/habits`, `GET /api/habits/{id}`,
     `PUT /api/habits/{id}`, `DELETE /api/habits/{id}`,
     `POST /api/habits/{id}/complete`.
   - The complete endpoint MUST accept an optional `{"date": "YYYY-MM-DD"}` so
     completions can be **backdated**; duplicate `(habit, date)` → **409**.
   - `GET /api/habits/{id}` returns `current_streak` and a descending `history`
     array — implement the streak rules in `SPEC.md` §3 precisely.
3. **Server-side HTML view.** `GET /` returns `text/html` (status 200) listing
   the habits, rendered on the server.
4. **Validation.** Reject empty/missing `name` and malformed dates with a 4xx
   (422 preferred, 400 accepted). Reject unknown ids with 404.
5. **Tests.** Include automated tests for the core behavior (create, complete,
   streak calculation, validation, duplicate prevention).
6. **Seed / demo data.** Seed a couple of demo habits (and some completions) on
   first run so the HTML view and list endpoint aren't empty.
7. **Run instructions.** Provide a `run.sh` at the app root that:
   - starts the server on the port from the `PORT` env var (default **8080**),
   - applies database migrations,
   - seeds demo data,
   - and **blocks** while the server runs.
   Also provide a short `README` explaining how to run it.

## The API contract

The authoritative contract lives in `SPEC.md` (sibling of this prompt). Do not
paraphrase it from memory — open it and implement it to the letter. The streak
semantics and the exact status codes are what the tests check.

## How you'll be evaluated

- **Speed:** wall-clock time to first green (visible acceptance tests pass) and
  the number of tool calls you make.
- **Correctness:** visible acceptance tests + a hidden test suite that probes
  streak edge cases (gaps, resets, out-of-order backdated completions,
  duplicate prevention across explicit/default dates, malformed dates, delete
  cascade).
- **Maintainability:** lines of code, presence of tests, quality of run
  instructions, and idiomatic use of the framework's conventions.

Keep the implementation clean and idiomatic for your framework. No dead code, no
TODOs. Get it running and prove it with the tests.
