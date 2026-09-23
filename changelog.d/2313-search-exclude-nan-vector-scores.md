### Fixed

- `autumn-search`: vector similarity search now excludes zero-norm/NaN
  candidates in the SQL `WHERE` predicate, before ordering and limiting.
  Previously a zero-norm stored embedding made pgvector's `<=>` return
  `NaN`, which sorts ahead of every finite score under the filtered query's
  `ORDER BY ... DESC` — so zero-vector documents (naturally produced by
  `HashingEmbedder` for empty text) could consume every `LIMIT` slot while
  valid neighbours were omitted, and the Rust-side `is_finite` filter ran
  too late to give those slots back
  ([#2313](https://github.com/autumn-foundation/autumn/issues/2313)).
