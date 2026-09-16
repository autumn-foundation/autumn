# Confidential Fields (operator-blind data)

[Attribute encryption](attribute-encryption.md) protects a column at rest under
keys **the operator holds**. That stops a stolen disk. It does not stop a rogue
admin, a subpoena, or a leaked backup, because the server that serves the data
can also read it.

A `#[confidential]` field removes the operator from the trust boundary. The value
is sealed **on the client**, under a key the server never receives. The server
stores, backs up, replays and returns the envelope, and only the owning client
opens it.

Use it for fields a user should not have to trust the host with: health
records, legal matters, private messages, financial detail.

```rust
use autumn_web::confidential::{BlindIndex, Sealed};

#[autumn_web::model(table = "notes")]
pub struct Note {
    pub id: i32,
    pub owner_id: String,

    // Sealed under the owner's key. The server holds the envelope only.
    #[confidential(blind_index)]
    pub body: Sealed,

    // Companion column: the client's equality token for `body`.
    pub body_bidx: BlindIndex,
}
```

The column type is `Sealed`, not `String`. The server never holds the plaintext,
so the model does not declare a type that suggests it does.

## Sealing a value (client side)

```rust
use autumn_web::confidential::{FieldContext, RootKey};

let key = RootKey::generate();           // held by the client, never sent
// `note_uid` is a client-chosen id the row also carries: it binds the envelope
// to this row. `FieldContext::new` omits it and binds only the column.
let ctx = FieldContext::for_record("notes", "body", &owner_id, &note_uid);

let sealed = key.seal(&ctx, "biopsy scheduled 12 May")?;
let token  = key.blind_index(&ctx, "biopsy scheduled 12 May");

// POST { "body": sealed, "body_bidx": token } — both are opaque strings.
```

Reading it back is the same call in reverse:

```rust
let plaintext = key.unseal(&ctx, &note.body)?;
```

`RootKey` has no `Serialize`, no `Display`, no `Clone` and no accessor for its
bytes, and it zeroizes on drop. Nothing constructs one from configuration or the
credentials store, so a server build cannot acquire one by accident.

### Key custody is yours

This release assumes a single client-held key. Recovery, multi-device sync and
social recovery are out of scope — if the user loses the key, the data is gone.
That is the guarantee working, not a defect.

Key **rotation** is out of scope too. The envelope carries no key id, so a client
with more than one key must try each one. Rotating means re-sealing every value
the old key sealed.

## Equality lookups: the blind index

Sealing is randomized, so two seals of one value never match. `WHERE body = $1`
can therefore never work, and Autumn refuses it **at build time**:

```rust
#[autumn_web::repository(Note)]
trait NoteRepo {
    async fn find_by_body(&self, body: &str) -> Vec<Note>;   // compile error
    async fn find_by_body_bidx(&self, body_bidx: &BlindIndex) -> Vec<Note>; // works
}
```

The client computes the token; the server only compares it:

```text
token = hex(HMAC-SHA256(index_key, "autumn:confidential:bidx:v1:" || plaintext)[0..16])
```

`index_key` is derived from the client's root key and the field context, so:

- the token is **deterministic** for one key, one column and one owner, which is
  what makes the lookup work;
- the token is a fixed 32 hex characters whatever the plaintext, so it leaks no
  length;
- an operator cannot recompute it, so guessing the plaintext does not confirm the
  guess — **unless** the application lets the operator make a client index a
  value of the operator's choosing, which publishes the token for that exact
  plaintext and turns the column into a confirmation oracle;
- the token is per column and per owner, so it cannot be correlated across
  columns or across users.

The token reveals one thing: two rows of one owner that hold the same value get
the same token. See "Can see".

## What the operator can and cannot see

This is the threat model. The adversary is **the operator of the server**: anyone
who can read the database, the logs, a backup artifact, a replay capsule or the
admin UI. An adversary who can also read and write the application's memory is
out of scope for this release.

### Cannot see

| Sink | Why |
| --- | --- |
| `database` | The column type is `Sealed`, so the only value bound into an INSERT or UPDATE is the envelope. |
| `access_log` | The access log carries no bodies, and confidential column names are folded into the log parameter filter. |
| `db_backup` | A backup is a dump of the database, which holds only envelopes. |
| `replay_capsule` | A capsule copies the request body and the SQL binds, both of which carry envelopes. |
| `version_history` | Confidential columns are version-sensitive, so a revision records that the column changed, not what it changed to. |
| `admin_ui` | The admin cell renderer redacts registered confidential columns, and never offers an editable control for one. |
| `admin_csv_export` | The CSV export drops confidential columns and their blind-index companions, so a downloaded file carries neither. |

`autumn_web::confidential::OPERATOR_BLIND_SINKS` holds this table, and the
`confidential_threat_model` test asserts that the code and this page agree.

### Can see

- That the row exists, and its id, timestamps and foreign keys.
- The approximate length of the plaintext, from the length of the envelope.
  AES-GCM is not length-hiding. Pad the value client side if the length matters.
