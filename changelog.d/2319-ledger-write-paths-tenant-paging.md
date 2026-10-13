### Breaking Changes

- **Breaking:** more framework writes to a ledgered table are refused with
  `LedgerError::OutOfBandWrite` (issue #2319): `#[votable]` reactions,
  `#[commentable]` comment writes, `has_many(through)` link writes, capsule
  imports, and writes (or `destroy` cascades) from an unledgered repository on
  the same table. A model factory panics. A ledgered repository that declares a derived `delete_by_*`, or a
  ledgered `tenant_scoped` repository with an `Option<String>` tenant column,
  does not compile. An update that moves a ledgered record to another tenant
  fails with `LedgerError::TenantChange`
  ([migration guide](docs/migrations/next.md)).

### Added

- **ledger:** `ledger_revisions_page` reads a record's chain in keyset pages
  (`LedgerPageRequest`, `LedgerRevisionPage`). `ledger_verify` reads the chain
  in pages too, so its memory does not grow with the chain. The new
  `LedgerChainVerifier` checks a chain one revision at a time (issue #2319).
