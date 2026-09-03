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
//! This is a **DNS client for one narrow purpose**, not a resolver.
//!
//! # Why the probe goes to the AUTHORITATIVE servers
//!
//! The obvious implementation — ask `1.1.1.1` whether the record is there yet —
//! is quietly broken. The first probe fires the instant the provider's API
//! returns, which is *before* the record is live on the zone's own nameservers,
//! so the recursive resolver answers `NXDOMAIN` and **caches that negatively**
//! for the zone's SOA minimum (RFC 2308). Route 53 defaults to 900s and
//! Cloudflare to 1800s — both longer than the 300s propagation budget. Every
//! later probe then reads the cached negative answer, the wait times out, and
//! the next hourly attempt repeats it. Forever.
//!
//! So the configured `[server.tls.acme.dns] resolvers` are used to *discover*
//! the zone's authoritative nameservers ([`authoritative_resolvers`]), and the
//! propagation probe is sent to those directly with recursion **not** desired —
//! which is also what the CA effectively does. The configured resolvers remain
//! the fallback when discovery fails (a split-horizon setup, a resolver that
//! will not answer `NS`), because a recursive probe is still better than none.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures::future::BoxFuture;

use super::TxtRecord;

/// `A` query type (RFC 1035 §3.2.2).
const QTYPE_A: u16 = 1;
/// `NS` query type.
const QTYPE_NS: u16 = 2;
/// `TXT` query type.
const QTYPE_TXT: u16 = 16;
/// `OPT` pseudo-record type (EDNS0, RFC 6891).
const QTYPE_OPT: u16 = 41;
/// `IN` class.
const QCLASS_IN: u16 = 1;
/// Recursion-desired flag in the header's second 16-bit word.
const FLAG_RECURSION_DESIRED: u16 = 0x0100;
/// Query/response flag: set on a response.
const FLAG_RESPONSE: u16 = 0x8000;
/// Truncation flag.
const FLAG_TRUNCATED: u16 = 0x0200;
/// How many compression pointers a single name may follow before the message is
/// rejected as malformed. Bounds the classic decompression loop.
const MAX_NAME_POINTERS: usize = 32;
/// Fixed DNS header length.
const HEADER_LEN: usize = 12;
/// Maximum UDP answer we read. 4 KiB comfortably holds a handful of 43-byte
/// challenge values plus overhead; a larger answer sets the TC bit, which is
/// reported rather than silently truncated.
const MAX_RESPONSE: usize = 4096;

/// What one resolver said about a TXT name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

/// Sends one DNS query to one server.
///
/// A trait so the propagation wait and the authoritative-server discovery can be
/// driven deterministically in tests, against scripted answers rather than the
/// network.
pub trait DnsLookup: Send + Sync {
    /// Ask `server` a question of type `qtype` about `name`.
    ///
    /// # Errors
    ///
    /// Returns a message for a transport failure, a malformed or unrelated
    /// answer, or a server-side failure (`SERVFAIL`, `REFUSED`). An absent record
    /// is `Ok` with no matching records, not an error — it simply has not
    /// propagated yet.
    fn query<'a>(
        &'a self,
        server: SocketAddr,
        name: &'a str,
        qtype: u16,
        recursion_desired: bool,
    ) -> BoxFuture<'a, Result<DnsAnswer, String>>;
}

/// The production [`DnsLookup`]: a UDP query straight to the server.
pub struct UdpDnsLookup {
    timeout: Duration,
}

impl UdpDnsLookup {
    /// Build a lookup with a per-query timeout.
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for UdpDnsLookup {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

impl DnsLookup for UdpDnsLookup {
    fn query<'a>(
        &'a self,
        server: SocketAddr,
        name: &'a str,
        qtype: u16,
        recursion_desired: bool,
    ) -> BoxFuture<'a, Result<DnsAnswer, String>> {
        Box::pin(async move {
            let id = query_id();
            let query = encode_query(id, name, qtype, recursion_desired)?;
            let socket = tokio::net::UdpSocket::bind(unspecified_bind(server))
                .await
                .map_err(|e| format!("could not open a UDP socket to query {server}: {e}"))?;
            socket
                .connect(server)
                .await
                .map_err(|e| format!("could not connect to {server}: {e}"))?;
            socket
                .send(&query)
                .await
                .map_err(|e| format!("could not send a query for {name} to {server}: {e}"))?;
            let mut buf = vec![0_u8; MAX_RESPONSE];
            let read = tokio::time::timeout(self.timeout, socket.recv(&mut buf))
                .await
                .map_err(|_| {
                    format!(
                        "{server} did not answer a query for {name} within {}s",
                        self.timeout.as_secs()
                    )
                })?
                .map_err(|e| format!("could not read the answer from {server}: {e}"))?;
            parse_response(id, name, &buf[..read])
                .map_err(|e| format!("{server} answered a query for {name}: {e}"))
        })
    }
}

