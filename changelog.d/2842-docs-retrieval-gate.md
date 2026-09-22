### Added

- **📖 Folio: make Autumn's log settings findable, and gate retrieval
  (questions 12/18 → 18/18, 0 pages added):** `docs/guide/logging-pii.md`
  carried `[log] level`, `log.format` and the access-log switches under the
  title "Logging & PII" — the name the README listed it by — so a reader
  asking how to change the log level read it as a privacy page and never
  opened it. Searching the title and headings of all 162 guide pages for
  "log level", "debug logging" or "json logs" returned **zero results**,
  while the `[log]` section itself appeared in 9 fences across 7 pages: the
  answer existed, was correct, was linked, and was unreachable by anyone who
  arrived with words rather than a link. The runtime half of the same
  question was worse — `PUT /actuator/loggers/{name}`, which changes a live
  `tracing` subscriber with no redeploy and is the only answer that helps
  during an incident, appeared on **no reader-facing page at all**: it was
  documented in `skills/autumn-web/SKILL.md` (a context pack for agents),
  named in one Spring-comparison table row as a Rust call rather than an
  endpoint, and otherwise mentioned only in `deployment.md`'s list of
  endpoints production turns off. This is a findability defect with a
  coverage tail, so the fix adds no page: `logging-pii.md` is retitled
  "Logging: log levels, format, and PII scrubbing" (the path, and so every
  inbound link, is unchanged), opens on the four questions it answers, and
  gains `## Set the log level`, `### Turn on debug logging for one target`,
  `## Choose the log format (pretty or JSON)` and `## Change log levels at
  runtime, without a restart` — the last documenting the request and
  response shapes, that `applied` and not the status code is what says a
  change reached the subscriber, that overrides die with the process, and
  that the endpoint needs `[actuator] sensitive = true`, and — the part that
  would otherwise be found at 3am — that the `prod` profile turns CSRF on, so
  the bare `curl` is a `403` there and the page shows both ways through
  (`[security.csrf] exempt_paths`, or the double-submit cookie/header pair).
  `[log] level` also accepts `off`, which the runtime endpoint does not, and
  the page now states the *profile-specific* defaults rather than a flat
  `info`/`Auto`: `dev` is `debug`/`Pretty`, `prod` is `info`/`Json`, both set
  outright by smart defaults, and any other profile falls back to
  `info`/`Auto`. "Why is dev so noisy" has a table to land on. The
  `Auto` /
  `Pretty` / `Json` table moves here from `getting-started.md`, which keeps a
  one-paragraph summary and a link, so the answer has one home rather than
  two that drift.

  Found by measuring the direction no existing gate measures.
  `scripts/check-docs-retrieval.sh` (new, wired into CI with its own
  self-test — run `--self-test` for the count, which grows with the matcher
  and so is deliberately not pinned here) pairs a reader question with the
  page that answers it and checks
  the page says so in its slug, its H1 or a heading — body text deliberately
  excluded, because "the answer is in there somewhere" is the defect, not the
  pass condition. The eleven existing docs gates all check a page the reader
  has already reached, and `check-docs-orphans.sh` checks a path of links to
  it exists; none asks what the reader actually arrives with, which is not
  "which link do I click" but "what do I type". Baseline: 18 questions, 6
  defects, all six this class. After: 0. The 12 that already passed are
  pinned as a regression set, so a future retitle cannot take them away
  quietly. Fenced content is excluded from the heading index — a `#` line in
  a shell or TOML fence is a comment, and indexing one lets a fixture row go
  green on a code comment on the very page it names.
