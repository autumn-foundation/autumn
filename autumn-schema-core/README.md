# autumn-schema-core

Canonical, dialect-independent schema IR + type mappings for the
[Autumn](https://github.com/madmax983/autumn) web framework.

This is a **leaf** crate: it depends only on `serde` and pulls in neither
`diesel`, `syn`, nor `autumn-web`, so both `autumn-macros` and `autumn-cli` can
depend on it without a dependency cycle.

It holds the canonical table-shape IR (`Schema` / `Table` / `Column` /
`ColumnType`) plus the bidirectional PostgreSQL/SQLite type mappings mirrored
byte-for-byte from `autumn-cli/src/generate/dsl.rs`. Parity unit tests in
`dsl.rs` lock the two together so neither can drift.

Part of the "Autumn Declarative Schema" wave (tracking issue #1975).
