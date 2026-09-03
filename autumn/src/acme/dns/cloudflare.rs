//! The Cloudflare DNS-01 provider (issue #1620).
//!
//! Talks to the Cloudflare v4 REST API with a scoped API token
//! (`Zone:DNS:Edit` on the zone). Three calls per record:
//!
//! 1. resolve the zone id for the record's name, walking the label suffixes from
//!    most to least specific (`_acme-challenge.a.b.myapp.com` → `a.b.myapp.com`
//!    → `b.myapp.com` → `myapp.com`), so a delegated sub-zone wins over its
//!    parent;
//! 2. `POST /zones/{id}/dns_records` to add the TXT value — **add**, never
//!    replace, because an apex + wildcard order publishes two different values
//!    at one name;
//! 3. `GET …?type=TXT&name=…&content=…` then `DELETE /…/{record_id}` to remove
//!    exactly the one pair.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;
use serde_json::Value;

use super::http::{HttpRequest, HttpResponse, HttpTransport};
use super::{DnsProvider, SecretString, TxtRecord, sanitize_upstream};

/// Cloudflare's API base. A constant rather than config: pointing ACME's DNS
/// writes at an arbitrary host is not a deployment shape we support, and the
/// tests inject an [`HttpTransport`] instead.
const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// TTL for the ephemeral challenge record. Cloudflare's minimum for an explicit
/// TTL is 60s; the record lives for seconds to minutes.
const CHALLENGE_TTL_SECS: u32 = 60;

/// The Cloudflare [`DnsProvider`].
pub struct CloudflareProvider {
    api_token: SecretString,
    transport: Arc<dyn HttpTransport>,
    /// Resolved `zone name → zone id`, so a renewal that publishes two records
    /// for one zone (apex + wildcard) does not repeat the lookup.
    zone_ids: RwLock<HashMap<String, String>>,
}

impl CloudflareProvider {
    /// Build a provider authenticating with `api_token`.
    #[must_use]
    pub fn new(api_token: SecretString, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            api_token,
            transport,
            zone_ids: RwLock::new(HashMap::new()),
        }
    }

    fn authorized(&self, request: HttpRequest) -> HttpRequest {
        request
            .header(
                "authorization",
                format!("Bearer {}", self.api_token.expose()),
            )
            .header("accept", "application/json")
    }

    async fn send(&self, request: HttpRequest, what: &str) -> Result<Value, String> {
        let url = request.url.clone();
        let response = self.transport.send(self.authorized(request)).await?;
        parse_api_response(&response, what, &url, &[self.api_token.expose()])
    }

    /// Resolve the Cloudflare zone id that owns `fqdn`.
    ///
    /// The cache is keyed on the whole challenge name, not on the suffix that
    /// turned out to be its zone. Keying it on the suffix looked equivalent and
    /// is not: a scan of every candidate against a suffix-keyed cache returns a
    /// cached PARENT before the more specific child has ever been queried. Once
    /// one order resolved `example.com`, a later name in the separately
    /// delegated `sub.example.com` would resolve to the parent zone, and the
    /// challenge would be written where nothing is authoritative for it — a
    /// record that exists, answers nowhere, and times out every renewal
    /// (issue #1620).
    async fn zone_id(&self, fqdn: &str) -> Result<String, String> {
        if let Some(cached) = self
            .zone_ids
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(fqdn)
        {
            return Ok(cached.clone());
        }
        let candidates = zone_candidates(fqdn);
        for candidate in &candidates {
            let body = self
                .send(
                    HttpRequest::new(
                        "GET",
                        // Encoded, like `list_url`: an unencoded `&` in the
                        // candidate would add a second `name=` parameter and
                        // could steer the lookup at somebody else's zone.
                        format!("{API_BASE}/zones?name={}", urlencode(candidate)),
                    ),
                    "look up the Cloudflare zone",
                )
                .await?;
            if let Some(id) = first_result_id(&body) {
                self.zone_ids
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(fqdn.to_owned(), id.clone());
                return Ok(id);
            }
        }
        Err(format!(
            "no Cloudflare zone found for {fqdn}: the API token's account has no zone matching \
             any of {}. Check that the token is scoped to the right zone and that the domain is \
             hosted on Cloudflare",
            candidates.join(", ")
        ))
    }
}

