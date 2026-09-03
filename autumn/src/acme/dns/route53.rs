//! The AWS Route 53 DNS-01 provider (issue #1620).
//!
//! Route 53's `ChangeResourceRecordSets` replaces a whole resource record set
//! rather than adding one value to it, and an apex + wildcard order publishes
//! **two** different TXT values at `_acme-challenge.<domain>`. So every write is
//! a read-modify-write: list the current TXT values at the name, union (or
//! subtract) one value, and `UPSERT` the full set back — or `DELETE` the set
//! when the last value goes.
//!
//! # Why not `aws-sdk-route53`
//!
//! The SDK's default HTTPS client pulls `aws-lc-rs` into the graph, and the
//! workspace pins a single `ring` crypto backend everywhere (see the [`acme`]
//! module docs). Route 53 here is three signed requests against a documented
//! XML API, so it is spelled out directly: `SigV4` over the `hmac`/`sha2` crates
//! already in the graph, pinned in tests to AWS's own published signing test
//! vector.
//!
//! [`acme`]: crate::acme

use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;
use hmac::{Hmac, Mac as _};
use sha2::{Digest as _, Sha256};

use super::http::{HttpRequest, HttpResponse, HttpTransport};
use super::{DnsProvider, SecretString, TxtRecord, sanitize_upstream};

/// Route 53's API host. Route 53 is a global service with a single endpoint.
const API_HOST: &str = "route53.amazonaws.com";
/// The Route 53 API version prefix.
const API_PREFIX: &str = "/2013-04-01";
/// `SigV4` service name.
const SERVICE: &str = "route53";
/// TTL for the ephemeral challenge record set.
const CHALLENGE_TTL_SECS: u32 = 60;

/// The AWS credentials and zone hints Route 53 issuance needs.
#[derive(Clone)]
#[non_exhaustive]
pub struct Route53Credentials {
    /// AWS access key id.
    pub access_key_id: String,
    /// AWS secret access key.
    pub secret_access_key: SecretString,
    /// Session token, for temporary (STS/role) credentials.
    pub session_token: Option<SecretString>,
    /// Region used for `SigV4` signing. Route 53 signs against `us-east-1` in the
    /// commercial partition.
    pub region: String,
    /// An explicit hosted zone id, skipping the `ListHostedZonesByName` lookup.
    pub hosted_zone_id: Option<String>,
}

impl std::fmt::Debug for Route53Credentials {
    /// Redacted wholesale, like [`DnsCredential`](super::DnsCredential): the
    /// access key id and hosted zone id are not secrets, but they identify the
    /// account, and a `{:?}` of a config struct should never become a partial
    /// credential dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Route53Credentials(<redacted>)")
    }
}

/// The Route 53 [`DnsProvider`].
pub struct Route53Provider {
    credentials: Route53Credentials,
    transport: Arc<dyn HttpTransport>,
    /// Resolved `zone name → hosted zone id`, so publishing two records for one
    /// zone does not repeat the lookup.
    zone_ids: RwLock<std::collections::HashMap<String, String>>,
    /// Injectable clock, so the `SigV4` timestamp is deterministic in tests.
    now: fn() -> std::time::SystemTime,
}

