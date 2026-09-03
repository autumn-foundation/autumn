//! Confirming that a published `_acme-challenge` TXT record is actually visible
//! in public DNS (issue #1620).
//!
//! A DNS-01 challenge fails — and burns an ACME authorization — if the CA
//! queries before the record has propagated. So after writing a record, autumn
//! waits until **every** configured resolver returns the expected value, bounded
//! by `[server.tls.acme.dns] propagation_timeout_secs`. The timeout error names
//! the exact record, value and resolver that never caught up, because "DNS-01
//! failed" without that is unactionable.
//!
//! # Why a hand-rolled query
//!
//! One question type (`TXT`), one class (`IN`), against explicit resolver
//! addresses — the answer parsing is ~100 lines and, unlike a stub-resolver
//! crate, it lets the wait be driven deterministically in tests against an
//! in-process UDP server. It is also the same code `autumn doctor` uses for its
//! DNS-01 preflight, so the check and the runtime agree by construction.
//!
//! This is a **DNS client for one narrow purpose**, not a resolver: it sends a
//! recursion-desired query straight to the addresses in
//! `[server.tls.acme.dns] resolvers` and reads the answer section.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures::future::BoxFuture;

use super::TxtRecord;

/// `TXT` query type (RFC 1035 §3.2.2).
const QTYPE_TXT: u16 = 16;
/// `IN` class.
const QCLASS_IN: u16 = 1;
/// Recursion-desired flag in the header's second 16-bit word.
const FLAG_RECURSION_DESIRED: u16 = 0x0100;
/// Truncation flag.
const FLAG_TRUNCATED: u16 = 0x0200;
/// Fixed DNS header length.
const HEADER_LEN: usize = 12;
/// Maximum UDP answer we read. 4 KiB comfortably holds a handful of 43-byte
/// challenge values plus overhead; a larger answer sets the TC bit, which is
/// reported rather than silently truncated.
const MAX_RESPONSE: usize = 4096;

/// What one resolver said about a TXT name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtAnswer {
    /// The TXT values in the answer section (character-strings concatenated per
    /// record, as RFC 1035 §3.3.14 requires).
    pub values: Vec<String>,
    /// The response code: `0` NOERROR, `3` NXDOMAIN, …
    pub rcode: u8,
}

impl TxtAnswer {
    /// Whether the name exists but currently carries no TXT record.
    #[must_use]
    pub const fn is_nxdomain(&self) -> bool {
        self.rcode == 3
    }
}

/// Queries one resolver for a name's TXT values.
///
/// A trait so the propagation wait can be driven deterministically in tests.
pub trait TxtLookup: Send + Sync {
    /// Ask `resolver` for the TXT records at `name`.
    ///
    /// # Errors
    ///
    /// Returns a message for a transport failure or a server-side failure
    /// (`SERVFAIL`, `REFUSED`). An absent record is `Ok` with no values, not an
    /// error — it simply has not propagated yet.
    fn lookup_txt<'a>(
        &'a self,
        resolver: SocketAddr,
        name: &'a str,
    ) -> BoxFuture<'a, Result<TxtAnswer, String>>;
}

/// The production [`TxtLookup`]: a UDP query straight to the resolver.
pub struct UdpTxtLookup {
    timeout: Duration,
}

