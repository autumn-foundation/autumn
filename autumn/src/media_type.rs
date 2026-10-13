//! Case-insensitive media-type matching (RFC 9110 §8.3.1).

/// Return `true` when `content_type` names the media type `essence`.
///
/// Type and subtype ignore ASCII case. Parameters (`; charset=…`) and leading
/// white space are ignored.
pub fn is_media_type(content_type: &str, essence: &str) -> bool {
    content_type
        .trim_start()
        .split(';')
        .next()
        .is_some_and(|ty| ty.trim_end().eq_ignore_ascii_case(essence))
}

#[cfg(test)]
mod tests {
    use super::is_media_type;

    const FORM: &str = "application/x-www-form-urlencoded";

    #[test]
    fn matches_any_case() {
        for ct in [
            FORM,
            "Application/X-WWW-Form-Urlencoded",
            "APPLICATION/X-WWW-FORM-URLENCODED; charset=UTF-8",
        ] {
            assert!(is_media_type(ct, FORM), "{ct}");
        }
    }

    #[test]
    fn ignores_parameters_and_leading_space() {
        assert!(is_media_type(
            "  application/x-www-form-urlencoded;charset=utf-8",
            FORM
        ));
        assert!(is_media_type(
            "application/x-www-form-urlencoded ; a=b",
            FORM
        ));
    }

    #[test]
    fn rejects_other_types() {
        for ct in [
            "",
            "text/plain",
            "application/json",
            "application/x-www-form-urlencoded-evil",
            "xapplication/x-www-form-urlencoded",
        ] {
            assert!(!is_media_type(ct, FORM), "{ct}");
        }
    }
}
