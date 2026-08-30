//! `validator` delegations for [`Classified`] (issue #1654).
//!
//! `#[model]` keeps a `#[classified]` column's `#[validate(...)]` rules on the
//! generated read struct (the "effective merged model" path from #1778), so the
//! rules have to see through the taint wrapper. Each impl forwards to the inner
//! value's own rule and adds nothing: classification governs where a value may
//! *go*, never whether it is well-formed.
//!
//! # Why the `as_*_string` accessors return `None`
//!
//! Two of `validator`'s traits split into a `bool` verdict with a default body
//! and a *required accessor that hands back the value* --
//! `ValidateEmail::as_email_string` and `ValidateUrl::as_url_string` both return
//! `Option<Cow<'_, str>>`, and `Option<Cow<str>>` is `Serialize`. Forwarding
//! those accessors would have reopened the exact hole this module exists to
//! close: `Json(customer.email.as_email_string())` would compile and ship the
//! plaintext with no boundary and no record.
//!
//! So the *verdict* is overridden to forward to the inner value, and the
//! accessor returns `None`. The rule is evaluated on the real value; nothing
//! hands the value back. Every other impl here already returns only a `bool`.
//! [`ValidateLength`] is the one exception, and it yields a character count,
//! never the characters.
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
    fn validate_email(&self) -> bool {
        self.inner().validate_email()
    }

    /// Deliberately `None`: see the module docs. The verdict above is what the
    /// rule needs; this accessor would hand the plaintext to any caller.
    fn as_email_string(&self) -> Option<Cow<'_, str>> {
        None
    }
}

impl<T: ValidateUrl, F: ClassifiedField> ValidateUrl for Classified<T, F> {
    fn validate_url(&self) -> bool {
        self.inner().validate_url()
    }

    /// Deliberately `None`: see the module docs.
    fn as_url_string(&self) -> Option<Cow<'_, str>> {
        None
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
    use crate::classify::Classification;

    struct EmailField;
    impl crate::classify::ClassifiedField for EmailField {
        const MODEL: &'static str = "Customer";
        const MODEL_PATH: &'static str = "test::Customer";
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
    fn the_value_returning_accessors_hand_back_nothing() {
        // `Option<Cow<str>>` is `Serialize`, so forwarding these would be an
        // unrecorded release. The verdicts above already prove the rules run.
        let c = classified("ada@example.com");
        assert!(c.as_email_string().is_none());
        assert!(c.as_url_string().is_none());
        assert!(c.validate_email());
    }

    #[test]
    fn ip_rules_see_through_the_wrapper() {
        assert!(classified("127.0.0.1").validate_ipv4());
        assert!(!classified("127.0.0.1").validate_ipv6());
    }
}
