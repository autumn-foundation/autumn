# Attribute Encryption (at-rest column encryption)

Autumn can encrypt individual `#[model]` columns at rest. A field marked
`#[encrypted]` is stored as opaque ciphertext in the database but behaves like a
normal `String` in your Rust code: `repo.find(id)` returns plaintext, and
`repo.update(..)` / inserts accept plaintext. No per-call-site changes, no
hand-rolled AEAD wrappers.

This is the right tool for **sensitive data columns**: OAuth refresh tokens,
government IDs, MFA seeds, PHI fields, payment metadata, end-user content keys.

```rust
#[autumn_web::model]
pub struct Account {
    #[id]
    pub id: i64,
    pub username: String,

    // Randomized AEAD: a fresh nonce per write. The default. No equality lookups.
    #[encrypted]
    pub api_token: String,

    // Deterministic AEAD: stable ciphertext, so `WHERE email = ?` works — at the
    // cost of leaking *equality* of plaintext through equality of ciphertext.
    // Requires the explicit `deterministic` opt-in.
    #[encrypted(deterministic)]
    pub email: String,
}
```

## Declaring an encrypted column from `autumn generate`

You do not have to hand-write the attribute. The scaffold/model field DSL takes
an `{encrypted}` modifier, so a new resource ships encrypted from the first
command:

```bash
autumn generate scaffold Account username:String \
  'api_token:String{encrypted}' 'email:String{encrypted:deterministic}'
```

