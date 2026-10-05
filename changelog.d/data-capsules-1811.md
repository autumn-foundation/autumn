### Added

- **gdpr:** portable data capsules (issue #1811). `GdprRegistry::capsule`
  registers the models of a subject. `autumn data capsule export|import|verify`
  and `POST /actuator/capsules/{export,import,verify}` write one signed
  directory with the records, a `manifest.json` of models, fields and
  relationships, the referenced blobs, and an offline HTML viewer. Import
  verifies every file first and is lossless on Postgres. See
  `docs/guide/data-capsules.md`.
- **security:** `SigningSecretConfig` is now public, so code can build a
  `CapsuleSigner` from it.
