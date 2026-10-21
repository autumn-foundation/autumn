### Performance

- **metrics:** recording through `metrics::counter`/`gauge`/`histogram`/`timer`
  no longer copies its labels to look up a series that already exists. Once
  a label set's series exists (the first sample registers it), a handle you
  keep and reuse records without allocating; it used to make 3–5 allocations
  per sample. The per-call
  `counter(NAME).with_label(..).increment(1)` form makes half as many as
  before: 3 instead of 6 for one label. A metric name that is already
  registered is no longer re-validated on every call. See "Labels and
  cardinality" in [the metrics guide](docs/guide/metrics.md).