/// Discover the addresses of the nameservers **authoritative** for `fqdn`.
///
/// Walks the label suffixes of `fqdn` from most to least specific asking `NS`
/// through `recursive` (the configured resolvers), takes the first suffix that
/// answers with nameserver names, and resolves those names to addresses. The
/// `_acme-challenge` label is dropped first: it is a record name inside the
/// zone, never a zone cut of its own.
///
/// Returns an empty vector when discovery fails at any step — the caller then
/// falls back to probing the recursive resolvers, which is worse but not
/// nothing. See the module docs for why the authoritative probe matters.
pub async fn authoritative_resolvers(
    fqdn: &str,
    recursive: &[SocketAddr],
    lookup: &dyn DnsLookup,
) -> Vec<SocketAddr> {
    let base = normalize_name(fqdn);
    let base = base.strip_prefix("_acme-challenge.").unwrap_or(&base);
    let labels: Vec<&str> = base.split('.').filter(|l| !l.is_empty()).collect();

    for start in 0..labels.len().saturating_sub(1) {
        let zone = labels[start..].join(".");
        let mut names = Vec::new();
        for server in recursive {
            if let Ok(answer) = lookup.query(*server, &zone, QTYPE_NS, true).await {
                names = answer.ns_names();
                if !names.is_empty() {
                    break;
                }
            }
        }
        if names.is_empty() {
            continue;
        }
        let mut addrs = Vec::new();
        for name in &names {
            for server in recursive {
                if let Ok(answer) = lookup.query(*server, name, QTYPE_A, true).await {
                    for addr in answer.a_addrs() {
                        let socket = SocketAddr::new(std::net::IpAddr::V4(addr), 53);
                        if !addrs.contains(&socket) {
                            addrs.push(socket);
                        }
                    }
                    if !addrs.is_empty() {
                        break;
                    }
                }
            }
        }
        if !addrs.is_empty() {
            return addrs;
        }
    }
    Vec::new()
}

/// Query one resolver for a name's TXT values, blocking.
///
/// The same wire code as [`UdpDnsLookup`], for `autumn doctor`'s synchronous
/// check path.
///
/// # Errors
///
/// As [`DnsLookup::query`].
pub fn lookup_txt_blocking(
    resolver: SocketAddr,
    name: &str,
    timeout: Duration,
) -> Result<TxtAnswer, String> {
    let id = query_id();
    let query = encode_query(id, name, QTYPE_TXT, true)?;
    let socket = std::net::UdpSocket::bind(unspecified_bind(resolver))
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
    let read = socket
        .recv(&mut buf)
        .map_err(|e| format!("resolver {resolver} did not answer a TXT query for {name}: {e}"))?;
    parse_txt_response(id, name, &buf[..read])
        .map_err(|e| format!("resolver {resolver} answered a TXT query for {name}: {e}"))
}

/// The wildcard local address to bind before querying `resolver`, matching its
/// address family.
///
/// Built rather than parsed, so there is no fallible step and no `# Panics`
/// caveat on the query functions.
const fn unspecified_bind(resolver: SocketAddr) -> SocketAddr {
    match resolver {
        SocketAddr::V4(_) => {
            SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
        }
        SocketAddr::V6(_) => {
            SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0)
        }
    }
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
        .map_or(0, |d| u16::try_from(d.subsec_nanos() & 0xFFFF).unwrap_or(0));
    counter ^ clock
}

/// Encode a `TXT`/`IN` query for `name`, as [`encode_query`] with `QTYPE_TXT`.
///
/// # Errors
///
/// As [`encode_query`].
pub fn encode_txt_query(id: u16, name: &str) -> Result<Vec<u8>, String> {
    encode_query(id, name, QTYPE_TXT, true)
}

/// Encode a DNS query for `name` of type `qtype`.
///
/// An EDNS0 `OPT` record advertising a 4096-byte buffer is always included, so a
/// record set that would not fit a bare 512-byte UDP answer comes back whole
/// instead of truncated (see [`MAX_RESPONSE`]).
///
/// `recursion_desired` is `false` for the authoritative probe: those servers are
/// authoritative for the name, so recursion is both unnecessary and usually
/// refused.
///
/// # Errors
///
/// Returns a message when a label is empty or longer than 63 bytes, or the whole
/// name exceeds 255 bytes.
pub fn encode_query(
    id: u16,
    name: &str,
    qtype: u16,
    recursion_desired: bool,
) -> Result<Vec<u8>, String> {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() {
        return Err("cannot query an empty DNS name".to_owned());
    }
    let mut out = Vec::with_capacity(HEADER_LEN + name.len() + 17);
    out.extend_from_slice(&id.to_be_bytes());
    let flags = if recursion_desired {
        FLAG_RECURSION_DESIRED
    } else {
        0
    };
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1_u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0_u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0_u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&1_u16.to_be_bytes()); // ARCOUNT: the EDNS0 OPT below

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
        return Err(format!(
            "DNS name `{name}` is longer than 255 bytes encoded"
        ));
    }
    out.push(0); // root label
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());

    // EDNS0 OPT (RFC 6891 §6.1.2): root name, type OPT, CLASS = the UDP payload
    // size we can accept, zero TTL/flags, zero RDLENGTH.
    out.push(0);
    out.extend_from_slice(&QTYPE_OPT.to_be_bytes());
    out.extend_from_slice(&u16::try_from(MAX_RESPONSE).unwrap_or(4096).to_be_bytes());
    out.extend_from_slice(&0_u32.to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes());
    Ok(out)
}

