//! Operator-blind confidential fields (issue #1771).
//!
//! [`encryption`](crate::encryption) protects columns at rest under keys **the
//! operator holds**: it stops a stolen disk, not a rogue admin, a subpoena or a
//! leaked backup. This module removes the operator from the trust boundary for
//! the fields an application marks `#[confidential]`.
//!
//! A confidential value is sealed **on the client**, under a [`RootKey`] the
//! server never receives. The server's only representation of the value is
//! [`Sealed`] — an opaque envelope with no accessor that yields plaintext, no
//! `Display`, and a redacted `Debug`. Every operator-reachable sink therefore
//! holds ciphertext by construction: the database, the access log, `autumn db
//! backup` output, a replay capsule, record version history and the admin UI. Equality lookups still work, through a client-computed
//! [`BlindIndex`] token.
//!
//! ```ignore
//! #[autumn_web::model(table = "notes")]
//! pub struct Note {
//!     pub id: i32,
//!     pub owner_id: String,
//!     #[confidential(blind_index)]
//!     pub body: Sealed,
//!     pub body_bidx: BlindIndex,
//! }
//!
//! // Client side (never on the server):
//! let ctx = FieldContext::for_record("notes", "body", &owner_id, &note_uid);
//! let sealed = key.seal(&ctx, "my diagnosis")?;
//! let token = key.blind_index(&ctx, "my diagnosis");
//! ```
//!
//! # Envelope format
//!
//! A sealed value is base64 ([`STANDARD`](base64::engine::general_purpose::STANDARD))
//! over this binary envelope:
//!
//! ```text
//! byte  0       magic   = 0xCF        (Autumn confidential field)
//! byte  1       version = 0x01
//! byte  2       alg     = 0x01        (AES-256-GCM)
//! bytes 3..15   nonce   : 12 bytes
//! bytes 15..    ciphertext + 16-byte AES-GCM authentication tag
//! ```
//!
//! There is no key id: the key is the client's, and the server has no key ring
//! to select from.
//!
//! # Key derivation
//!
//! Each field gets its own keys, derived from the root key and the
//! [`FieldContext`] (table, column, owner):
//!
//! ```text
//! scope      = len(table) || table || len(column) || column || len(owner) || owner
//! seal_key   = HMAC-SHA256(root, "autumn:confidential:seal:v1:"  || scope)
//! index_key  = HMAC-SHA256(root, "autumn:confidential:index:v1:" || scope)
//! token      = hex(HMAC-SHA256(index_key, "autumn:confidential:bidx:v1:" || plaintext)[0..16])
//! aad        = magic || version || alg || scope || [len(record) || record]
//! ```
//!
//! The scope, the envelope header and an optional record identifier are the
//! AES-GCM associated data, so an envelope moved to another column, table or
//! owner fails to authenticate. Pass a record identifier
//! ([`FieldContext::for_record`]) to bind the row as well; without one, an
//! operator can still move a value among that owner's own rows.
//!
//! # What the operator can still see
//!
//! Sealing hides the value, not the record. See [`OPERATOR_VISIBLE`] and
//! [`OPERATOR_BLIND_SINKS`], and `docs/guide/confidential-fields.md` for the
//! full threat model.

use std::fmt;

use aes_gcm::Aes256Gcm;
use aes_gcm::KeyInit;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroize as _;

/// Envelope magic byte. Distinct from [`crate::encryption`]'s `0xA7`, so the two
/// formats can never be confused for one another.
const MAGIC: u8 = 0xCF;
const VERSION: u8 = 0x01;
const ALG_AES_256_GCM: u8 = 0x01;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 3 + NONCE_LEN;
/// AES-GCM authentication tag length. An envelope shorter than header + tag
/// cannot hold a valid ciphertext.
const TAG_LEN: usize = 16;
/// Bytes of HMAC output kept in a blind-index token: 128 bits, which makes an
/// accidental collision negligible while keeping the token short.
const TOKEN_BYTES: usize = 16;

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    hmac_sha256_parts(key, msg, &[])
}