- Whether one value equals another, for one owner and one column, from the
  blind-index token: the token is stable, so two rows of one owner that hold the
  same value match. An application that lets the operator make a client index a
  chosen value gets a confirmation oracle for a guessed plaintext.
- With a `FieldContext::new` context, which values sit in which of that owner's
  rows: the operator can move, copy or roll back an envelope among those rows and
  the client cannot tell. `FieldContext::for_record` closes this.
- Every column the application did not mark `#[confidential]`.

### Outside the guarantee

- **Operator-blind compute.** Range queries, sorting, full-text search,
  aggregation and server-side rendering over sealed data are not possible here.
  Equality through the blind index is the whole query surface.
- **A hostile server build.** The guarantee is that plaintext never *reaches* the
  server, not that a modified server cannot ask the client for it. An operator
  who ships malicious client code defeats any end-to-end scheme.
- **Hand-written SQL.** The build-time refusal covers the generated repository
  surface. A raw query over the sealed column compiles — and matches nothing,
  because the stored value is ciphertext.
- **Memory on the client.** `RootKey` zeroizes on drop and the AES key schedule
  is wiped with it, but a compiler or an allocator can still leave copies. The
  wipe is best effort, not a guarantee against a memory dump of a live client.

## What the build refuses

`#[confidential]` is rejected at compile time wherever the operator would have to
read, compare, order or join the value:

| Combination | Why |
| --- | --- |
| a non-`Sealed` field type | The server never holds the plaintext, so the column is declared as the envelope it stores. |
| `#[encrypted]` | Seals the column under a key the operator holds — the boundary this removes. |
| `#[classified]` | A classification gates where a plaintext may go; there is no server-side plaintext to gate. |
| `#[searchable]` | A search index over ciphertext matches nothing. |
| `#[unique]`, `#[indexed]` | Randomized ciphertext never collides and never serves a lookup. Constrain the `_bidx` column instead. |
| `#[normalize]` | A normalizer rewrites the column in place, which needs plaintext. |
| `#[references]` | A foreign key is a server-side join. |
| `#[id]`, `#[lock_version]`, `#[position]`, `#[state_machine]`, `tenant_id` | Framework-managed columns the server must be able to compare. |
| `#[default]` | A default is a server-side value, and the server cannot seal one. |
| `#[translatable]` | A per-locale container is a JSON document, not one sealed value. |
| `#[serde(rename)]`, `#[serde(rename_all)]`, `#[diesel(column_name)]` | The column is registered under its Rust name, which the query guard, the log filter, version history and admin redaction all key off. |
| the model's shard key | The router reads the column to pick a shard, and a sealed value is opaque to it. |
| `find_by_<field>`, `find_or_create_by_<field>`, a grouped aggregate over it | Each builds a WHERE or a GROUP BY over the column, and the stored ciphertext never repeats. |
| `cursor_key = <field>` | Keyset pagination orders by the column and compares it. Over randomized ciphertext the order is arbitrary. |

## Envelope format

A sealed value is base64 over:

```text
byte  0       magic   = 0xCF        (Autumn confidential field)
byte  1       version = 0x01
byte  2       alg     = 0x01        (AES-256-GCM)
bytes 3..15   nonce   : 12 bytes
bytes 15..    ciphertext + 16-byte AES-GCM authentication tag
```

There is no key id: the key is the client's, and the server has no key ring to
select from.

Keys are derived per field:

```text
scope      = len(table) || table || len(column) || column || len(owner) || owner
seal_key   = HMAC-SHA256(root, "autumn:confidential:seal:v1:"  || scope)
index_key  = HMAC-SHA256(root, "autumn:confidential:index:v1:" || scope)
aad        = magic || version || alg || scope || [len(record) || record]
```

The scope, the envelope header and the record identifier are the AES-GCM
associated data. An envelope copied into another column, table or user therefore
fails to authenticate, so an operator who can write the database cannot move one
user's value into another user's record and have it open. The header is inside
the associated data too, so a v1 envelope cannot be re-labelled as a later
version to steer the parser.

Each part is length-prefixed, so the encoding is injective whatever characters
the parts hold.

Pass a record identifier to bind the row as well. Without one, an operator can
still move an envelope among that owner's own rows in the same column.

## How this differs from `#[encrypted]`

|  | `#[encrypted]` | `#[confidential]` |
| --- | --- | --- |
| Who holds the key | The operator, in the credentials store | The client, never sent to the server |
| Server sees plaintext | Yes, in memory and in Rust code | No |
| Field type | `String` | `Sealed` |
| Equality lookup | `#[encrypted(deterministic)]`, server side | Client-computed blind index |
| Protects against | A stolen disk or database dump | The operator, plus everything above |
| Boot requirement | A configured key ring | None — there is no server key |

They compose by column: encrypt the fields the application's own logic needs, and
seal the fields it does not.

## See also

- [Attribute encryption](attribute-encryption.md) — at-rest columns under
  operator-held keys.
- [Data classification](data-classification.md) — `#[classified]`, which gates
  where a plaintext may travel.
- [Data scrubbing](data-scrubbing.md) — the log parameter filter confidential
  column names are folded into.
