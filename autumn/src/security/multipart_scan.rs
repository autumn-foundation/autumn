//! Byte-level `multipart/form-data` field scanning shared by the CSRF guard
//! (`csrf.rs`) and the submit-token replay guard (`submit_token.rs`).
//!
//! Both guards need to peek a single named field's value out of a buffered
//! request body — the CSRF token or the `_submit_token` field — without
//! disturbing the handler's own `Multipart` extraction downstream. This was
//! historically two byte-identical copies of `find_bytes`/`scan_multipart_field`,
//! one per guard; a Content-Type case-sensitivity bug was fixed in one copy
//! and had to be mirrored into the other a day later (see git history on
//! `csrf.rs`/`submit_token.rs` around 2026-07-16/17), so the two copies are
//! kept in lockstep here instead.

/// Return the byte position of the first occurrence of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scan a buffered `multipart/form-data` body for a named text field.
///
/// Returns the field value as a `&str` slice into `bytes`, or `None` when the
/// field is absent or the body is malformed / truncated.  Callers pre-limit the
/// buffer via `max_scan_bytes` so we never allocate more than that.
pub fn scan_multipart_field<'a>(
    bytes: &'a [u8],
    boundary: &str,
    field_name: &str,
) -> Option<&'a str> {
    let delimiter = format!("--{boundary}");
    let delim = delimiter.as_bytes();
    let end_marker = format!("\r\n{delimiter}");
    let end_bytes = end_marker.as_bytes();
    let mut pos = 0;

    loop {
        let rel = find_bytes(&bytes[pos..], delim)?;
        pos += rel + delim.len();

        // After the boundary: \r\n begins a part; anything else ends the multipart.
        match bytes.get(pos..pos + 2) {
            Some(b"\r\n") => pos += 2,
            _ => break, // final boundary (--), truncated, or malformed
        }

        let header_end = find_bytes(&bytes[pos..], b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&bytes[pos..pos + header_end]).ok()?;
        let value_start = pos + header_end + 4;

        let is_match = headers.lines().any(|line| {
            if !line
                .to_ascii_lowercase()
                .starts_with("content-disposition:")
            {
                return false;
            }
            line.split(';').skip(1).any(|attr| {
                attr.trim()
                    .strip_prefix("name=")
                    .map(|v| v.trim_matches('"'))
                    == Some(field_name)
            })
        });

        if is_match {
            let end = find_bytes(&bytes[value_start..], end_bytes)
                .map_or(bytes.len(), |i| value_start + i);
            return std::str::from_utf8(&bytes[value_start..end]).ok();
        }

        let next = find_bytes(&bytes[value_start..], end_bytes)?;
        // Advance to the start of the boundary delimiter (skip only the leading
        // \r\n of end_bytes so the next loop iteration finds --boundary at
        // rel=0 and processes it normally).
        pos = value_start + next + 2;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multipart_body(boundary: &str, fields: &[(&str, &str)]) -> String {
        use std::fmt::Write as _;
        let mut body = String::new();
        for (name, value) in fields {
            let _ = write!(
                body,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            );
        }
        let _ = write!(body, "--{boundary}--\r\n");
        body
    }

    #[test]
    fn find_bytes_locates_needle() {
        assert_eq!(find_bytes(b"abcdef", b"cd"), Some(2));
    }

    #[test]
    fn find_bytes_returns_none_when_absent() {
        assert_eq!(find_bytes(b"abcdef", b"zz"), None);
    }

    #[test]
    fn find_bytes_empty_needle_matches_at_start() {
        assert_eq!(find_bytes(b"abcdef", b""), Some(0));
    }

    #[test]
    fn scan_multipart_field_finds_only_field() {
        let boundary = "B";
        let body = multipart_body(boundary, &[("_csrf", "tok-1")]);
        assert_eq!(
            scan_multipart_field(body.as_bytes(), boundary, "_csrf"),
            Some("tok-1")
        );
    }

    #[test]
    fn scan_multipart_field_finds_field_after_other_field() {
        // Regression: skipping a non-matching part must not advance `pos`
        // past the next part's headers (the +2 fix in scan_multipart_field).
        let boundary = "B";
        let body = multipart_body(boundary, &[("name", "alice"), ("_csrf", "tok-2")]);
        assert_eq!(
            scan_multipart_field(body.as_bytes(), boundary, "_csrf"),
            Some("tok-2")
        );
    }

    #[test]
    fn scan_multipart_field_returns_none_when_field_absent() {
        let boundary = "B";
        let body = multipart_body(boundary, &[("name", "alice")]);
        assert_eq!(
            scan_multipart_field(body.as_bytes(), boundary, "_csrf"),
            None
        );
    }

    #[test]
    fn scan_multipart_field_returns_none_on_malformed_body() {
        let boundary = "B";
        assert_eq!(
            scan_multipart_field(b"not a multipart body", boundary, "_csrf"),
            None
        );
    }

    #[test]
    fn scan_multipart_field_finds_field_as_last_part() {
        let boundary = "B";
        let body = multipart_body(
            boundary,
            &[("name", "alice"), ("age", "9"), ("_csrf", "tok-3")],
        );
        assert_eq!(
            scan_multipart_field(body.as_bytes(), boundary, "_csrf"),
            Some("tok-3")
        );
    }
}