/// HMAC over two message parts, without joining them first.
///
/// The blind index MACs a fixed-length constant followed by the plaintext.
/// Concatenating them would put a copy of the plaintext in a heap block that
/// nothing wipes, so the parts are fed to the MAC directly. The prefix is a
/// constant, so the split adds no ambiguity.
fn hmac_sha256_parts(key: &[u8], first: &[u8], second: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(first);
    mac.update(second);
    mac.finalize().into_bytes().into()
}

/// Errors produced when sealing, unsealing or parsing a confidential value.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfidentialError {
    /// The root key was not valid 64-character hex.
    #[error("invalid root key: expected 64 hex characters, got {len}")]
    InvalidKeyFormat {
        /// Length of the supplied string.
        len: usize,
    },

    /// The envelope is not parseable (bad base64, bad magic, truncated).
    #[error("malformed sealed envelope: {0}")]
    MalformedEnvelope(&'static str),

    /// The envelope uses a version or algorithm this build does not know.
    #[error("unsupported sealed envelope (version={version:#04x}, alg={alg:#04x})")]
    UnsupportedEnvelope {
        /// Envelope version byte.
        version: u8,
        /// Algorithm id byte.
        alg: u8,
    },

    /// The AEAD refused the plaintext. AES-GCM caps one message at about
    /// 64 GiB, which is the only way to reach this.
    #[error("seal failed: the value is too large for one AES-GCM message")]
    SealFailed,

    /// AEAD authentication failed: the wrong key, the wrong field context, or
    /// corrupted ciphertext.
    #[error("unseal failed: wrong key, wrong field context, or corrupted ciphertext")]
    UnsealFailed,

    /// The recovered plaintext was not UTF-8.
    #[error("unsealed value is not valid UTF-8")]
    NotUtf8,

    /// A blind-index token was not 32 lowercase hex characters.
    #[error("invalid blind-index token: expected {expected} lowercase hex characters")]
    InvalidToken {
        /// The required token length.
        expected: usize,
    },
}

// ---------------------------------------------------------------------------
// Field context
// ---------------------------------------------------------------------------

/// Names the field a value belongs to: its table, its column, the owner whose
/// key seals it, and optionally the record it sits in.
///
/// The context does two jobs. Its **scope** — table, column and owner — derives
/// the field's keys, so one root key gives every column of every owner an
/// independent key. The scope, plus the optional **record** identifier, is the
/// AES-GCM associated data, so an envelope is bound to where it was written.
///
/// Each part is length-prefixed, so the encoding is injective: no two different
/// triples can produce the same bytes, whatever characters the parts hold.
///
/// # Bind the record where you can
///
/// [`FieldContext::new`] binds the column and the owner, not the row. An
/// operator who can write the database can therefore still move, copy or roll
/// back one owner's envelope **among that owner's own rows in the same
/// column**, and the client cannot tell. [`FieldContext::for_record`] closes
/// that by adding a stable record identifier to the associated data. The record
/// is deliberately outside key derivation, so the blind index stays comparable
/// across rows and the equality lookup keeps working.
///
/// Use a client-chosen identifier (a UUID the client puts in the row) rather
/// than a server-assigned primary key, so the same context is available at
/// insert time and at read time.
#[derive(Clone, PartialEq, Eq)]
pub struct FieldContext {
    /// Length-prefixed `table`, `column`, `owner`. Derives the field keys.
    scope: Vec<u8>,
    /// Length-prefixed record identifier, empty when the record is not bound.
    record: Vec<u8>,
}

/// Append `part` as a 4-byte big-endian length followed by its bytes.
fn push_part(out: &mut Vec<u8>, part: &str) {
    // A part longer than 4 GiB is not a table, column, owner or record id.
    let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(part.as_bytes());
}

