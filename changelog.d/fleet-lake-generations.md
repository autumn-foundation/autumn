### Fixed

- **fleet lake:** the lake reads `ShipMode::Generations` replicas (issue
  #3770). Before, it read only WAL replicas, and skipped each member of a
  generations fleet (for example a DuckDB fleet) as `Unreplicated` with no
  error. Now core plans and restores such a member from its
  `manifest.json` and checks each file hash. `has` and `keep` get the
  manifest's generation id. A member that the sink cannot read fails with
  `LakeError::Ingest`; a manifest of another version fails with
  `LakeError::Restore`. Neither is `Unreplicated`.

### Added

- **fleet lake:** `LakeFormat` and `LakeMember::format` give the format of
  each member's replica. `LakeSink::check_identity(member)` checks the
  identity of a generations member before `add`; the default refuses the
  member.
