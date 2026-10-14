### Fixed

- **db scrub:** closes the open gaps of issue #2366.
  - `--artifact` scrubs only the targets the artifact restored.
  - The source-overwrite guard compares hostless URLs and shard URLs, and no longer blocks the destination profile.
  - Purge targets are in the trigger warning.
  - `#[encrypted]` columns are found in raw identifiers, inline modules and any `src/` file.
  - A structural `#[encrypted]` column can be declared `safe`.
  - Row-level security refuses only a role it binds.
  - PII on an index-expression or exclusion-constraint column is refused. Partition-only constraints fold into the parent.
  - Encrypted columns are type- and length-checked at plan time.
  - A schema change after classification is refused.
  - `#[cfg(test)]` anonymize registrations are ignored.