impl FieldContext {
    /// Bind `owner`'s value in `table`.`column`, without binding the record.
    ///
    /// See "Bind the record where you can" above for what this leaves open.
    #[must_use]
    pub fn new(table: &str, column: &str, owner: &str) -> Self {
        let mut scope = Vec::with_capacity(table.len() + column.len() + owner.len() + 12);
        push_part(&mut scope, table);
        push_part(&mut scope, column);
        push_part(&mut scope, owner);
        Self {
            scope,
            record: Vec::new(),
        }
    }

    /// Bind the record as well, so an envelope moved to another row of the same
    /// column no longer unseals.
    ///
    /// `record` is any stable identifier for the row the client can reproduce on
    /// read.
    #[must_use]
    pub fn for_record(table: &str, column: &str, owner: &str, record: &str) -> Self {
        let mut ctx = Self::new(table, column, owner);
        push_part(&mut ctx.record, record);
        ctx
    }

    /// The key-derivation input: scope only, so the blind index stays comparable
    /// across the rows of one owner.
    fn scope_bytes(&self) -> &[u8] {
        &self.scope
    }

    /// The AES-GCM associated data: the envelope header, the scope and the
    /// record.
    ///
    /// The header is in here rather than only on the wire, so the version and
    /// algorithm bytes an unsealer reads its parser from are authenticated. A
    /// future v2 envelope therefore cannot be re-labelled as a v1 one.
    fn aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(3 + self.scope.len() + self.record.len());
        aad.push(MAGIC);
        aad.push(VERSION);
        aad.push(ALG_AES_256_GCM);
        aad.extend_from_slice(&self.scope);
        aad.extend_from_slice(&self.record);
        aad
    }
}