That emits the `#[encrypted]` / `#[encrypted(deterministic)]` attributes shown
above, an unbounded `TEXT` column sized for the ciphertext envelope, and an
admin that redacts the column — no hand-editing. `{encrypted}` is `String`/`Text`
and non-null only (matching the v1 attribute), and the generator refuses to pair
randomized encryption with `:unique`, `--query`, `--searchable`, or `--default`,
so you find out at generate time rather than at runtime. See the
[generators guide](./generators.md#encrypted-columns-with-encrypted).

To encrypt a column that already exists, use the backfill migration instead —
`autumn generate migration EncryptApiTokenOnAccounts` — and read
[Backfilling an existing plaintext column](#backfilling-an-existing-plaintext-column)
first: the backfill must run **before** the attribute is added.

## When to use this vs. the credentials store vs. log scrubbing

| Concern | Use |
| --- | --- |
| Secrets needed to **boot** the app (DB URL, API keys, signing secret) | [Credentials store](./credentials.md) (#682) |
| Keeping PII out of **logs and traces** | [Log scrubbing](./logging-pii.md) (#697) |
| Sensitive **data columns** stored per-row in your tables | **Attribute encryption (this page)** |

Attribute encryption *composes with* the other two: encryption key material lives
in the credentials store, and encrypted columns are automatically scrubbed from
logs.

## Configuring keys

Key material lives in the encrypted [credentials store](./credentials.md) under
the `active_record_encryption` namespace (the name mirrors Rails so the mental
model transfers). Run:

```bash
autumn credentials edit
```

and add:

```toml
[active_record_encryption]
# Current key — used for all new writes. 64 hex chars.
primary_key = "‹openssl rand -hex 32›"

# Required only if any column uses #[encrypted(deterministic)].
deterministic_key = "‹openssl rand -hex 32›"

# Mixed into key derivation. Pick once and keep it stable.
key_derivation_salt = "‹openssl rand -hex 16›"

# Optional: keys retired by rotation, kept so old rows stay readable.
retired_keys = []
```

If a model declares an encrypted column but `primary_key` is missing, the app
**fails fast at boot in production** (and logs a warning in dev so local examples
still start) with a diagnostic that names the exact missing credential path (the
same shape as #597):

```text
Attribute encryption misconfiguration: Encrypted column `accounts.api_token`
requires a master key, but `active_record_encryption.primary_key` is not
configured.
  hint: run `autumn credentials edit` and add:
    [active_record_encryption]
    primary_key = "<64 hex chars from `openssl rand -hex 32`>"
```

## Randomized vs. deterministic — the tradeoff

* **Randomized** (`#[encrypted]`, the default): every write gets a fresh random
  nonce, so encrypting the same plaintext twice produces different ciphertext.
  This is the safe choice. You **cannot** run `WHERE col = ?` equality queries on
  a randomized column, because the value you search for won't match the stored
  ciphertext.

* **Deterministic** (`#[encrypted(deterministic)]`): the nonce is derived from
  the plaintext, so equal plaintexts produce equal ciphertext and equality
  lookups work. The cost: an observer of the database can tell which rows share
  the same value, even without the key. **Only opt in when you need lookups, and
  never on low-entropy columns** (e.g. a boolean-ish flag), where equality
  leakage is most damaging.

Randomized is default and deterministic is an explicit opt-in *by design*: the
cost of accidentally shipping deterministic encryption on a sensitive low-entropy
column is high; the cost of typing one extra word when you actually need lookup
is low.

### Equality lookups on deterministic columns

Encrypt the search value to its stable ciphertext and filter on it:

```rust
use autumn_web::encryption::deterministic_ciphertext;

let needle = deterministic_ciphertext("alice@example.com")?;
let account: Account = accounts::table
    .filter(accounts::email.eq(needle))
    .select(Account::as_select())
    .first(&mut conn)
    .await?;
```

## On-disk format

Each encrypted value is a base64 string wrapping a self-describing binary
envelope:

```text
byte  0      magic   = 0xA7
byte  1      version = 0x01
byte  2      alg     = 0x01   (AES-256-GCM)
byte  3      mode    = 0x00 randomized | 0x01 deterministic
bytes 4..8   key_id  : u32 big-endian
bytes 8..20  nonce   : 12 bytes
bytes 20..   ciphertext + 16-byte AES-GCM tag
```

The single AEAD primitive is **AES-256-GCM** (via the vetted `aes-gcm` crate).
There is no app-author algorithm choice in v1.

Because the envelope embeds the `key_id`, an external decryption tool — given the
master key material and the documented key derivation — can decode any column
value. Key derivation:

```text
data_key = HMAC-SHA256(master_bytes, b"autumn:data:v1:" || salt)   # randomized
det_key  = HMAC-SHA256(master_bytes, b"autumn:det:v1:"  || salt)   # deterministic

# key_id identifies the *derived key that encrypted this value*, so it depends on
# the envelope's mode byte: data_key for a randomized envelope (mode = 0x00),
# det_key for a deterministic one (mode = 0x01).
derived_key = data_key  if mode == 0x00 (randomized)
            = det_key   if mode == 0x01 (deterministic)
key_id      = u32::from_be_bytes( SHA256(b"autumn:id:v1:" || derived_key)[0..4] )
```

To decrypt a deterministic envelope, derive `det_key` and compute its `key_id`
from `det_key` (not `data_key`); otherwise the id will not match the envelope and
the right key cannot be selected.

## Key rotation

Rotation never rewrites existing rows. To rotate:

1. Generate a new key: `openssl rand -hex 32`.
2. `autumn credentials edit`: move the **old** `primary_key` into `retired_keys`
   and set the new key as `primary_key`:

   ```toml
   [active_record_encryption]
   primary_key  = "‹new key›"
   retired_keys = ["‹previous primary_key›"]
   ```

3. Deploy. New writes use the new key; existing rows still decrypt because the
   envelope's `key_id` selects the retired key transparently.

An encrypted column is rewritten with the current key **only when that column is
itself part of a write** — i.e. a save that sets it, or an `UpdateX` whose patch
includes that field. Ordinary edit traffic that changes *other* fields leaves the
encrypted column out of the generated changeset, so its ciphertext keeps
referencing the retired key. **Do not assume normal updates migrate old rows.** To
migrate eagerly, run a one-off task that loads each row and re-saves the encrypted
column(s) explicitly (read the plaintext, write it back so the wrapper re-encrypts
under the current key). A retired key can be dropped from `retired_keys` only once
no row's encrypted columns reference its `key_id` anymore — verify with a scan, not
by assuming update activity has covered every row.

The **deterministic key** rotates the same way: set the new `deterministic_key`
and move the previous one into `retired_keys`. Each retired key is derived in
both the randomized and deterministic domains, so existing deterministic rows
keep decrypting by `key_id`. Note that equality lookups only match rows written
under the *current* deterministic key, so re-encrypt deterministic columns
promptly after rotating their key if you rely on lookups.

## Backfilling an existing plaintext column

Converting an existing plaintext column to encrypted is an **offline backfill**.
Generate the documented migration scaffold (the name shape is
`Encrypt<Column>On<Table>`):

```bash
autumn generate migration EncryptApiTokenOnAccounts
```

This emits a migration whose `up.sql` documents the key configuration and the
encrypt backfill, and whose `down.sql` documents the reverse (restoring plaintext
from ciphertext given the keys). An unbounded `TEXT` column needs no type change.
A **bounded `VARCHAR(n)` column must be widened first** (e.g. `ALTER TABLE … ALTER
COLUMN … TYPE TEXT`): the envelope adds a 20-byte header and a 16-byte GCM tag and
is then base64-encoded (~1.37×), so the ciphertext can exceed the original limit
(a `VARCHAR(255)` value easily grows past 255 chars) and the backfill or later
writes would fail with a length violation. The generated scaffold emits the
widening `ALTER` statements for you.

Order matters. **Backfill before adding `#[encrypted]` to the model field.**
Once the attribute is present the column's reader decrypts on load, so any
still-plaintext row would fail with a malformed-envelope error. Run a one-off
task over a *temporary* plaintext model (one without `#[encrypted]`) that reads
each row's plaintext and writes the envelope produced by
`autumn_web::encryption::encrypt_text(<mode>, &plaintext)`.

**Use the mode the column will be deployed with.** For a column you will mark
`#[encrypted(deterministic)]`, backfill with `Mode::Deterministic` — otherwise the
pre-existing rows are written as randomized envelopes, and `find_by_*` /
`exists_by_*` (which encode the query value as *deterministic* ciphertext and
compare with `=`) will never match them until they are rewritten. Use
`Mode::Randomized` for plain `#[encrypted]` columns. The generated migration
scaffold notes the same rule.

Only after every row is ciphertext do you add `#[encrypted]` and deploy the
encrypted reader. The **rollback** task does the inverse with
`decrypt_text(&envelope)` (again via a temporary plaintext model), then you
remove the attribute.

> Always take a backup before a backfill, and keep the keys: a row encrypted with
> a key you have lost is unrecoverable by design.

## Composition with other subsystems

* **Logs & traces (#697):** encrypted column names are automatically added to the
  log parameter scrubber. Their values never appear in trace/error parameter
  output. The wrapper types also redact in `Debug`.

* **Record version history (#700):** encrypted columns are automatically treated
  as *sensitive* in the version diff. By default the history stores a
  `changed (encrypted)` marker — never the plaintext (which the in-memory model
  would otherwise serialize). Plaintext never enters the version-history table.

  Per-field opt-in: `#[encrypted(versioned_ciphertext)]` stores the before/after
  **ciphertext** instead of the marker (deterministically encrypted so the diff
  stays accurate, and re-encryptable on key rotation). This requires a
  `deterministic_key`; if encryption fails the value falls back to the
  `<encrypted>` marker so plaintext still never leaks.

* **Admin plugin:** encrypted columns render **redacted** (`••••••••`) in admin
  list and detail views by default, and edit forms never pre-fill their plaintext.
  Showing decrypted plaintext is an explicit per-field opt-in,
  `#[encrypted(admin_visible)]`, surfaced only in the admin's read views; the
  admin surface itself is authorization-gated (`AdminPlugin::require_role`,
  composing with #496) — this feature does not invent its own authorization.

### Per-field options summary

```rust
#[encrypted]                          // randomized; redacted everywhere (default)
#[encrypted(deterministic)]           // stable ciphertext; supports equality lookups
#[encrypted(admin_visible)]           // show decrypted plaintext in admin read views
#[encrypted(versioned_ciphertext)]    // store ciphertext (not a marker) in version history
// options combine, e.g. #[encrypted(deterministic, admin_visible)]
```

## Development escape hatch

By default the encryption wrapper types render `<encrypted>` in `Debug`. For
local debugging only you may opt back into plaintext:

```rust
// DEV ONLY — never enable in production.
autumn_web::encryption::set_debug_plaintext(true);
```

## Performance

Encryption adds an AES-256-GCM operation per encrypted value per read/write.
AES-256-GCM is hardware-accelerated on modern CPUs (AES-NI). The framework's
benchmark suite includes a mixed encrypted/plaintext read benchmark — run it
with:

```bash
cargo bench -p autumn-web --bench attribute_encryption
```

It reports p50/p99 per-row read latency for plaintext, mixed (50% encrypted),
and all-encrypted workloads over 10k rows, and enforces a budget of **≤10% p99
read regression** measured against a representative database-read baseline. In
practice the dominant cost of a request is the database round trip and
serialization (hundreds of microseconds), not the per-column AEAD (~a
microsecond), so the budget is met comfortably.

## Limitations (v1)

* Encrypted columns are non-null `String` fields. Encrypt structured data by
  serializing it to a string first.
* The primary key is app-global (no per-tenant key partitioning yet).
* No KMS integration, no per-row data keys, no searchable encryption beyond
  deterministic equality, no format-preserving encryption. See the issue's
  "Out of Scope" for the roadmap.
