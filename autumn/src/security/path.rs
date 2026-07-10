//! Path normalization helper for security middleware.
//!
//! Security middlewares (CSRF, CAPTCHA) that match exemption prefixes against
//! the request path must evaluate a *normalized* path. Otherwise an attacker
//! can craft a request like `/api/../submit` that satisfies an `/api/`
//! exemption prefix while any downstream component that resolves dot-segments
//! (a reverse proxy, a nested service, `tower-http`'s path normalization)
//! routes it to a protected endpoint.

/// Normalizes a URL path for security decisions.
///
/// - Treats percent-encoded slashes (`%2f` / `%2F`) as segment separators,
///   so hybrids like `%2e%2e%2f` cannot smuggle a dot-segment inside a
///   single opaque segment.
/// - Resolves `.` and `..` segments (`..` never climbs above the root).
/// - Collapses duplicate slashes (`//`).
/// - Treats percent-encoded dot-segments (e.g. `%2e%2e`, `.%2E`) as their
///   decoded equivalents, since the raw [`http::Uri`] path is not
///   percent-decoded at this layer.
///
/// Decoding is single-pass: double-encoded forms (`%252e`, `%252f`) are left
/// untouched, matching the one decode step the application's own router (or a
/// proxy performing a single percent-decode) applies. Other segments are
/// preserved byte-for-byte (no general percent-decoding is performed). A
/// trailing slash in the input is preserved so that segment-boundary prefix
/// checks keep working.
pub fn clean_path(path: &str) -> String {
    let decoded = decode_encoded_slashes(path);
    let path: &str = &decoded;
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            // Collapse duplicate slashes.
            continue;
        }
        match dot_segment_len(segment) {
            Some(1) => {}
            Some(_) => {
                segments.pop();
            }
            None => segments.push(segment),
        }
    }

    let mut normalized = String::with_capacity(path.len());
    if path.starts_with('/') {
        normalized.push('/');
    }
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            normalized.push('/');
        }
        normalized.push_str(segment);
    }
    if path.ends_with('/') && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

/// Decodes percent-encoded slashes (`%2f` / `%2F`) to literal `/` so that
/// they are treated as segment separators during normalization. Without this,
/// `POST /api/%2e%2e%2fsubmit` stays a single opaque segment under `/api/`
/// (and would satisfy an `/api/` exemption prefix) while a downstream that
/// percent-decodes before resolving dot-segments routes it to `/submit`.
///
/// Single-pass by construction: the scan never revisits produced output, so
/// double-encoded `%252f` (which decodes once to the literal three characters
/// `%2f`, not to a separator) is left untouched — consistent with the
/// single-pass `%2e` handling in [`dot_segment_len`].
fn decode_encoded_slashes(path: &str) -> std::borrow::Cow<'_, str> {
    let bytes = path.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0; // start of the pending not-yet-copied literal run
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'%'
            && bytes[i + 1] == b'2'
            && (bytes[i + 2] == b'f' || bytes[i + 2] == b'F')
        {
            let out = out.get_or_insert_with(|| String::with_capacity(path.len()));
            out.push_str(&path[copied..i]);
            out.push('/');
            i += 3;
            copied = i;
        } else {
            i += 1;
        }
    }
    out.map_or(std::borrow::Cow::Borrowed(path), |mut out| {
        out.push_str(&path[copied..]);
        std::borrow::Cow::Owned(out)
    })
}

/// Returns `Some(dot_count)` when `segment` consists solely of one or two
/// dots, where each dot may be literal (`.`) or percent-encoded (`%2e` /
/// `%2E`). Returns `None` for every other segment.
fn dot_segment_len(segment: &str) -> Option<usize> {
    let bytes = segment.as_bytes();
    let mut i = 0;
    let mut dots = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                dots += 1;
                i += 1;
            }
            b'%' if bytes.len() >= i + 3
                && bytes[i + 1] == b'2'
                && (bytes[i + 2] == b'e' || bytes[i + 2] == b'E') =>
            {
                dots += 1;
                i += 3;
            }
            _ => return None,
        }
        if dots > 2 {
            return None;
        }
    }
    if dots == 0 { None } else { Some(dots) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_plain_paths() {
        assert_eq!(clean_path("/api/public"), "/api/public");
        assert_eq!(clean_path("/api/public/"), "/api/public/");
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path(""), "");
    }

    #[test]
    fn resolves_dot_dot_segments() {
        assert_eq!(clean_path("/api/../protected"), "/protected");
        assert_eq!(clean_path("/api/v1/../../protected"), "/protected");
        assert_eq!(clean_path("/api/public/.."), "/api");
        assert_eq!(clean_path("/api/public/../"), "/api/");
        assert_eq!(clean_path("/../.."), "/");
    }

    #[test]
    fn resolves_single_dot_segments() {
        assert_eq!(clean_path("/api/./items"), "/api/items");
        assert_eq!(clean_path("/./api"), "/api");
    }

    #[test]
    fn collapses_duplicate_slashes() {
        assert_eq!(clean_path("//api///items"), "/api/items");
    }

    #[test]
    fn resolves_percent_encoded_dot_segments() {
        assert_eq!(clean_path("/api/%2e%2e/protected"), "/protected");
        assert_eq!(clean_path("/api/%2E%2e/protected"), "/protected");
        assert_eq!(clean_path("/api/.%2e/protected"), "/protected");
        assert_eq!(clean_path("/api/%2e/items"), "/api/items");
    }

    #[test]
    fn treats_encoded_slashes_as_separators() {
        // `%2f` hides the segment boundary from a naive split, so the encoded
        // dot-segment must still resolve and drop the `/api` prefix.
        assert_eq!(clean_path("/api/%2e%2e%2fsubmit"), "/submit");
        assert_eq!(clean_path("/api/%2E%2E%2Fsubmit"), "/submit");
        assert_eq!(clean_path("/api%2f..%2fprotected"), "/protected");
        // A legit path with an encoded slash and no dot-segments normalizes
        // to its decoded form; an `/api/` exemption prefix still matches.
        assert_eq!(clean_path("/api/file%2fname"), "/api/file/name");
        assert_eq!(clean_path("/api%2f"), "/api/");
    }

    #[test]
    fn leaves_double_encoded_forms_untouched() {
        // Decoding is single-pass: `%252f` decodes (once, downstream) to the
        // literal text `%2f`, not to a separator, so it must not be treated
        // as one here.
        assert_eq!(clean_path("/api/file%252fname"), "/api/file%252fname");
        assert_eq!(
            clean_path("/api/%252e%252e%252fsubmit"),
            "/api/%252e%252e%252fsubmit"
        );
    }

    #[test]
    fn keeps_segments_that_merely_contain_dots() {
        assert_eq!(clean_path("/api/..name"), "/api/..name");
        assert_eq!(clean_path("/api/a%2e%2e"), "/api/a%2e%2e");
        assert_eq!(clean_path("/api/..."), "/api/...");
        assert_eq!(clean_path("/file.txt"), "/file.txt");
    }
}