/// Redacted: the owner identifier is a stable per-user value, and `Debug` output
/// reaches logs and error pages.
impl fmt::Debug for FieldContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FieldContext(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// Root key
// ---------------------------------------------------------------------------

/// A client-held 32-byte root key. **The server must never hold one.**
///
/// The type has no `Serialize`, no `Display`, no `Clone` and no accessor for its
/// bytes, so no expression writes it to a log line, a response or a file. It
/// zeroizes on drop. The only inputs are [`RootKey::generate`],
/// [`RootKey::from_bytes`] and [`RootKey::from_hex`]; nothing reads it out of
/// configuration or the credentials store, which is what keeps a server build
/// from acquiring one by accident.
pub struct RootKey {
    bytes: [u8; 32],
}

impl RootKey {
    /// Draw a fresh root key from the operating system RNG.
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random number generator is unavailable.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("OS RNG failed");
        Self { bytes }
    }

    /// Adopt 32 bytes of existing key material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { bytes }
    }

    /// Parse a 64-character hex key.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::InvalidKeyFormat`] for anything else.
    pub fn from_hex(hex_str: &str) -> Result<Self, ConfidentialError> {
        let hex_str = hex_str.trim();
        // Decoded straight into the array: `hex::decode` would leave a second
        // copy of the key in a heap block that nothing wipes.
        let mut bytes = [0u8; 32];
        if hex_str.len() != 64 || hex::decode_to_slice(hex_str, &mut bytes).is_err() {
            bytes.zeroize();
            return Err(ConfidentialError::InvalidKeyFormat { len: hex_str.len() });
        }
        Ok(Self { bytes })
    }

    fn derive(&self, domain: &[u8], ctx: &FieldContext) -> [u8; 32] {
        let mut msg = Vec::with_capacity(domain.len() + ctx.scope_bytes().len());
        msg.extend_from_slice(domain);
        msg.extend_from_slice(ctx.scope_bytes());
        hmac_sha256(&self.bytes, &msg)
    }

    /// Seal `plaintext` for the field `ctx` names.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::UnsealFailed`] only if the AEAD refuses the
    /// input, which cannot happen for a valid key and nonce.
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random number generator is unavailable.
    pub fn seal(&self, ctx: &FieldContext, plaintext: &str) -> Result<Sealed, ConfidentialError> {
        use aes_gcm::Nonce;
        use aes_gcm::aead::{Aead, Payload};
        use base64::Engine as _;

        let mut seal_key = self.derive(b"autumn:confidential:seal:v1:", ctx);
        // Infallible: the key is always 32 bytes, which is what `Key` names.
        let cipher = Aes256Gcm::new(&seal_key.into());
        seal_key.zeroize();

        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).expect("OS RNG failed");

        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &ctx.aad(),
                },
            )
            .map_err(|_| ConfidentialError::SealFailed)?;

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.push(MAGIC);
        out.push(VERSION);
        out.push(ALG_AES_256_GCM);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(Sealed(
            base64::engine::general_purpose::STANDARD.encode(out),
        ))
    }

    /// Recover the plaintext of `sealed` for the field `ctx` names.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::UnsealFailed`] for the wrong key or a
    /// different field context, and [`ConfidentialError::NotUtf8`] if the
    /// recovered bytes are not text.
    pub fn unseal(&self, ctx: &FieldContext, sealed: &Sealed) -> Result<String, ConfidentialError> {
        use aes_gcm::Nonce;
        use aes_gcm::aead::{Aead, Payload};

        let raw = sealed.to_bytes()?;
        // `to_bytes` already validated the header, so these slices are in range.
        let nonce = &raw[3..HEADER_LEN];
        let ciphertext = &raw[HEADER_LEN..];

        let mut seal_key = self.derive(b"autumn:confidential:seal:v1:", ctx);
        // Infallible: the key is always 32 bytes, which is what `Key` names.
        let cipher = Aes256Gcm::new(&seal_key.into());
        seal_key.zeroize();

        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &ctx.aad(),
                },
            )
            .map_err(|_| ConfidentialError::UnsealFailed)?;
        String::from_utf8(plaintext).map_err(|e| {
            // The error owns the recovered bytes; `Zeroizing` wipes them when it
            // drops at the end of this closure.
            let _wiped = zeroize::Zeroizing::new(e.into_bytes());
            ConfidentialError::NotUtf8
        })
    }

    /// Compute the deterministic equality token for `plaintext` in the field
    /// `ctx` names.
    ///
    /// Equal plaintexts give equal tokens under one key and context, which is
    /// what makes `WHERE <column>_bidx = $1` work. The token gives nothing else
    /// away: it is a keyed MAC of fixed length, so it reveals neither the
    /// plaintext nor its length, and an operator without the key cannot confirm
    /// a guessed plaintext by recomputing it.
    #[must_use]
    pub fn blind_index(&self, ctx: &FieldContext, plaintext: &str) -> BlindIndex {
        let mut index_key = self.derive(b"autumn:confidential:index:v1:", ctx);
        let tag = hmac_sha256_parts(
            &index_key,
            b"autumn:confidential:bidx:v1:",
            plaintext.as_bytes(),
        );
        index_key.zeroize();
        BlindIndex(hex::encode(&tag[..TOKEN_BYTES]))
    }
}