impl DnsProvider for CloudflareProvider {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn upsert_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let zone = self.zone_id(&record.fqdn).await?;
            // If this exact (name, value) is already present — a retried order,
            // or a leftover from an interrupted run — adding it again is a 400.
            // Treat "already there" as success.
            let listed = self
                .send(
                    HttpRequest::new("GET", list_url(&zone, record)),
                    "list Cloudflare TXT records",
                )
                .await?;
            if !matching_record_ids(&listed, record).is_empty() {
                return Ok(());
            }
            let body = serde_json::json!({
                "type": "TXT",
                "name": record.fqdn,
                "content": record.value,
                "ttl": CHALLENGE_TTL_SECS,
            })
            .to_string();
            self.send(
                HttpRequest::new("POST", format!("{API_BASE}/zones/{zone}/dns_records"))
                    .header("content-type", "application/json")
                    .body(body),
                "create the Cloudflare TXT record",
            )
            .await
            .map(|_| ())
        })
    }

    fn delete_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let zone = self.zone_id(&record.fqdn).await?;
            let listed = self
                .send(
                    HttpRequest::new("GET", list_url(&zone, record)),
                    "list Cloudflare TXT records",
                )
                .await?;
            for id in matching_record_ids(&listed, record) {
                self.send(
                    HttpRequest::new(
                        "DELETE",
                        format!("{API_BASE}/zones/{zone}/dns_records/{id}"),
                    ),
                    "delete the Cloudflare TXT record",
                )
                .await?;
            }
            Ok(())
        })
    }
}

/// The list URL for exactly one `(name, value)` pair — never "everything at this
/// name", so a sibling challenge value is neither read nor removed.
fn list_url(zone: &str, record: &TxtRecord) -> String {
    format!(
        "{API_BASE}/zones/{zone}/dns_records?type=TXT&name={}&content={}",
        urlencode(&record.fqdn),
        urlencode(&record.value)
    )
}

/// Percent-encode a query-string value (RFC 3986 unreserved set kept as-is).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Suffixes of `fqdn` that could be its Cloudflare zone, most specific first.
///
/// The `_acme-challenge` label itself is dropped, and a bare TLD is not offered
/// as a zone candidate.
fn zone_candidates(fqdn: &str) -> Vec<String> {
    // The `_acme-challenge` label is NOT stripped: delegating
    // `_acme-challenge.example.com` to a zone of its own (so an ACME client
    // needs credentials for nothing else) is a supported and recommended
    // setup, and stripping the label would skip that zone and write the
    // record into the parent, where nothing answers for it (issue #1620).
    // It is simply the most specific candidate, tried first.
    let name = fqdn.trim().trim_end_matches('.');
    let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
    // A zone always has at least two labels (`myapp.com`); stopping there also
    // avoids ever asking Cloudflare about `com`.
    (0..labels.len().saturating_sub(1))
        .map(|start| labels[start..].join("."))
        .collect()
}

