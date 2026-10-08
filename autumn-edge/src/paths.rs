//! Typed path helpers for `#[edge]` routes.
//!
//! The route macro emits a `__autumn_path_{name}(…) -> String` helper for each
//! route. For an `#[edge]` route, that helper calls the encoders in this
//! module, so it compiles for `wasm32-wasip1` too. A handler that builds a
//! link then has the same source at the origin and at the edge (#1790).
//!
//! The encoders give the same bytes as `autumn_web::paths`. A test in
//! `autumn-web` compares the two.

/// Fluent query-string builder for the strings that path helpers return.
pub trait PathExt {
    /// Append a percent-encoded `key=value` query parameter.
    ///
    /// The first call adds `?key=value`. Each next call adds `&key=value`.
    ///
    /// ```
    /// use autumn_edge::paths::PathExt;
    ///
    /// let url = "/posts".to_owned().with_query("q", "hello world");
    /// assert_eq!(url, "/posts?q=hello%20world");
    /// ```
    #[must_use]
    fn with_query(self, key: impl std::fmt::Display, value: impl std::fmt::Display) -> String;
}

impl PathExt for String {
    fn with_query(mut self, key: impl std::fmt::Display, value: impl std::fmt::Display) -> String {
        let sep = if self.contains('?') { '&' } else { '?' };
        self.push(sep);
        percent_encode_to(&key.to_string(), &mut self);
        self.push('=');
        percent_encode_to(&value.to_string(), &mut self);
        self
    }
}

/// Percent-encode one dynamic path segment. `a/b` becomes `a%2Fb`.
#[doc(hidden)]
#[must_use]
pub fn encode_path_segment(value: impl std::fmt::Display) -> String {
    let value = value.to_string();
    let mut out = String::with_capacity(value.len());
    percent_encode_to(&value, &mut out);
    out
}

/// Percent-encode a catch-all path parameter. Keeps `/`, and encodes the
/// dot segments `.` and `..`.
#[doc(hidden)]
#[must_use]
pub fn encode_catch_all_param(value: impl std::fmt::Display) -> String {
    let value = value.to_string();
    let mut out = String::with_capacity(value.len());
    for (i, segment) in value.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        match segment {
            "." => out.push_str("%2E"),
            ".." => out.push_str("%2E%2E"),
            _ => percent_encode_to(segment, &mut out),
        }
    }
    out
}

/// Append `s` to `out`, percent-encoded per RFC 3986. Unreserved bytes
/// (ALPHA, DIGIT, `-`, `_`, `.`, `~`) stay as they are.
fn percent_encode_to(s: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            for nibble in [byte >> 4, byte & 0x0F] {
                // A nibble is below 16, so `get` always finds a digit.
                if let Some(&digit) = HEX.get(usize::from(nibble)) {
                    out.push(char::from(digit));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_keeps_unreserved_bytes() {
        assert_eq!(encode_path_segment("Az09-_.~"), "Az09-_.~");
    }

    #[test]
    fn segment_encodes_slash_space_and_utf8() {
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
        assert_eq!(encode_path_segment("élève"), "%C3%A9l%C3%A8ve");
        assert_eq!(encode_path_segment(42), "42");
    }

    #[test]
    fn catch_all_keeps_slashes_and_encodes_dot_segments() {
        assert_eq!(encode_catch_all_param("a/b/c d"), "a/b/c%20d");
        assert_eq!(encode_catch_all_param("a/../b"), "a/%2E%2E/b");
        assert_eq!(encode_catch_all_param("a/./b"), "a/%2E/b");
        assert_eq!(encode_catch_all_param(".."), "%2E%2E");
        assert_eq!(encode_catch_all_param(""), "");
    }

    #[test]
    fn with_query_adds_then_appends() {
        let url = "/posts"
            .to_owned()
            .with_query("page", 2)
            .with_query("q", "a&b=c");
        assert_eq!(url, "/posts?page=2&q=a%26b%3Dc");
    }
}
