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
/// - Resolves `.` and `..` segments (`..` never climbs above the root).
/// - Collapses duplicate slashes (`//`).
/// - Treats percent-encoded dot-segments (e.g. `%2e%2e`, `.%2E`) as their
///   decoded equivalents, since the raw [`http::Uri`] path is not
///   percent-decoded at this layer.
///
/// Non-dot segments are preserved byte-for-byte (no general percent-decoding
/// is performed). A trailing slash in the input is preserved so that
/// segment-boundary prefix checks keep working.
pub(crate) fn clean_path(path: &str) -> String {
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
    normalized.push_str(&segments.join("/"));
    if path.ends_with('/') && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
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
    fn keeps_segments_that_merely_contain_dots() {
        assert_eq!(clean_path("/api/..name"), "/api/..name");
        assert_eq!(clean_path("/api/a%2e%2e"), "/api/a%2e%2e");
        assert_eq!(clean_path("/api/..."), "/api/...");
        assert_eq!(clean_path("/file.txt"), "/file.txt");
    }
}
