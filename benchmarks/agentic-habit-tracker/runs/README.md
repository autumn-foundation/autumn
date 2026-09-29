# runs/

One subdirectory per trial: `runs/<run-id>/`. A good `run-id` encodes the
framework, phase, and date, e.g. `autumn-p1-2026-07-12` or
`django-p2-2026-07-12`.

Each run directory stores everything needed to reproduce and audit the trial:

```
runs/<run-id>/
  prompt.md          # exact prompt handed to the agent (copy of prompts/<fw>.md
                     # or prompts/phase2_maintenance.md, incl. its Prompt-Version)
  transcript.md      # the agent's full transcript / tool-call log
  app/               # the generated application (or a pointer/README if the app
                     # lives elsewhere, e.g. a separate repo or worktree)
  test-output.txt    # captured stdout of contract_test.py and hidden_test.py
                     # (the PASS/FAIL lines and RESULT: X/Y summaries)
  metrics.json       # per-run metrics (see ../metrics-schema.json)
  score.txt          # captured output of scripts/score.py metrics.json
```

`runs/` is intentionally kept in the tree via `.gitkeep`. Whether to commit
individual run artifacts (transcripts, generated apps) is up to the operator;
large generated apps may be pointed to rather than committed.
