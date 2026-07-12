# Agentic-DX benchmark: Habit Tracker

A deterministic, framework-agnostic harness for measuring the developer
experience of building a small web app **with a coding agent**. We point an
agent at a build brief, time it while it builds a habit-tracker app, then score
the result on three separate axes: **speed**, **correctness**, and
**maintainability**.

The core design constraint: the acceptance tests are **framework-agnostic**.
They talk to a running HTTP server over HTTP only (`acceptance/contract_test.py`
and `hidden/hidden_test.py` are stdlib-only Python), so the *same* test files
work unchanged against Autumn, Django, Rails, Spring Boot, Phoenix, Loco, or
anything else that satisfies the contract in `SPEC.md`.

## Contents

```
SPEC.md                     app spec + precise JSON API contract (source of truth)
rubric.md                   the three-axis scoring rubric
metrics-schema.json         per-run metrics.json schema
prompts/
  _core.md                  framework-agnostic build brief
  autumn.md django.md rails.md springboot.md phoenix.md loco.md
                            core brief + per-framework bootstrapping notes
  phase2_maintenance.md     the phase-2 "archived habits" maintenance brief
acceptance/contract_test.py visible acceptance tests (run against a live server)
hidden/
  HIDDEN_TESTS.md           high-level description of what the hidden suite probes
  hidden_test.py            the actual hidden checks (not shown to the agent)
scripts/score.py            deterministic scorer (reads metrics.json)
runs/                       one subdir per trial (see runs/README.md)
```

## The app being built

A habit tracker: create/list/view/update/delete habits, mark a habit complete on
a given day (with **backdated** completions supported so streaks are testable
deterministically), and compute a per-habit **current streak** and completion
history. Plus at least one server-rendered HTML view. The exact endpoints,
status codes, and streak semantics are pinned in `SPEC.md`.

## Phases

- **Phase 1 — greenfield build.** The agent builds the app from
  `prompts/<framework>.md`. Measures build speed, correctness, and the
  maintainability of what it produced.
- **Phase 2 — maintenance change.** The agent applies the "archived habits"
  change in `prompts/phase2_maintenance.md` to the phase-1 app: add an
  `archived` boolean, add `POST /api/habits/{id}/archive`, exclude archived
  habits from `GET /api/habits`, and expose them at `GET /api/habits?archived=true`
  (archived habits are hidden, not deleted). Measures how quickly and cleanly an
  agent extends an existing codebase.

## Running a trial

1. **Pick a run id and framework**, e.g. `autumn-p1-2026-07-12`. Create
   `runs/<run-id>/` and copy the chosen prompt into it as `prompt.md`.
2. **Start the timer and point the agent** at `prompts/<framework>.md`. Have it
   build the app under `runs/<run-id>/app`. Capture its transcript and count its
   tool calls.
3. **Boot the app**: `cd runs/<run-id>/app && PORT=8080 ./run.sh`. `run.sh` must
   apply migrations, seed demo data, and start the server on `PORT` (default
   8080), blocking while it runs.
4. **Run the visible tests** against the live server. Record wall-clock time to
   the first fully-green run:
   ```sh
   BASE_URL=http://localhost:8080 python3 acceptance/contract_test.py
   ```
   Both test scripts retry-connect for up to 30s, so you can start them right
   after launching the server.
5. **Run the hidden tests**:
   ```sh
   BASE_URL=http://localhost:8080 python3 hidden/hidden_test.py
   ```
6. **Fill in `runs/<run-id>/metrics.json`** per `metrics-schema.json` (the
   `RESULT: X/Y passed` lines give the check counts; add the timing, tool-call,
   LOC, and manual maintainability numbers).
7. **Score it**:
   ```sh
   python3 scripts/score.py runs/<run-id>/metrics.json | tee runs/<run-id>/score.txt
   ```
   The scorer prints the three axes separately and never collapses them into a
   single number. It is deterministic: same `metrics.json` in, same output out.

For phase 2, repeat steps 2-7 with `prompts/phase2_maintenance.md` against the
phase-1 app, recording a second `metrics.json` with `"phase": 2`.

## Requirements

- Python 3 (stdlib only — no `pip install` needed for the test harness).
- Whatever toolchain the chosen framework needs (Rust, Python/Django, Ruby/Rails,
  JVM, Elixir, ...), plus a database as described in that framework's prompt.