impl Route53Provider {
    /// Build a provider signing with `credentials`.
    #[must_use]
    pub fn new(credentials: Route53Credentials, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            credentials,
            transport,
            zone_ids: RwLock::new(std::collections::HashMap::new()),
            now: std::time::SystemTime::now,
        }
    }

    async fn send(&self, request: HttpRequest, what: &str) -> Result<String, String> {
        let signed = sign_request(request, &self.credentials, &amz_date((self.now)()))?;
        let url = signed.url.clone();
        let response = self.transport.send(signed).await?;
        check_response(&response, what, &url, &self.credentials.live_secrets())
    }

    /// The hosted zone id owning `fqdn`.
    async fn zone_id(&self, fqdn: &str) -> Result<String, String> {
        if let Some(explicit) = self
            .credentials
            .hosted_zone_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            let id = strip_zone_prefix(explicit);
            // The id goes straight into the request PATH, so it is held to Route
            // 53's own charset rather than trusted: a stray `/`, `?` or `..`
            // from a mistyped credential would otherwise reshape the request.
            if !is_hosted_zone_id(id) {
                return Err(format!(
                    "the configured Route 53 `hosted_zone_id` is not a hosted zone id: expected \
                     something like `Z0123456789ABCDEFGHIJ` (letters and digits), got `{id}`"
                ));
            }
            return Ok(id.to_owned());
        }
        let candidates = zone_candidates(fqdn);
        for candidate in &candidates {
            if let Some(cached) = self
                .zone_ids
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(candidate)
            {
                return Ok(cached.clone());
            }
        }
        for candidate in &candidates {
            let body = self
                .send(
                    HttpRequest::new(
                        "GET",
                        format!(
                            // More than one, because split-horizon DNS gives a
                            // name BOTH a private and a public hosted zone;
                            // `maxitems=1` would hand back whichever sorts first
                            // and hide the other.
                            "https://{API_HOST}{API_PREFIX}/hostedzonesbyname?dnsname={candidate}.&maxitems=10"
                        ),
                    ),
                    "look up the Route 53 hosted zone",
                )
                .await?;
            if let Some(id) = hosted_zone_id_for(&body, candidate) {
                self.zone_ids
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(candidate.clone(), id.clone());
                return Ok(id);
            }
        }
        Err(format!(
            "no Route 53 hosted zone found for {fqdn}: the credentials see no public hosted zone \
             matching any of {}. Check the credentials' account, or set `hosted_zone_id` in the \
             DNS credential to name the zone explicitly",
            candidates.join(", ")
        ))
    }

    /// The TXT values currently published at `fqdn`.
    async fn current_values(&self, zone: &str, fqdn: &str) -> Result<Vec<String>, String> {
        let body = self
            .send(
                HttpRequest::new(
                    "GET",
                    format!(
                        "https://{API_HOST}{API_PREFIX}/hostedzone/{zone}/rrset?maxitems=1&name={fqdn}.&type=TXT"
                    ),
                ),
                "list the Route 53 TXT record set",
            )
            .await?;
        Ok(txt_values_for(&body, fqdn))
    }

    /// Apply `values` at `fqdn`: `UPSERT` the set, or `DELETE` it when empty.
    async fn write_values(
        &self,
        zone: &str,
        fqdn: &str,
        values: &[String],
        previous: &[String],
    ) -> Result<(), String> {
        let (action, applied) = if values.is_empty() {
            // Route 53 rejects an empty ResourceRecords list, so removing the
            // last value means deleting the whole set — and a DELETE must carry
            // the set exactly as it stands today.
            ("DELETE", previous)
        } else {
            ("UPSERT", values)
        };
        if applied.is_empty() {
            // Nothing was there and nothing is wanted: a no-op, not an error.
            return Ok(());
        }
        let body = change_batch_xml(action, fqdn, applied);
        self.send(
            HttpRequest::new(
                "POST",
                format!("https://{API_HOST}{API_PREFIX}/hostedzone/{zone}/rrset/"),
            )
            .header("content-type", "application/xml")
            .body(body),
            "change the Route 53 TXT record set",
        )
        .await
        .map(|_| ())
    }
}

impl DnsProvider for Route53Provider {
    fn name(&self) -> &'static str {
        "route53"
    }

    fn upsert_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let zone = self.zone_id(&record.fqdn).await?;
            let current = self.current_values(&zone, &record.fqdn).await?;
            if current.contains(&record.value) {
                return Ok(());
            }
            let mut next = current.clone();
            next.push(record.value.clone());
            self.write_values(&zone, &record.fqdn, &next, &current)
                .await
        })
    }

    fn delete_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let zone = self.zone_id(&record.fqdn).await?;
            let current = self.current_values(&zone, &record.fqdn).await?;
            if !current.contains(&record.value) {
                return Ok(());
            }
            let next: Vec<String> = current
                .iter()
                .filter(|v| **v != record.value)
                .cloned()
                .collect();
            self.write_values(&zone, &record.fqdn, &next, &current)
                .await
        })
    }
}

impl Route53Credentials {
    /// The credential values that must never appear in an error message copied
    /// from an AWS response body.
    fn live_secrets(&self) -> Vec<&str> {
        let mut secrets = vec![self.secret_access_key.expose()];
        if let Some(token) = &self.session_token {
            secrets.push(token.expose());
        }
        secrets
    }
}

// ── Route 53 XML ─────────────────────────────────────────────────────────────

/// Build a `ChangeResourceRecordSets` body applying `action` to `fqdn` with
/// exactly `values`.
fn change_batch_xml(action: &str, fqdn: &str, values: &[String]) -> String {
    let mut records = String::new();
    for value in values {
        use std::fmt::Write as _;
        let _ = write!(
            records,
            "<ResourceRecord><Value>&quot;{}&quot;</Value></ResourceRecord>",
            xml_escape(value)
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ChangeResourceRecordSetsRequest \
         xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\">\
         <ChangeBatch><Comment>autumn ACME DNS-01 challenge</Comment><Changes><Change>\
         <Action>{action}</Action>\
         <ResourceRecordSet><Name>{}.</Name><Type>TXT</Type><TTL>{CHALLENGE_TTL_SECS}</TTL>\
         <ResourceRecords>{records}</ResourceRecords></ResourceRecordSet>\
         </Change></Changes></ChangeBatch></ChangeResourceRecordSetsRequest>",
        xml_escape(fqdn)
    )
}

/// Escape the five XML predefined entities.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Decode the five XML predefined entities.
fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The inner text of every `<tag>…</tag>` in `xml`.
///
/// Deliberately a scanner rather than a parser: the two Route 53 responses read
/// here are flat, machine-generated, and namespace-free at the element level.
fn xml_elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

/// The inner text of the first `<tag>…</tag>` in `xml`.
fn xml_element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    xml_elements(xml, tag).into_iter().next()
}

/// Strip Route 53's `/hostedzone/` id prefix.
fn strip_zone_prefix(id: &str) -> &str {
    id.trim().trim_start_matches("/hostedzone/")
}

