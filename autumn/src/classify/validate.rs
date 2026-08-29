//! `validator` delegations for [`Classified`] (issue #1654).
//!
//! `#[model]` keeps a `#[classified]` column's `#[validate(...)]` rules on the
//! generated read struct (the "effective merged model" path from #1778), so the
//! rules have to see through the taint wrapper. Each impl forwards to the inner
//! value's own rule and adds nothing: classification governs where a value may
//! *go*, never whether it is well-formed.
//!
//! Nothing here can leak: every method returns a `bool` (or a borrowed view the
//! validator consumes immediately), never an owned copy of the value.
//!
//! `ValidateDoesNotContain` is deliberately absent -- `validator` blanket-implements
//! it for every `ValidateContains`, so implementing it here would collide. So are
//! the rules behind `validator`'s optional `card` / `unic` features, which this
//! workspace does not enable, and the numeric `ValidateRange`, which cannot apply
//! to the `String` columns v1 classifies.

use std::borrow::Cow;

use validator::{
    ValidateContains, ValidateEmail, ValidateIp, ValidateLength, ValidateRegex, ValidateUrl,
};

use super::{Classified, ClassifiedField};

impl<T: ValidateEmail, F: ClassifiedField> ValidateEmail for Classified<T, F> {
    fn as_email_string(&self) -> Option<Cow<'_, str>> {
        self.inner().as_email_string()
    }
}

impl<T: ValidateUrl, F: ClassifiedField> ValidateUrl for Classified<T, F> {
    fn as_url_string(&self) -> Option<Cow<'_, str>> {
        self.inner().as_url_string()
    }
}

impl<T: ValidateContains, F: ClassifiedField> ValidateContains for Classified<T, F> {
    fn validate_contains(&self, needle: &str) -> bool {
        self.inner().validate_contains(needle)
    }
}

impl<T: ValidateLength<u64>, F: ClassifiedField> ValidateLength<u64> for Classified<T, F> {
    fn length(&self) -> Option<u64> {
        self.inner().length()
    }
}

impl<T: ValidateRegex, F: ClassifiedField> ValidateRegex for Classified<T, F> {
    fn validate_regex(&self, regex: impl validator::AsRegex) -> bool {
        self.inner().validate_regex(regex)
    }
}

impl<T: ValidateIp, F: ClassifiedField> ValidateIp for Classified<T, F> {
    fn validate_ipv4(&self) -> bool {
        self.inner().validate_ipv4()
    }

    fn validate_ipv6(&self) -> bool {
        self.inner().validate_ipv6()
    }

    fn validate_ip(&self) -> bool {
        self.inner().validate_ip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Classification, ClassifiedField as _};

    struct EmailField;
    impl crate::classify::ClassifiedField for EmailField {
        const MODEL: &'static str = "Customer";
        const FIELD: &'static str = "email";
        const CLASSIFICATION: Classification = Classification::PersonalData;
    }

    fn classified(value: &str) -> Classified<String, EmailField> {
        Classified::new(value.to_string())
    }

    #[test]
    fn email_rules_see_through_the_wrapper() {
        assert!(classified("ada@example.com").validate_email());
        assert!(!classified("not-an-email").validate_email());
    }

    #[test]
    fn length_rules_see_through_the_wrapper() {
        assert!(classified("abcd").validate_length(Some(2), Some(8), None));
        assert!(!classified("a").validate_length(Some(2), Some(8), None));
    }

    #[test]
    fn contains_and_its_blanket_negation_see_through_the_wrapper() {
        use validator::ValidateDoesNotContain as _;
        assert!(classified("ada@example.com").validate_contains("@"));
        assert!(classified("ada@example.com").validate_does_not_contain("#"));
    }

    #[test]
    fn url_rules_see_through_the_wrapper() {
        assert!(classified("https://example.com").validate_url());
        assert!(!classified("not a url").validate_url());
    }

    #[test]
    fn ip_rules_see_through_the_wrapper() {
        assert!(classified("127.0.0.1").validate_ipv4());
        assert!(!classified("127.0.0.1").validate_ipv6());
    }

    #[test]
    fn the_marker_is_still_the_field_identity() {
        assert_eq!(EmailField::FIELD, "email");
    }
}