/// Turn a Cloudflare API response into either its `result` value or an
/// operator-facing error.
///
/// Cloudflare answers `{"success": bool, "errors": [...], "result": ...}`; a
/// non-2xx status and `success: false` both mean failure. The message carries
/// the status and Cloudflare's own error text — never the request headers, so
/// the bearer token cannot ride along.
fn parse_api_response(
    response: &HttpResponse,
    what: &str,
    url: &str,
    secrets: &[&str],
) -> Result<Value, String> {
    let redacted_url = url.split('?').next().unwrap_or(url);
    let parsed: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    if !response.is_success() || parsed.get("success") == Some(&Value::Bool(false)) {
        let detail = parsed
            .get("errors")
            .and_then(Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|d| !d.is_empty())
            .map_or_else(
                || "no error detail returned".to_owned(),
                |detail| sanitize_upstream(&detail, secrets),
            );
        return Err(format!(
            "could not {what} (HTTP {} from {redacted_url}): {detail}",
            response.status
        ));
    }
    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

/// The `id` of the first entry in a Cloudflare list `result`.
fn first_result_id(result: &Value) -> Option<String> {
    result
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

/// The ids of the entries in a Cloudflare list `result` that really are this
/// `(name, value)` pair.
///
/// The request already filters on `type`/`name`/`content`, but the whole
/// apex-plus-wildcard invariant — never read or remove the sibling challenge
/// value — would rest on Cloudflare honouring `content=` as an exact match.
/// Re-checking client-side makes it an invariant this code enforces rather than
/// one it delegates: a filter that ever loosened to substring semantics would
/// otherwise make `upsert_txt` skip the second value (the order then never
/// validates) and `delete_txt` remove both (pulling a record out from under a CA
/// that is still validating it).
fn matching_record_ids(result: &Value, record: &TxtRecord) -> Vec<String> {
    let wanted_name = record.fqdn.to_ascii_lowercase();
    result
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|e| {
                    e.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.trim_end_matches('.').eq_ignore_ascii_case(&wanted_name))
                        && e.get("content").and_then(Value::as_str) == Some(record.value.as_str())
                })
                .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::http::RecordingTransport;
    use super::*;

    const ZONE_LOOKUP: &str = "GET /client/v4/zones";
    const RECORDS: &str = "GET /client/v4/zones/zone123/dns_records";
    const CREATE: &str = "POST /client/v4/zones/zone123/dns_records";

    fn zone_hit() -> &'static str {
        r#"{"success":true,"errors":[],"result":[{"id":"zone123","name":"myapp.com"}]}"#
    }

    fn empty_list() -> &'static str {
        r#"{"success":true,"errors":[],"result":[]}"#
    }

    fn provider(transport: std::sync::Arc<RecordingTransport>) -> CloudflareProvider {
        CloudflareProvider::new(
            SecretString::new("cf-test-token"),
            transport as std::sync::Arc<dyn HttpTransport>,
        )
    }

    /// Regression (#1620): a cached PARENT zone must not shadow a separately
    /// delegated child.
    ///
    /// The cache used to be keyed on the suffix that turned out to be the zone,
    /// and looked up by scanning every candidate. Once any order resolved
    /// `example.com`, a later name in `sub.example.com` — a real zone of its own,
    /// delegated away — matched the cached parent on the second candidate and
    /// returned it without ever asking Cloudflare about the child. The challenge
    /// then went into the parent zone, which is not authoritative for the name:
    /// the record exists, answers nowhere, and every renewal times out.
    #[tokio::test]
    async fn a_cached_parent_zone_does_not_shadow_a_delegated_child() {
        // Two zone lookups, in the order a correct implementation makes them:
        // `example.com` for the first name, then `sub.example.com` for the
        // second — NOT the cached parent.
        let transport = RecordingTransport::new(&[(
            ZONE_LOOKUP,
            r#"{"success":true,"errors":[],"result":[{"id":"zone-parent","name":"example.com"}]}"#,
        )])
        .then(
            ZONE_LOOKUP,
            200,
            r#"{"success":true,"errors":[],"result":[{"id":"zone-child","name":"sub.example.com"}]}"#,
        );
        let provider = provider(std::sync::Arc::clone(&transport));

        // First order populates the cache with the parent zone.
        assert_eq!(
            provider
                .zone_id("_acme-challenge.example.com")
                .await
                .expect("the parent zone resolves"),
            "zone-parent"
        );

        // A later name in the delegated child must resolve to the CHILD zone.
        assert_eq!(
            provider
                .zone_id("_acme-challenge.sub.example.com")
                .await
                .expect("the child zone resolves"),
            "zone-child",
            "a cached parent must not be returned for a name in a delegated child zone"
        );

        // …and it must have actually asked, most-specific candidate first.
        let asked: Vec<String> = transport.sent().iter().map(|r| r.url.clone()).collect();
        assert!(
            asked.iter().any(|u| u.contains("name=sub.example.com")),
            "the more specific candidate must be queried before any cached suffix: {asked:?}"
        );
    }

    /// The cache still does its job: resolving the same name twice asks once.
    #[tokio::test]
    async fn a_repeated_name_is_served_from_the_zone_cache() {
        let transport = RecordingTransport::new(&[(ZONE_LOOKUP, zone_hit())]);
        let provider = provider(std::sync::Arc::clone(&transport));

        for _ in 0..3 {
            assert_eq!(
                provider
                    .zone_id("_acme-challenge.myapp.com")
                    .await
                    .expect("resolves"),
                "zone123"
            );
        }
        assert_eq!(
            transport
                .calls()
                .iter()
                .filter(|c| c.as_str() == ZONE_LOOKUP)
                .count(),
            1,
            "the zone lookup must be cached per challenge name"
        );
    }

    /// The behaviour the apex+wildcard case depends on: publishing a SECOND
    /// value at a name that already carries one must ADD it, never replace the
    /// record set — and must not touch the value already there.
    #[tokio::test]
    async fn publishing_a_second_value_at_one_name_adds_it() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            // The apex value is not present yet…
            (RECORDS, empty_list()),
            (
                CREATE,
                r#"{"success":true,"errors":[],"result":{"id":"rec-apex"}}"#,
            ),
        ])
        .then(RECORDS, 200, empty_list())
        .then(
            CREATE,
            200,
            r#"{"success":true,"errors":[],"result":{"id":"rec-wild"}}"#,
        );
        let provider = provider(std::sync::Arc::clone(&transport));

        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("apex publishes");
        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-wildcard"))
            .await
            .expect("wildcard publishes");

        let calls = transport.calls();
        assert_eq!(
            calls.iter().filter(|c| *c == CREATE).count(),
            2,
            "both values must be created: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("DELETE")),
            "publishing must never delete the sibling value: {calls:?}"
        );
        // The zone is looked up once and cached for the second record.
        assert_eq!(
            calls.iter().filter(|c| *c == ZONE_LOOKUP).count(),
            1,
            "the zone id must be cached: {calls:?}"
        );
        // Each create carries its own value.
        let created: Vec<&str> = transport
            .sent()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| {
                if r.body.contains("value-apex") {
                    "value-apex"
                } else {
                    "value-wildcard"
                }
            })
            .collect();
        assert_eq!(created, vec!["value-apex", "value-wildcard"]);
    }

    /// Removing one value must delete exactly that record, leaving the sibling
    /// challenge value live — the CA may still be validating against it.
    #[tokio::test]
    async fn deleting_one_value_leaves_the_sibling_alone() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            (
                RECORDS,
                r#"{"success":true,"errors":[],"result":[
                    {"id":"rec-apex","name":"_acme-challenge.myapp.com","content":"value-apex"},
                    {"id":"rec-wild","name":"_acme-challenge.myapp.com","content":"value-wildcard"}
                ]}"#,
            ),
        ])
        .then(
            "DELETE /client/v4/zones/zone123/dns_records/rec-apex",
            200,
            r#"{"success":true,"errors":[],"result":{"id":"rec-apex"}}"#,
        );
        let provider = provider(std::sync::Arc::clone(&transport));

        provider
            .delete_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("the apex record is removed");

        let calls = transport.calls();
        assert!(
            calls.contains(&"DELETE /client/v4/zones/zone123/dns_records/rec-apex".to_owned()),
            "the apex record must be deleted: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.contains("rec-wild")),
            "the sibling wildcard value must survive: {calls:?}"
        );
    }

    /// A record already present is not created again: a retried order (or a
    /// leftover from an interrupted one) would otherwise get a 400.
    #[tokio::test]
    async fn an_already_published_value_is_not_created_twice() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            (
                RECORDS,
                r#"{"success":true,"errors":[],"result":[
                    {"id":"rec-apex","name":"_acme-challenge.myapp.com","content":"value-apex"}
                ]}"#,
            ),
        ]);
        let provider = provider(std::sync::Arc::clone(&transport));

        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "value-apex"))
            .await
            .expect("an already-present value is a no-op");
        assert!(
            !transport.calls().iter().any(|c| c == CREATE),
            "got: {:?}",
            transport.calls()
        );
    }

    /// The zone walk goes most-specific-first and stops at the first hit, so a
    /// delegated sub-zone wins over its parent.
    #[tokio::test]
    async fn the_zone_walk_prefers_the_most_specific_zone() {
        let transport = RecordingTransport::new(&[
            // `tenants.myapp.com` is not a zone…
            (ZONE_LOOKUP, empty_list()),
        ])
        .then(ZONE_LOOKUP, 200, zone_hit())
        .then(RECORDS, 200, empty_list())
        .then(
            CREATE,
            200,
            r#"{"success":true,"errors":[],"result":{"id":"rec"}}"#,
        );
        let provider = provider(std::sync::Arc::clone(&transport));

        provider
            .upsert_txt(&TxtRecord::new("tenants.myapp.com", "v"))
            .await
            .expect("falls back to the parent zone");

        let asked: Vec<String> = transport
            .sent()
            .iter()
            .filter(|r| r.url.contains("/zones?"))
            .map(|r| r.url.clone())
            .collect();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[0].contains("name=tenants.myapp.com"), "{asked:?}");
        assert!(asked[1].contains("name=myapp.com"), "{asked:?}");
    }

    /// The bearer token goes on every request — and only as a header, never in a
    /// URL that could end up in an error message or a proxy log.
    #[tokio::test]
    async fn every_request_carries_the_token_as_a_header_only() {
        let transport = RecordingTransport::new(&[
            (ZONE_LOOKUP, zone_hit()),
            (RECORDS, empty_list()),
            (
                CREATE,
                r#"{"success":true,"errors":[],"result":{"id":"r"}}"#,
            ),
        ]);
        let provider = provider(std::sync::Arc::clone(&transport));
        provider
            .upsert_txt(&TxtRecord::new("myapp.com", "v"))
            .await
            .expect("publishes");

        for request in transport.sent() {
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer cf-test-token"),
                "every call must be authenticated"
            );
            assert!(
                !request.url.contains("cf-test-token"),
                "the token must never reach a URL: {}",
                request.url
            );
        }
    }

    /// A zone the token cannot see is an actionable error, not a silent write
    /// into whatever zone came back.
    #[tokio::test]
    async fn no_matching_zone_is_an_actionable_error() {
        let transport = RecordingTransport::new(&[(ZONE_LOOKUP, empty_list())]);
        let err = provider(transport)
            .upsert_txt(&TxtRecord::new("myapp.com", "v"))
            .await
            .expect_err("no zone means no write");
        assert!(err.contains("no Cloudflare zone found"), "got: {err}");
        assert!(err.contains("myapp.com"), "got: {err}");
    }

    #[test]
    fn zone_candidates_are_most_specific_first_and_skip_the_tld() {
        // `_acme-challenge.<name>` leads: it can be a delegated zone of its own,
        // which is a recommended way to scope an ACME credential (#1620).
        assert_eq!(
            zone_candidates("_acme-challenge.a.b.myapp.com"),
            vec![
                "_acme-challenge.a.b.myapp.com".to_owned(),
                "a.b.myapp.com".to_owned(),
                "b.myapp.com".to_owned(),
                "myapp.com".to_owned(),
            ]
        );
        assert_eq!(
            zone_candidates("_acme-challenge.myapp.com"),
            vec![
                "_acme-challenge.myapp.com".to_owned(),
                "myapp.com".to_owned(),
            ]
        );
        // Never asks Cloudflare about a bare TLD.
        assert!(!zone_candidates("_acme-challenge.myapp.com").contains(&"com".to_owned()));
    }

    /// Regression (#1620): `_acme-challenge.example.com` delegated to a zone of
    /// its own must be found, not skipped in favour of the parent.
    ///
    /// This is the setup that lets an operator hand autumn a credential scoped
    /// to nothing but the challenge zone. Stripping the label wrote the record
    /// into the parent zone instead, where nothing is authoritative for it.
    #[tokio::test]
    async fn a_delegated_challenge_zone_is_used_when_it_exists() {
        let transport = RecordingTransport::new(&[(
            ZONE_LOOKUP,
            r#"{"success":true,"errors":[],"result":[{"id":"zone-challenge","name":"_acme-challenge.myapp.com"}]}"#,
        )]);
        let provider = provider(std::sync::Arc::clone(&transport));

        assert_eq!(
            provider
                .zone_id("_acme-challenge.myapp.com")
                .await
                .expect("the delegated challenge zone resolves"),
            "zone-challenge"
        );
        let asked: Vec<String> = transport.sent().iter().map(|r| r.url.clone()).collect();
        assert!(
            asked
                .iter()
                .any(|u| u.contains("name=_acme-challenge.myapp.com")),
            "the challenge name itself must be offered as a zone candidate: {asked:?}"
        );
    }

    #[test]
    fn list_url_pins_both_the_name_and_the_value() {
        let record = TxtRecord::new("myapp.com", "abc+/=def");
        let url = list_url("zone123", &record);
        assert!(url.contains("type=TXT"), "{url}");
        assert!(url.contains("name=_acme-challenge.myapp.com"), "{url}");
        // The value is percent-encoded, so a base64url value with `-`/`_` and a
        // padded `=` cannot break out of the query string.
        assert!(url.contains("content=abc%2B%2F%3Ddef"), "{url}");
    }

    #[test]
    fn urlencode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(urlencode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn api_errors_name_the_operation_and_carry_cloudflares_message() {
        let response = HttpResponse {
            status: 403,
            body: r#"{"success":false,"errors":[{"code":9109,"message":"Invalid access token"}]}"#
                .to_owned(),
        };
        let err = parse_api_response(
            &response,
            "create the Cloudflare TXT record",
            "https://api.cloudflare.com/client/v4/zones/z/dns_records",
            &[],
        )
        .expect_err("403 is a failure");
        assert!(
            err.contains("create the Cloudflare TXT record"),
            "got: {err}"
        );
        assert!(err.contains("403"), "got: {err}");
        assert!(err.contains("Invalid access token"), "got: {err}");
    }

    // Cloudflare answers 200 with `success: false` for some failures; treating
    // that as success would let the order proceed to a challenge that can never
    // validate.
    #[test]
    fn a_200_with_success_false_is_still_an_error() {
        let response = HttpResponse {
            status: 200,
            body: r#"{"success":false,"errors":[{"message":"zone not found"}],"result":null}"#
                .to_owned(),
        };
        assert!(parse_api_response(&response, "x", "https://api/y", &[]).is_err());
    }

    // The error message is surfaced to operators through logs, health details and
    // the alert payload — the query string can carry the challenge value but must
    // never carry credentials, so the URL is truncated at the `?`.
    #[test]
    fn api_errors_drop_the_query_string() {
        let response = HttpResponse {
            status: 500,
            body: String::new(),
        };
        let err = parse_api_response(&response, "x", "https://api/y?name=a&content=b", &[])
            .expect_err("500 is a failure");
        assert!(!err.contains("content=b"), "got: {err}");
        assert!(err.contains("https://api/y"), "got: {err}");
    }

    #[test]
    fn result_ids_are_read_from_a_list_response() {
        let record = TxtRecord::new("myapp.com", "value-one");
        let listed = serde_json::json!([
            { "id": "rec1", "name": "_acme-challenge.myapp.com", "content": "value-one" },
            { "id": "rec2", "name": "_acme-challenge.myapp.com.", "content": "value-one" },
        ]);
        assert_eq!(first_result_id(&listed).as_deref(), Some("rec1"));
        assert_eq!(matching_record_ids(&listed, &record), vec!["rec1", "rec2"]);
        assert!(first_result_id(&serde_json::json!([])).is_none());
        assert!(matching_record_ids(&Value::Null, &record).is_empty());
    }

    // The apex+wildcard invariant — never read or remove the SIBLING challenge
    // value — must be enforced here, not delegated to Cloudflare honouring
    // `content=` as an exact match.
    #[test]
    fn a_sibling_value_at_the_same_name_is_never_matched() {
        let apex = TxtRecord::new("myapp.com", "value-apex");
        // What a loosened server-side filter would return: both values at the
        // one name, plus an unrelated record.
        let listed = serde_json::json!([
            { "id": "rec-apex", "name": "_acme-challenge.myapp.com", "content": "value-apex" },
            { "id": "rec-wild", "name": "_acme-challenge.myapp.com", "content": "value-wildcard" },
            { "id": "rec-other", "name": "_acme-challenge.other.com", "content": "value-apex" },
        ]);
        assert_eq!(
            matching_record_ids(&listed, &apex),
            vec!["rec-apex"],
            "only the exact (name, value) pair may be acted on"
        );
    }

    // A Cloudflare error body is published on `/actuator/health` and pushed to
    // the operator's alert destination, so the bearer token must not survive a
    // round trip through it.
    #[test]
    fn api_errors_scrub_the_bearer_token() {
        const TOKEN: &str = "cf-live-token-DO-NOT-LEAK-9f3a";
        let response = HttpResponse {
            status: 400,
            body: format!(
                r#"{{"success":false,"errors":[{{"message":"bad request for token {TOKEN}"}}]}}"#
            ),
        };
        let err = parse_api_response(&response, "x", "https://api/y", &[TOKEN])
            .expect_err("400 is a failure");
        assert!(!err.contains(TOKEN), "leaked: {err}");
        assert!(err.contains("<redacted>"), "got: {err}");
    }
}
