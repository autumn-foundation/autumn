#![no_main]
//! Fuzz target: body handling.
//!
//! The first byte selects a sub-surface; the remainder is the raw body:
//!   0 => inbound-mail SES/SNS JSON parsing
//!   1 => inbound-mail generic RFC 5322 / MIME parsing (incl. multipart bodies)
//!   2 => inbound-mail RFC 5322 address-list parsing
//!   3 => `application/x-www-form-urlencoded` body decoding

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 4 {
        0 => __fuzz::parse_ses(rest),
        1 => __fuzz::parse_generic(rest),
        2 => __fuzz::parse_address_list(&String::from_utf8_lossy(rest)),
        _ => __fuzz::decode_urlencoded_form(rest),
    }
});
