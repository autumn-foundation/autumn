//! Typed path helpers and the [`PathExt`] fluent query-string builder.
//!
//! Route macros emit a `__autumn_path_{name}(…) -> String` companion alongside
//! every handler. This module provides the [`PathExt`] extension trait that
//! lets callers append query parameters to those strings with a single
//! chained expression:
//!
//! ```ignore
//! let url = paths::list_posts().with_query("page", 2).with_query("size", 10);
//! // → "/posts?page=2&size=10"
//! ```

/// Fluent query-string builder for path strings produced by typed path helpers.
///
/// Automatically imported via [`autumn_web::prelude`].
pub trait PathExt {
    /// Append a percent-encoded `key=value` query parameter.
    ///
    /// The first call adds `?key=value`; subsequent calls add `&key=value`.
    /// Both key and value are percent-encoded (RFC 3986 §2.1).
    ///
    /// # Examples
    ///
    /// ```
    /// use autumn_web::paths::PathExt;
    ///
    /// let url = "/posts".to_string().with_query("page", 2).with_query("q", "hello world");
    /// assert_eq!(url, "/posts?page=2&q=hello%20world");
    /// ```
    #[must_use]
    fn with_query(self, key: impl std::fmt::Display, value: impl std::fmt::Display) -> String;
}

impl PathExt for String {
    /// Appends a percent-encoded `key=value` query parameter directly to `self`.
    /// This optimization eliminates allocating separate Strings for `key`, `value`,
    /// and `format!`, reducing heap allocations per query parameter.
    fn with_query(mut self, key: impl std::fmt::Display, value: impl std::fmt::Display) -> String {
        let sep = if self.contains('?') { '&' } else { '?' };
        self.push(sep);
        percent_encode_to(&key.to_string(), &mut self);
        self.push('=');
        percent_encode_to(&value.to_string(), &mut self);
        self
    }
}

/// Percent-encode one dynamic route path segment.
///
/// Route macro helpers use this before interpolating path parameters so
/// Display values like `a/b` remain a single segment (`a%2Fb`).
#[doc(hidden)]
#[must_use]
pub fn encode_path_segment(value: impl std::fmt::Display) -> String {
    percent_encode(&value.to_string())
}

/// Percent-encode a catch-all path parameter (starts with `*`).
///
/// Splitting on `/` allows preserving directory slashes while percent-encoding
/// other characters in each segment.
#[doc(hidden)]
#[must_use]
pub fn encode_catch_all_param(value: impl std::fmt::Display) -> String {
    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + 16);
    for (i, segment) in s.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if segment == "." {
            out.push_str("%2E");
        } else if segment == ".." {
            out.push_str("%2E%2E");
        } else {
            percent_encode_to(segment, &mut out);
        }
    }
    out
}

/// Percent-encode a query component per RFC 3986.
///
/// Unreserved characters (ALPHA / DIGIT / `-` / `_` / `.` / `~`) are left
/// unchanged; everything else is `%XX`-encoded.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    percent_encode_to(s, &mut out);
    out
}

/// Appends a percent-encoded query component per RFC 3986 directly into the provided buffer.
/// This prevents allocating intermediate Strings and copying bytes into the final string.
fn percent_encode_to(s: &str, out: &mut String) {
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b => {
                out.push('%');
                let hi = b >> 4;
                let lo = b & 0xF;
                out.push(
                    char::from_digit(u32::from(hi), 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                out.push(
                    char::from_digit(u32::from(lo), 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_query_first_param() {
        assert_eq!("/posts".to_string().with_query("page", 1), "/posts?page=1");
    }

    #[test]
    fn with_query_second_param() {
        let url = "/posts"
            .to_string()
            .with_query("page", 1)
            .with_query("size", 20);
        assert_eq!(url, "/posts?page=1&size=20");
    }

    #[test]
    fn with_query_encodes_space() {
        assert_eq!(
            "/search".to_string().with_query("q", "hello world"),
            "/search?q=hello%20world"
        );
    }

    #[test]
    fn with_query_encodes_equals_and_ampersand() {
        assert_eq!(
            "/x".to_string().with_query("filter", "a=b&c"),
            "/x?filter=a%3Db%26c"
        );
    }

    #[test]
    fn with_query_leaves_unreserved_chars_alone() {
        assert_eq!(
            "/x".to_string()
                .with_query("tag", "hello-world_foo.bar~baz"),
            "/x?tag=hello-world_foo.bar~baz"
        );
    }

    #[test]
    fn test_encode_catch_all_param() {
        assert_eq!(encode_catch_all_param("a/b/c d"), "a/b/c%20d");
        assert_eq!(encode_catch_all_param("foo bar/baz"), "foo%20bar/baz");
    }

    #[test]
    fn test_encode_catch_all_param_dot_segments() {
        assert_eq!(encode_catch_all_param("a/../b"), "a/%2E%2E/b");
        assert_eq!(encode_catch_all_param("a/./b"), "a/%2E/b");
        assert_eq!(encode_catch_all_param(".."), "%2E%2E");
        assert_eq!(encode_catch_all_param("."), "%2E");
    }
}
