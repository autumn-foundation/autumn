### Performance

- **money:** `ledger::post` names a transaction with a time-ordered UUID v7
  instead of a random v4. The two indexes keyed by the id take their inserts
  on the same few pages, which cuts the WAL a post writes by 27–32% on a
  ledger of 100k–3M transactions (`docs/perf/money-post/`). Ids stay opaque
  UUID strings; ones already stored are unchanged.
