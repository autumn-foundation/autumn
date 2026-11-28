### Performance

- **scheduler:** the Postgres tick coordinator prunes expired tick rows once
  per tick, not once per replica that tries it. The replica that wins a tick
  runs the table-wide prune after its claim. A replica that loses deletes
  only its own key's row, and only when that row has expired. The prune was
  61–66% of the tick table's shared buffers. A fleet of 2, 6 or 12 replicas
  now reads 21–30% fewer buffers per attempt (prune buffers −62% to −89%).
  Each attempt still issues two statements, and the claim results are
  unchanged. See `docs/perf/scheduler-tick-claim`.