impl UdpTxtLookup {
    /// Build a lookup with a per-query timeout.
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for UdpTxtLookup {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

impl TxtLookup for UdpTxtLookup {
    fn lookup_txt<'a>(
        &'a self,
        resolver: SocketAddr,
        name: &'a str,
    ) -> BoxFuture<'a, Result<TxtAnswer, String>> {
        Box::pin(async move {
            let id = query_id();
            let query = encode_txt_query(id, name)?;
            let bind: SocketAddr = if resolver.is_ipv4() {
                "0.0.0.0:0".parse().expect("valid v4 bind address")
            } else {
                "[::]:0".parse().expect("valid v6 bind address")
            };
            let socket = tokio::net::UdpSocket::bind(bind)
                .await
                .map_err(|e| format!("could not open a UDP socket to query {resolver}: {e}"))?;
            socket
                .connect(resolver)
                .await
                .map_err(|e| format!("could not connect to resolver {resolver}: {e}"))?;
            socket
                .send(&query)
                .await
                .map_err(|e| format!("could not send a TXT query for {name} to {resolver}: {e}"))?;
            let mut buf = vec![0_u8; MAX_RESPONSE];
            let read = tokio::time::timeout(self.timeout, socket.recv(&mut buf))
                .await
                .map_err(|_| {
                    format!(
                        "resolver {resolver} did not answer a TXT query for {name} within {}s",
                        self.timeout.as_secs()
                    )
                })?
                .map_err(|e| format!("could not read the answer from resolver {resolver}: {e}"))?;
            parse_txt_response(id, name, &buf[..read])
                .map_err(|e| format!("resolver {resolver} answered a TXT query for {name}: {e}"))
        })
    }
}

/// Query one resolver for a name's TXT values, blocking.
///
/// The same wire code as [`UdpTxtLookup`], for `autumn doctor`'s synchronous
/// check path.
///
/// # Errors
///
/// As [`TxtLookup::lookup_txt`].
pub fn lookup_txt_blocking(
    resolver: SocketAddr,
    name: &str,
    timeout: Duration,
) -> Result<TxtAnswer, String> {
    let id = query_id();
    let query = encode_txt_query(id, name)?;
    let bind: SocketAddr = if resolver.is_ipv4() {
        "0.0.0.0:0".parse().expect("valid v4 bind address")
    } else {
        "[::]:0".parse().expect("valid v6 bind address")
    };
    let socket = std::net::UdpSocket::bind(bind)
        .map_err(|e| format!("could not open a UDP socket to query {resolver}: {e}"))?;
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("could not set a read timeout: {e}"))?;
    socket
        .connect(resolver)
        .map_err(|e| format!("could not connect to resolver {resolver}: {e}"))?;
    socket
        .send(&query)
        .map_err(|e| format!("could not send a TXT query for {name} to {resolver}: {e}"))?;
    let mut buf = vec![0_u8; MAX_RESPONSE];
    let read = socket.recv(&mut buf).map_err(|e| {
        format!("resolver {resolver} did not answer a TXT query for {name}: {e}")
    })?;
    parse_txt_response(id, name, &buf[..read])
        .map_err(|e| format!("resolver {resolver} answered a TXT query for {name}: {e}"))
}

/// A per-query transaction id.
///
/// Not a security boundary — the query goes to an explicitly configured resolver
/// over a connected socket — but a distinct id per query means a late answer to
/// a previous query is rejected rather than mistaken for this one's.
fn query_id() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(1);
    // Mix in the low bits of the clock so two processes (or a restart) do not
    // walk the same sequence.
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u16);
    counter ^ clock
}

/// Encode a recursion-desired `TXT`/`IN` query for `name`.
///
/// # Errors
///
/// Returns a message when a label is empty or longer than 63 bytes, or the whole
/// name exceeds 255 bytes.
pub fn encode_txt_query(id: u16, name: &str) -> Result<Vec<u8>, String> {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() {
        return Err("cannot query an empty DNS name".to_owned());
    }
    let mut out = Vec::with_capacity(HEADER_LEN + name.len() + 6);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&FLAG_RECURSION_DESIRED.to_be_bytes());
    out.extend_from_slice(&1_u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0_u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0_u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0_u16.to_be_bytes()); // ARCOUNT

    let mut encoded_len = 1; // the root label
    for label in name.split('.') {
        if label.is_empty() {
            return Err(format!("DNS name `{name}` has an empty label"));
        }
        let len = u8::try_from(label.len())
            .ok()
            .filter(|len| *len <= 63)
            .ok_or_else(|| format!("DNS label `{label}` is longer than 63 bytes"))?;
        encoded_len += 1 + label.len();
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    if encoded_len > 255 {
        return Err(format!("DNS name `{name}` is longer than 255 bytes encoded"));
    }
    out.push(0); // root label
    out.extend_from_slice(&QTYPE_TXT.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());
    Ok(out)
}

