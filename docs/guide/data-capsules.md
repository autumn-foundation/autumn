# Portable Data Capsules

A data capsule holds all data of one subject. A subject is a user, a tenant,
or an account. The capsule is one signed directory. A browser opens it with no
server. The app can import it again with no loss of data.

Use a capsule for GDPR and CCPA access requests, for account moves, and for
data portability.

## Contents of a capsule

| Path | Content |
| --- | --- |
| `manifest.json` | The models, fields, relationships, blobs, and the SHA-256 of each file. |
| `signature.json` | An HMAC-SHA256 of `manifest.json`. |
| `records/<table>.json` | The records of each model. |
| `blobs/<sha256>` | The blob bytes that the records refer to (feature `storage`). |
| `viewer/index.html` | An offline HTML viewer. It has no script. It loads nothing from a network. |

A capsule holds personal data. On Unix, export makes each directory `0700`
and each file `0600`. Export refuses an output path that is a link.

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
    .capsule(
        CapsuleModel::new("users", "id")
            .blob("avatar")
            .exclude("password_hash"),
    )
    .capsule(
        CapsuleModel::new("posts", "author_id")
            .belongs_to("author_id", "users")
            .belongs_to("parent_id", "posts"),
    );

autumn_web::app()
    .state_initializer(move |state| state.insert_extension(registry))
    // ...
