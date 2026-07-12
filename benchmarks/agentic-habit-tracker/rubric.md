# Scoring rubric

Three **separate** axes. They are reported independently and never collapsed
into a single overall number — a framework can be fast but sloppy, or slow but
rock-solid, and the benchmark is meant to surface that. `scripts/score.py`
computes the speed and correctness numbers deterministically from a run's
`metrics.json`; the two manual maintainability sub-scores are entered by a
reviewer.

## Axis 1 — SPEED

Raw signals:

- `wall_clock_seconds` — wall-clock time from the moment the agent starts until
  the **first green** (all visible acceptance checks pass).
- `agent_tool_calls` — number of tool calls the agent made to reach first green.

Normalized (0-100, higher = faster), documented reference points in `score.py`:

- **time_score**: 100 at `<= 120s`, 0 at `>= 3600s`, linear between.
- **tool_call_score**: 100 at `<= 20` calls, 0 at `>= 300` calls, linear between.
- **SPEED (normalized)** = mean of the two.

Both raw and normalized values are reported.

## Axis 2 — CORRECTNESS

- `visible_pass_rate` = `visible_checks_passed / visible_checks_total`
  (from `acceptance/contract_test.py`).
- `hidden_pass_rate` = `hidden_checks_passed / hidden_checks_total`
  (from `hidden/hidden_test.py`).

Score (0-100):

```
CORRECTNESS = (0.40 * visible_pass_rate + 0.60 * hidden_pass_rate) * 100
```

Hidden checks are weighted higher because they probe the streak edge cases that
distinguish a correct implementation from one that only passes the obvious happy
path.

## Axis 3 — MAINTAINABILITY

Reported as a set of signals, not reduced to one score:

- `generated_loc` — lines of code the agent generated for the app (lower is
  better, but **not zero** — an empty app fails correctness). Count app source
  only; exclude vendored dependencies and lockfiles.
- `has_tests` — boolean: did the agent ship automated tests for core behavior?
- `readme_quality` — manual 0-3: are the run instructions present, accurate, and
  sufficient to boot + exercise the app? (0 none, 1 minimal, 2 good, 3 excellent).
- `uses_conventions` — manual 0-3: does the code follow the framework's idioms
  and layout rather than fighting them? (0 anti-idiomatic, 3 fully idiomatic).

## Phases

Each framework can be run in two phases (see `README.md`):

- **Phase 1** — greenfield build from `prompts/<fw>.md`.
- **Phase 2** — the maintenance change from `prompts/phase2_maintenance.md`
  (archived habits) applied to the phase-1 app.

Score each phase separately with its own `metrics.json` (the `phase` field
distinguishes them). Phase 2 especially exercises the maintainability axis: how
small and idiomatic the change is, and whether existing tests keep passing.

## Determinism

`score.py` is a pure function of `metrics.json` — same input, same output. It
performs no timing, no network, and no filesystem discovery beyond reading the
given metrics file. All wall-clock measurement happens during the trial and is
recorded into `metrics.json`; scoring only reads it back.