/// Parse a DNS response into the TXT values it carries for `name`.
///
/// # Errors
///
/// Returns a message for a malformed message, a transaction-id mismatch, a
/// truncated (`TC`) answer, or a server-side failure rcode. `NXDOMAIN` and an
/// empty `NOERROR` answer are **not** errors — they mean "not published yet".
pub fn parse_txt_response(id: u16, name: &str, msg: &[u8]) -> Result<TxtAnswer, String> {
    if msg.len() < HEADER_LEN {
        return Err(format!(
            "the response is {} bytes, shorter than a DNS header",
            msg.len()
        ));
    }
    let response_id = u16::from_be_bytes([msg[0], msg[1]]);
    if response_id != id {
        return Err(format!(
            "transaction id {response_id:#06x} does not match the query's {id:#06x} (a late \
             answer to a previous query)"
        ));
    }
    let flags = u16::from_be_bytes([msg[2], msg[3]]);
    if flags & FLAG_TRUNCATED != 0 {
        return Err(
            "the answer was truncated (TC); the record set is too large for a UDP answer"
                .to_owned(),
        );
    }
    let rcode = u8::try_from(flags & 0x000F).unwrap_or(0);
    match rcode {
        // NOERROR and NXDOMAIN both mean "the resolver answered"; whether the
        // value is there yet is the caller's decision.
        0 | 3 => {}
        2 => return Err("SERVFAIL — the zone's nameservers did not answer".to_owned()),
        5 => return Err("REFUSED — the resolver refused the query".to_owned()),
        other => return Err(format!("rcode {other}")),
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    let ancount = u16::from_be_bytes([msg[6], msg[7]]);

    let mut offset = HEADER_LEN;
    for _ in 0..qdcount {
        offset = skip_name(msg, offset)?;
        // QTYPE + QCLASS
        offset = offset
            .checked_add(4)
            .filter(|end| *end <= msg.len())
            .ok_or_else(|| "the question section is truncated".to_owned())?;
    }

    let mut values = Vec::new();
    for _ in 0..ancount {
        offset = skip_name(msg, offset)?;
        let header_end = offset
            .checked_add(10)
            .filter(|end| *end <= msg.len())
            .ok_or_else(|| "an answer record header is truncated".to_owned())?;
        let rtype = u16::from_be_bytes([msg[offset], msg[offset + 1]]);
        let rdlength = usize::from(u16::from_be_bytes([msg[offset + 8], msg[offset + 9]]));
        let rdata_end = header_end
            .checked_add(rdlength)
            .filter(|end| *end <= msg.len())
            .ok_or_else(|| "an answer record's RDATA is truncated".to_owned())?;
        if rtype == QTYPE_TXT {
            values.push(decode_txt_rdata(&msg[header_end..rdata_end])?);
        }
        offset = rdata_end;
    }
    let _ = name;
    Ok(TxtAnswer { values, rcode })
}

/// Advance past a (possibly compressed) domain name, returning the offset after
/// it.
fn skip_name(msg: &[u8], mut offset: usize) -> Result<usize, String> {
    loop {
        let len = *msg
            .get(offset)
            .ok_or_else(|| "a domain name runs past the end of the message".to_owned())?;
        if len & 0xC0 == 0xC0 {
            // A compression pointer is two bytes and always terminates the name.
            return offset
                .checked_add(2)
                .filter(|end| *end <= msg.len())
                .ok_or_else(|| "a compression pointer is truncated".to_owned());
        }
        if len & 0xC0 != 0 {
            return Err(format!("unsupported DNS label type {:#04x}", len & 0xC0));
        }
        offset = offset
            .checked_add(1 + usize::from(len))
            .filter(|end| *end <= msg.len())
            .ok_or_else(|| "a domain name label is truncated".to_owned())?;
        if len == 0 {
            return Ok(offset);
        }
    }
}

/// Decode TXT RDATA: one or more length-prefixed character-strings,
/// concatenated (RFC 1035 §3.3.14 / RFC 7208 §3.3).
fn decode_txt_rdata(rdata: &[u8]) -> Result<String, String> {
    let mut out = String::new();
    let mut offset = 0;
    while offset < rdata.len() {
        let len = usize::from(rdata[offset]);
        let start = offset + 1;
        let end = start
            .checked_add(len)
            .filter(|end| *end <= rdata.len())
            .ok_or_else(|| "a TXT character-string runs past its RDATA".to_owned())?;
        out.push_str(&String::from_utf8_lossy(&rdata[start..end]));
        offset = end;
    }
    Ok(out)
}

/// Which of `expected` are not present in `observed`.
///
/// Pure, so the propagation decision is testable without a resolver.
#[must_use]
pub fn missing_values(expected: &[String], observed: &[String]) -> Vec<String> {
    expected
        .iter()
        .filter(|value| !observed.contains(value))
        .cloned()
        .collect()
}

/// Group records into `fqdn → expected values`, so an apex + wildcard order's
/// two values at one name are checked as a set.
#[must_use]
pub fn group_by_name(records: &[TxtRecord]) -> BTreeMap<String, Vec<String>> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in records {
        let values = grouped.entry(record.fqdn.clone()).or_default();
        if !values.contains(&record.value) {
            values.push(record.value.clone());
        }
    }
    grouped
}

