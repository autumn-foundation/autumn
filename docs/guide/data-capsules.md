# Portable Data Capsules

A data capsule holds all data of one subject (a user, a tenant, or an
account). It is one signed directory. A browser can open it with no server.
The app can import it again with no loss of data.

Use a capsule for GDPR and CCPA access requests, for account moves, and for
data portability.

## Contents of a capsule

| Path | Content |
| --- | --- |
| `manifest.json` | The models, fields, relationships, blobs, and the SHA-256 of each file. |
| `signature.json` | An HMAC-SHA256 of `manifest.json`. |
| `records/<table>.json` | The records of each model. |
| `blobs/<sha256>` | The referenced blob bytes (feature `storage`). |
| `viewer/index.html` | An offline HTML viewer. It has no script and loads nothing from a network. |

The viewer has one page for each model. A `belongs_to` value is a link to the
target record. Each record also shows the records that point at it.

## Register the models

Add a capsule specification to the `GdprRegistry` of the app:

```rust,ignore
use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
use autumn_web::gdpr::portability::CapsuleModel;

let registry = GdprRegistry::new()
    .register(ModelRegistration::hard_delete("users"))
    .register(ModelRegistration::hard_delete("posts"))
    .capsule(CapsuleModel::new("users", "id").blob("avatar"))
    .capsule(
        CapsuleModel::new("posts", "author_id")
            .belongs_to("author_id", "users")
            .belongs_to("parent_id", "posts"),
    );

AppBuilder::new()
    .state_initializer(move |state| state.insert_extension(registry))
    // ...
```

- The second argument of `CapsuleModel::new` is the subject column. Export
  takes the rows where this column is the subject id.
- `primary_key` changes the key column. The default is `id`.
- `belongs_to(column, target)` adds a link to `target.id`. Use
  `references(column, target, target_column)` for a different key.
- `blob(column)` marks a column that holds a `Blob` or a blob key.

## Set the signing secret

Capsules use `[security.signing_secret]`. Set it with
`AUTUMN_SECURITY__SIGNING_SECRET`. Without a secret, export and verify stop
with an error, because a random key makes capsules that no other process can
verify. `previous_secrets` continue to verify old capsules after a rotation.

## Use the CLI

```bash
autumn data capsule export --subject 42 --out ./capsule-42
autumn data capsule verify ./capsule-42
autumn data capsule import ./capsule-42
```

The CLI compiles the app and runs it with the app models, database, blob
store, and secret. `import` on a profile that is not `dev` or `test` needs
`--force`. Add `--json` for the raw report.

## Use the actuator

The actuator has three `POST` endpoints. They are available only when
`actuator.sensitive = true`. Install a capsule directory first:

```rust,ignore
use autumn_web::gdpr::portability::CapsuleDirectory;

.state_initializer(|state| state.insert_extension(CapsuleDirectory::new("var/capsules")))
```

| Endpoint | Body | Result |
| --- | --- | --- |
| `/actuator/capsules/export` | `{"subject": "42"}` | `{"capsule": "<name>", "report": {...}}` |
| `/actuator/capsules/verify` | `{"capsule": "<name>"}` | `{"report": {...}}` |
| `/actuator/capsules/import` | `{"capsule": "<name>"}` | `{"summary": {...}}` |

A capsule name is one plain path segment in the capsule directory. The
endpoints read and write nothing outside that directory.

Status codes: `400` for a bad name, `404` for an unknown capsule, `409` for a
record or blob that exists, `422` for a capsule that fails verification, and
`501` when capsules are not configured.

## Use the Rust API

```rust,ignore
use autumn_web::gdpr::portability::{
    CapsuleSigner, DataCapsule, PgCapsuleStore, export_subject, import_capsule,
};

let store = PgCapsuleStore::new(pool);
let signer = CapsuleSigner::from_config(&config.security.signing_secret)?;

let capsule = export_subject(registry.capsule_models(), &store, "42").await?;
capsule.write_dir("capsule-42".as_ref(), &signer)?;

let loaded = DataCapsule::read_dir("capsule-42".as_ref(), &signer)?;
import_capsule(&loaded, registry.capsule_models(), &store).await?;
```

`CapsuleService` gives the same steps with blobs included. Install a
`CapsuleService` extension to use a different `CapsuleStore`, for example on
SQLite.

## Integrity

`verify` and `import` check:

- The signature of `manifest.json`.
- The SHA-256 of each file in the manifest.
- That no file is missing and no file is added.

Import reads only a capsule that passes these checks. It reads each file one
time, so the bytes it imports are the bytes it verified.

## Import rules

- Each table in the capsule must be a registered capsule model.
- Import writes parent tables before child tables. A cycle of `belongs_to`
  links is an error. A link to the same table is permitted.
- Postgres import uses one transaction. A conflict writes no record.
- Import skips generated columns and moves serial and identity sequences past
  the largest imported key.
- Blobs are written before records. A blob key that holds different bytes is
  a conflict, and then no blob is written.

## Data fidelity

Postgres export reads rows with `to_jsonb`. `numeric`, `real`,
`double precision`, and `money` travel as text, so no digit is lost. Time
stamps, `bytea`, `uuid`, arrays, and `jsonb` keep their exact values.

## Limits

- The capsule is in memory during export and import.
- A record that points at a row of a different subject must find that row in
  the target database, or the import fails.
- A number in a `jsonb` column that has more digits than a 64-bit float can
  hold loses digits.
- Import into a different schema is not supported.
