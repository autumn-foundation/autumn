#![no_main]
//! Fuzz target: cookie / signed-session decode.
//!
//! Layout of the input bytes:
//!   [name_len:1][cookie_name][secret_len:1][secret][cookie_header_bytes...]
//! The cookie header bytes are parsed by the same `get_cookie` +
//! `{id}.{hmac_hex}` verification path `SessionLayer` uses.

use autumn::__fuzz;
use libfuzzer_sys::fuzz_target;

fn take_prefixed<'a>(data: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    let Some((&len, rest)) = data.split_first() else {
        return (&[], &[]);
    };
    let len = (len as usize).min(rest.len());
    rest.split_at(len)
}

fuzz_target!(|data: &[u8]| {
    let (name_bytes, rest) = take_prefixed(data);
    let (secret, cookie_header) = take_prefixed(rest);
    let cookie_name = String::from_utf8_lossy(name_bytes);

    let _ = __fuzz::decode_cookie(cookie_header, &cookie_name, secret);
});