impl Drop for RootKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RootKey(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// Sealed
// ---------------------------------------------------------------------------

/// The server-side representation of a confidential value: an opaque envelope.
///
/// This is the type a `#[confidential]` column is declared as, so every struct
/// the model macro generates — insert, patch, changeset, factory, JSON view —
/// carries ciphertext and nothing else. The type has no `Display`, no `Deref`
/// and no accessor that returns plaintext. [`Sealed::as_envelope`] returns the
/// ciphertext, which is what the client needs and what the operator may already
/// read from the database.
#[derive(Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "db", derive(diesel::AsExpression, diesel::FromSqlRow))]
#[cfg_attr(feature = "db", diesel(sql_type = diesel::sql_types::Text))]
pub struct Sealed(String);

impl Sealed {
    /// Adopt an envelope string that arrived from a client or the database.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::MalformedEnvelope`] or
    /// [`ConfidentialError::UnsupportedEnvelope`] when the header does not
    /// parse, so junk is refused at the boundary rather than stored.
    pub fn from_envelope(mut envelope: String) -> Result<Self, ConfidentialError> {
        // Trimmed once, here, and in place, so two `Sealed` values are equal
        // exactly when they decode to the same envelope. An operator cannot make
        // one row look different from another by adding a space.
        envelope.truncate(envelope.trim_end().len());
        let leading = envelope.len() - envelope.trim_start().len();
        envelope.drain(..leading);
        let candidate = Self(envelope);
        candidate.to_bytes()?;
        Ok(candidate)
    }

    /// The base64 envelope, exactly as stored.
    #[must_use]
    pub fn as_envelope(&self) -> &str {
        &self.0
    }

    /// Decode and validate the envelope, returning its raw bytes.
    ///
    /// # Errors
    ///
    /// As [`Sealed::from_envelope`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, ConfidentialError> {
        use base64::Engine as _;

        let raw = base64::engine::general_purpose::STANDARD
            .decode(&self.0)
            .map_err(|_| ConfidentialError::MalformedEnvelope("not valid base64"))?;
        if raw.len() < HEADER_LEN + TAG_LEN {
            return Err(ConfidentialError::MalformedEnvelope("truncated envelope"));
        }
        if raw[0] != MAGIC {
            return Err(ConfidentialError::MalformedEnvelope("bad magic byte"));
        }
        if raw[1] != VERSION || raw[2] != ALG_AES_256_GCM {
            return Err(ConfidentialError::UnsupportedEnvelope {
                version: raw[1],
                alg: raw[2],
            });
        }
        Ok(raw)
    }
}

/// An envelope no key opens.
///
/// The server cannot seal a value, so a confidential column has no meaningful
/// default. This exists because the `#[model]` factory and patch structs need
/// one. It is structurally valid, so it round-trips through the database, and
/// [`RootKey::unseal`] always refuses it, which is correct for a value nobody
/// sealed.
impl Default for Sealed {
    fn default() -> Self {
        use base64::Engine as _;
        let mut raw = Vec::with_capacity(HEADER_LEN + TAG_LEN);
        raw.push(MAGIC);
        raw.push(VERSION);
        raw.push(ALG_AES_256_GCM);
        raw.resize(HEADER_LEN + TAG_LEN, 0);
        Self(base64::engine::general_purpose::STANDARD.encode(raw))
    }
}

/// Redacted. The envelope is ciphertext, but `Debug` output reaches logs, panic
/// messages and error pages, where a per-user envelope is still a correlatable
/// identifier.
impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sealed(<sealed>)")
    }
}

impl Serialize for Sealed {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sealed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let envelope = String::deserialize(deserializer)?;
        Self::from_envelope(envelope).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Blind index
// ---------------------------------------------------------------------------

/// A deterministic equality token for a confidential value.
///
/// The client computes it with [`RootKey::blind_index`] and sends it alongside
/// the sealed value. The server stores it in its own column and compares it,
/// which is the only server-side predicate a confidential field supports.
#[derive(Clone, Eq)]
#[cfg_attr(feature = "db", derive(diesel::AsExpression, diesel::FromSqlRow))]
#[cfg_attr(feature = "db", diesel(sql_type = diesel::sql_types::Text))]
pub struct BlindIndex(String);

/// Hand-written to stay consistent with the constant-time [`PartialEq`] below:
/// equal tokens hash equally, which a derived `Hash` could not be shown to do
/// once `PartialEq` stopped being derived.
impl std::hash::Hash for BlindIndex {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// Redacted, for the same reason the token is filtered out of logs and the CSV
/// export: it is stable per value per owner, so anyone who reads it can tell
/// which of an owner's rows hold the same value. `Debug` output reaches logs,
/// panic messages and error pages, which would walk straight past those filters.
impl fmt::Debug for BlindIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BlindIndex(<token>)")
    }
}

