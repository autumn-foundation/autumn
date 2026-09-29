//! Property-based invariants for migration checksum computation.
//!
//! Covers [`migration_checksum`], [`migration_checksum_bytes`], and the
//! line-ending / trailing-whitespace normalisation they share. These back the
//! "did an applied migration's `up.sql` change?" guard, so their stability and
//! infallibility under arbitrary input are load-bearing.
#![cfg(feature = "db")]

use autumn_web::migrate::{migration_checksum, migration_checksum_bytes};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The checksum is a pure function of the input: computing it twice yields
    /// the same hex digest. (Idempotence / determinism of the digest.)
    #[test]
    fn checksum_is_deterministic(s in ".*") {
        prop_assert_eq!(migration_checksum(&s), migration_checksum(&s));
    }

    /// Normalisation collapses `\r\n` and lone `\r` to `\n`, so a document and
    /// its CRLF- or CR-flavoured twin hash identically. This is what lets a
    /// migration recorded on Windows validate on Linux and vice versa.
    #[test]
    fn checksum_is_line_ending_agnostic(lines in prop::collection::vec("[a-zA-Z0-9 ;_]{0,12}", 0..8)) {
        let lf = lines.join("\n");
        let crlf = lines.join("\r\n");
        let cr = lines.join("\r");
        prop_assert_eq!(migration_checksum(&lf), migration_checksum(&crlf));
        prop_assert_eq!(migration_checksum(&lf), migration_checksum(&cr));
    }

    /// Trailing whitespace is trimmed before hashing, so appending trailing
    /// blank lines / spaces / tabs does not change the digest.
    #[test]
    fn checksum_ignores_trailing_whitespace(
        s in "[a-zA-Z0-9 ;_\n]{0,40}",
        tail in "[ \t\r\n]{0,16}",
    ) {
        let with_tail = format!("{s}{tail}");
        prop_assert_eq!(migration_checksum(&s), migration_checksum(&with_tail));
    }

    /// The bytes variant never panics on ARBITRARY (possibly non-UTF-8) input
    /// and always returns a 64-char lowercase-hex SHA-256 digest.
    #[test]
    fn checksum_bytes_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let digest = migration_checksum_bytes(&bytes);
        prop_assert_eq!(digest.len(), 64);
        prop_assert!(digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    /// For valid UTF-8, the bytes variant agrees with the string variant.
    #[test]
    fn checksum_bytes_matches_string_for_utf8(s in ".*") {
        prop_assert_eq!(migration_checksum_bytes(s.as_bytes()), migration_checksum(&s));
    }
}
