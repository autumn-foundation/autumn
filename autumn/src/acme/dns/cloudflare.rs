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
use super::{DnsProvider, SecretString, TxtRecord};

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
        parse_api_response(&response, what, &url)
    }

    /// Resolve the Cloudflare zone id that owns `fqdn`.
    async fn zone_id(&self, fqdn: &str) -> Result<String, String> {
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
                    HttpRequest::new("GET", format!("{API_BASE}/zones?name={candidate}")),
                    "look up the Cloudflare zone",
                )
                .await?;
            if let Some(id) = first_result_id(&body) {
                self.zone_ids
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(candidate.clone(), id.clone());
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
            if !find_record_ids(
                &self
                    .send(
                        HttpRequest::new("GET", list_url(&zone, record)),
                        "list Cloudflare TXT records",
                    )
                    .await?,
            )
            .is_empty()
            {
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
            for id in find_record_ids(&listed) {
                self.send(
                    HttpRequest::new("DELETE", format!("{API_BASE}/zones/{zone}/dns_records/{id}")),
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
    let name = fqdn
        .trim()
        .trim_end_matches('.')
        .trim_start_matches("_acme-challenge.");
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
            .unwrap_or_else(|| "no error detail returned".to_owned());
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

/// Every `id` in a Cloudflare list `result`.
fn find_record_ids(result: &Value) -> Vec<String> {
    result
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_candidates_are_most_specific_first_and_skip_the_tld() {
        assert_eq!(
            zone_candidates("_acme-challenge.a.b.myapp.com"),
            vec![
                "a.b.myapp.com".to_owned(),
                "b.myapp.com".to_owned(),
                "myapp.com".to_owned(),
            ]
        );
        assert_eq!(
            zone_candidates("_acme-challenge.myapp.com"),
            vec!["myapp.com".to_owned()]
        );
        // Never asks Cloudflare about a bare TLD.
        assert!(!zone_candidates("_acme-challenge.myapp.com").contains(&"com".to_owned()));
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
        let err = parse_api_response(&response, "create the Cloudflare TXT record", "https://api.cloudflare.com/client/v4/zones/z/dns_records")
            .expect_err("403 is a failure");
        assert!(err.contains("create the Cloudflare TXT record"), "got: {err}");
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
        assert!(parse_api_response(&response, "x", "https://api/y").is_err());
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
        let err = parse_api_response(&response, "x", "https://api/y?name=a&content=b")
            .expect_err("500 is a failure");
        assert!(!err.contains("content=b"), "got: {err}");
        assert!(err.contains("https://api/y"), "got: {err}");
    }

    #[test]
    fn result_ids_are_read_from_a_list_response() {
        let listed = serde_json::json!([{ "id": "rec1" }, { "id": "rec2" }]);
        assert_eq!(first_result_id(&listed).as_deref(), Some("rec1"));
        assert_eq!(find_record_ids(&listed), vec!["rec1", "rec2"]);
        assert!(first_result_id(&serde_json::json!([])).is_none());
        assert!(find_record_ids(&Value::Null).is_empty());
    }
}