/// Constant time, so an application that compares a submitted token against a
/// stored one does not turn the comparison into a timing oracle. The database
/// comparison is not constant time either, but this is the one an application
/// writes by hand.
impl PartialEq for BlindIndex {
    fn eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        // Both are validated to `TOKEN_LEN` hex characters, so the lengths match
        // whenever the values could.
        self.0.len() == other.0.len() && bool::from(self.0.as_bytes().ct_eq(other.0.as_bytes()))
    }
}

impl BlindIndex {
    /// Character length of a token: 16 bytes of HMAC, hex encoded.
    pub const TOKEN_LEN: usize = TOKEN_BYTES * 2;

    /// Adopt a token string that arrived from a client or the database.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::InvalidToken`] unless the value is exactly
    /// [`BlindIndex::TOKEN_LEN`] lowercase hex characters.
    pub fn from_token(token: String) -> Result<Self, ConfidentialError> {
        let valid = token.len() == Self::TOKEN_LEN
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(token))
        } else {
            Err(ConfidentialError::InvalidToken {
                expected: Self::TOKEN_LEN,
            })
        }
    }

    /// The token, as stored and compared.
    #[must_use]
    pub fn as_token(&self) -> &str {
        &self.0
    }
}

/// A random token, which no plaintext produces.
///
/// Present for the same reason as [`Sealed`]'s: the `#[model]` factory and patch
/// structs need a default. It is drawn fresh rather than fixed: a shared
/// constant would be a token every defaulted row holds, so one lookup would
/// match rows across owners.
///
/// # Panics
///
/// Panics if the operating system's random number generator is unavailable.
impl Default for BlindIndex {
    fn default() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::getrandom(&mut bytes).expect("OS RNG failed");
        Self(hex::encode(bytes))
    }
}

impl Serialize for BlindIndex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BlindIndex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        Self::from_token(token).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Compile-time registration of a confidential column, emitted by `#[model]`.
///
/// Drives log-parameter scrubbing, version-history redaction and admin
/// redaction for surfaces that have no compile-time view of the model.
#[derive(Debug)]
pub struct ConfidentialColumnDescriptor {
    /// Model type name (e.g. `Note`).
    pub model: &'static str,
    /// Database table name.
    pub table: &'static str,
    /// Column holding the sealed envelope.
    pub column: &'static str,
    /// Companion column holding the blind-index token, when the field declared
    /// `#[confidential(blind_index)]`.
    pub blind_index: Option<&'static str>,
}

inventory::collect!(ConfidentialColumnDescriptor);

/// Every confidential column registered across the binary.
#[must_use]
pub fn registered_confidential_columns() -> Vec<&'static ConfidentialColumnDescriptor> {
    inventory::iter::<ConfidentialColumnDescriptor>
        .into_iter()
        .collect()
}