/// Whether `id` has the shape of a Route 53 hosted zone id.
fn is_hosted_zone_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The **public** hosted zone id in a `ListHostedZonesByName` response whose
/// `Name` is exactly `candidate`.
///
/// Two filters, each guarding a real failure:
///
/// - **Exact name.** Route 53 answers with zones lexicographically at or after
///   `dnsname`, so without a name check a lookup for `myapp.com` in an account
///   that does not host it would silently return the *next* zone.
/// - **Not private.** See [`is_private_zone`].
fn hosted_zone_id_for(xml: &str, candidate: &str) -> Option<String> {
    let wanted = format!("{}.", candidate.trim_end_matches('.').to_ascii_lowercase());
    xml_elements(xml, "HostedZone")
        .into_iter()
        .find_map(|zone| {
            let name = xml_element(zone, "Name")?.to_ascii_lowercase();
            if name != wanted || is_private_zone(zone) {
                return None;
            }
            Some(strip_zone_prefix(xml_element(zone, "Id")?).to_owned())
        })
}

/// Whether a `<HostedZone>` element is a private (VPC-only) zone.
///
/// Split-horizon DNS — one private hosted zone for the VPC and one public zone
/// for the internet, same name — is a mainstream AWS shape. Writing the
/// challenge into the private zone makes it invisible to the CA, so every order
/// times out on the propagation wait while dead records pile up in the internal
/// zone.
fn is_private_zone(zone: &str) -> bool {
    xml_element(zone, "PrivateZone").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// The TXT values of the `ResourceRecordSet` for `fqdn` in a
/// `ListResourceRecordSets` response.
///
/// Route 53 stores a TXT value quoted; the quotes are stripped so the caller
/// compares against the raw challenge value.
fn txt_values_for(xml: &str, fqdn: &str) -> Vec<String> {
    let wanted = format!("{}.", fqdn.trim_end_matches('.').to_ascii_lowercase());
    xml_elements(xml, "ResourceRecordSet")
        .into_iter()
        .find(|set| {
            xml_element(set, "Name").is_some_and(|n| {
                // Route 53 escapes a leading underscore label as-is but octal-
                // escapes some characters; `_acme-challenge` needs no decoding.
                n.to_ascii_lowercase() == wanted
            }) && xml_element(set, "Type") == Some("TXT")
        })
        .map(|set| {
            xml_elements(set, "Value")
                .into_iter()
                .map(|v| xml_unescape(v).trim_matches('"').to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Zone-name candidates for `fqdn`, most specific first (see the Cloudflare
/// provider for the same rule).
fn zone_candidates(fqdn: &str) -> Vec<String> {
    let name = fqdn
        .trim()
        .trim_end_matches('.')
        .trim_start_matches("_acme-challenge.");
    let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
    (0..labels.len().saturating_sub(1))
        .map(|start| labels[start..].join("."))
        .collect()
}

/// Turn a Route 53 response into its body or an operator-facing error.
///
/// `secrets` are the credential values still live in this process; see
/// [`sanitize_upstream`] for why they have to be scrubbed out of AWS's own error
/// text before it is published.
fn check_response(
    response: &HttpResponse,
    what: &str,
    url: &str,
    secrets: &[&str],
) -> Result<String, String> {
    if response.is_success() {
        return Ok(response.body.clone());
    }
    // AWS answers a signature mismatch by echoing the canonical request it
    // expected — which carries every signed header, `x-amz-security-token`
    // included. This message reaches the unauthenticated `/actuator/health` and
    // the operator's alert destination, so it is scrubbed and bounded first.
    let detail = xml_element(&response.body, "Message").map_or_else(
        || "no error detail returned".to_owned(),
        |message| sanitize_upstream(&xml_unescape(message), secrets),
    );
    let redacted_url = url.split('?').next().unwrap_or(url);
    Err(format!(
        "could not {what} (HTTP {} from {redacted_url}): {detail}",
        response.status
    ))
}

// ── AWS Signature Version 4 ──────────────────────────────────────────────────

/// A `SigV4` timestamp (`YYYYMMDDTHHMMSSZ`) for `at`.
fn amz_date(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_amz_date(secs)
}

/// Format a UNIX timestamp as `YYYYMMDDTHHMMSSZ` (pure; the seam the tests pin).
fn format_amz_date(unix_secs: u64) -> String {
    // Days since the epoch → civil date, via Howard Hinnant's `civil_from_days`.
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(0);
    let rem = unix_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode per `SigV4` rules. `encode_slash = false` keeps `/` literal, as
/// the canonical URI requires.
fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The `(path, canonical_query_string)` of `url`.
fn split_url(url: &str) -> (String, String) {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path_and_query = without_scheme
        .find('/')
        .map_or("/", |index| &without_scheme[index..]);
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(p, q)| (p, q));
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (uri_encode(k, true), uri_encode(v, true))
        })
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    (uri_encode(path, false), canonical_query)
}

/// Sign `request` with `SigV4`, returning it with the `authorization`,
/// `x-amz-date`, `host` (and, when present, `x-amz-security-token`) headers set.
///
/// `timestamp` is the `YYYYMMDDTHHMMSSZ` signing time, injected so the signature
/// is deterministic in tests.
///
/// # Errors
///
/// Returns a message when the URL has no host.
fn sign_request(
    request: HttpRequest,
    credentials: &Route53Credentials,
    timestamp: &str,
) -> Result<HttpRequest, String> {
    sign_request_for_service(request, credentials, timestamp, SERVICE)
}

/// As [`sign_request`], with the `SigV4` service name injected.
///
/// The service is a parameter for exactly one reason: AWS's published `SigV4` test
/// suite signs for the service literally named `service`, and a hand-rolled
/// signer is only worth trusting if it reproduces the vendor's own vector
/// end-to-end. Production always passes [`SERVICE`].
fn sign_request_for_service(
    mut request: HttpRequest,
    credentials: &Route53Credentials,
    timestamp: &str,
    service: &str,
) -> Result<HttpRequest, String> {
    let host = request
        .url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("Route 53 request URL has no host: {}", request.url))?
        .to_owned();
    let date = timestamp
        .get(..8)
        .ok_or_else(|| "malformed SigV4 timestamp".to_owned())?
        .to_owned();

    request.headers.insert("host".to_owned(), host);
    request
        .headers
        .insert("x-amz-date".to_owned(), timestamp.to_owned());
    if let Some(token) = &credentials.session_token {
        request
            .headers
            .insert("x-amz-security-token".to_owned(), token.expose().to_owned());
    }

    let (canonical_uri, canonical_query) = split_url(&request.url);
    // `BTreeMap` already holds the headers sorted by name, which is what the
    // canonical request and `SignedHeaders` both require.
    // SigV4 sorts by the LOWERCASED header name, and `SignedHeaders` must list
    // them in exactly the order `canonical_headers` renders them. Lowercasing on
    // the way into the map makes the `BTreeMap`'s own ordering canonical, so a
    // future `.header("Content-Type", …)` cannot silently desynchronise the two.
    request.headers = request
        .headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value))
        .collect();
    let mut canonical_headers = String::new();
    for (name, value) in &request.headers {
        use std::fmt::Write as _;
        let _ = writeln!(canonical_headers, "{name}:{}", value.trim());
    }
    let signed_headers = request
        .headers
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(";");
    let payload_hash = sha256_hex(request.body.as_bytes());

    let canonical_request = format!(
        "{}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        request.method
    );
    let scope = format!(
        "{date}/{}/{service}/aws4_request",
        credentials.region.trim()
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let key_date = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key.expose()).as_bytes(),
        date.as_bytes(),
    );
    let key_region = hmac_sha256(&key_date, credentials.region.trim().as_bytes());
    let key_service = hmac_sha256(&key_region, service.as_bytes());
    let key_signing = hmac_sha256(&key_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&key_signing, string_to_sign.as_bytes()));

    request.headers.insert(
        "authorization".to_owned(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, \
             Signature={signature}",
            credentials.access_key_id.trim()
        ),
    );
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::super::http::RecordingTransport;
    use super::*;

    const ZONE_LOOKUP: &str = "GET /2013-04-01/hostedzonesbyname";
    const RRSET_LIST: &str = "GET /2013-04-01/hostedzone/ZPUBLIC/rrset";
    const RRSET_CHANGE: &str = "POST /2013-04-01/hostedzone/ZPUBLIC/rrset/";

    fn zone_hit() -> &'static str {
        r"<ListHostedZonesByNameResponse><HostedZones>
          <HostedZone><Id>/hostedzone/ZPUBLIC</Id><Name>myapp.com.</Name>
            <Config><PrivateZone>false</PrivateZone></Config></HostedZone>
          </HostedZones></ListHostedZonesByNameResponse>"
    }

    fn rrset_with(values: &[&str]) -> String {
        let mut records = String::new();
        for value in values {
            use std::fmt::Write as _;
            let _ = write!(
                records,
                "<ResourceRecord><Value>&quot;{value}&quot;</Value></ResourceRecord>"
            );
        }
        format!(
            "<ListResourceRecordSetsResponse><ResourceRecordSets><ResourceRecordSet>\
             <Name>_acme-challenge.myapp.com.</Name><Type>TXT</Type><TTL>60</TTL>\
             <ResourceRecords>{records}</ResourceRecords></ResourceRecordSet>\
             </ResourceRecordSets></ListResourceRecordSetsResponse>"
        )
    }

    fn empty_rrset() -> &'static str {
        "<ListResourceRecordSetsResponse><ResourceRecordSets></ResourceRecordSets>\
         </ListResourceRecordSetsResponse>"
    }

    fn changed() -> &'static str {
        "<ChangeResourceRecordSetsResponse><ChangeInfo><Status>PENDING</Status></ChangeInfo>\
         </ChangeResourceRecordSetsResponse>"
    }

    fn r53(transport: std::sync::Arc<RecordingTransport>) -> Route53Provider {
        Route53Provider::new(
            credentials(),
            transport as std::sync::Arc<dyn HttpTransport>,
        )
    }

    /// Route 53 replaces a whole record set rather than adding to it, so the
    /// apex+wildcard case turns on read-modify-write: the second publish must
    /// send BOTH values, or the first one is silently dropped and its
    /// authorization can never validate.
    #[tokio::test]
    async fn publishing_a_second_value_upserts_the_whole_set() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            (RRSET_LIST, empty_rrset()),
            (RRSET_CHANGE, changed()),
        ])
        .then(RRSET_LIST, 200, &rrset_with(&["value-apex"]))
        .then(RRSET_CHANGE, 200, changed());
        let provider = r53(std::sync::Arc::clone(&transport));

        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("apex publishes");
        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-wildcard"))
            .await
            .expect("wildcard publishes");

        let changes: Vec<String> = transport
            .sent()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| r.body.clone())
            .collect();
        assert_eq!(changes.len(), 2, "one change per publish");
        assert!(changes[0].contains("<Action>UPSERT</Action>"));
        assert_eq!(changes[0].matches("<ResourceRecord>").count(), 1);
        // The second change must carry BOTH values.
        assert!(
            changes[1].contains("&quot;value-apex&quot;"),
            "{}",
            changes[1]
        );
        assert!(
            changes[1].contains("&quot;value-wildcard&quot;"),
            "the existing value must be carried through the read-modify-write: {}",
            changes[1]
        );
        assert_eq!(changes[1].matches("<ResourceRecord>").count(), 2);
    }

    /// Removing one of two values UPSERTs the remainder; removing the last one
    /// DELETEs the set, carrying it exactly as it stands (Route 53 requires it).
    #[tokio::test]
    async fn removing_values_upserts_the_remainder_then_deletes_the_set() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            (RRSET_LIST, &rrset_with(&["value-apex", "value-wildcard"])),
            (RRSET_CHANGE, changed()),
        ])
        .then(RRSET_LIST, 200, &rrset_with(&["value-wildcard"]))
        .then(RRSET_CHANGE, 200, changed());
        let provider = r53(std::sync::Arc::clone(&transport));

        provider
            .delete_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("first removal");
        provider
            .delete_txt(&TxtRecord::new("myapp.com", "value-wildcard"))
            .await
            .expect("second removal");

        let changes: Vec<String> = transport
            .sent()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| r.body.clone())
            .collect();
        assert_eq!(changes.len(), 2);
        // The sibling survives the first removal.
        assert!(
            changes[0].contains("<Action>UPSERT</Action>"),
            "{}",
            changes[0]
        );
        assert!(!changes[0].contains("value-apex"), "{}", changes[0]);
        assert!(
            changes[0].contains("&quot;value-wildcard&quot;"),
            "{}",
            changes[0]
        );
        // The last removal deletes the set, carrying it as it stands.
        assert!(
            changes[1].contains("<Action>DELETE</Action>"),
            "{}",
            changes[1]
        );
        assert!(
            changes[1].contains("&quot;value-wildcard&quot;"),
            "{}",
            changes[1]
        );
    }

    /// Removing a value that is not there changes nothing — a cleanup that runs
    /// twice (a retry, a crash between the write and the delete) must not send a
    /// DELETE for a set it did not read.
    #[tokio::test]
    async fn removing_an_absent_value_is_a_no_op() {
        let transport =
            RecordingTransport::new(&[(ZONE_LOOKUP, zone_hit()), (RRSET_LIST, empty_rrset())]);
        let provider = r53(std::sync::Arc::clone(&transport));
        provider
            .delete_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("no-op");
        assert!(
            !transport.calls().iter().any(|c| c == RRSET_CHANGE),
            "got: {:?}",
            transport.calls()
        );
    }

    /// Every request is `SigV4`-signed, and the zone is looked up once and cached.
    #[tokio::test]
    async fn every_request_is_signed_and_the_zone_is_cached() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            (RRSET_LIST, empty_rrset()),
            (RRSET_CHANGE, changed()),
        ])
        .then(RRSET_LIST, 200, &rrset_with(&["value-apex"]))
        .then(RRSET_CHANGE, 200, changed());
        let provider = r53(std::sync::Arc::clone(&transport));
        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("publishes");
        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-wildcard"))
            .await
            .expect("publishes");

        for request in transport.sent() {
            let auth = request
                .headers
                .get("authorization")
                .expect("every Route 53 call must be signed");
            assert!(auth.starts_with("AWS4-HMAC-SHA256 "), "got: {auth}");
            assert!(auth.contains("Signature="), "got: {auth}");
            assert!(request.headers.contains_key("x-amz-date"));
            assert!(
                !request.url.contains("wJalrXUtnFEMI"),
                "the secret key must never reach a URL"
            );
        }
        assert_eq!(
            transport
                .calls()
                .iter()
                .filter(|c| *c == ZONE_LOOKUP)
                .count(),
            1,
            "the hosted zone id must be cached across records"
        );
    }

    /// An explicit `hosted_zone_id` skips the lookup entirely.
    #[tokio::test]
    async fn an_explicit_hosted_zone_id_skips_the_lookup() {
        let transport =
            RecordingTransport::new(&[(RRSET_LIST, empty_rrset()), (RRSET_CHANGE, changed())]);
        let mut creds = credentials();
        creds.hosted_zone_id = Some("/hostedzone/ZPUBLIC".to_owned());
        let provider = Route53Provider::new(
            creds,
            std::sync::Arc::clone(&transport) as std::sync::Arc<dyn HttpTransport>,
        );
        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "v"))
            .await
            .expect("publishes");
        assert!(
            !transport.calls().iter().any(|c| c == ZONE_LOOKUP),
            "got: {:?}",
            transport.calls()
        );
    }

    fn credentials() -> Route53Credentials {
        Route53Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: SecretString::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            session_token: None,
            region: "us-east-1".to_owned(),
            hosted_zone_id: None,
        }
    }

    /// AWS's published `get-vanilla` `SigV4` test-suite case, asserted against
    /// the **signature** rather than against a string the test rebuilt itself.
    ///
    /// Hand-rolled signing is only trustworthy if it reproduces the vendor's own
    /// vector end to end, because every way of getting canonicalisation wrong
    /// still "looks right": swapping the region and service HMAC stages, dropping
    /// the payload hash from the canonical request, or using CRLF in the
    /// string-to-sign all produce a plausible-looking signature that AWS rejects
    /// with `SignatureDoesNotMatch` on every single request.
    #[test]
    fn sigv4_reproduces_the_aws_get_vanilla_signature() {
        let signed = sign_request_for_service(
            HttpRequest::new("GET", "https://example.amazonaws.com/"),
            &credentials(),
            "20150830T123600Z",
            // The suite signs for a service literally named `service`.
            "service",
        )
        .expect("signs");

        assert_eq!(
            signed.headers["authorization"],
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            "the signature must match AWS's published get-vanilla vector"
        );
        assert_eq!(signed.headers["host"], "example.amazonaws.com");
        assert_eq!(signed.headers["x-amz-date"], "20150830T123600Z");
    }

    /// The same vector with a body, so the payload hash is pinned too: a signer
    /// that dropped it from the canonical request would still pass the
    /// empty-body case above.
    #[test]
    fn sigv4_covers_the_request_body() {
        let with_body = sign_request_for_service(
            HttpRequest::new("POST", "https://example.amazonaws.com/").body("<x/>"),
            &credentials(),
            "20150830T123600Z",
            "service",
        )
        .expect("signs");
        let without_body = sign_request_for_service(
            HttpRequest::new("POST", "https://example.amazonaws.com/"),
            &credentials(),
            "20150830T123600Z",
            "service",
        )
        .expect("signs");
        assert_ne!(
            with_body.headers["authorization"], without_body.headers["authorization"],
            "the body must be covered by the signature"
        );
    }

    /// The query string is part of the canonical request, so two requests that
    /// differ only there must sign differently — and the same query in a
    /// different order must sign identically (`SigV4` sorts it).
    #[test]
    fn sigv4_covers_the_query_string_and_is_order_independent() {
        let sign = |url: &str| {
            sign_request_for_service(
                HttpRequest::new("GET", url),
                &credentials(),
                "20150830T123600Z",
                "service",
            )
            .expect("signs")
            .headers["authorization"]
                .clone()
        };
        let a = sign("https://example.amazonaws.com/?name=one&type=TXT");
        let b = sign("https://example.amazonaws.com/?type=TXT&name=one");
        let c = sign("https://example.amazonaws.com/?name=two&type=TXT");
        assert_eq!(a, b, "canonical query order must not depend on input order");
        assert_ne!(a, c, "the query must be covered by the signature");
    }

    /// `SignedHeaders` must list the headers in the same order
    /// `canonical_headers` renders them, whatever case they were inserted with.
    #[test]
    fn header_names_are_canonicalised_regardless_of_input_case() {
        let mixed = sign_request_for_service(
            HttpRequest::new("POST", "https://example.amazonaws.com/")
                .header("Content-Type", "application/xml")
                .body("<x/>"),
            &credentials(),
            "20150830T123600Z",
            "service",
        )
        .expect("signs");
        let lower = sign_request_for_service(
            HttpRequest::new("POST", "https://example.amazonaws.com/")
                .header("content-type", "application/xml")
                .body("<x/>"),
            &credentials(),
            "20150830T123600Z",
            "service",
        )
        .expect("signs");
        assert_eq!(
            mixed.headers["authorization"], lower.headers["authorization"],
            "header case must not change the signature"
        );
        assert!(
            mixed.headers["authorization"].contains("SignedHeaders=content-type;host;x-amz-date"),
            "SignedHeaders must be lowercase and sorted: {}",
            mixed.headers["authorization"]
        );
    }

    /// The empty-payload hash AWS documents, pinned so a future body-handling
    /// change cannot silently sign the wrong payload.
    #[test]
    fn empty_payload_hash_is_the_documented_constant() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn signing_never_puts_the_secret_in_the_request() {
        let signed = sign_request(
            HttpRequest::new(
                "POST",
                "https://route53.amazonaws.com/2013-04-01/hostedzone/Z1/rrset/",
            )
            .body("<x/>"),
            &credentials(),
            "20150830T123600Z",
        )
        .expect("signs");
        let rendered = format!("{signed:?}");
        assert!(
            !rendered.contains("wJalrXUtnFEMI"),
            "the secret key must never reach the wire or a Debug render: {rendered}"
        );
        // …while the key *id* (not a secret) is in the credential scope.
        assert!(signed.headers["authorization"].contains("AKIDEXAMPLE"));
    }

    #[test]
    fn a_session_token_is_signed_and_sent() {
        let mut creds = credentials();
        creds.session_token = Some(SecretString::new("session-token-value"));
        let signed = sign_request(
            HttpRequest::new(
                "GET",
                "https://route53.amazonaws.com/2013-04-01/hostedzonesbyname",
            ),
            &creds,
            "20150830T123600Z",
        )
        .expect("signs");
        assert_eq!(
            signed.headers["x-amz-security-token"],
            "session-token-value"
        );
        assert!(
            signed.headers["authorization"].contains("x-amz-security-token"),
            "the token header must be part of SignedHeaders or AWS rejects the request"
        );
    }

    #[test]
    fn canonical_query_is_sorted_and_encoded() {
        let (path, query) = split_url(
            "https://route53.amazonaws.com/2013-04-01/hostedzone/Z1/rrset?type=TXT&name=_acme-challenge.myapp.com.&maxitems=1",
        );
        assert_eq!(path, "/2013-04-01/hostedzone/Z1/rrset");
        assert_eq!(
            query, "maxitems=1&name=_acme-challenge.myapp.com.&type=TXT",
            "query parameters must be sorted by encoded key"
        );
    }

    #[test]
    fn format_amz_date_is_a_correct_civil_date() {
        // 2015-08-30T12:36:00Z — the AWS test-suite timestamp.
        assert_eq!(format_amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(format_amz_date(0), "19700101T000000Z");
        // A leap day.
        assert_eq!(format_amz_date(1_709_208_000), "20240229T120000Z");
    }

    // AC/R3: an apex + wildcard order publishes TWO values at one name. The
    // record set written back must carry both.
    #[test]
    fn change_batch_carries_every_value_in_the_set() {
        let xml = change_batch_xml(
            "UPSERT",
            "_acme-challenge.myapp.com",
            &["value-one".to_owned(), "value-two".to_owned()],
        );
        assert!(xml.contains("<Action>UPSERT</Action>"), "{xml}");
        assert!(
            xml.contains("<Name>_acme-challenge.myapp.com.</Name>"),
            "{xml}"
        );
        assert!(xml.contains("&quot;value-one&quot;"), "{xml}");
        assert!(xml.contains("&quot;value-two&quot;"), "{xml}");
        assert_eq!(xml.matches("<ResourceRecord>").count(), 2, "{xml}");
    }

    #[test]
    fn hosted_zone_lookup_requires_an_exact_name_match() {
        let xml = r"<ListHostedZonesByNameResponse><HostedZones>
            <HostedZone><Id>/hostedzone/Z0001</Id><Name>myapp.com.</Name></HostedZone>
            </HostedZones></ListHostedZonesByNameResponse>";
        assert_eq!(
            hosted_zone_id_for(xml, "myapp.com").as_deref(),
            Some("Z0001")
        );

        // Route 53 answers with the NEXT zone lexicographically when the asked-for
        // one does not exist; returning it would write the challenge into a
        // stranger's zone.
        let other = r"<ListHostedZonesByNameResponse><HostedZones>
            <HostedZone><Id>/hostedzone/Z9999</Id><Name>notmyapp.com.</Name></HostedZone>
            </HostedZones></ListHostedZonesByNameResponse>";
        assert_eq!(hosted_zone_id_for(other, "myapp.com"), None);
    }

    #[test]
    fn txt_values_are_read_unquoted_from_the_matching_record_set() {
        let xml = r"<ListResourceRecordSetsResponse><ResourceRecordSets>
            <ResourceRecordSet><Name>_acme-challenge.myapp.com.</Name><Type>TXT</Type><TTL>60</TTL>
              <ResourceRecords>
                <ResourceRecord><Value>&quot;value-one&quot;</Value></ResourceRecord>
                <ResourceRecord><Value>&quot;value-two&quot;</Value></ResourceRecord>
              </ResourceRecords>
            </ResourceRecordSet>
            </ResourceRecordSets></ListResourceRecordSetsResponse>";
        assert_eq!(
            txt_values_for(xml, "_acme-challenge.myapp.com"),
            vec!["value-one".to_owned(), "value-two".to_owned()]
        );
        // A different name in the same response is not this record's set.
        assert!(txt_values_for(xml, "_acme-challenge.other.com").is_empty());
    }

    // Route 53's `rrset?name=` list is a *cursor*: it returns the record sets at
    // or after the requested name, so a name that does not exist yet answers with
    // some other set. Reading that as "the current values" would make the first
    // publish clobber an unrelated record.
    #[test]
    fn a_cursor_result_for_another_name_reads_as_empty() {
        let xml = r"<ListResourceRecordSetsResponse><ResourceRecordSets>
            <ResourceRecordSet><Name>www.myapp.com.</Name><Type>TXT</Type><TTL>300</TTL>
              <ResourceRecords><ResourceRecord><Value>&quot;unrelated&quot;</Value></ResourceRecord></ResourceRecords>
            </ResourceRecordSet>
            </ResourceRecordSets></ListResourceRecordSetsResponse>";
        assert!(txt_values_for(xml, "_acme-challenge.myapp.com").is_empty());
    }

    #[test]
    fn error_responses_carry_the_aws_message_and_drop_the_query() {
        let response = HttpResponse {
            status: 403,
            body: "<ErrorResponse><Error><Message>The security token included in the request is \
                   invalid</Message></Error></ErrorResponse>"
                .to_owned(),
        };
        let err = check_response(
            &response,
            "change the Route 53 TXT record set",
            "https://route53.amazonaws.com/2013-04-01/hostedzone/Z1/rrset?name=secretish",
            &[],
        )
        .expect_err("403 is a failure");
        assert!(err.contains("403"), "got: {err}");
        assert!(err.contains("security token"), "got: {err}");
        assert!(!err.contains("name=secretish"), "got: {err}");
    }

    // AWS's `SignatureDoesNotMatch` reply echoes the canonical request, which
    // contains every signed header — including the STS session token this
    // process is holding. That reply is published on the unauthenticated
    // `/actuator/health` and pushed to Slack/PagerDuty, so it must not carry the
    // credential through.
    #[test]
    fn a_signature_mismatch_reply_cannot_carry_the_session_token_through() {
        let mut creds = credentials();
        creds.session_token = Some(SecretString::new("FwoGZXIvYXdzEHwaDNOT-A-REAL-TOKEN"));
        let response = HttpResponse {
            status: 403,
            body: "<ErrorResponse><Error><Message>The request signature we calculated does not \
                   match the signature you provided. The Canonical String for this request should \
                   have been &apos;POST /2013-04-01/hostedzone/Z1/rrset/ \
                   host:route53.amazonaws.com \
                   x-amz-security-token:FwoGZXIvYXdzEHwaDNOT-A-REAL-TOKEN \
                   &apos;</Message></Error></ErrorResponse>"
                .to_owned(),
        };
        let err = check_response(
            &response,
            "change the Route 53 TXT record set",
            "https://route53.amazonaws.com/2013-04-01/hostedzone/Z1/rrset/",
            &creds.live_secrets(),
        )
        .expect_err("403 is a failure");
        assert!(
            !err.contains("FwoGZXIvYXdzEHwaDNOT-A-REAL-TOKEN"),
            "the session token must never reach an operator-facing message: {err}"
        );
        assert!(err.contains("<redacted>"), "got: {err}");
        // The operator still learns what went wrong.
        assert!(err.contains("signature"), "got: {err}");
        // …and the secret key is scrubbed on the same path.
        assert!(!err.contains("wJalrXUtnFEMI"), "got: {err}");
    }

    #[test]
    fn credentials_debug_is_redacted_wholesale() {
        let rendered = format!("{:?}", credentials());
        assert!(!rendered.contains("AKIDEXAMPLE"), "leaked: {rendered}");
        assert!(!rendered.contains("wJalrXUtnFEMI"), "leaked: {rendered}");
    }

    #[test]
    fn explicit_zone_ids_drop_the_hostedzone_prefix() {
        assert_eq!(strip_zone_prefix("/hostedzone/Z123"), "Z123");
        assert_eq!(strip_zone_prefix("Z123"), "Z123");
    }

    // The zone id goes into the request PATH; a mistyped credential must not be
    // able to reshape the request.
    #[test]
    fn hosted_zone_ids_are_held_to_route53s_charset() {
        assert!(is_hosted_zone_id("Z0123456789ABCDEFGHIJ"));
        for bad in ["", "Z1/rrset", "Z1?x=1", "../..", "Z1 2", &"Z".repeat(33)] {
            assert!(!is_hosted_zone_id(bad), "`{bad}` must be rejected");
        }
    }

    // Split-horizon DNS gives one name both a private and a public hosted zone.
    // Writing the challenge into the private one makes it invisible to the CA,
    // so every order times out with a message blaming NS delegation.
    #[test]
    fn a_private_hosted_zone_is_never_selected() {
        let xml = r"<ListHostedZonesByNameResponse><HostedZones>
            <HostedZone><Id>/hostedzone/ZPRIVATE</Id><Name>myapp.com.</Name>
              <Config><PrivateZone>true</PrivateZone></Config></HostedZone>
            <HostedZone><Id>/hostedzone/ZPUBLIC</Id><Name>myapp.com.</Name>
              <Config><PrivateZone>false</PrivateZone></Config></HostedZone>
            </HostedZones></ListHostedZonesByNameResponse>";
        assert_eq!(
            hosted_zone_id_for(xml, "myapp.com").as_deref(),
            Some("ZPUBLIC"),
            "the public zone must win even when the private one is listed first"
        );

        // With only a private zone, the lookup finds nothing rather than writing
        // into a zone the CA cannot see.
        let private_only = r"<ListHostedZonesByNameResponse><HostedZones>
            <HostedZone><Id>/hostedzone/ZPRIVATE</Id><Name>myapp.com.</Name>
              <Config><PrivateZone>true</PrivateZone></Config></HostedZone>
            </HostedZones></ListHostedZonesByNameResponse>";
        assert_eq!(hosted_zone_id_for(private_only, "myapp.com"), None);
    }
}
