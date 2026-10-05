### Added

- **gdpr:** portable data capsules (issue #1811). A capsule holds all data of
  one subject in one signed directory. The directory holds the records, a
  `manifest.json` of models, fields, and relationships, the blobs, and an
  offline HTML viewer. Register models with `GdprRegistry::capsule`. Use
  `autumn data capsule export|import|verify` or
  `POST /actuator/capsules/{export,import,verify}`. Import verifies every file
  first. On Postgres, import has no loss of data. See
  `docs/guide/data-capsules.md`.
- **actuator:** `ProvideActuatorState::data_capsules`, a method with a
  default. A custom actuator state can give a `CapsuleService`.
- **security:** `SigningSecretConfig` is now re-exported from
  `autumn_web::security`.