/// Why the propagation wait gave up, in operator-facing form.
///
/// Kept separate from the message so the wait's own logic is testable and the
/// wording lives in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropagationTimeout {
    /// The record name that never carried every expected value.
    pub fqdn: String,
    /// The specific values still missing.
    pub missing: Vec<String>,
    /// The resolver that still did not see them.
    pub resolver: String,
    /// What that resolver last returned for the name, or the error it gave.
    pub observed: String,
    /// The budget that elapsed, in seconds.
    pub waited_secs: u64,
}

impl std::fmt::Display for PropagationTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "DNS-01 propagation timed out after {}s: the TXT record `{}` still does not carry {} \
             at resolver {} ({}). Check that the record was written to the zone that actually \
             serves this name and that its NS delegation is live, then raise \
             [server.tls.acme.dns] propagation_timeout_secs if the provider is simply slow",
            self.waited_secs,
            self.fqdn,
            self.missing
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", "),
            self.resolver,
            self.observed
        )
    }
}

/// Wait until every record in `records` is visible at every resolver, or the
/// budget runs out.
///
/// # Errors
///
/// Returns a [`PropagationTimeout`] rendering naming the exact record, values
/// and resolver that never caught up.
pub async fn wait_for_propagation(
    records: &[TxtRecord],
    resolvers: &[SocketAddr],
    timeout: Duration,
    poll_interval: Duration,
    lookup: &dyn TxtLookup,
) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }
    if resolvers.is_empty() {
        return Err(
            "[server.tls.acme.dns] resolvers is empty, so DNS-01 propagation cannot be confirmed"
                .to_owned(),
        );
    }
    let wanted = group_by_name(records);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_gap: Option<PropagationTimeout> = None;

    loop {
        let mut all_visible = true;
        'round: for (fqdn, expected) in &wanted {
            for resolver in resolvers {
                let (missing, observed) = match lookup.lookup_txt(*resolver, fqdn).await {
                    Ok(answer) => {
                        let missing = missing_values(expected, &answer.values);
                        let observed = if answer.is_nxdomain() {
                            "the name does not exist yet".to_owned()
                        } else {
                            format!(
                                "that resolver currently returns {} TXT value(s)",
                                answer.values.len()
                            )
                        };
                        (missing, observed)
                    }
                    // A resolver error is a not-yet, not a hard failure: a
                    // freshly-created name often SERVFAILs while the zone
                    // catches up. It is recorded so the timeout can report it.
                    Err(e) => (expected.clone(), e),
                };
                if !missing.is_empty() {
                    last_gap = Some(PropagationTimeout {
                        fqdn: fqdn.clone(),
                        missing,
                        resolver: resolver.to_string(),
                        observed,
                        waited_secs: timeout.as_secs(),
                    });
                    all_visible = false;
                    break 'round;
                }
            }
        }
        if all_visible {
            return Ok(());
        }
        // Re-check the budget only after a full round, so a wait configured with
        // a single-probe budget still probes once before giving up.
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep(poll_interval.min(deadline - now)).await;
        if tokio::time::Instant::now() >= deadline {
            // One last round after the final sleep, so the full budget is used.
            continue;
        }
    }

    Err(last_gap.map_or_else(
        || "DNS-01 propagation could not be confirmed".to_owned(),
        |gap| gap.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn query_name(msg: &[u8]) -> String {
        let mut offset = HEADER_LEN;
        let mut labels = Vec::new();
        loop {
            let len = usize::from(msg[offset]);
            if len == 0 {
                break;
            }
            labels.push(String::from_utf8_lossy(&msg[offset + 1..offset + 1 + len]).into_owned());
            offset += 1 + len;
        }
        labels.join(".")
    }

    /// Build a TXT answer for `name` carrying `values`, using a compression
    /// pointer for the answer's owner name — exactly what a real resolver sends.
    fn txt_response(id: u16, name: &str, values: &[&str], rcode: u8) -> Vec<u8> {
        let question = encode_txt_query(id, name).expect("question encodes");
        let mut msg = question.clone();
        let flags: u16 = 0x8180 | u16::from(rcode);
        msg[2..4].copy_from_slice(&flags.to_be_bytes());
        let ancount = u16::try_from(values.len()).unwrap();
        msg[6..8].copy_from_slice(&ancount.to_be_bytes());
        for value in values {
            // Owner name as a compression pointer to the question's name.
            msg.extend_from_slice(&[0xC0, HEADER_LEN as u8]);
            msg.extend_from_slice(&QTYPE_TXT.to_be_bytes());
            msg.extend_from_slice(&QCLASS_IN.to_be_bytes());
            msg.extend_from_slice(&60_u32.to_be_bytes());
            let rdata_len = u16::try_from(value.len() + 1).unwrap();
            msg.extend_from_slice(&rdata_len.to_be_bytes());
            msg.push(u8::try_from(value.len()).unwrap());
            msg.extend_from_slice(value.as_bytes());
        }
        msg
    }

    #[test]
    fn a_query_encodes_the_name_as_labels() {
        let msg = encode_txt_query(0x1234, "_acme-challenge.myapp.com").expect("encodes");
        assert_eq!(&msg[0..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([msg[2], msg[3]]), FLAG_RECURSION_DESIRED);
        assert_eq!(u16::from_be_bytes([msg[4], msg[5]]), 1, "QDCOUNT");
        assert_eq!(query_name(&msg), "_acme-challenge.myapp.com");
        assert_eq!(&msg[msg.len() - 4..], &[0, 16, 0, 1], "TXT/IN");
        // A trailing dot is the same name.
        assert_eq!(
            encode_txt_query(1, "myapp.com.").unwrap(),
            encode_txt_query(1, "myapp.com").unwrap()
        );
    }

    #[test]
    fn a_query_rejects_unencodable_names() {
        assert!(encode_txt_query(1, "  ").is_err());
        assert!(encode_txt_query(1, "a..b").is_err());
        assert!(encode_txt_query(1, &format!("{}.com", "x".repeat(64))).is_err());
        let long = std::iter::repeat_n("abcdefghij", 30)
            .collect::<Vec<_>>()
            .join(".");
        assert!(encode_txt_query(1, &long).is_err());
    }

    #[test]
    fn a_response_with_a_compression_pointer_parses() {
        let id = 0xABCD;
        let msg = txt_response(id, "_acme-challenge.myapp.com", &["value-one", "value-two"], 0);
        let answer = parse_txt_response(id, "_acme-challenge.myapp.com", &msg).expect("parses");
        assert_eq!(answer.rcode, 0);
        assert_eq!(answer.values, vec!["value-one", "value-two"]);
    }

    #[test]
    fn nxdomain_is_not_an_error_it_is_not_published_yet() {
        let id = 7;
        let msg = txt_response(id, "_acme-challenge.myapp.com", &[], 3);
        let answer = parse_txt_response(id, "_acme-challenge.myapp.com", &msg).expect("parses");
        assert!(answer.is_nxdomain());
        assert!(answer.values.is_empty());
    }

    #[test]
    fn server_failures_are_errors() {
        for (rcode, needle) in [(2_u8, "SERVFAIL"), (5, "REFUSED")] {
            let msg = txt_response(9, "x.myapp.com", &[], rcode);
            let err = parse_txt_response(9, "x.myapp.com", &msg)
                .expect_err("a server failure must surface");
            assert!(err.contains(needle), "got: {err}");
        }
    }

    #[test]
    fn a_mismatched_transaction_id_is_rejected() {
        let msg = txt_response(1, "x.myapp.com", &["v"], 0);
        let err = parse_txt_response(2, "x.myapp.com", &msg).expect_err("id must match");
        assert!(err.contains("transaction id"), "got: {err}");
    }

    #[test]
    fn a_truncated_answer_is_reported_rather_than_silently_short() {
        let mut msg = txt_response(3, "x.myapp.com", &["v"], 0);
        let flags = u16::from_be_bytes([msg[2], msg[3]]) | FLAG_TRUNCATED;
        msg[2..4].copy_from_slice(&flags.to_be_bytes());
        let err = parse_txt_response(3, "x.myapp.com", &msg).expect_err("TC must surface");
        assert!(err.contains("truncated"), "got: {err}");
    }

    #[test]
    fn malformed_messages_never_panic() {
        let good = txt_response(4, "x.myapp.com", &["v"], 0);
        // Every prefix of a well-formed message must be rejected, not panic.
        for len in 0..good.len() {
            let _ = parse_txt_response(4, "x.myapp.com", &good[..len]);
        }
        // A record claiming more RDATA than is present.
        let mut lying = good.clone();
        let rdlen_at = lying.len() - 1 - 1 - "v".len();
        lying[rdlen_at..rdlen_at + 2].copy_from_slice(&9999_u16.to_be_bytes());
        assert!(parse_txt_response(4, "x.myapp.com", &lying).is_err());
    }

    // RFC 1035 §3.3.14: TXT RDATA is one or more character-strings, and a value
    // longer than 255 bytes arrives split. They concatenate.
    #[test]
    fn multi_string_txt_rdata_concatenates() {
        assert_eq!(decode_txt_rdata(&[3, b'a', b'b', b'c']).unwrap(), "abc");
        assert_eq!(
            decode_txt_rdata(&[2, b'a', b'b', 2, b'c', b'd']).unwrap(),
            "abcd"
        );
        assert_eq!(decode_txt_rdata(&[]).unwrap(), "");
        assert!(decode_txt_rdata(&[5, b'a']).is_err());
    }

    #[test]
    fn missing_values_compares_sets() {
        let expected = vec!["a".to_owned(), "b".to_owned()];
        assert!(missing_values(&expected, &["a".to_owned(), "b".to_owned(), "z".to_owned()]).is_empty());
        assert_eq!(
            missing_values(&expected, &["a".to_owned()]),
            vec!["b".to_owned()]
        );
    }

    // An apex + wildcard order publishes two DIFFERENT values at ONE name; the
    // wait must require both before telling the CA to validate.
    #[test]
    fn grouping_merges_two_values_at_one_name() {
        let records = vec![
            TxtRecord::new("myapp.com", "value-apex"),
            TxtRecord::new("myapp.com", "value-wildcard"),
        ];
        let grouped = group_by_name(&records);
        assert_eq!(grouped.len(), 1);
        assert_eq!(
            grouped["_acme-challenge.myapp.com"],
            vec!["value-apex".to_owned(), "value-wildcard".to_owned()]
        );
    }

    /// A scripted lookup: a STABLE answer per resolver, repeated on every round.
    ///
    /// Keyed per resolver rather than a shared queue, because the wait polls in
    /// rounds — a queue would hand round two whatever round one did not consume,
    /// so "this resolver is the lagging one" could not be expressed at all.
    /// A resolver with no scripted answer returns NXDOMAIN (not published yet).
    struct ScriptedLookup {
        answers: std::collections::HashMap<String, Result<TxtAnswer, String>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl ScriptedLookup {
        fn new(answers: Vec<(SocketAddr, Result<TxtAnswer, String>)>) -> Arc<Self> {
            Arc::new(Self {
                answers: answers
                    .into_iter()
                    .map(|(addr, answer)| (addr.to_string(), answer))
                    .collect(),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl TxtLookup for ScriptedLookup {
        fn lookup_txt<'a>(
            &'a self,
            resolver: SocketAddr,
            name: &'a str,
        ) -> BoxFuture<'a, Result<TxtAnswer, String>> {
            self.calls
                .lock()
                .unwrap()
                .push((resolver.to_string(), name.to_owned()));
            let answer = self
                .answers
                .get(&resolver.to_string())
                .cloned()
                .unwrap_or_else(|| {
                    Ok(TxtAnswer {
                        values: Vec::new(),
                        rcode: 3,
                    })
                });
            Box::pin(async move { answer })
        }
    }

    fn resolver(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn propagation_succeeds_once_every_resolver_sees_every_value() {
        let visible = || {
            Ok(TxtAnswer {
                values: vec!["value-apex".to_owned(), "value-wildcard".to_owned()],
                rcode: 0,
            })
        };
        let lookup = ScriptedLookup::new(vec![
            (resolver(53), visible()),
            (resolver(5353), visible()),
        ]);
        let records = vec![
            TxtRecord::new("myapp.com", "value-apex"),
            TxtRecord::new("myapp.com", "value-wildcard"),
        ];
        wait_for_propagation(
            &records,
            &[resolver(53), resolver(5353)],
            Duration::from_secs(5),
            Duration::from_millis(10),
            lookup.as_ref(),
        )
        .await
        .expect("both resolvers see both values");
        // One round, one query per resolver: the two values share a record name,
        // so they are checked as a set rather than queried separately.
        assert_eq!(lookup.call_count(), 2);
    }

    // AC5: "a bounded, documented wait for TXT record propagation whose timeout
    // error names the exact record that failed to propagate."
    #[tokio::test]
    async fn a_timeout_names_the_record_the_value_and_the_resolver() {
        let lookup = ScriptedLookup::new(Vec::new()); // every resolver: NXDOMAIN
        let records = vec![TxtRecord::new("myapp.com", "value-apex")];
        let err = wait_for_propagation(
            &records,
            &[resolver(5353)],
            Duration::from_millis(60),
            Duration::from_millis(10),
            lookup.as_ref(),
        )
        .await
        .expect_err("an unpublished record must time out");
        assert!(err.contains("_acme-challenge.myapp.com"), "got: {err}");
        assert!(err.contains("value-apex"), "got: {err}");
        assert!(err.contains("127.0.0.1:5353"), "got: {err}");
        assert!(err.contains("does not exist yet"), "got: {err}");
        assert!(
            err.contains("propagation_timeout_secs"),
            "the message must say which knob to turn: {err}"
        );
    }

    // One resolver lagging is enough to keep waiting: telling the CA to validate
    // while a resolver it might pick still 404s is how an authorization is burnt.
    #[tokio::test]
    async fn one_lagging_resolver_blocks_the_wait() {
        let lookup = ScriptedLookup::new(vec![
            (
                resolver(53),
                Ok(TxtAnswer {
                    values: vec!["v".to_owned()],
                    rcode: 0,
                }),
            ),
            (
                resolver(5353),
                Ok(TxtAnswer {
                    values: Vec::new(),
                    rcode: 0,
                }),
            ),
        ]);
        let err = wait_for_propagation(
            &[TxtRecord::new("myapp.com", "v")],
            &[resolver(53), resolver(5353)],
            Duration::from_millis(40),
            Duration::from_millis(10),
            lookup.as_ref(),
        )
        .await
        .expect_err("the lagging resolver must hold the wait");
        assert!(err.contains("127.0.0.1:5353"), "got: {err}");
    }

    // A resolver error is a not-yet rather than a hard failure — but it is
    // reported verbatim if the budget then runs out, because SERVFAIL on
    // `_acme-challenge` is usually a broken delegation.
    #[tokio::test]
    async fn a_resolver_error_is_carried_into_the_timeout_message() {
        let lookup = ScriptedLookup::new(vec![(
            resolver(53),
            Err("SERVFAIL — the zone's nameservers did not answer".to_owned()),
        )]);
        let err = wait_for_propagation(
            &[TxtRecord::new("myapp.com", "v")],
            &[resolver(53)],
            Duration::from_millis(20),
            Duration::from_millis(10),
            lookup.as_ref(),
        )
        .await
        .expect_err("times out");
        assert!(err.contains("SERVFAIL"), "got: {err}");
    }

    #[tokio::test]
    async fn no_records_is_an_immediate_pass_and_no_resolvers_is_an_error() {
        let lookup = ScriptedLookup::new(Vec::new());
        assert!(
            wait_for_propagation(
                &[],
                &[],
                Duration::from_secs(1),
                Duration::from_millis(1),
                lookup.as_ref()
            )
            .await
            .is_ok()
        );
        let err = wait_for_propagation(
            &[TxtRecord::new("myapp.com", "v")],
            &[],
            Duration::from_secs(1),
            Duration::from_millis(1),
            lookup.as_ref(),
        )
        .await
        .expect_err("no resolvers cannot confirm anything");
        assert!(err.contains("resolvers"), "got: {err}");
    }

    // The real UDP path, against an in-process resolver: proves the wire format
    // autumn sends is one a server can answer, and that the answer round-trips.
    #[tokio::test]
    async fn udp_lookup_round_trips_against_a_real_socket() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind fake resolver");
        let addr = server.local_addr().expect("local addr");
        tokio::spawn(async move {
            let mut buf = vec![0_u8; 512];
            let Ok((read, peer)) = server.recv_from(&mut buf).await else {
                return;
            };
            let id = u16::from_be_bytes([buf[0], buf[1]]);
            let name = query_name(&buf[..read]);
            let response = txt_response(id, &name, &["propagated-value"], 0);
            let _ = server.send_to(&response, peer).await;
        });

        let lookup = UdpTxtLookup::new(Duration::from_secs(5));
        let answer = lookup
            .lookup_txt(addr, "_acme-challenge.myapp.com")
            .await
            .expect("the fake resolver answers");
        assert_eq!(answer.values, vec!["propagated-value"]);
    }

    #[tokio::test]
    async fn udp_lookup_times_out_against_a_silent_resolver() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind silent resolver");
        let addr = server.local_addr().expect("local addr");
        let lookup = UdpTxtLookup::new(Duration::from_millis(50));
        let err = lookup
            .lookup_txt(addr, "_acme-challenge.myapp.com")
            .await
            .expect_err("a silent resolver must time out, not hang");
        assert!(err.contains("did not answer"), "got: {err}");
    }
}
