#![no_main]
//! Fuzz target: body handling.
//!
//! The first byte selects a sub-surface; the remainder is the raw body:
//!   0 => inbound-mail SES/SNS JSON parsing
//!   1 => inbound-mail generic RFC 5322 / MIME parsing (incl. multipart bodies)
//!   2 => inbound-mail RFC 5322 address-list parsing
//!   3 => `application/x-www-form-urlencoded` body decoding
//!   4 => inbound-mail Mailgun `multipart/form-data` webhook parsing, where the
//!        next byte is the `Content-Type` length and the rest is the body

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 5 {
        0 => __fuzz::parse_ses(rest),
        1 => __fuzz::parse_generic(rest),
        2 => __fuzz::parse_address_list(&String::from_utf8_lossy(rest)),
        3 => __fuzz::decode_urlencoded_form(rest),
        _ => {
            let Some((&ct_len, rest)) = rest.split_first() else {
                return;
            };
            let (content_type, body) = rest.split_at(usize::from(ct_len).min(rest.len()));
            __fuzz::parse_mailgun_form_data(body, &String::from_utf8_lossy(content_type));
        }
    }
});