```

- The second argument of `CapsuleModel::new` is the subject column. Export
  takes the rows where this column is the subject id.
- `primary_key` changes the key column. The default is `id`.
- `belongs_to(column, target)` adds a link to `target.id`. Use
  `references(column, target, target_column)` for a different key.
- `blob(column)` marks a column that holds a `Blob` or a blob key.
- `exclude(column)` keeps a column out of the capsule.

Every column that a model names must be in the table: the subject column, the
key, and each link, blob, and excluded column. A typo fails the export
(`400`), so a capsule never lacks records, blobs, or links.

**Exclude all secrets.** Export copies every column that you do not exclude.
Exclude password hashes, tokens, and internal flags. Import cannot restore an
excluded column, so the column must accept `NULL` or have a default.

**Check blob ownership.** Export copies each blob that a blob column names. If
users can write a blob key into a row, make sure that the key belongs to the
subject. If a record holds a blob key and no blob store is configured, export
fails. A blob that the store does not have is skipped with a warning, but its
record still names it: import then fails (`400`) rather than write a record
that points at nothing, or at other bytes under the same key. If a blob changes while export reads it, export fails with a conflict
(`409`): run it again. A `Blob` value names its store (`provider_id`). If it
names another store than the configured one, for example after a switch of
backend, export fails with a conflict (`409`): the same key in the new store
can hold other bytes.

## Set the signing secret

Capsules use `[security.signing_secret]`. Set it with
`AUTUMN_SECURITY__SIGNING_SECRET`. Export, verify, and import need a secret.
Without one, they stop with an error. With the `prod` profile, the CLI uses
the same secret rules as the server: a short secret, or a short key in
`previous_secrets`, stops the command.

Verify also accepts the keys in `previous_secrets`. Old capsules stay valid
after a key rotation.

## Use the CLI

```bash
autumn data capsule export --subject 42 --out ./capsule-42
autumn data capsule verify ./capsule-42
autumn data capsule import ./capsule-42
```

The CLI compiles the app. Then it runs the app with the app models, database,
blob store, and secret. With `-p`, the app runs from the directory of that
workspace member, so it reads its own `autumn.toml` and `.env`.

On a profile that is not `dev` or `test`, `import` needs `--force`. Add
`--json` to get the raw report. `verify` uses the signer of an installed
`CapsuleService`, if the app has one. `verify` does not open the database or
run a migration.

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

A capsule name is one plain path segment in the capsule directory. Export
makes the name from the time and a count, not from the subject, so a listing of
the directory does not show who asked for an export. The endpoints do not
follow a link, and do not read or write outside that directory.

| Status | Cause |
| --- | --- |
| `400` | The name or the input is not valid. |
| `404` | The capsule is not in the directory. |
| `409` | A record or a blob exists, or a parent record is missing. |
| `422` | The capsule fails verification. |
| `501` | Capsules are not configured. |

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

`PgCapsuleStore` needs the `db` feature. It is not available with `sqlite`.
`CapsuleService` does the same steps and includes blobs. To use a different
`CapsuleStore`, install a `CapsuleService` extension. You can do this on
SQLite.

**Sharded apps must install a `CapsuleService`.** The default store reads the
control database, but the rows of a subject are on a shard. With
`[[database.shards]]`, the default service is not available (`501`). Give a
`CapsuleStore` that reads the shard of the subject.

## Integrity

`verify` and `import` do these checks:

- The signature of `manifest.json` is correct.
- The SHA-256 of each file agrees with the manifest.
- No file is missing. No file is added. No entry is a link.

Import reads only a capsule that passes these checks. It reads each file one
time, so it imports the same bytes that it verified.

On Unix, export and verify open the capsule directory one time and do not
follow a link. All reads and writes go through that open directory. If a
process changes a directory to a link after the open, the reads and writes do
not follow it. Export makes each file new and never replaces a file.

The capsule itself must not be a link. The directories above it are resolved
one time, when the capsule is opened. A link among them is followed, as the
operator configured it. So only the app user may write to the configured
capsule directory and to each directory above it.

## Import rules

- Each table in the capsule must be a registered capsule model.
- Import writes parent tables before child tables. A cycle of `belongs_to`
  links is an error. A link to the same table is permitted.
- Postgres import uses one transaction. If one record fails, import writes no
  record.
- Import skips generated columns.
- Import moves each serial or identity sequence past the imported keys, in
  the direction of the sequence. A sequence makes only `start + k * step`, so
  only keys on that path count: with `INCREMENT BY 3` from 1, key 500 is not
  a value it makes and does not move it. It does this only after all inserts and
  deferred constraints succeed, and after it checks every move, so a failed
  import does not change a sequence.
- A key outside the range of its sequence is a conflict (`409`). A sequence
  with `CACHE` above 1 is refused (`501`): other sessions can hold cached
  values that an imported key can take. A `CYCLE` sequence is refused
  (`501`) too: after its last value, it starts again at an imported key.
- The import role needs `UPDATE` on each sequence it moves, and `SELECT` or
  `USAGE` to read where the sequence is. Without them, import stops before
  it moves any sequence.
- **Import when no other writer uses the tables.** Another session can take a
  value from a sequence and insert its row later. If the capsule holds that
  key, the import succeeds and the other insert fails with a duplicate key.
  The same holds for a blob that another writer replaces after import checks
  it. No lock can prevent this, so run import in a maintenance window.
- Import writes blobs before records. If the record import fails, import
  removes the blobs that it wrote. If a blob key holds different bytes or a
  different MIME type, import stops and writes no blob. If a blob changes
  while import reads it, import stops with a conflict (`409`). If another writer
  takes a key during the import, import stops and deletes the blobs that it
  wrote. Import needs a blob store with a conditional create
  (`BlobStore::put_if_absent`). `LocalBlobStore` and the S3 backend have one.
  With a store that has none, import of a capsule with blobs fails (`501`).
- An imported `Blob` value names the target store: import sets its
  `provider_id`, `etag`, `content_type` and `byte_size` to those of the store
  that now holds the bytes. A
  plain key string stays as it is.

## Data accuracy

Postgres export reads all models in one snapshot. It reads rows with
`to_jsonb`. Export writes `numeric`, `real`, `double precision`, and `money`
as text, also through a domain. Export writes `money` and each `money[]`
item as a plain number, so the value does not depend on the `lc_monetary`
locale. The manifest keeps the base type of each domain, so import also reads
a domain over `money` or `money[]` as plain numbers. Time stamps, `bytea`,
`uuid`, and arrays keep their exact values. An array with bounds other than 1
(for example `[0:2]={1,2,3}`) travels as its array literal, so it keeps its
bounds.

The subject id must be a valid value of the subject column type. For example,
`abc` for a `bigint` column is an input error (`400`). The column type must not
change the value: `ab123-extra` for a `varchar(5)` column, or `1.234` for a
`numeric(6, 2)` column, is also an input error.

## Limits

- Export and import keep the full capsule in memory.
- A record can point at a row of a different subject. Then that row must be
  in the target database, or the import fails with `409`.
- A number in a `json` or `jsonb` column can lose digits after the 17th
  significant digit.
- You cannot import into a different schema.
