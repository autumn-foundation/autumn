### Added

- **cli:** `autumn schema width` reports every Diesel table wider than 32
  columns — across replayed migrations, `diesel::table!` blocks and `#[model]`
  structs — and whether `diesel` resolves with `64-column-tables` or
  `128-column-tables`. It exits non-zero on a finding, for use as a CI gate;
  `--max <N>` raises the limit, `--all` lists every table.
- **cli:** `autumn build` warns, without failing, when the app has a table
  wider than 32 columns or turns on a wide diesel feature. A wider tuple cap
  makes every crate that depends on diesel slower and hungrier to compile
  (measured on one crate: 1.16 GB / 1m18s at 32, 2.92 GB / 2m04s at 64,
  9.95 GB / 8m16s at 128). See `docs/guide/table-width.md`.

### Testing

- **ci:** the `lint` job runs `autumn schema width .` over the repository, so
  no framework, plugin, starter or example migration ships a table wider than
  32 columns, and no workspace crate turns on a wide diesel feature.