/// One resource record's payload, decoded for the types this client asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Rdata {
    /// A `TXT` record's character-strings, concatenated (RFC 1035 §3.3.14).
    Txt(String),
    /// The domain name in an `NS` (or `CNAME`) record, decompressed.
    Name(String),
    /// An `A` record's address.
    A(std::net::Ipv4Addr),
    /// A record type this client does not decode.
    Other,
}

/// One decoded answer-section record.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResourceRecord {
    /// The record's owner name, lowercased and without the trailing dot.
    pub name: String,
    /// The record type.
    pub rtype: u16,
    /// The decoded payload.
    pub rdata: Rdata,
}

/// A parsed DNS response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DnsAnswer {
    /// The response code: `0` NOERROR, `3` NXDOMAIN, …
    pub rcode: u8,
    /// The answer-section records.
    pub records: Vec<ResourceRecord>,
}

impl DnsAnswer {
    /// The `TXT` values whose owner name is `name`.
    #[must_use]
    pub fn txt_values(&self, name: &str) -> Vec<String> {
        let wanted = normalize_name(name);
        self.records
            .iter()
            .filter(|r| r.name == wanted)
            .filter_map(|r| match &r.rdata {
                Rdata::Txt(value) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    /// The `NS` names in the answer.
    #[must_use]
    pub fn ns_names(&self) -> Vec<String> {
        self.records
            .iter()
            .filter(|r| r.rtype == QTYPE_NS)
            .filter_map(|r| match &r.rdata {
                Rdata::Name(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// The `A` addresses in the answer.
    #[must_use]
    pub fn a_addrs(&self) -> Vec<std::net::Ipv4Addr> {
        self.records
            .iter()
            .filter_map(|r| match &r.rdata {
                Rdata::A(addr) => Some(*addr),
                _ => None,
            })
            .collect()
    }
}

/// Lowercase a domain name and drop any trailing dot, so two spellings of the
/// same name compare equal.
fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Parse a DNS response into the TXT values it carries for `name`.
///
/// # Errors
///
/// As [`parse_response`].
pub fn parse_txt_response(id: u16, name: &str, msg: &[u8]) -> Result<TxtAnswer, String> {
    let answer = parse_response(id, name, msg)?;
    Ok(TxtAnswer {
        values: answer.txt_values(name),
        rcode: answer.rcode,
    })
}

/// Parse a DNS response to a query for `name`.
///
/// Validates that the message is a *response* to *this* query — the QR bit is
/// set, the transaction id matches, and the echoed question is the name that was
/// asked — before reading the answer section. Without those checks an unrelated
/// or reflected datagram would be read as propagation evidence.
///
/// # Errors
///
/// Returns a message for a malformed message, a mismatched id or question, a
/// truncated (`TC`) answer, or a server-side failure rcode. `NXDOMAIN` and an
/// empty `NOERROR` answer are **not** errors — they mean "not published yet".
pub fn parse_response(id: u16, name: &str, msg: &[u8]) -> Result<DnsAnswer, String> {
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
    if flags & FLAG_RESPONSE == 0 {
        return Err("the datagram is a query, not a response".to_owned());
    }
    if flags & FLAG_TRUNCATED != 0 {
        return Err(
            "the answer was truncated (TC) even with EDNS0; the record set is too large for a \
             UDP answer"
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
    for index in 0..qdcount {
        let (question, next) = read_name(msg, offset)?;
        // The echoed question must be the name that was asked. A server that
        // answers a different question is answering someone else's query.
        if index == 0 && question != normalize_name(name) {
            return Err(format!(
                "the response echoes the question `{question}`, not the queried `{}`",
                normalize_name(name)
            ));
        }
        offset = next
            .checked_add(4)
            .filter(|end| *end <= msg.len())
            .ok_or_else(|| "the question section is truncated".to_owned())?;
    }

    let mut records = Vec::new();
    for _ in 0..ancount {
        let (owner, next) = read_name(msg, offset)?;
        offset = next;
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
        let rdata = match rtype {
            QTYPE_TXT => Rdata::Txt(decode_txt_rdata(&msg[header_end..rdata_end])?),
            // An NS target is a domain name in the message, so it may be
            // compressed against an earlier one — decode it against the WHOLE
            // message rather than the RDATA slice.
            QTYPE_NS => Rdata::Name(read_name(msg, header_end)?.0),
            QTYPE_A if rdlength == 4 => Rdata::A(std::net::Ipv4Addr::new(
                msg[header_end],
                msg[header_end + 1],
                msg[header_end + 2],
                msg[header_end + 3],
            )),
            _ => Rdata::Other,
        };
        records.push(ResourceRecord {
            name: owner,
            rtype,
            rdata,
        });
        offset = rdata_end;
    }
    Ok(DnsAnswer { rcode, records })
}

/// Read a (possibly compressed) domain name, returning it and the offset just
/// past its encoding in the record stream.
///
/// Following a compression pointer does not advance the returned offset past the
/// two pointer bytes — that is what the record stream contains. The number of
/// pointers followed is bounded by [`MAX_NAME_POINTERS`], so a message whose
/// pointers form a cycle is rejected instead of looping forever.
fn read_name(msg: &[u8], start: usize) -> Result<(String, usize), String> {
    let mut labels: Vec<String> = Vec::new();
    let mut offset = start;
    let mut after: Option<usize> = None;
    let mut pointers = 0;
    loop {
        let len = *msg
            .get(offset)
            .ok_or_else(|| "a domain name runs past the end of the message".to_owned())?;
        if len & 0xC0 == 0xC0 {
            let low = *msg
                .get(offset + 1)
                .ok_or_else(|| "a compression pointer is truncated".to_owned())?;
            pointers += 1;
            if pointers > MAX_NAME_POINTERS {
                return Err("a domain name follows too many compression pointers".to_owned());
            }
            // The record stream continues after the pointer, wherever the name
            // itself is stored.
            after.get_or_insert(offset + 2);
            let target = usize::from(u16::from_be_bytes([len & 0x3F, low]));
            if target >= msg.len() || target >= offset {
                // A pointer must point BACKWARDS; anything else is malformed and
                // is the shape a decompression cycle takes.
                return Err("a compression pointer does not point backwards".to_owned());
            }
            offset = target;
            continue;
        }
        if len & 0xC0 != 0 {
            return Err(format!("unsupported DNS label type {:#04x}", len & 0xC0));
        }
        let label_start = offset + 1;
        let label_end = label_start
            .checked_add(usize::from(len))
            .filter(|end| *end <= msg.len())
            .ok_or_else(|| "a domain name label is truncated".to_owned())?;
        if len == 0 {
            return Ok((
                labels.join(".").to_ascii_lowercase(),
                after.unwrap_or(label_start),
            ));
        }
        labels.push(String::from_utf8_lossy(&msg[label_start..label_end]).into_owned());
        offset = label_end;
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
#[non_exhaustive]
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

/// Which nameservers to probe for each challenge name.
///
/// A multi-domain order spans as many zones as it has base domains, and one
/// zone's authoritative nameservers are not authoritative for another: asked
/// about a name they do not serve they answer REFUSED or a referral, never the
/// TXT value. Probing every name at one zone's servers therefore cannot
/// succeed — the wait burns its whole budget and then the caller deletes every
/// record, so an otherwise-correct multi-domain order fails every time.
///
/// Targets are held per name, with a `fallback` (the configured recursive
/// resolvers) used for any name whose authoritative set could not be
/// discovered. Discovery failing for one zone must not drag the others down to
/// the fallback (issue #1620).
#[derive(Debug, Clone, Default)]
pub struct ProbeTargets {
    /// Per challenge FQDN, in first-seen order. A `Vec` rather than a map: an
    /// order has a handful of names at most, and insertion order keeps the
    /// probe sequence (and so any timeout message) predictable.
    per_name: Vec<(String, Vec<SocketAddr>)>,
    fallback: Vec<SocketAddr>,
}

impl ProbeTargets {
    /// Targets that probe `fallback` for every name.
    #[must_use]
    pub fn flat(fallback: &[SocketAddr]) -> Self {
        Self {
            per_name: Vec::new(),
            fallback: fallback.to_vec(),
        }
    }

    /// Probe `servers` for `fqdn`, replacing any previous entry for it.
    pub fn set(&mut self, fqdn: &str, servers: Vec<SocketAddr>) {
        match self.per_name.iter_mut().find(|(name, _)| name == fqdn) {
            Some((_, existing)) => *existing = servers,
            None => self.per_name.push((fqdn.to_owned(), servers)),
        }
    }

    /// The servers to probe for `fqdn`: its own, else the fallback.
    #[must_use]
    pub fn for_name(&self, fqdn: &str) -> &[SocketAddr] {
        self.per_name
            .iter()
            .find(|(name, _)| name == fqdn)
            .map_or(self.fallback.as_slice(), |(_, servers)| servers.as_slice())
    }
}

/// Wait until every record in `records` is visible at every server probed for
/// its name, or the budget runs out.
///
/// # Errors
///
/// Returns a [`PropagationTimeout`] rendering naming the exact record, values
/// and resolver that never caught up.
pub async fn wait_for_propagation(
    records: &[TxtRecord],
    targets: &ProbeTargets,
    timeout: Duration,
    poll_interval: Duration,
    lookup: &dyn DnsLookup,
) -> Result<(), String> {
    if records.is_empty() {
        return Ok(());
    }
    let wanted = group_by_name(records);
    // Checked per name rather than once: a name can only be confirmed if
    // *something* answers for it, and with per-zone targets one name having no
    // server is possible while another has several.
    if let Some((fqdn, _)) = wanted
        .iter()
        .find(|(fqdn, _)| targets.for_name(fqdn).is_empty())
    {
        return Err(format!(
            "no nameserver to probe for {fqdn}, so DNS-01 propagation cannot be confirmed: \
             the zone's authoritative servers could not be discovered and \
             [server.tls.acme.dns] resolvers is empty"
        ));
    }
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let mut last_gap: Option<PropagationTimeout> = None;

    loop {
        let mut all_visible = true;
        'round: for (fqdn, expected) in &wanted {
            for resolver in targets.for_name(fqdn) {
                // Recursion is deliberately NOT desired: `resolvers` here are
                // the zone's authoritative servers whenever discovery succeeded,
                // and they answer for the name directly. A recursive resolver
                // used as a fallback answers a non-recursive query from its
                // cache, which is the same thing this probe wants to read.
                let (missing, observed) =
                    match lookup.query(*resolver, fqdn, QTYPE_TXT, false).await {
                        Ok(answer) => {
                            let values = answer.txt_values(fqdn);
                            let missing = missing_values(expected, &values);
                            let observed = if answer.rcode == 3 {
                                "the name does not exist there yet".to_owned()
                            } else {
                                format!("it currently returns {} TXT value(s)", values.len())
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
                        // Filled in below with the ELAPSED time; the loop may
                        // run one round past the deadline, so the configured
                        // budget would systematically understate the wait.
                        waited_secs: 0,
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
        // Loop back for another round even when the sleep crossed the deadline:
        // the budget is spent on probing, and the deadline check at the top of
        // the next iteration is what ends the wait.
        tokio::time::sleep(poll_interval.min(deadline - now)).await;
    }

    Err(last_gap.map_or_else(
        || "DNS-01 propagation could not be confirmed".to_owned(),
        |mut gap| {
            gap.waited_secs = started.elapsed().as_secs();
            gap.to_string()
        },
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

    /// The question section of a query for `name`, with the header's counts
    /// rewritten for a response — the base every fake answer below is built on.
    ///
    /// Deliberately drops the query's EDNS0 OPT record: it lives in the
    /// ADDITIONAL section, so anything appended here must follow the question
    /// directly or the parser would read the OPT as the first answer.
    fn response_head(id: u16, name: &str, ancount: u16, rcode: u8) -> Vec<u8> {
        let query = encode_txt_query(id, name).expect("question encodes");
        // Header + QNAME + QTYPE/QCLASS, stopping before the OPT record.
        let mut end = HEADER_LEN;
        while query[end] != 0 {
            end += 1 + usize::from(query[end]);
        }
        end += 1 + 4;
        let mut msg = query[..end].to_vec();
        let flags: u16 = FLAG_RESPONSE | FLAG_RECURSION_DESIRED | u16::from(rcode);
        msg[2..4].copy_from_slice(&flags.to_be_bytes());
        msg[6..8].copy_from_slice(&ancount.to_be_bytes());
        msg[10..12].copy_from_slice(&0_u16.to_be_bytes()); // ARCOUNT: no OPT here
        msg
    }

    /// Append one answer record whose owner name is a compression pointer to the
    /// question's name — the shape a real resolver sends.
    fn push_answer(msg: &mut Vec<u8>, rtype: u16, rdata: &[u8]) {
        msg.extend_from_slice(&[0xC0, u8::try_from(HEADER_LEN).expect("header fits u8")]);
        msg.extend_from_slice(&rtype.to_be_bytes());
        msg.extend_from_slice(&QCLASS_IN.to_be_bytes());
        msg.extend_from_slice(&60_u32.to_be_bytes());
        msg.extend_from_slice(
            &u16::try_from(rdata.len())
                .expect("rdata fits u16")
                .to_be_bytes(),
        );
        msg.extend_from_slice(rdata);
    }

    /// Build a TXT answer for `name` carrying `values`.
    fn txt_response(id: u16, name: &str, values: &[&str], rcode: u8) -> Vec<u8> {
        let mut msg = response_head(
            id,
            name,
            u16::try_from(values.len()).expect("answer count fits u16"),
            rcode,
        );
        for value in values {
            let mut rdata = vec![u8::try_from(value.len()).expect("string fits u8")];
            rdata.extend_from_slice(value.as_bytes());
            push_answer(&mut msg, QTYPE_TXT, &rdata);
        }
        msg
    }

    /// Encode a domain name as uncompressed DNS labels.
    fn encode_labels(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.split('.').filter(|l| !l.is_empty()) {
            out.push(u8::try_from(label.len()).expect("label fits u8"));
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    /// Build an `NS` answer for `zone` naming `servers`.
    fn ns_response(id: u16, zone: &str, servers: &[&str]) -> Vec<u8> {
        let mut msg = response_head(
            id,
            zone,
            u16::try_from(servers.len()).expect("answer count fits u16"),
            0,
        );
        for server in servers {
            push_answer(&mut msg, QTYPE_NS, &encode_labels(server));
        }
        msg
    }

    /// Build an `A` answer for `name` carrying `addrs`.
    fn a_response(id: u16, name: &str, addrs: &[[u8; 4]]) -> Vec<u8> {
        let mut msg = response_head(
            id,
            name,
            u16::try_from(addrs.len()).expect("answer count fits u16"),
            0,
        );
        for addr in addrs {
            push_answer(&mut msg, QTYPE_A, addr);
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
        // QTYPE/QCLASS sit right after the root label, BEFORE the EDNS0 OPT
        // record the query now appends (see `a_query_advertises_an_edns0_buffer`).
        let qtype_at = msg.len() - 11 - 4;
        assert_eq!(&msg[qtype_at..qtype_at + 4], &[0, 16, 0, 1], "TXT/IN");
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
        let msg = txt_response(
            id,
            "_acme-challenge.myapp.com",
            &["value-one", "value-two"],
            0,
        );
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
        let mut lying = good;
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

    // EDNS0 (RFC 6891): without an OPT record advertising a bigger buffer, a
    // server caps the answer at 512 bytes and sets TC — which the propagation
    // wait would then retry for its whole budget against a record that IS
    // published.
    #[test]
    fn a_query_advertises_an_edns0_buffer() {
        let msg = encode_txt_query(0x1234, "_acme-challenge.myapp.com").expect("encodes");
        assert_eq!(u16::from_be_bytes([msg[10], msg[11]]), 1, "ARCOUNT");
        // The OPT record is the last 11 bytes: root name, type, class (= the
        // advertised UDP payload size), TTL, RDLENGTH.
        let opt = &msg[msg.len() - 11..];
        assert_eq!(opt[0], 0, "OPT's owner name is the root");
        assert_eq!(u16::from_be_bytes([opt[1], opt[2]]), QTYPE_OPT);
        assert_eq!(
            usize::from(u16::from_be_bytes([opt[3], opt[4]])),
            MAX_RESPONSE,
            "the advertised buffer must match what we actually read"
        );
    }

    // The propagation probe must not be recursive: it goes to the zone's own
    // nameservers, which answer for the name directly.
    #[test]
    fn recursion_can_be_turned_off() {
        let recursive = encode_query(1, "myapp.com", QTYPE_TXT, true).expect("encodes");
        let authoritative = encode_query(1, "myapp.com", QTYPE_TXT, false).expect("encodes");
        assert_eq!(
            u16::from_be_bytes([recursive[2], recursive[3]]),
            FLAG_RECURSION_DESIRED
        );
        assert_eq!(u16::from_be_bytes([authoritative[2], authoritative[3]]), 0);
    }

    // A datagram without the QR bit is a query, not an answer — reading one as
    // propagation evidence would let a reflected packet satisfy the wait.
    #[test]
    fn a_query_is_not_accepted_as_a_response() {
        let mut msg = txt_response(11, "x.myapp.com", &["v"], 0);
        let flags = u16::from_be_bytes([msg[2], msg[3]]) & !FLAG_RESPONSE;
        msg[2..4].copy_from_slice(&flags.to_be_bytes());
        let err = parse_response(11, "x.myapp.com", &msg).expect_err("QR must be set");
        assert!(err.contains("not a response"), "got: {err}");
    }

    // An answer to a DIFFERENT question is somebody else's answer.
    #[test]
    fn a_response_echoing_another_question_is_rejected() {
        let msg = txt_response(12, "other.myapp.com", &["v"], 0);
        let err = parse_response(12, "x.myapp.com", &msg).expect_err("question must match");
        assert!(err.contains("echoes the question"), "got: {err}");
    }

    // TXT records belonging to a different owner name in the same answer must
    // not count towards this name's propagation.
    #[test]
    fn txt_values_are_scoped_to_their_owner_name() {
        let answer = DnsAnswer {
            rcode: 0,
            records: vec![
                ResourceRecord {
                    name: "_acme-challenge.myapp.com".to_owned(),
                    rtype: QTYPE_TXT,
                    rdata: Rdata::Txt("mine".to_owned()),
                },
                ResourceRecord {
                    name: "_acme-challenge.other.com".to_owned(),
                    rtype: QTYPE_TXT,
                    rdata: Rdata::Txt("theirs".to_owned()),
                },
            ],
        };
        assert_eq!(
            answer.txt_values("_acme-challenge.myapp.com"),
            vec!["mine".to_owned()]
        );
        assert_eq!(
            answer.txt_values("_ACME-CHALLENGE.MyApp.com."),
            vec!["mine"]
        );
    }

    #[test]
    fn ns_and_a_records_decode() {
        let msg = ns_response(21, "myapp.com", &["ns1.provider.net", "ns2.provider.net"]);
        let answer = parse_response(21, "myapp.com", &msg).expect("parses");
        assert_eq!(
            answer.ns_names(),
            vec!["ns1.provider.net".to_owned(), "ns2.provider.net".to_owned()]
        );

        let msg = a_response(
            22,
            "ns1.provider.net",
            &[[192, 0, 2, 10], [198, 51, 100, 7]],
        );
        let answer = parse_response(22, "ns1.provider.net", &msg).expect("parses");
        assert_eq!(
            answer.a_addrs(),
            vec![
                std::net::Ipv4Addr::new(192, 0, 2, 10),
                std::net::Ipv4Addr::new(198, 51, 100, 7)
            ]
        );
    }

    // Decoding a compressed name is the one place a hostile message can loop
    // forever. A pointer must point strictly backwards, and the hop count is
    // bounded either way.
    #[test]
    fn a_self_referential_compression_pointer_is_rejected_not_looped() {
        // A pointer at offset 12 that points at itself.
        let mut msg = vec![0_u8; 14];
        msg[12] = 0xC0;
        msg[13] = 12;
        let err = read_name(&msg, 12).expect_err("a self-pointer must be rejected");
        assert!(err.contains("backwards"), "got: {err}");

        // …and one pointing forwards, the other half of the same cycle.
        let mut msg = vec![0_u8; 32];
        msg[12] = 0xC0;
        msg[13] = 20;
        msg[20] = 0xC0;
        msg[21] = 12;
        assert!(read_name(&msg, 12).is_err());
    }

    #[test]
    fn a_backwards_compression_pointer_resolves_and_advances_past_the_pointer() {
        // "myapp.com" at offset 12, then a pointer to it at offset 23.
        let mut msg = vec![0_u8; 12];
        msg.extend_from_slice(&encode_labels("myapp.com"));
        let pointer_at = msg.len();
        msg.extend_from_slice(&[0xC0, 12]);
        let (name, next) = read_name(&msg, pointer_at).expect("resolves");
        assert_eq!(name, "myapp.com");
        assert_eq!(
            next,
            pointer_at + 2,
            "the record stream continues after the two pointer bytes"
        );
    }

    /// A lookup that answers `NS` and `A` from a script, so authoritative
    /// discovery can be driven without a network.
    struct DiscoveryLookup {
        ns: std::collections::HashMap<String, Vec<String>>,
        a: std::collections::HashMap<String, Vec<std::net::Ipv4Addr>>,
        asked: Mutex<Vec<(String, u16)>>,
    }

    impl DiscoveryLookup {
        fn new(ns: &[(&str, &[&str])], a: &[(&str, &[[u8; 4]])]) -> Arc<Self> {
            Arc::new(Self {
                ns: ns
                    .iter()
                    .map(|(zone, servers)| {
                        (
                            (*zone).to_owned(),
                            servers.iter().map(|s| (*s).to_owned()).collect(),
                        )
                    })
                    .collect(),
                a: a.iter()
                    .map(|(name, addrs)| {
                        (
                            (*name).to_owned(),
                            addrs
                                .iter()
                                .map(|o| std::net::Ipv4Addr::new(o[0], o[1], o[2], o[3]))
                                .collect(),
                        )
                    })
                    .collect(),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl DnsLookup for DiscoveryLookup {
        fn query<'a>(
            &'a self,
            _server: SocketAddr,
            name: &'a str,
            qtype: u16,
            _recursion_desired: bool,
        ) -> BoxFuture<'a, Result<DnsAnswer, String>> {
            self.asked.lock().unwrap().push((name.to_owned(), qtype));
            let owner = normalize_name(name);
            let records = match qtype {
                QTYPE_NS => self
                    .ns
                    .get(&owner)
                    .map(|servers| {
                        servers
                            .iter()
                            .map(|s| ResourceRecord {
                                name: owner.clone(),
                                rtype: QTYPE_NS,
                                rdata: Rdata::Name(s.clone()),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                QTYPE_A => self
                    .a
                    .get(&owner)
                    .map(|addrs| {
                        addrs
                            .iter()
                            .map(|addr| ResourceRecord {
                                name: owner.clone(),
                                rtype: QTYPE_A,
                                rdata: Rdata::A(*addr),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            Box::pin(async move {
                Ok(DnsAnswer {
                    rcode: if records.is_empty() { 3 } else { 0 },
                    records,
                })
            })
        }
    }

    // The whole point of the authoritative probe: the zone's own nameservers are
    // found from the challenge FQDN, with the `_acme-challenge` label dropped and
    // the NS names resolved to addresses.
    #[tokio::test]
    async fn authoritative_discovery_finds_the_zones_nameservers() {
        let lookup = DiscoveryLookup::new(
            &[("myapp.com", &["ns1.provider.net", "ns2.provider.net"])],
            &[
                ("ns1.provider.net", &[[192, 0, 2, 10]]),
                ("ns2.provider.net", &[[198, 51, 100, 7]]),
            ],
        );
        let found = authoritative_resolvers(
            "_acme-challenge.myapp.com",
            &[resolver(53)],
            lookup.as_ref(),
        )
        .await;
        assert_eq!(
            found,
            vec![
                SocketAddr::from(([192, 0, 2, 10], 53)),
                SocketAddr::from(([198, 51, 100, 7], 53)),
            ]
        );
        // The `_acme-challenge` label is a record inside the zone, never a zone
        // cut, so the NS question is asked about the zone itself.
        let asked = lookup.asked.lock().unwrap().clone();
        assert!(
            asked.contains(&("myapp.com".to_owned(), QTYPE_NS)),
            "got: {asked:?}"
        );
        assert!(
            !asked
                .iter()
                .any(|(name, _)| name.starts_with("_acme-challenge")),
            "the challenge label must be stripped before the NS lookup: {asked:?}"
        );
    }

    // A delegated sub-zone wins over its parent: the most specific suffix that
    // answers NS is the zone that actually serves the record.
    #[tokio::test]
    async fn discovery_prefers_the_most_specific_delegated_zone() {
        let lookup = DiscoveryLookup::new(
            &[
                ("tenants.myapp.com", &["ns1.sub.net"]),
                ("myapp.com", &["ns1.parent.net"]),
            ],
            &[
                ("ns1.sub.net", &[[192, 0, 2, 1]]),
                ("ns1.parent.net", &[[192, 0, 2, 2]]),
            ],
        );
        let found = authoritative_resolvers(
            "_acme-challenge.tenants.myapp.com",
            &[resolver(53)],
            lookup.as_ref(),
        )
        .await;
        assert_eq!(found, vec![SocketAddr::from(([192, 0, 2, 1], 53))]);
    }

    // Discovery is best-effort: when it finds nothing the caller falls back to
    // the configured resolvers rather than failing the order.
    #[tokio::test]
    async fn discovery_returns_empty_when_nothing_answers() {
        let lookup = DiscoveryLookup::new(&[], &[]);
        assert!(
            authoritative_resolvers(
                "_acme-challenge.myapp.com",
                &[resolver(53)],
                lookup.as_ref()
            )
            .await
            .is_empty()
        );
        // …and an NS answer whose names do not resolve is also a miss.
        let lookup = DiscoveryLookup::new(&[("myapp.com", &["ns1.provider.net"])], &[]);
        assert!(
            authoritative_resolvers(
                "_acme-challenge.myapp.com",
                &[resolver(53)],
                lookup.as_ref()
            )
            .await
            .is_empty()
        );
    }

    #[test]
    fn missing_values_compares_sets() {
        let expected = vec!["a".to_owned(), "b".to_owned()];
        assert!(
            missing_values(&expected, &["a".to_owned(), "b".to_owned(), "z".to_owned()]).is_empty()
        );
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

    impl DnsLookup for ScriptedLookup {
        fn query<'a>(
            &'a self,
            server: SocketAddr,
            name: &'a str,
            qtype: u16,
            _recursion_desired: bool,
        ) -> BoxFuture<'a, Result<DnsAnswer, String>> {
            self.calls
                .lock()
                .unwrap()
                .push((server.to_string(), name.to_owned()));
            // These tests script TXT answers only; discovery queries answer
            // empty so `authoritative_resolvers` falls back, which is the path
            // under test here.
            if qtype != QTYPE_TXT {
                return Box::pin(async move {
                    Ok(DnsAnswer {
                        rcode: 0,
                        records: Vec::new(),
                    })
                });
            }
            let owner = normalize_name(name);
            let answer = self
                .answers
                .get(&server.to_string())
                .cloned()
                .unwrap_or_else(|| {
                    Ok(TxtAnswer {
                        values: Vec::new(),
                        rcode: 3,
                    })
                })
                .map(|txt| DnsAnswer {
                    rcode: txt.rcode,
                    records: txt
                        .values
                        .into_iter()
                        .map(|value| ResourceRecord {
                            name: owner.clone(),
                            rtype: QTYPE_TXT,
                            rdata: Rdata::Txt(value),
                        })
                        .collect(),
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
        let lookup =
            ScriptedLookup::new(vec![(resolver(53), visible()), (resolver(5353), visible())]);
        let records = vec![
            TxtRecord::new("myapp.com", "value-apex"),
            TxtRecord::new("myapp.com", "value-wildcard"),
        ];
        wait_for_propagation(
            &records,
            &ProbeTargets::flat(&[resolver(53), resolver(5353)]),
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
            &ProbeTargets::flat(&[resolver(5353)]),
            Duration::from_millis(60),
            Duration::from_millis(10),
            lookup.as_ref(),
        )
        .await
        .expect_err("an unpublished record must time out");
        assert!(err.contains("_acme-challenge.myapp.com"), "got: {err}");
        assert!(err.contains("value-apex"), "got: {err}");
        assert!(err.contains("127.0.0.1:5353"), "got: {err}");
        assert!(err.contains("does not exist there yet"), "got: {err}");
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
            &ProbeTargets::flat(&[resolver(53), resolver(5353)]),
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
            &ProbeTargets::flat(&[resolver(53)]),
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
                &ProbeTargets::default(),
                Duration::from_secs(1),
                Duration::from_millis(1),
                lookup.as_ref()
            )
            .await
            .is_ok()
        );
        let err = wait_for_propagation(
            &[TxtRecord::new("myapp.com", "v")],
            &ProbeTargets::default(),
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

        let lookup = UdpDnsLookup::new(Duration::from_secs(5));
        let answer = lookup
            .query(addr, "_acme-challenge.myapp.com", QTYPE_TXT, false)
            .await
            .expect("the fake resolver answers");
        assert_eq!(
            answer.txt_values("_acme-challenge.myapp.com"),
            vec!["propagated-value"]
        );
    }

    #[tokio::test]
    async fn udp_lookup_times_out_against_a_silent_resolver() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind silent resolver");
        let addr = server.local_addr().expect("local addr");
        let lookup = UdpDnsLookup::new(Duration::from_millis(50));
        let err = lookup
            .query(addr, "_acme-challenge.myapp.com", QTYPE_TXT, false)
            .await
            .expect_err("a silent resolver must time out, not hang");
        assert!(err.contains("did not answer"), "got: {err}");
    }
}
