### Added

- **cli:** `autumn schema width` reports every Diesel `table!` and `#[model]`
  wider than 32 columns, and every migration table wider than 32 that no
  declaration maps, plus whether `diesel` resolves with `64-column-tables` or
  `128-column-tables`. The cap is per declaration: a wide table declared in
  halves (the second with `#[sql_name = "<table>"]`) passes. It exits non-zero
  on a finding, for use as a CI gate; `--max <N>` raises the limit, `--all`
  lists every table.
- **cli:** `autumn build` warns, without failing, on the same findings. A
  wider tuple cap makes every crate that depends on diesel slower and hungrier
  to compile (measured on one crate: 1.16 GB / 1m18s at 32, 2.92 GB / 2m04s at
  64, 9.95 GB / 8m16s at 128). See `docs/guide/table-width.md`.

### Testing

- **ci:** the `lint` job runs `autumn schema width .` over the repository, so
  no framework, plugin, starter or example ships a Diesel declaration wider
  than 32 columns or an undeclared migration table wider than 32, and no
  workspace crate turns on a wide diesel feature.
