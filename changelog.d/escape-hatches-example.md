### Documentation

- **examples:** new `examples/escape-hatches`. One stockroom app shows each
  kind of escape hatch, each with a real reason and a test. It starts from
  `#[model]`, `#[repository]` and `with_lock`. It uses a hatch only where they
  cannot do the job. The hatches: `Db::tx` with guarded Diesel writes, a
  set-based `UPDATE`, `diesel::sql_query`, `#[intercept]`/`.scoped`/`.layer`
  tower layers, raw Axum routers through `.nest` and a `Plugin`,
  `.error_pages`, `.exception_filter`, and a `DatabasePoolProvider` that reads
  a rotated password file.