/// Distinct column names of every confidential column, blind-index companions
/// included.
///
/// Fed into the log parameter scrubber. The sealed column is ciphertext and the
/// token is already in the database, so neither is a plaintext leak; both are
/// per-user values that would let anyone reading a log correlate requests, so
/// both are filtered.
#[must_use]
pub fn registered_confidential_column_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for d in registered_confidential_columns() {
        names.push(d.column.to_owned());
        if let Some(bidx) = d.blind_index {
            names.push(bidx.to_owned());
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Whether `column` of `table` is a registered confidential column.
#[must_use]
pub fn is_confidential_column(table: &str, column: &str) -> bool {
    registered_confidential_columns()
        .iter()
        .any(|d| d.table == table && d.column == column)
}

/// Whether any registered confidential column, or its blind-index companion,
/// has this name (table-agnostic).
///
/// Used by surfaces that lack table context, such as the admin cell renderer and
/// the CSV export. The token is included: it is stable per value per owner, so
/// publishing it outside the database hands out a correlation handle. Errs
/// toward privacy: a same-named column on another table is also redacted.
#[must_use]
pub fn is_confidential_column_name(column: &str) -> bool {
    registered_confidential_columns()
        .iter()
        .any(|d| d.column == column || d.blind_index == Some(column))
}

/// Confidential column names for one table.
#[must_use]
pub fn confidential_columns_for_table(table: &str) -> Vec<&'static str> {
    registered_confidential_columns()
        .iter()
        .filter(|d| d.table == table)
        .map(|d| d.column)
        .collect()
}

/// Append this table's confidential columns to `columns`, de-duplicating.
///
/// The blind-index companion is included: a history of tokens is a history of
/// which values repeated, which outlives the row that held them.
///
/// Used by generated `VersionedRecord::version_sensitive_columns`, so record
/// version history keeps a "changed" marker instead of copying the envelope
/// into a second table.
pub fn merge_confidential_columns_for_table(table: &str, columns: &mut Vec<&'static str>) {
    for d in registered_confidential_columns() {
        if d.table != table {
            continue;
        }
        for column in [Some(d.column), d.blind_index].into_iter().flatten() {
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
    }
}

/// Whether `column` appears in `columns`.
///
/// A `const fn` so the `#[repository]` macro can refuse a server-side predicate
/// over a confidential column at build time, from the column list `#[model]`
/// publishes. Not part of the public API.
#[doc(hidden)]
#[must_use]
pub const fn __column_is_confidential(columns: &[&str], column: &str) -> bool {
    let mut i = 0;
    while i < columns.len() {
        if const_str_eq(columns[i], column) {
            return true;
        }
        i += 1;
    }
    false
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// Threat model
// ---------------------------------------------------------------------------

/// One operator-reachable sink a confidential value passes through, and why the
/// operator reads only ciphertext there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorSink {
    /// Stable identifier, matching the heading in
    /// `docs/guide/confidential-fields.md`.
    pub id: &'static str,
    /// Why plaintext cannot reach this sink.
    pub why: &'static str,
}

/// The sinks a confidential value reaches, and the reason each holds only
/// ciphertext. This is the "the operator cannot see" set the guide documents and
/// `confidential_threat_model` asserts in CI.
pub const OPERATOR_BLIND_SINKS: &[OperatorSink] = &[
    OperatorSink {
        id: "database",
        why: "the column type is `Sealed`, so the only value bound into an INSERT \
              or UPDATE is the envelope",
    },
    OperatorSink {
        id: "access_log",
        why: "the access log carries no bodies, and confidential column names are \
              folded into the log parameter filter",
    },
    OperatorSink {
        id: "db_backup",
        why: "a backup is a dump of the database, which holds only envelopes",
    },
    OperatorSink {
        id: "replay_capsule",
        why: "a capsule copies the request body and the SQL binds, both of which \
              carry envelopes",
    },
    OperatorSink {
        id: "version_history",
        why: "confidential columns are version-sensitive, so a revision records \
              that the column changed, not what it changed to",
    },
    OperatorSink {
        id: "admin_ui",
        why: "the admin cell renderer redacts registered confidential columns, and \
              never offers an editable control for one",
    },
    OperatorSink {
        id: "admin_csv_export",
        why: "the CSV export drops confidential columns and their blind-index \
              companions, so a downloaded file carries neither",
    },
];

/// What sealing does **not** hide. Stated so the guarantee is not overclaimed.
pub const OPERATOR_VISIBLE: &[&str] = &[
    "whether one value equals another, for one owner and one column, from the \
     blind-index token. An application that lets the operator make a client seal \
     a value of the operator's choosing turns that into a confirmation oracle \
     for a guessed plaintext",
    "with a `FieldContext::new` context, the operator can move, copy or roll back \
     one owner's envelope among that owner's own rows in the same column, and the \
     client cannot tell. `FieldContext::for_record` closes this",
    "that the row exists, and its id, timestamps and foreign keys",
    "the approximate length of the plaintext, from the length of the envelope",
    "every column the application did not mark `#[confidential]`",
];

#[cfg(feature = "db")]
mod diesel_types;
