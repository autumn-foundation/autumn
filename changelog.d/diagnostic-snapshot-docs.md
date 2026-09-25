### Documentation

- **docs:** document `autumn export` in the [deployment
  guide](docs/guide/deployment.md#capturing-a-diagnostic-snapshot-for-a-bug-report-autumn-export).
  The command that captures a running app's health, metrics, tasks and loggers
  into one JSON file — the thing a bug report wants attached — appeared on no
  reader-facing page, so the reader most likely to need it was someone
  assembling a support ticket mid-incident. Three reader questions
  ("diagnostic snapshot", "export diagnostics", "bug report") landed nowhere
  before this; the bare word "diagnostics" landed on four pages that answer
  something else.
- **docs:** the new section states what no `autumn export` doc said: the
  command **fails outright** against an app running the production default
  `actuator.sensitive = false`. `/actuator/tasks` and `/actuator/loggers` are
  mounted only under `sensitive = true`, an unmounted actuator path answers
  `404`, and `export` treats any endpoint it cannot read as fatal — it writes
  no file, prints `Failed to fetch tasks from …: HTTP 404 Not Found`, and exits
  `1`. There is no partial snapshot and no flag to ask for one, and `sensitive`
  is a single app-wide switch, so turning it on to take a snapshot also mounts
  `/actuator/env`, `/actuator/configprops`, `/actuator/jobs` and
  `/actuator/shadow`.

### Testing

- **📖 Folio:** `scripts/check-docs-cli-coverage.sh`'s triaged backlog drops
  from 6 entries to 5 and CLI documentation coverage rises to 178/195 (91%),
  with the `export` waiver retired rather than re-dated.
  `scripts/docs-retrieval-questions.tsv` gains the three questions that failed
  cold, so a retitle cannot take the answer away again.
