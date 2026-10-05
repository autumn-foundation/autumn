# Portable data capsules (issue #1811)

Status: implemented in the first slice. Language: ASD-STE100.

## Goal

Make one command that writes all data of one subject (user, tenant, account)
to a capsule. The capsule is signed. It has an offline viewer. The app can
import it again with no loss of data.

## Brainstorming

- Put the capsule specification on `GdprRegistry`. Apps install that registry
  already, so export and import use the same data.
- Use a directory as the capsule. A browser can open `viewer/index.html`
  directly from the disk.
- Write `records/<table>.json`, `blobs/<sha256>`, `manifest.json`,
  `signature.json` and `viewer/`.
- Put the SHA-256 of each file in the manifest. Sign the manifest with
  HMAC-SHA256 under `[security.signing_secret]`.
- Use a `CapsuleStore` trait. A memory store is for tests. A Postgres store is
  for production.
- Make the viewer pages with the `static_gen` layout (`url_to_file_path`,
  `StaticManifest`). `StaticFileLayer` can then also serve the viewer.
- Give three surfaces: a Rust API, actuator endpoints, and
  `autumn data capsule export|import|verify`.
- Later: Ed25519 signatures, one-file `.tar.gz` packing, automatic foreign-key
  discovery.

## Reverse brainstorming (how can this fail?)

| Failure | Prevention |
| --- | --- |
| A changed record file is not found | The manifest has the SHA-256 of each file. Verify checks all hashes. |
| A changed manifest is not found | HMAC over the manifest bytes, constant-time compare. |
| An added file (for example, a script in the viewer) | Verify rejects files that are not in the manifest. |
| A removed file | Verify rejects missing files. |
| A capsule from a different key | Verify fails. Import reads only verified capsules. |
| No secret is set, so the key is random | Export and verify stop with an error. |
| Path traversal in table names, file names, or blob keys | Names must be plain identifiers. Blob files have hex names. |
| Script injection in the viewer | All values are HTML-escaped. The viewer has no script. A CSP meta tag blocks remote loads. |
| Large numbers lose precision | Postgres exports `numeric`, `real`, `double precision` and `money` as text. |
| Child rows are imported before parent rows | Import sorts models by `belongs_to`. A cycle is an error. |
| Sequences stay low after import | Import moves each serial or identity sequence past the largest key. |
| Generated columns fail on insert | Import skips generated columns. |
| A partial import | Postgres import uses one transaction. |
| An unknown format version | Import rejects it. |

## Six thinking hats

- **White (facts):** `gdpr.rs` has only types. `hmac`, `sha2`, `hex` are
  always-on dependencies. No zip crate is in the workspace. `capsule` is
  already the name of the replay capsules.
- **Red (feelings):** Users want one folder. They want to open it with a
  double-click.
- **Black (risks):** Data precision, the signing key, script injection, path
  traversal, foreign-key order, sequences, and the name collision.
- **Yellow (benefits):** It uses parts that exist. Portability becomes a
  feature. The tests prove fidelity.
- **Green (ideas):** The viewer uses the `static_gen` layout. A later slice
  can add public-key signatures and one-file packing.
- **Blue (process):** Red, green, refactor for each step: registry, manifest,
  integrity, round trip, viewer, blobs, Postgres, actuator, CLI, docs.

## Names

- Module: `autumn_web::gdpr::portability`. Type: `DataCapsule`.
- CLI: `autumn data capsule export|import|verify`.
- One-shot environment: `AUTUMN_DATA_CAPSULE`, `AUTUMN_DATA_CAPSULE_SUBJECT`,
  `AUTUMN_DATA_CAPSULE_PATH`.
- Actuator: `POST /actuator/capsules/{export,import,verify}` (sensitive only).

## Out of scope

Schema transformation, continuous sync, import into other products, and
customer-managed keys.
