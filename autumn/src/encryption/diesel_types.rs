//! Diesel `serialize_as` / `deserialize_as` wrapper types that make encryption
//! transparent to application code.
//!
//! The `#[model]` macro routes an `#[encrypted]` `String` field through one of
//! these wrappers via `#[diesel(serialize_as = ..., deserialize_as = ...)]`. On
//! the way to the database the wrapper encrypts (ciphertext at rest); on the way
//! back it decrypts (plaintext in Rust). The public field stays a plain `String`,
//! so `repo.find(id)` and `repo.update(..)` need no per-call-site changes.

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql};
use diesel::serialize::{self, IsNull, Output, ToSql};
use diesel::sql_types::Text;
use diesel::{AsExpression, FromSqlRow};

use super::{Mode, decrypt_text, encrypt_text};

macro_rules! encrypted_text_wrapper {
    ($(#[$meta:meta])* $name:ident, $mode:expr) => {
        $(#[$meta])*
        #[derive(AsExpression, FromSqlRow, Clone)]
        #[diesel(sql_type = Text)]
        pub struct $name(String);

        impl From<String> for $name {
            fn from(plaintext: String) -> Self {
                Self(plaintext)
            }
        }

        impl From<$name> for String {
            fn from(wrapper: $name) -> Self {
                wrapper.0
            }
        }

        // Encrypted columns never leak plaintext through `Debug` (composes with
        // #697 log scrubbing). The development escape hatch may opt back in.
        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                if super::debug_plaintext_enabled() {
                    write!(f, "{}({:?})", stringify!($name), self.0)
                } else {
                    write!(f, "{}(<encrypted>)", stringify!($name))
                }
            }
        }

        impl ToSql<Text, ::diesel::sqlite::Sqlite> for $name {
            fn to_sql<'b>(
                &'b self,
                out: &mut Output<'b, '_, ::diesel::sqlite::Sqlite>,
            ) -> serialize::Result {
                let envelope = encrypt_text($mode, &self.0)?;
                out.set_value(envelope);
                Ok(IsNull::No)
            }
        }

        #[cfg(feature = "postgres")]
        impl ToSql<Text, ::diesel::pg::Pg> for $name {
            fn to_sql<'b>(
                &'b self,
                out: &mut Output<'b, '_, ::diesel::pg::Pg>,
            ) -> serialize::Result {
                use ::std::io::Write as _;
                let envelope = encrypt_text($mode, &self.0)?;
                out.write_all(envelope.as_bytes())?;
                Ok(IsNull::No)
            }
        }

        impl<DB> FromSql<Text, DB> for $name
        where
            DB: Backend,
            String: FromSql<Text, DB>,
        {
            fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
                let envelope = String::from_sql(bytes)?;
                let plaintext = decrypt_text(&envelope)?;
                Ok(Self(plaintext))
            }
        }
    };
}

encrypted_text_wrapper!(
    /// Randomized AEAD wrapper (fresh nonce per write; no equality lookups).
    RandomizedText,
    Mode::Randomized
);

encrypted_text_wrapper!(
    /// Deterministic AEAD wrapper (stable ciphertext; supports equality lookups).
    DeterministicText,
    Mode::Deterministic
);
