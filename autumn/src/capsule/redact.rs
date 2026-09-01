//! Redaction of everything a capsule copies off the wire.
//!
//! A capsule holds a real production request, so it is only safe to write once
//! the sensitive parts are gone. This module masks headers, query parameters
//! and structured bodies through the same
//! [`ParameterFilter`](crate::log::filter::ParameterFilter) the access log and
//! the dev error page use, so one `[log] filter_parameters` list governs every
//! place Autumn writes request data down.
//!
//! Masking also feeds forward: every value it removes is retained in a
//! [`RedactedValues`] set so [`mask_binds`] can blank any SQL bind parameter
//! that echoes one of them. A password redacted out of the request body must
//! not reappear in the `INSERT` that stored it.
//!
//! What is **not** masked: unstructured bodies (no keys to match on), URL path
//! segments, and database result rows. A body that *declares* structure but
//! does not parse as it — malformed JSON, or the prefix teed before a handler
//! abandoned the read — is not copied at all: with no keys to match on there
//! is nothing to mask, so it is recorded as skipped with a note. See
//! `docs/guide/failure-capsules.md`.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;

use crate::capsule::schema::{BindValue, CapsuleBody, CapsuleRequest};
use crate::log::filter::{FILTERED_PLACEHOLDER, ParameterFilter};

/// The unredacted request *head* the capture layer snapshotted, held in memory
/// until the request either succeeds (and the snapshot is dropped) or fails
/// (and this is redacted into a [`CapsuleRequest`]).
///
/// The body is not part of the snapshot: it is copied while the handler reads
/// it (see [`CaptureScope::captured_body`](crate::capsule::CaptureScope::captured_body))
/// and composed with this head at persist time.
#[derive(Debug, Clone)]
pub struct RawRequest {
    /// HTTP method.
    pub method: String,
    /// Full request target, query string included.
    pub uri: axum::http::Uri,
    /// Negotiated HTTP version.
    pub version: axum::http::Version,
    /// Request headers, verbatim.
    pub headers: axum::http::HeaderMap,
    /// Matched route template, when routing had already resolved one.
    pub route: Option<String>,
}

/// The request body as the capture layer obtained it.
#[derive(Debug, Clone)]
pub enum CapturedBody {
    /// No body was present, or none of it had been read when the request
    /// failed.
    Absent,
    /// The body was copied as the handler read it.
    Buffered(Bytes),
    /// The body was over the cap and deliberately not copied.
    Skipped {
        /// `Content-Length` the client declared, when it declared one.
        declared_len: Option<usize>,
    },
}

/// Values redaction removed, kept so bind parameters echoing them can be
/// masked too.
///
/// Compared byte-wise: a bind carrying exactly the pre-mask bytes is masked,
/// anything else (a hash, a truncation, a re-encoding) is not. This is a
/// best-effort echo check, not a general secret scanner.
#[derive(Debug, Clone, Default)]
pub struct RedactedValues {
    values: BTreeSet<Vec<u8>>,
    /// Values that must only ever be masked where they stand as a whole
    /// token, however long they are — see [`Self::insert_whole_token_only`].
    whole_token_only: BTreeSet<Vec<u8>>,
    /// Values the request carried in their own right, which always get full
    /// substring masking. Tracked separately so the two classifications can
    /// meet on the same bytes in either order and the stronger one still wins.
    direct: BTreeSet<Vec<u8>>,
}

impl RedactedValues {
    /// Record a value that was masked out of the request.
    ///
    /// Every masked value is kept, however short. The length floor lives in
    /// [`mask_echoes`] alone, where it earns its keep: masking a three-letter
    /// value *inside* free-form prose would shred unrelated words. Bind masking
    /// compares whole values, so there is nothing to shred — and a three-digit
    /// CVV or a short PIN the filter removed from the request must not travel
    /// on in the tape simply for being short.
    pub fn insert(&mut self, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        self.values.insert(value.to_vec());
        self.direct.insert(value.to_vec());
    }

    /// Record a value that is a *field* of something structured, rather than a
    /// value the request carried in its own right.
    ///
    /// Masked identically for binds — a bind carrying these bytes is still the
    /// secret echoing back — but in free-form text only where it stands as a
    /// whole token, regardless of length. An auth-param list mixes secrets
    /// with metadata under names this code cannot rank (`Signature=…` next to
    /// `qop=auth`), and a four-character metadata value substring-masked
    /// everywhere would turn every later mention of *authentication* into a
    /// placeholder. A field that echoes into prose or a bind arrives delimited
    /// anyway, which is exactly what whole-token masking catches.
    pub fn insert_whole_token_only(&mut self, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        self.values.insert(value.to_vec());
        self.whole_token_only.insert(value.to_vec());
    }

    /// Whether these bytes were masked out of the request.
    #[must_use]
    pub fn contains(&self, value: &[u8]) -> bool {
        self.values.contains(value)
    }

    /// Whether anything was masked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Whether this value may only be masked as a whole token.
    ///
    /// A direct [`insert`](Self::insert) always wins: the same bytes can reach
    /// the set both ways — a filtered body password that the client also sent
    /// as a `Digest` parameter — and the request carried that value in its own
    /// right, so it needs full substring masking wherever it surfaces. Asking
    /// both sets rather than mutating either on insert makes that independent
    /// of which arrived first.
    fn is_whole_token_only(&self, value: &[u8]) -> bool {
        self.whole_token_only.contains(value) && !self.direct.contains(value)
    }

    /// The recorded values, longest first.
    ///
    /// Longest-first matters for substring masking: masking `"hunter2"` before
    /// `"hunter2secret"` would leave the tail of the longer secret behind.
    fn longest_first(&self) -> Vec<&[u8]> {
        let mut values: Vec<&[u8]> = self.values.iter().map(Vec::as_slice).collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values
    }
}

/// Shortest value [`mask_echoes`] will replace *anywhere* inside free-form
/// text. Below this length a value is still masked, but only where it stands
/// as a whole token — see [`replace_whole_tokens`]. Whole-value bind masking
/// has no length rule at all.
const MIN_ECHO_LEN: usize = 4;

/// Build the capsule's request record, masking every sensitive value.
///
/// Takes the request head and the body separately, because the capture layer
/// obtains them at different times: the head up front, the body as the handler
/// streams it.
///
/// Returns the redacted record, the set of pre-mask values for [`mask_binds`]
/// and [`mask_echoes`], and any notes about a body redaction refused to copy.
#[must_use]
pub fn redact_request(
    raw: &RawRequest,
    raw_body: &CapturedBody,
    filter: &ParameterFilter,
) -> (CapsuleRequest, RedactedValues, Vec<String>) {
    let mut values = RedactedValues::default();
    let mut keys = BTreeSet::new();
    let mut notes = Vec::new();

    let (headers, binary_headers) = redact_headers(&raw.headers, filter, &mut values, &mut keys);
    let uri = redact_uri(&raw.uri, filter, &mut values, &mut keys);
    let body = redact_body(
        &raw.headers,
        raw_body,
        filter,
        &mut values,
        &mut keys,
        &mut notes,
    );

    let request = CapsuleRequest {
        method: raw.method.clone(),
        uri,
        route: raw.route.clone(),
        http_version: format!("{:?}", raw.version),
        headers,
        binary_headers,
        body,
        redacted_keys: keys.into_iter().collect(),
        // Filled in by persist from the capture scope; redaction only sees
        // the request head.
        peer_addr: None,
        client_addr: None,
        client_host: None,
        client_scheme: None,
    };
    (request, values, notes)
}

/// Replace every occurrence of a redacted request value inside `text` with the
/// filtered placeholder.
///
/// Where [`mask_binds`] compares whole values, this looks for them *inside* a
/// string: a handler that fails with `"could not store password=hunter2"` — or
/// panics with the submitted value in its payload — would otherwise write back
/// out, in the capsule's outcome, exactly what redaction removed from the
/// request. Same minimum-length rule as the rest of the echo set, so short
/// values cannot shred unrelated prose — a floor that applies here and nowhere
/// else.
#[must_use]
pub fn mask_echoes(text: &str, redacted: &RedactedValues) -> String {
    if redacted.is_empty() || text.is_empty() {
        return text.to_owned();
    }
    // Longest first, so a longer secret wins over one that is a substring of
    // it: masking `hunter2` before `hunter2secret` would leave the longer
    // secret's tail behind.
    let mut needles: Vec<(&str, bool)> = Vec::new();
    for value in redacted.longest_first() {
        // Only values that were UTF-8 to begin with can appear in a message;
        // binary secrets are handled by `mask_binds`.
        let Ok(needle) = std::str::from_utf8(value) else {
            continue;
        };
        if needle.is_empty() {
            continue;
        }
        // A short value — a three-digit CVV, a PIN — and any structural field
        // are masked only where they stand as a whole token. Replacing them
        // everywhere would shred timestamps, identifiers, byte counts and
        // ordinary words that merely contain those characters, in failures
        // that have nothing to do with the secret; leaving them alone entirely
        // wrote the secret to disk whenever the failure quoted it back, as
        // `CVV 123 rejected` does.
        let whole_token_only = needle.len() < MIN_ECHO_LEN || redacted.is_whole_token_only(value);
        needles.push((needle, whole_token_only));
    }

    // One left-to-right pass over the *original* text, emitting placeholders
    // as it goes. Replacing each needle in turn across the accumulating output
    // instead would let a later secret match a placeholder an earlier one had
    // just written — with `FILTER` in the set, `[FILTERED]` became
    // `[[FILTERED]ED]`, which replay's own scrubbing never reproduces, turning
    // a matching failure into a `mismatch`. Text this pass has emitted is
    // never looked at again.
    let mut masked = String::with_capacity(text.len());
    let mut cursor = 0_usize;
    while let Some(rest) = text.get(cursor..).filter(|rest| !rest.is_empty()) {
        let matched = needles.iter().find(|(needle, whole_token_only)| {
            rest.starts_with(*needle)
                && (!*whole_token_only || stands_alone(text, cursor, needle, rest))
        });
        if let Some((needle, _)) = matched {
            masked.push_str(FILTERED_PLACEHOLDER);
            cursor = cursor.saturating_add(needle.len());
        } else if let Some(next) = rest.chars().next() {
            masked.push(next);
            cursor = cursor.saturating_add(next.len_utf8());
        } else {
            break;
        }
    }
    masked
}

/// Whether the occurrence of `needle` at `cursor` stands as a whole token —
/// that is, is not part of a longer alphanumeric run.
///
/// Both neighbours are read from the *original* text, which is what makes
/// back-to-back occurrences come out right: in `123123` the character after
/// the first `123` is the `1` of the second, so neither is a whole token.
/// Written with [`str::get`] rather than indexing so the request-path panic
/// gate's `string_slice`/`indexing_slicing` denials hold.
fn stands_alone(text: &str, cursor: usize, needle: &str, rest: &str) -> bool {
    let head = text.get(..cursor).unwrap_or_default();
    let tail = rest.get(needle.len()..).unwrap_or_default();

    // A dot only joins an identifier when it has one on *both* sides:
    // `api.v1.error` is one dotted name, but the dot in `CVV 123.` ends a
    // sentence. Treating every dot as interior would leave a secret before a
    // full stop unmasked, which trades a false mismatch for a real leak — so
    // the neighbour *past* the dot decides.
    let mut before = head.chars().rev();
    let joined_before = match before.next() {
        Some('.') => before.next().is_some_and(char::is_alphanumeric),
        Some(character) => is_identifier_char(character),
        None => false,
    };

    let mut after = tail.chars();
    let joined_after = match after.next() {
        Some('.') => after.next().is_some_and(char::is_alphanumeric),
        Some(character) => is_identifier_char(character),
        None => false,
    };

    !joined_before && !joined_after
}

/// Whether `character` can sit *inside* an identifier, and so does not end a
/// token.
///
/// Hyphen and underscore count, not only alphanumerics: `auth-error` and
/// `auth_error` are single identifiers in error strings, log keys and enum
/// spellings, so a whole-token-only `auth` that rewrote them to
/// `[FILTERED]-error` would shred exactly the static messages replay compares
/// against — the false `mismatch` this classification exists to prevent.
///
/// The dot is deliberately *not* here: it joins only when flanked by
/// alphanumerics, which [`stands_alone`] decides, because a trailing dot is
/// far more often a full stop than part of a name.
fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '-' || character == '_'
}

// ── Effect tape redaction (#1634) ───────────────────────────────────────────

/// Redact everything the effect tape recorded, in place.
///
/// The effect seams buffer *raw* values while the run is in flight — they have
/// no filter handle and no business paying redaction's cost on a request that
/// may never fail — so masking happens here, once, on the way to disk. The
/// same [`ParameterFilter`] the inbound request is masked through is used, for
/// a reason worth stating plainly: an **outbound** `Authorization` header
/// carries a downstream credential exactly the way an inbound one carries the
/// caller's, and a job payload or a cache value is as likely to hold a token
/// as a request body is.
///
/// Runs in two passes, because the two mechanisms feed each other:
///
/// 1. **Filter pass** — sensitive header names are blanked and structured
///    (JSON / urlencoded) payloads are scrubbed by key, seeding `values` with
///    everything that was removed.
/// 2. **Echo pass** — free-form text (URLs, cache keys, mail subjects, error
///    strings) is swept for any value the first pass, *or the request's own
///    redaction*, put in `values`.
///
/// Splitting them is what makes an outbound body's secret get masked out of an
/// error message recorded by a *different* seam.
#[allow(
    clippy::too_many_lines,
    reason = "one pass per seam, then one echo pass per seam; splitting them \
              would hide the two-phase ordering the doc comment above explains"
)]
pub fn redact_effects(
    effects: &mut crate::capsule::schema::CapsuleEffects,
    job: Option<&mut crate::capsule::schema::CapsuleJob>,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) {
    // The job-entry payload first: it is the *input* the recorded run ran on,
    // so anything masked out of it must be maskable out of every effect that
    // quoted it back.
    let mut job = job;
    if let Some(job) = job.as_mut() {
        job.payload = scrub_value(&job.payload, filter, "job_entry", values, keys);
    }
    for (index, exchange) in effects.http.iter_mut().enumerate() {
        // The query string of an *outbound* URL carries secrets as readily as
        // an inbound one's does — an API key passed as `?key=…` is the classic
        // case — so it is masked by the same rules, before anything else reads
        // the URL.
        exchange.url = redact_url_query(
            &exchange.url,
            filter,
            values,
            keys,
            &format!("http[{index}].url"),
        );
        // The URL a redirect *landed on* carries credentials at least as often
        // as the one the call started with — an OAuth callback with
        // `?access_token=` is the whole point of the pattern — so it is masked
        // by the same rules rather than trusted for having been server-chosen.
        if let Some(final_url) = exchange.final_url.as_ref() {
            exchange.final_url = Some(redact_url_query(
                final_url,
                filter,
                values,
                keys,
                &format!("http[{index}].final_url"),
            ));
        }
        let request_content_type = header_value(&exchange.request_headers, "content-type");
        let response_content_type = header_value(&exchange.response_headers, "content-type");
        redact_effect_headers(
            &mut exchange.request_headers,
            filter,
            values,
            keys,
            &format!("http[{index}].request_header"),
        );
        redact_effect_headers(
            &mut exchange.response_headers,
            filter,
            values,
            keys,
            &format!("http[{index}].response_header"),
        );
        exchange.request_body = redact_effect_body(
            &exchange.request_body,
            &request_content_type,
            filter,
            values,
            keys,
            &format!("http[{index}].request_body"),
        );
        exchange.response_body = redact_effect_body(
            &exchange.response_body,
            &response_content_type,
            filter,
            values,
            keys,
            &format!("http[{index}].response_body"),
        );
    }

    for (index, job) in effects.jobs.iter_mut().enumerate() {
        job.payload = scrub_value(&job.payload, filter, &format!("job[{index}]"), values, keys);
    }

    for (index, entry) in effects.cache.iter_mut().enumerate() {
        match entry {
            crate::capsule::schema::CacheEffect::Get { value, .. } => {
                if let Some(encoded) = value.as_mut() {
                    *encoded = scrub_encoded_json(
                        encoded,
                        filter,
                        &format!("cache[{index}]"),
                        values,
                        keys,
                    );
                }
            }
            crate::capsule::schema::CacheEffect::Insert { value, .. } => {
                *value =
                    scrub_encoded_json(value, filter, &format!("cache[{index}]"), values, keys);
            }
        }
    }

    for (index, mail) in effects.mail.iter_mut().enumerate() {
        mail.alternate_body = redact_effect_body(
            &mail.alternate_body,
            // An alternate body is the HTML half of a multipart message
            // whenever there is one to record.
            "text/html",
            filter,
            values,
            keys,
            &format!("mail[{index}].alternate_body"),
        );
        redact_effect_headers(
            &mut mail.extra_headers,
            filter,
            values,
            keys,
            &format!("mail[{index}].header"),
        );
        mail.body = redact_effect_body(
            &mail.body,
            "text/plain",
            filter,
            values,
            keys,
            &format!("mail[{index}].body"),
        );
    }

    // Echo pass — everything free-form, with the fully-seeded set.
    for exchange in &mut effects.http {
        exchange.url = mask_echoes(&exchange.url, values);
        mask_body_echoes(&mut exchange.request_body, values);
        mask_body_echoes(&mut exchange.response_body, values);
        for (_, value) in exchange
            .request_headers
            .iter_mut()
            .chain(exchange.response_headers.iter_mut())
        {
            *value = mask_echoes(value, values);
        }
        if let Some(error) = exchange.error.as_mut() {
            *error = mask_echoes(error, values);
        }
    }
    for entry in &mut effects.cache {
        match entry {
            crate::capsule::schema::CacheEffect::Get { key, .. }
            | crate::capsule::schema::CacheEffect::Insert { key, .. } => {
                // A cache key is routinely built out of the very arguments a
                // request supplied (`make_cache_key` hashes them into it, and
                // hand-built keys interpolate them), so it is free-form text
                // that can quote a secret back.
                *key = mask_echoes(key, values);
            }
        }
    }
    for mail in &mut effects.mail {
        mail.subject = mask_echoes(&mail.subject, values);
        mask_body_echoes(&mut mail.body, values);
        mask_body_echoes(&mut mail.alternate_body, values);
        // Recipients are the PII on this seam: an operator who filters
        // `email` must not find the address sitting in the clear one field
        // over, which is exactly the "filter defeated through a side door"
        // the client-identity handling refuses to allow.
        for recipient in &mut mail.to {
            *recipient = mask_echoes(recipient, values);
        }
        // Every free-form field a caller can put a request value into, not just
        // the obvious two: an address submitted in a form and reused as
        // `Reply-To`, an unsubscribe link carrying a token, a filename built
        // from user input. A value masked one field over must not sit in the
        // clear here.
        if let Some(reply_to) = mail.reply_to.as_mut() {
            *reply_to = mask_echoes(reply_to, values);
        }
        if let Some(unsubscribe) = mail.list_unsubscribe.as_mut() {
            *unsubscribe = mask_echoes(unsubscribe, values);
        }
        for (_, value) in &mut mail.extra_headers {
            *value = mask_echoes(value, values);
        }
        for attachment in &mut mail.attachments {
            attachment.filename = mask_echoes(&attachment.filename, values);
        }
        if let Some(from) = mail.from.as_mut() {
            *from = mask_echoes(from, values);
        }
        if let Some(error) = mail.error.as_mut() {
            *error = mask_echoes(error, values);
        }
    }
    // Job payloads and cache values are structured, so the filter pass masked
    // them *by key*; this catches the other half — a value the request already
    // had masked that reappears under a key the filter does not name.
    for effect in &mut effects.jobs {
        effect.payload = mask_json_echoes(&effect.payload, values);
        // A rejection's text is free-form and written by whatever refused the
        // enqueue — a `JobInterceptor` can quote the payload field or the
        // credential it rejected — so it can carry a value masked everywhere
        // else in the capsule.
        if let Some(error) = effect.error.as_mut() {
            *error = mask_echoes(error, values);
        }
    }
    if let Some(job) = job.as_mut() {
        job.payload = mask_json_echoes(&job.payload, values);
    }
    for entry in &mut effects.cache {
        match entry {
            crate::capsule::schema::CacheEffect::Get { value, .. } => {
                if let Some(encoded) = value.as_mut() {
                    *encoded = mask_encoded_json_echoes(encoded, values);
                }
            }
            crate::capsule::schema::CacheEffect::Insert { value, .. } => {
                *value = mask_encoded_json_echoes(value, values);
            }
        }
    }
    if let Some(tenant) = effects.tenant.as_mut()
        && let Some(id) = tenant.id.as_mut()
    {
        let masked = mask_echoes(id, values);
        // The tenant is *input*: replay serves it to tenant-scoped code and to
        // every database bind that scopes by it. A placeholder there would run
        // the whole request under a tenant production never resolved, so the
        // masking is recorded as a key and the capsule refused rather than
        // replayed. (`mask_echoes` leaves an unmasked id untouched.)
        if masked != *id {
            keys.insert("tenant.id".to_owned());
        }
        *id = masked;
    }
}

/// Mask the query string of a recorded outbound URL by parameter name.
///
/// Splices the masked query back onto the original prefix rather than
/// rebuilding the URL, exactly as [`redact_uri`] does for the inbound target:
/// re-serializing would drift percent-encoding and parameter order, and replay
/// matches the recorded URL against the one the handler builds.
fn redact_url_query(
    url: &str,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
    location: &str,
) -> String {
    let Some(split) = url.find('?') else {
        return url.to_owned();
    };
    let head = url.get(..split).unwrap_or_default();
    let query = url.get(split.saturating_add(1)..).unwrap_or_default();
    // A fragment is not part of the query; leave it attached to the tail.
    let (query, fragment) = query.find('#').map_or((query, ""), |hash| {
        (
            query.get(..hash).unwrap_or_default(),
            query.get(hash..).unwrap_or_default(),
        )
    });
    let Some(masked) = mask_raw_urlencoded(query, location, filter, values, keys) else {
        return url.to_owned();
    };
    format!("{head}?{masked}{fragment}")
}

/// Sweep every string leaf of a JSON document for redacted values.
fn mask_json_echoes(value: &serde_json::Value, redacted: &RedactedValues) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(mask_echoes(text, redacted)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| mask_json_echoes(item, redacted))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, child)| (key.clone(), mask_json_echoes(child, redacted)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// [`mask_json_echoes`] for the base64-encoded JSON cache values are stored as.
///
/// A value this cannot decode or parse is left alone: it was already replaced
/// wholesale by the filter pass if it was unreadable there too.
fn mask_encoded_json_echoes(encoded: &str, redacted: &RedactedValues) -> String {
    let Ok(bytes) = STANDARD.decode(encoded.as_bytes()) else {
        return encoded.to_owned();
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return encoded.to_owned();
    };
    let masked = mask_json_echoes(&parsed, redacted);
    if masked == parsed {
        return encoded.to_owned();
    }
    serde_json::to_vec(&masked).map_or_else(|_| encoded.to_owned(), |bytes| STANDARD.encode(&bytes))
}

/// The value of `name` in a recorded header list, lower-cased for matching.
fn header_value(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Blank every header the filter calls sensitive, retaining what was removed.
fn redact_effect_headers(
    headers: &mut [(String, String)],
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
    location: &str,
) {
    for (name, value) in headers.iter_mut() {
        if header_is_sensitive(name, filter) || effect_header_is_sensitive(name) {
            values.insert(value.as_bytes());
            record_credential_components(name, value.as_bytes(), values);
            keys.insert(format!("{location}:{name}"));
            value.clear();
            value.push_str(FILTERED_PLACEHOLDER);
        }
    }
}

/// Credential-carrying header names masked on the **outbound** seam whatever
/// the application's filter says.
///
/// The inbound list can afford to be narrow: a client's own credential is what
/// `[log] filter_parameters` is written for, and an operator who adds a custom
/// header knows to name it. Outbound is the mirror image — the credential is
/// the *application's*, the operator never sees these headers in a log to
/// notice them, and the spellings are conventional rather than app-specific.
/// Masking a header that turns out to hold nothing secret costs a replay
/// nothing: outbound matching is on method and URL.
const OUTBOUND_SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "apikey",
    "x-auth-token",
    "x-access-token",
    "x-amz-security-token",
    "x-goog-api-key",
];

/// Whether a recorded effect header is one of [`OUTBOUND_SENSITIVE_HEADERS`].
fn effect_header_is_sensitive(name: &str) -> bool {
    OUTBOUND_SENSITIVE_HEADERS
        .iter()
        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

/// Mask a recorded effect body by key, the way an inbound body is masked.
fn redact_effect_body(
    body: &CapsuleBody,
    content_type: &str,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
    location: &str,
) -> CapsuleBody {
    let CapsuleBody::Text(text) = body else {
        // `Base64` is opaque bytes and `Absent`/`Skipped` hold nothing; there
        // are no keys to match, so there is nothing this pass can do. The echo
        // pass still sweeps `Text` bodies, and a binary body cannot quote a
        // masked string back in a form a reader would recognise.
        return body.clone();
    };
    if content_type.contains("json") {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
            // Same posture as an unparseable inbound JSON body: keys the
            // filter could have matched may be in there, so retain the raw
            // text for the echo set and do not copy it.
            values.insert(text.as_bytes());
            keys.insert(format!("{location}:<unparseable json>"));
            return CapsuleBody::Text(FILTERED_PLACEHOLDER.to_owned());
        };
        let before = keys.len();
        let scrubbed = scrub_value(&parsed, filter, location, values, keys);
        if keys.len() == before {
            return body.clone();
        }
        retain_raw_json_string_spellings(text.as_bytes(), values);
        return serde_json::to_string(&scrubbed)
            .map_or_else(|_| CapsuleBody::Absent, CapsuleBody::Text);
    }
    if content_type.contains("application/x-www-form-urlencoded")
        && let Some(masked) = mask_raw_urlencoded(text, location, filter, values, keys)
    {
        return CapsuleBody::Text(masked);
    }
    body.clone()
}

/// Sweep a recorded body for values redaction removed elsewhere.
fn mask_body_echoes(body: &mut CapsuleBody, values: &RedactedValues) {
    if let CapsuleBody::Text(text) = body {
        *text = mask_echoes(text, values);
    }
}

/// Scrub base64-encoded JSON (the form cache values are recorded in) by key,
/// returning it re-encoded.
///
/// A value that is not base64, or not JSON, is replaced wholesale: a cache
/// entry this pass cannot read is one it cannot prove is free of secrets.
fn scrub_encoded_json(
    encoded: &str,
    filter: &ParameterFilter,
    location: &str,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> String {
    let Ok(bytes) = STANDARD.decode(encoded.as_bytes()) else {
        keys.insert(format!("{location}:<undecodable>"));
        return STANDARD.encode(FILTERED_PLACEHOLDER.as_bytes());
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        values.insert(&bytes);
        keys.insert(format!("{location}:<unparseable json>"));
        return STANDARD.encode(FILTERED_PLACEHOLDER.as_bytes());
    };
    let before = keys.len();
    let scrubbed = scrub_value(&parsed, filter, location, values, keys);
    if keys.len() == before {
        return encoded.to_owned();
    }
    retain_raw_json_string_spellings(&bytes, values);
    serde_json::to_vec(&scrubbed).map_or_else(
        |_| STANDARD.encode(FILTERED_PLACEHOLDER.as_bytes()),
        |bytes| STANDARD.encode(&bytes),
    )
}

/// Mask any bind parameter whose bytes exactly echo a redacted request value.
pub fn mask_binds(binds: &mut [BindValue], redacted: &RedactedValues) {
    if redacted.is_empty() {
        return;
    }
    for bind in binds {
        if let BindValue::Value(bytes) = bind
            && redacted.contains(bytes)
        {
            *bind = BindValue::Masked;
        }
    }
}

/// Headers the capsule masks unconditionally, over and above the filter.
///
/// These carry credentials by construction, but their names do not normalize
/// onto any `DEFAULT_FILTER_KEYS` entry (`proxy-authorization` →
/// `proxyauthorization` ≠ `authorization`), so an exact-match filter would
/// copy them verbatim. A capsule is a production-data artifact; a standard
/// credential header must never depend on app configuration to be masked.
const ALWAYS_SENSITIVE_HEADERS: &[&str] = &["proxy-authorization"];

/// Whether a header must be masked out of the capsule.
fn header_is_sensitive(name: &str, filter: &ParameterFilter) -> bool {
    filter.matches_key(name) || ALWAYS_SENSITIVE_HEADERS.contains(&name)
}

/// Text headers plus base64-encoded non-UTF-8 headers, as redaction splits
/// them.
type RedactedHeaders = (Vec<(String, String)>, Vec<(String, String)>);

/// Copy the headers in wire order, replacing sensitive values.
///
/// Returns the text headers and, separately, the non-sensitive headers whose
/// values are valid HTTP bytes but not valid UTF-8 (`obs-text` metadata), as
/// `(name, base64(value))` — substituting a placeholder for those would hand
/// the replayed handler different bytes and manufacture a mismatch.
fn redact_headers(
    headers: &axum::http::HeaderMap,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> RedactedHeaders {
    // A name with *any* obs-text value moves wholesale into the binary list:
    // splitting one name's values across the two lists would lose their
    // relative order (replay appends text headers before binary ones), and
    // `headers.get_all(name)` order is metadata code legitimately reads.
    let mut binary_names: BTreeSet<String> = BTreeSet::new();
    for (name, value) in headers {
        if !header_is_sensitive(name.as_str(), filter) && value.to_str().is_err() {
            binary_names.insert(name.as_str().to_owned());
        }
    }

    let mut out = Vec::with_capacity(headers.len());
    let mut binary = Vec::new();
    for (name, value) in headers {
        let name = name.as_str().to_owned();
        if header_is_sensitive(&name, filter) {
            values.insert(value.as_bytes());
            record_credential_components(&name, value.as_bytes(), values);
            keys.insert(format!("header:{name}"));
            out.push((name, FILTERED_PLACEHOLDER.to_owned()));
        } else if binary_names.contains(&name) {
            binary.push((name, STANDARD.encode(value.as_bytes())));
        } else {
            out.push((name, value.to_str().unwrap_or_default().to_owned()));
        }
    }
    (out, binary)
}

/// Retain the secret *inside* a structured credential header, not just the
/// header value as a whole.
///
/// A handler does not work with `Bearer hunter2` or with a whole `Cookie:`
/// line — it extracts `hunter2`, or one cookie's value, and that is the form
/// that reappears in an error message, a panic payload, or a SQL bind. The
/// full value is already in the echo set; without its components,
/// [`mask_echoes`] (a substring search) never matches what the handler
/// actually held.
///
/// Deliberately narrow: the token after a standard auth scheme, and each
/// cookie value. Cookie *names* are not retained — they are ordinary words
/// (`session`, `theme`) that would shred unrelated prose.
fn record_credential_components(name: &str, value: &[u8], values: &mut RedactedValues) {
    let Ok(text) = std::str::from_utf8(value) else {
        return;
    };
    let trimmed = text.trim();
    // Only headers whose *syntax* this understands. A custom sensitive header
    // named by `filter_parameters` carries whatever its application likes, and
    // reading `password: not valid` as a scheme and a credential would put
    // `valid` in the echo set — masking unrelated prose, and blanking any SQL
    // bind that happens to equal it, which also drops that bind from replay's
    // comparison. Guessing structure is worse here than recording nothing:
    // the whole header value is already retained either way.
    let name = name.to_ascii_lowercase();
    let is_authorization = matches!(name.as_str(), "authorization" | "proxy-authorization");
    let is_cookie = matches!(name.as_str(), "cookie" | "set-cookie");

    // `Authorization: <scheme> <credential>` / `Proxy-Authorization: …`.
    // Any syntactically valid scheme counts, not a list of the familiar ones:
    // `Negotiate`, `AWS4-HMAC-SHA256` and every vendor scheme carry exactly
    // the same risk, and a name this code has not heard of is precisely the
    // case where the credential would go unmasked. A scheme is an RFC 7235
    // token, so anything containing characters a token cannot hold — `=`, `;`,
    // `,` — is not a scheme, which is what keeps a `Cookie:` line from being
    // read as one. The rest of the value is kept whole, so a credential that
    // itself contains spaces (`AWS4-HMAC-SHA256 Credential=…, Signature=…`)
    // stays intact.
    if is_authorization && let Some((scheme, credential)) = trimmed.split_once(' ') {
        let credential = credential.trim();
        if is_auth_scheme(scheme) && !credential.is_empty() {
            values.insert(credential.as_bytes());
            if scheme.eq_ignore_ascii_case("basic") {
                record_basic_credentials(credential, values);
            }
            // A `token68` is one opaque blob, not a `name=value` list — its
            // trailing `=` is Base64 padding. Reading `dTpwdw==` as a param
            // would put a bare `=` in the echo set, and a lone `=` masked as a
            // whole token rewrites `x = y` in any later failure text and
            // blanks any bind equal to it.
            if !is_token68(credential) {
                record_auth_params(credential, values);
            }
        }
    }

    // `Cookie: a=1; b=2` carries one pair per cookie, all of them candidate
    // secrets. `Set-Cookie: session=abc; Path=/; Max-Age=0` carries *one*
    // cookie followed by attributes — and those attribute values are not
    // secrets. Retaining them was actively harmful: whole-token masking
    // matches a standalone `/` or `0`, so `failed at /` and `status 0` would
    // be rewritten in the outcome, and a SQL bind equal to either would be
    // blanked and dropped from replay's comparison.
    //
    // Cookie values are whole-token-only for the same reason auth-param
    // values are: a cookie jar mixes a session token with `theme=dark`, and
    // only the *name* separates them — which is the ranking this code declines
    // to make. Substring-masking `dark` would turn a later `darkness check
    // failed` into `[FILTERED]ness check failed`, and replay, which scrubs
    // with the same set but produces the static message, would call that a
    // mismatch.
    if is_cookie && trimmed.contains('=') {
        let pairs: Vec<&str> = if name == "set-cookie" {
            trimmed.split(';').take(1).collect()
        } else {
            trimmed.split(';').collect()
        };
        for pair in pairs {
            if let Some((_, cookie_value)) = pair.split_once('=') {
                let cookie_value = cookie_value.trim().trim_matches('"');
                if !cookie_value.is_empty() {
                    values.insert_whole_token_only(cookie_value.as_bytes());
                }
            }
        }
    }
}

/// Retain each value of an RFC 7235 auth-param list, not only the list.
///
/// A credential is either a `token68` (`Bearer hunter2`) or a comma-separated
/// `name=value` list — `AWS4-HMAC-SHA256 Credential=…, SignedHeaders=…,
/// Signature=…`, or a `Digest` response. A handler extracts one field from
/// that list, and `Signature=…` alone neither equals nor is contained in the
/// whole credential string, so the list on its own never matches what the
/// handler held.
///
/// This is standardized syntax rather than a guess, which is what separates it
/// from the custom headers this code refuses to parse. What it *cannot* do is
/// tell a secret from metadata: `Signature` and `qop` are both auth-params,
/// and only the names give them away — the same enumeration this code declined
/// to make for schemes. So every value is retained, but as whole-token-only,
/// which is what keeps `qop=auth` from masking the middle of *authentication*
/// in every later failure.
fn record_auth_params(credential: &str, values: &mut RedactedValues) {
    for param in split_auth_params(credential) {
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        // An auth-param name is a token. This rejects the `=` padding of a
        // `token68` credential, whose tail is not a name and whose value is
        // empty anyway.
        if !is_auth_scheme(name.trim()) {
            continue;
        }
        let value = unquote_auth_param(value.trim());
        if !value.is_empty() {
            values.insert_whole_token_only(value.as_bytes());
        }
    }
}

/// Whether `credential` is an RFC 7235 `token68` — the single-blob form of a
/// credential, as `Bearer`, `Basic` and a JWT all use.
///
/// `token68` is `1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`:
/// padding only ever trails. That is what tells it apart from an auth-param
/// list, whose `=` signs sit between a name and a value.
fn is_token68(credential: &str) -> bool {
    let unpadded = credential.trim_end_matches('=');
    !unpadded.is_empty()
        && unpadded.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
}

/// Split an auth-param list on the commas that actually delimit it.
///
/// A `quoted-string` may contain the delimiter — `Digest response="abc,def"`
/// is one param — so splitting on every comma cuts the value in half and
/// retains a fragment the handler never holds, which is the same as retaining
/// nothing.
///
/// Escapes are *tracked* but not resolved: a `\"` must not be mistaken for the
/// end of the quoted string, yet the backslashes have to survive into
/// [`unquote_auth_param`], which is the only place that can tell a syntactic
/// boundary quote from a literal one. Returns owned strings because the panic
/// gate denies the index arithmetic a borrowing split would need.
fn split_auth_params(credential: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in credential.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' && quoted {
            current.push(character);
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
            current.push(character);
        } else if character == ',' && !quoted {
            params.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    params.push(current);
    params
}

/// Strip an auth-param value's *syntactic* quotes and resolve its
/// quoted-pairs, in that order.
///
/// Order is the whole point. Unescaping first and trimming quotes afterwards
/// turns `response="\"hunter2\""` — whose value is literally `"hunter2"` — into
/// `hunter2`, because the trim cannot tell the delimiters it should remove
/// from the literal quotes it must keep. Walking from the opening delimiter
/// and stopping at the first *unescaped* quote answers that question exactly,
/// and a bind byte-equal to what the handler extracted then matches.
///
/// An unquoted value (`qop=auth`) is a token and is returned as it stands.
fn unquote_auth_param(value: &str) -> String {
    let mut characters = value.chars();
    if characters.next() != Some('"') {
        return value.to_owned();
    }
    let mut unquoted = String::new();
    let mut escaped = false;
    for character in characters {
        if escaped {
            unquoted.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            break;
        } else {
            unquoted.push(character);
        }
    }
    unquoted
}

/// Retain what a `Basic` credential *decodes to*, not only its Base64 text.
///
/// `Basic` is the one scheme whose credential has a standardized interior
/// (RFC 7617): a handler never works with `dXNlcjpzZWNyZXQ=` — it decodes,
/// and holds `user:secret` or the password alone. Those are the forms that
/// reappear in an error, a panic payload or a SQL bind, and neither is a
/// substring of the Base64, so [`mask_echoes`] would not match either one.
///
/// The password is retained, and the decoded `user:password` pair as a whole.
/// The *username* is not: it is the same hazard as a cookie name — `admin`
/// or `service` is an ordinary word, and masking it everywhere would shred
/// unrelated prose while protecting nothing that the pair and the password do
/// not already cover.
///
/// Everything here works on **bytes**, deliberately. RFC 7617 leaves the
/// charset open and the historical default is not UTF-8, so a username
/// carrying a legacy-encoded byte would take an ASCII password down with it if
/// the pair had to parse as text first. A handler splits the decoded bytes and
/// holds that password regardless; the echo set is byte-keyed, so a bind equal
/// to it is masked either way, and only free-form text masking — which needs
/// valid UTF-8 to search — quietly skips what it cannot represent.
fn record_basic_credentials(credential: &str, values: &mut RedactedValues) {
    // Not Base64: nothing to split, and the Base64 itself is already in the
    // set.
    let Ok(decoded) = STANDARD.decode(credential) else {
        return;
    };
    // RFC 7617 splits on the *first* colon; a password may contain more.
    let Some(colon) = decoded.iter().position(|byte| *byte == b':') else {
        return;
    };
    let Some(password) = decoded.get(colon.saturating_add(1)..) else {
        return;
    };
    if !password.is_empty() {
        values.insert(password);
    }
    values.insert(&decoded);
}

/// Whether `word` is a syntactically valid authorization scheme — an RFC 7235
/// token.
fn is_auth_scheme(word: &str) -> bool {
    !word.is_empty()
        && word.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Rewrite the request target with sensitive query parameters masked, leaving
/// every other byte exactly as the client sent it.
fn redact_uri(
    uri: &axum::http::Uri,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> String {
    let Some(query) = uri.query() else {
        return uri.to_string();
    };
    let original = uri.to_string();
    let Some(masked) = mask_raw_urlencoded(query, "query", filter, values, keys) else {
        return original;
    };
    // Splice the masked query onto the original prefix rather than rebuilding
    // from `uri.path()`: an absolute-form target
    // (`https://api.example/items?...`) keeps its scheme and authority, so a
    // proxy-style route that reads the full request target sees the same
    // shape during replay.
    let prefix = original.split_once('?').map_or("", |(prefix, _)| prefix);
    format!("{prefix}?{masked}")
}

/// The on-the-wire spelling a masked pair's value is replaced with — the
/// percent-encoded [`FILTERED_PLACEHOLDER`], so the rewritten string is still
/// a well-formed urlencoded pair list and decodes to `[FILTERED]`.
const FILTERED_PLACEHOLDER_URLENCODED: &str = "%5BFILTERED%5D";

/// Mask the sensitive pairs of a raw `key=value&…` string in place, leaving
/// every byte of every other pair exactly as the client sent it.
///
/// Returns `None` when no pair matched the filter: the caller keeps the
/// original representation untouched, so a handler that inspects the raw
/// string — or verifies a signature computed over it — sees the same bytes
/// during replay that it saw in production. Decoding, and the spelling drift
/// it brings (`%2f` → `%2F`, space → `+`, bare `flag` → `flag=`), happens
/// only to *matched* pairs, which are being rewritten anyway.
///
/// A matched pair contributes **both** value spellings to the echo set: the
/// decoded one (what a handler error usually quotes) and the on-the-wire one
/// (what an error echoing the raw request target or body carries).
fn mask_raw_urlencoded(
    raw: &str,
    context: &str,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> Option<String> {
    let mut changed = false;
    let masked: Vec<String> = raw
        .split('&')
        .map(|raw_pair| {
            let Some((decoded_key, decoded_value)) =
                url::form_urlencoded::parse(raw_pair.as_bytes()).next()
            else {
                // An empty segment (`a=1&&b=2`): sloppy, but the client sent
                // it, so it stays.
                return raw_pair.to_owned();
            };
            if !key_is_sensitive(&decoded_key, filter) {
                return raw_pair.to_owned();
            }
            changed = true;
            keys.insert(format!("{context}:{decoded_key}"));
            if !decoded_value.is_empty() {
                values.insert(decoded_value.as_bytes());
            }
            let (raw_key, raw_value) = raw_pair
                .split_once('=')
                .map_or((raw_pair, ""), |(key, value)| (key, value));
            if !raw_value.is_empty() {
                values.insert(raw_value.as_bytes());
            }
            format!("{raw_key}={FILTERED_PLACEHOLDER_URLENCODED}")
        })
        .collect();
    changed.then(|| masked.join("&"))
}

/// Whether a form/query key names a sensitive value.
///
/// Bracket and dot notation are expanded first so `user[password]` matches on
/// its `password` leaf, the same expansion the dev error page applies before
/// scrubbing a form body.
fn key_is_sensitive(key: &str, filter: &ParameterFilter) -> bool {
    key_segments(key)
        .iter()
        .any(|segment| filter.matches_key(segment))
}

/// Split a form key on bracket or dot notation into path segments.
fn key_segments(key: &str) -> Vec<String> {
    if let Some((head, rest)) = key.split_once('[') {
        let mut parts = vec![head.to_owned()];
        for segment in rest.split('[') {
            let segment = segment.trim_end_matches(']');
            if !segment.is_empty() {
                parts.push(segment.to_owned());
            }
        }
        parts
    } else if key.contains('.') {
        key.split('.')
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![key.to_owned()]
    }
}

/// `redacted_keys` entry recording that a body was masked wholesale because it
/// did not parse as the structure it declared.
const UNPARSEABLE_BODY_KEY: &str = "body:<unparseable>";

/// Note recorded when a body declared JSON but did not parse as JSON.
const UNPARSEABLE_JSON_NOTE: &str = "request body declared a JSON content type but did not parse as JSON; it was masked out of \
     the capsule rather than copied verbatim, because there are no keys to redact on";

/// Note recorded when a body declared a multipart form.
const MULTIPART_BODY_NOTE: &str = "request body declared a multipart content type, which this slice does not parse; it was \
     masked out of the capsule rather than copied verbatim, because its fields (and any uploaded \
     file) cannot be redacted without parsing them";

/// Note recorded when a body declared a urlencoded form but did not parse as
/// one.
const UNPARSEABLE_FORM_NOTE: &str = "request body declared a urlencoded form but did not parse as one; it was masked out of the \
     capsule rather than copied verbatim, because there are no keys to redact on";

/// Redact the request body according to its content type.
fn redact_body(
    headers: &axum::http::HeaderMap,
    raw_body: &CapturedBody,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
    notes: &mut Vec<String>,
) -> CapsuleBody {
    let bytes = match raw_body {
        CapturedBody::Absent => return CapsuleBody::Absent,
        CapturedBody::Skipped { declared_len } => {
            return CapsuleBody::Skipped {
                declared_len: *declared_len,
            };
        }
        CapturedBody::Buffered(bytes) => bytes,
    };
    if bytes.is_empty() {
        return CapsuleBody::Absent;
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if content_type.contains("json") {
        // A body that claims to be JSON but is not one — malformed, or the
        // prefix the tap copied before the handler abandoned the read — has no
        // keys for the filter to match, so copying it verbatim would write
        // `{"password":"secret",` straight into the capsule. Mask it instead,
        // and seed the echo set from the raw text so an outcome that quotes
        // the body (or a value inside it) is still scrubbed.
        let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return unparseable_body(bytes, UNPARSEABLE_JSON_NOTE, values, keys, notes);
        };
        let keys_before = keys.len();
        let scrubbed = scrub_value(&parsed, filter, "body", values, keys);
        if keys.len() > keys_before {
            // The echo set holds the *decoded* masked values, but an error
            // that quotes the raw request body carries the on-wire spelling —
            // `"token"` does not contain `token`. Walk the raw text's
            // string literals and retain every spelling whose decoded form
            // was just masked, so `scrub_outcome` catches either.
            retain_raw_json_string_spellings(bytes, values);
        }
        if keys.len() == keys_before {
            // Nothing was masked: keep the exact bytes the client sent.
            // Re-serializing would drift whitespace, number spellings and key
            // order, and a handler that verifies a signature over the raw
            // body would reject the replay of a request it accepted in
            // production. (serde_json just proved the bytes are UTF-8.)
            return std::str::from_utf8(bytes).map_or(CapsuleBody::Absent, |text| {
                CapsuleBody::Text(text.to_owned())
            });
        }
        return serde_json::to_string(&scrubbed).map_or(CapsuleBody::Absent, CapsuleBody::Text);
    }

    if content_type.contains("application/x-www-form-urlencoded") {
        // Same conservatism, one step earlier: the urlencoded parser is lossy
        // and accepts anything, so a JSON document sent under a form content
        // type would come back as one giant key that matches no filter and is
        // then copied verbatim. `form_text` says whether this really is a
        // form before any of it is copied.
        let Some(text) = form_text(bytes) else {
            return unparseable_body(bytes, UNPARSEABLE_FORM_NOTE, values, keys, notes);
        };
        // The masking splices only the matched pairs, so an untouched form —
        // or the untouched neighbours of a masked field — keeps the exact
        // bytes the client sent, and a signature computed over the raw body
        // still verifies during replay.
        return CapsuleBody::Text(
            mask_raw_urlencoded(text, "body", filter, values, keys)
                .unwrap_or_else(|| text.to_owned()),
        );
    }

    // A multipart body *has* key structure — a file upload's form fields are
    // exactly the kind of thing `filter_parameters` names — but this slice does
    // not parse multipart, and copying it verbatim would write every part
    // through unredacted, password field and uploaded file alike. Skip it: the
    // capsule records the length and says why, rather than becoming the one
    // place a submitted secret survives in the clear.
    if content_type.starts_with("multipart/") || content_type.contains("multipart/form-data") {
        return unparseable_body(bytes, MULTIPART_BODY_NOTE, values, keys, notes);
    }

    // Anything else has no key structure to match on, so it is copied
    // verbatim — see the module docs on what redaction does not cover.
    std::str::from_utf8(bytes).map_or_else(
        |_| CapsuleBody::Base64(STANDARD.encode(bytes)),
        |text| CapsuleBody::Text(text.to_owned()),
    )
}

/// Record a body that declared a structure it does not have, without copying
/// any of it.
///
/// Reusing [`CapsuleBody::Skipped`] says the one thing a reader needs to know —
/// the bytes were deliberately not carried, only their length — and replay
/// already handles it by sending an empty body with a warning. The note says
/// *why* this one was skipped.
///
/// Skipping removes the bytes from the *request* record, but a handler that
/// read (part of) the body can still quote it into a 5xx message or a panic
/// payload, and `scrub_outcome` can only mask what the echo set knows about —
/// no key ever matched here, so nothing from this body joined it. Seed the
/// set conservatively instead: the whole raw text, and every string literal
/// in value position (see [`record_string_literal_values`]).
fn unparseable_body(
    bytes: &[u8],
    note: &'static str,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
    notes: &mut Vec<String>,
) -> CapsuleBody {
    keys.insert(UNPARSEABLE_BODY_KEY.to_owned());
    notes.push(note.to_owned());
    if let Ok(text) = std::str::from_utf8(bytes) {
        values.insert(text.as_bytes());
        record_string_literal_values(text, values);
    }
    CapsuleBody::Skipped {
        declared_len: Some(bytes.len()),
    }
}

/// Validate that a body really is a urlencoded form, returning its text.
///
/// `url::form_urlencoded::parse` never fails, so this validates the shape
/// first: UTF-8, every `&`-separated segment a `key=value` pair, and every key
/// made of characters a client would not have had to percent-encode. Rejecting
/// costs a captured body; accepting a non-form costs an unredactable copy of
/// it, so the doubt is resolved towards rejecting.
fn form_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut pairs = 0usize;
    for segment in text.split('&') {
        // A trailing or doubled separator is sloppy but still a form.
        if segment.is_empty() {
            continue;
        }
        let (key, _) = segment.split_once('=')?;
        if key.is_empty() || key.contains(is_not_form_key_char) {
            return None;
        }
        pairs = pairs.saturating_add(1);
    }
    if pairs == 0 {
        return None;
    }
    Some(text)
}

/// Whether a character disqualifies the string it appears in from being a
/// urlencoded form key.
///
/// Deliberately narrow: it names the characters a form key cannot carry
/// unencoded (structural JSON punctuation, quotes, whitespace, controls)
/// rather than trying to enumerate the ones it can, so unencoded-but-harmless
/// keys such as `user[email]` still pass.
const fn is_not_form_key_char(c: char) -> bool {
    c.is_ascii_control()
        || c.is_whitespace()
        || matches!(c, '{' | '}' | '"' | '\'' | '<' | '>' | '\\')
}

/// Recursively mask sensitive leaves of a JSON body, recording what was hit.
fn scrub_value(
    value: &serde_json::Value,
    filter: &ParameterFilter,
    path: &str,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let child_path = format!("{path}.{key}");
                if filter.matches_key(key) {
                    record_masked_value(child, values);
                    keys.insert(child_path);
                    out.insert(
                        key.clone(),
                        serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned()),
                    );
                } else {
                    out.insert(
                        key.clone(),
                        scrub_value(child, filter, &child_path, values, keys),
                    );
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| scrub_value(item, filter, path, values, keys))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Retain the bytes of a masked JSON leaf so a bind echoing it is masked too.
fn record_masked_value(value: &serde_json::Value, values: &mut RedactedValues) {
    match value {
        serde_json::Value::String(text) => values.insert(text.as_bytes()),
        serde_json::Value::Null => {}
        // A masked container's *leaves* are what a handler extracts, echoes,
        // or binds — `{"value":"hunter2"}` in the set would never match the
        // `hunter2` an error quotes or an INSERT binds. Keep the container's
        // serialization (an error may echo the whole thing) and recurse so
        // every scalar beneath the matched key is retained too. Depth is
        // bounded by serde_json's own recursion limit at parse time.
        serde_json::Value::Object(map) => {
            values.insert(value.to_string().as_bytes());
            for child in map.values() {
                record_masked_value(child, values);
            }
        }
        serde_json::Value::Array(items) => {
            values.insert(value.to_string().as_bytes());
            for child in items {
                record_masked_value(child, values);
            }
        }
        other => values.insert(other.to_string().as_bytes()),
    }
}

/// Record the decoded form (and escaped spelling) of every string literal in
/// *value* position of malformed JSON-like text.
///
/// Best-effort seeding for a body that declared a structure it does not have:
/// the parser refused it, so the filter never saw its keys, but the secrets
/// are still in there and a handler may echo one into the outcome. A literal
/// followed by `:` is a key and is skipped — field names are ordinary words,
/// and masking them would shred outcome prose like `password rejected` —
/// while every other literal is treated as a value worth masking wherever it
/// is echoed. Text ending *inside* a literal (the truncated-tap case, and the
/// most likely place for a secret to sit) records the unterminated remainder.
/// Escape handling mirrors [`retain_raw_json_string_spellings`].
fn record_string_literal_values(text: &str, values: &mut RedactedValues) {
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let Some(after_quote) = rest.get(start.saturating_add(1)..) else {
            return;
        };
        // Find the closing quote, skipping escaped characters.
        let mut end = None;
        let mut escape = false;
        for (index, c) in after_quote.char_indices() {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                end = Some(index);
                break;
            }
        }
        let Some(end) = end else {
            if !after_quote.is_empty() {
                values.insert(after_quote.as_bytes());
            }
            return;
        };
        let Some(inner) = after_quote.get(..end) else {
            return;
        };
        let Some(after_literal) = after_quote.get(end.saturating_add(1)..) else {
            return;
        };
        let is_key = after_literal.trim_start().starts_with(':');
        if !is_key && !inner.is_empty() {
            let literal = format!("\"{inner}\"");
            if let Ok(decoded) = serde_json::from_str::<String>(&literal) {
                values.insert(decoded.as_bytes());
                if inner.contains('\\') {
                    values.insert(inner.as_bytes());
                }
            } else {
                // A literal serde cannot decode (a broken escape) is retained
                // as spelled — that is the form an echo would carry.
                values.insert(inner.as_bytes());
            }
        }
        rest = after_literal;
    }
}

/// Retain the on-wire spelling of every raw JSON string literal whose decoded
/// value redaction just masked.
///
/// A masked value's decoded form is in the echo set, but the client may have
/// spelled it with escapes — `"token"`, `"line\nbreak"` — and an error
/// that quotes the *raw* body carries that spelling, which a search for the
/// decoded bytes cannot find. This walks the raw text's `"`-delimited
/// literals (honouring backslash escapes), decodes each through serde, and
/// retains the literal's inner bytes whenever the decoded value is already in
/// the set and the spellings differ.
fn retain_raw_json_string_spellings(bytes: &[u8], values: &mut RedactedValues) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let Some(after_quote) = rest.get(start.saturating_add(1)..) else {
            return;
        };
        // Find the closing quote, skipping escaped characters.
        let mut end = None;
        let mut escape = false;
        for (index, c) in after_quote.char_indices() {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                end = Some(index);
                break;
            }
        }
        let Some(end) = end else {
            return;
        };
        let Some(inner) = after_quote.get(..end) else {
            return;
        };
        if inner.contains('\\') {
            // Only escaped spellings can differ from their decoded form.
            let literal = format!("\"{inner}\"");
            if let Ok(decoded) = serde_json::from_str::<String>(&literal)
                && values.contains(decoded.as_bytes())
            {
                values.insert(inner.as_bytes());
            }
        }
        let Some(next) = after_quote.get(end.saturating_add(1)..) else {
            return;
        };
        rest = next;
    }
}

#[cfg(test)]
mod tests {
    // ── Effect-tape redaction (#1634) ───────────────────────────────────

    mod effects {
        use super::super::*;
        use crate::capsule::schema::{
            CacheEffect, CapsuleEffects, CapsuleJob, HttpEffect, JobEffect, MailEffect,
            TenantEffect,
        };

        fn filter() -> ParameterFilter {
            ParameterFilter::new(&["api_key".to_owned(), "email".to_owned()], &[])
        }

        fn exchange(url: &str) -> HttpEffect {
            HttpEffect {
                method: "POST".to_owned(),
                url: url.to_owned(),
                request_headers: vec![
                    ("content-type".to_owned(), "application/json".to_owned()),
                    ("x-api-key".to_owned(), "sk-live-42".to_owned()),
                ],
                request_body: CapsuleBody::Text(
                    r#"{"api_key":"sk-live-42","amount":10}"#.to_owned(),
                ),
                status: 502,
                response_headers: vec![(
                    "set-cookie".to_owned(),
                    "session=abc; HttpOnly".to_owned(),
                )],
                response_body: CapsuleBody::Text(r#"{"api_key":"sk-live-42"}"#.to_owned()),
                error: None,
                ..Default::default()
            }
        }

        fn redact(effects: &mut CapsuleEffects) -> BTreeSet<String> {
            let mut values = RedactedValues::default();
            let mut keys = BTreeSet::new();
            redact_effects(effects, None, &filter(), &mut values, &mut keys);
            keys
        }

        #[test]
        fn an_outbound_credential_header_is_masked_whatever_the_app_filter_says() {
            // `x-api-key` is in no default inbound filter list, but on the
            // outbound seam the credential is the *application's* and the
            // operator never sees the header in a log to think to name it.
            let mut effects = CapsuleEffects {
                http: vec![exchange("https://api.example/charge")],
                ..CapsuleEffects::default()
            };
            let keys = redact(&mut effects);
            let exchange = effects.http.first().expect("one exchange");
            assert!(
                exchange
                    .request_headers
                    .iter()
                    .any(|(name, value)| name == "x-api-key" && value == "[FILTERED]"),
                "{:?}",
                exchange.request_headers
            );
            assert!(
                exchange
                    .response_headers
                    .iter()
                    .any(|(name, value)| name == "set-cookie" && value == "[FILTERED]"),
                "a Set-Cookie on the response half carries a credential too: {:?}",
                exchange.response_headers
            );
            assert!(
                keys.iter()
                    .any(|key| key.contains("http[0].request_header:x-api-key")),
                "{keys:?}"
            );
        }

        #[test]
        fn an_outbound_url_query_is_masked_by_parameter_name() {
            let mut effects = CapsuleEffects {
                http: vec![exchange(
                    "https://api.example/charge?api_key=sk-live-42&page=2",
                )],
                ..CapsuleEffects::default()
            };
            redact(&mut effects);
            let url = &effects.http.first().expect("one exchange").url;
            assert!(!url.contains("sk-live-42"), "{url}");
            assert!(
                url.contains("page=2"),
                "the rest of the query survives: {url}"
            );
        }

        #[test]
        fn outbound_json_bodies_are_masked_by_key_on_both_halves() {
            let mut effects = CapsuleEffects {
                http: vec![exchange("https://api.example/charge")],
                ..CapsuleEffects::default()
            };
            redact(&mut effects);
            let exchange = effects.http.first().expect("one exchange");
            let serialized = serde_json::to_string(exchange).expect("serializes");
            assert!(
                !serialized.contains("sk-live-42"),
                "no half of the exchange may carry the credential: {serialized}"
            );
            let CapsuleBody::Text(request_body) = &exchange.request_body else {
                panic!("expected a text body, got {:?}", exchange.request_body);
            };
            assert!(
                request_body.contains(r#""amount":10"#),
                "unfiltered fields survive: {request_body}"
            );
        }

        #[test]
        fn job_payloads_cache_values_and_the_job_entry_are_masked_by_key() {
            use base64::Engine as _;
            let secret = STANDARD.encode(br#"{"api_key":"sk-live-42"}"#);
            let mut effects = CapsuleEffects {
                jobs: vec![JobEffect {
                    name: "notify".to_owned(),
                    payload: serde_json::json!({"api_key": "sk-live-42", "order": 7}),
                    delay_secs: None,
                    due_at: None,
                    error: None,
                }],
                cache: vec![CacheEffect::Insert {
                    key: "creds".to_owned(),
                    value: secret,
                    ttl_secs: None,
                }],
                ..CapsuleEffects::default()
            };
            let mut entry = CapsuleJob {
                name: "notify".to_owned(),
                payload: serde_json::json!({"api_key": "sk-live-42"}),
            };
            let mut values = RedactedValues::default();
            let mut keys = BTreeSet::new();
            redact_effects(
                &mut effects,
                Some(&mut entry),
                &filter(),
                &mut values,
                &mut keys,
            );

            let serialized = serde_json::to_string(&effects).expect("serializes");
            assert!(!serialized.contains("sk-live-42"), "{serialized}");
            assert!(
                !serde_json::to_string(&entry)
                    .expect("serializes")
                    .contains("sk-live-42"),
                "a job capsule's own arguments are input, and are masked like any other"
            );
            let CacheEffect::Insert { value, .. } = effects.cache.first().expect("one entry")
            else {
                panic!("expected an insert, got {:?}", effects.cache.first());
            };
            let decoded = STANDARD.decode(value.as_bytes()).expect("base64");
            assert!(
                !String::from_utf8_lossy(&decoded).contains("sk-live-42"),
                "cache values are masked inside their base64 envelope"
            );
        }

        #[test]
        fn a_value_masked_out_of_the_request_is_masked_wherever_an_effect_echoes_it() {
            // The two-pass design: the filter pass seeds the echo set, the echo
            // pass sweeps everything free-form — including seams whose own keys
            // never matched the filter.
            let mut values = RedactedValues::default();
            values.insert(b"hunter2secret");
            let mut effects = CapsuleEffects {
                jobs: vec![JobEffect {
                    name: "notify".to_owned(),
                    // `pw` is in no filter list.
                    payload: serde_json::json!({"pw": "hunter2secret"}),
                    delay_secs: None,
                    due_at: None,
                    error: None,
                }],
                mail: vec![MailEffect {
                    to: vec!["hunter2secret@example.com".to_owned()],
                    from: None,
                    subject: "hunter2secret".to_owned(),
                    body: CapsuleBody::Text("your key is hunter2secret".to_owned()),
                    error: None,
                    ..Default::default()
                }],
                tenant: Some(TenantEffect {
                    id: Some("hunter2secret".to_owned()),
                }),
                ..CapsuleEffects::default()
            };
            let mut keys = BTreeSet::new();
            redact_effects(&mut effects, None, &filter(), &mut values, &mut keys);

            let serialized = serde_json::to_string(&effects).expect("serializes");
            assert!(
                !serialized.contains("hunter2secret"),
                "a value redaction already removed must not reappear on another seam: \
                 {serialized}"
            );
        }

        #[test]
        fn an_unparseable_json_effect_body_is_replaced_rather_than_copied() {
            let mut effects = CapsuleEffects {
                http: vec![HttpEffect {
                    request_body: CapsuleBody::Text(r#"{"api_key":"sk-live-42","#.to_owned()),
                    ..exchange("https://api.example/charge")
                }],
                ..CapsuleEffects::default()
            };
            redact(&mut effects);
            let body = &effects.http.first().expect("one exchange").request_body;
            assert_eq!(
                body,
                &CapsuleBody::Text("[FILTERED]".to_owned()),
                "a body whose keys cannot be read cannot be shown to be safe"
            );
        }
    }

    use super::*;
    use axum::http::{Request, header};

    fn filter_with(extra: &[&str]) -> ParameterFilter {
        let extra: Vec<String> = extra.iter().map(|k| (*k).to_owned()).collect();
        ParameterFilter::new(&extra, &[])
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "fixture bodies read better handed over than borrowed at every call site"
    )]
    fn redact(
        builder: axum::http::request::Builder,
        body: CapturedBody,
        filter: &ParameterFilter,
    ) -> (CapsuleRequest, RedactedValues) {
        let (request, values, _) = redact_with_notes(builder, body, filter);
        (request, values)
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "fixture bodies read better handed over than borrowed at every call site"
    )]
    fn redact_with_notes(
        builder: axum::http::request::Builder,
        body: CapturedBody,
        filter: &ParameterFilter,
    ) -> (CapsuleRequest, RedactedValues, Vec<String>) {
        let request = builder.body(()).expect("test request builds");
        let (parts, ()) = request.into_parts();
        let raw = RawRequest {
            method: parts.method.as_str().to_owned(),
            uri: parts.uri,
            version: parts.version,
            headers: parts.headers,
            route: None,
        };
        redact_request(&raw, &body, filter)
    }

    fn header_value<'a>(request: &'a CapsuleRequest, name: &str) -> &'a str {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map_or_else(
                || panic!("header {name} must be present in the capsule"),
                |(_, value)| value.as_str(),
            )
    }

    /// A multipart body is the one shape that carries both form fields and
    /// file contents, and this slice cannot parse it — so it must never be
    /// copied. Copying it verbatim made the capsule the one place a submitted
    /// password survived unmasked.
    #[test]
    fn a_multipart_body_is_never_copied_into_the_capsule() {
        let body = "--X\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\n\
                    hunter2-in-the-clear\r\n--X--\r\n";
        let (request, _values, notes) = redact_with_notes(
            Request::post("/upload")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=X"),
            CapturedBody::Buffered(Bytes::from_static(body.as_bytes())),
            &filter_with(&[]),
        );

        match &request.body {
            CapsuleBody::Skipped { declared_len } => {
                assert_eq!(*declared_len, Some(body.len()));
            }
            other => panic!("a multipart body must be skipped, got {other:?}"),
        }
        let rendered = serde_json::to_string(&request).expect("request serializes");
        assert!(
            !rendered.contains("hunter2-in-the-clear") && !rendered.contains("password"),
            "no part of a multipart body may reach the capsule: {rendered}"
        );
        assert!(
            notes.iter().any(|note| note.contains("multipart")),
            "the capsule must say why the body is missing, got {notes:?}"
        );
    }

    /// The echo set has no length floor: a three-character CVV the filter
    /// removed from the request must not travel on in the tape. The floor
    /// belongs to substring masking of prose, and to nothing else.
    #[test]
    fn a_short_masked_value_still_masks_its_bind() {
        let (_request, values) = redact(
            Request::post("/pay").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(b"{\"cvv\":\"123\",\"amount\":10}")),
            &filter_with(&["cvv"]),
        );

        let mut binds = vec![
            BindValue::Value(b"123".to_vec()),
            BindValue::Value(b"10".to_vec()),
        ];
        mask_binds(&mut binds, &values);
        assert_eq!(
            binds.first(),
            Some(&BindValue::Masked),
            "a short value the filter removed must still be masked out of the binds"
        );
        assert_eq!(
            binds.get(1),
            Some(&BindValue::Value(b"10".to_vec())),
            "unrelated binds are untouched"
        );

        // …but short values are still not hunted for inside free-form prose,
        // where they would shred unrelated words.
        assert_eq!(
            mask_echoes("the 123rd attempt", &values),
            "the 123rd attempt",
            "substring masking keeps its length floor"
        );
    }

    #[test]
    fn proxy_authorization_is_masked_unconditionally() {
        let (request, values) = redact(
            Request::get("/x").header("proxy-authorization", "Basic cHJveHk6c2VjcmV0cGFzcw=="),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert_eq!(
            header_value(&request, "proxy-authorization"),
            FILTERED_PLACEHOLDER,
            "a standard credential header must not depend on app config to be masked"
        );
        assert!(
            values.contains(b"Basic cHJveHk6c2VjcmV0cGFzcw=="),
            "the masked value must join the echo set"
        );
    }

    /// A handler does not hold `Bearer hunter2secret` — it holds the token.
    /// The whole header value alone in the echo set never matches the form
    /// that reaches an error message or a SQL bind.
    #[test]
    fn credential_components_join_the_echo_set() {
        let (_request, values) = redact(
            Request::get("/")
                .header(header::AUTHORIZATION, "Bearer hunter2secret")
                .header(header::COOKIE, "session=sess-abcdef; theme=dark"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            values.contains(b"Bearer hunter2secret"),
            "the whole header value is still retained"
        );
        assert!(
            values.contains(b"hunter2secret"),
            "the token after the auth scheme must be retained on its own"
        );
        assert!(
            values.contains(b"sess-abcdef"),
            "each cookie value must be retained on its own"
        );
        assert!(
            !values.contains(b"session"),
            "cookie names are ordinary words and must stay out of the echo set"
        );
        // A cookie jar mixes a session token with ordinary preferences, and
        // only the name separates them — so values are whole-token-only, like
        // auth-params.
        assert_eq!(
            mask_echoes("darkness check failed", &values),
            "darkness check failed",
            "a `theme=dark` cookie must not shred words that merely contain it"
        );
        assert_eq!(
            mask_echoes("theme dark rejected", &values),
            format!("theme {FILTERED_PLACEHOLDER} rejected"),
            "it is still masked where it stands as a whole token"
        );
        // `Set-Cookie` attributes are not secrets, and retaining them is worse
        // than useless: whole-token masking matches a standalone `/` or `0`.
        let (_request, set_cookie) = redact(
            Request::get("/").header("set-cookie", "session=abc-secret; Path=/; Max-Age=0"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            set_cookie.contains(b"abc-secret"),
            "the cookie value counts"
        );
        assert!(!set_cookie.contains(b"/"), "`Path=/` is an attribute");
        assert!(!set_cookie.contains(b"0"), "`Max-Age=0` is an attribute");
        assert_eq!(
            mask_echoes("failed at / with status 0", &set_cookie),
            "failed at / with status 0",
            "attribute values must not rewrite unrelated outcome text"
        );

        // `Basic` is the one scheme whose credential has a standardized
        // interior: a handler decodes it, so the Base64 text alone never
        // matches what the handler actually held.
        let (_request, basic) = redact(
            // base64("alice:hunter2:pass") — a password containing a colon.
            Request::get("/").header(header::AUTHORIZATION, "Basic YWxpY2U6aHVudGVyMjpwYXNz"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            basic.contains(b"YWxpY2U6aHVudGVyMjpwYXNz"),
            "the Base64 credential is still retained"
        );
        assert!(
            basic.contains(b"alice:hunter2:pass"),
            "the decoded pair must be retained"
        );
        assert!(
            basic.contains(b"hunter2:pass"),
            "the password must be retained on its own, splitting on the first colon only"
        );
        assert!(
            !basic.contains(b"alice"),
            "the username is an ordinary word, like a cookie name"
        );
        // RFC 7617 leaves the charset open, so a legacy-encoded username must
        // not take an ASCII password down with it. base64(b"\xffuser:hunter2").
        let (_request, latin1) = redact(
            Request::get("/").header(header::AUTHORIZATION, "Basic /3VzZXI6aHVudGVyMg=="),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            latin1.contains(b"hunter2"),
            "a valid password must survive a username that is not UTF-8"
        );
        assert!(
            latin1.contains(b"\xffuser:hunter2".as_slice()),
            "the decoded pair is retained as bytes, not as text"
        );

        // Anything that is not Base64 of `user:password` contributes nothing
        // beyond the whole value — no panic, no half-parsed component.
        let (_request, not_basic) = redact(
            Request::get("/").header(header::AUTHORIZATION, "Basic not-base64!!"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            not_basic.contains(b"not-base64!!"),
            "the token still counts"
        );

        // A scheme this code has never heard of is exactly where a credential
        // would otherwise go unmasked.
        assert!(is_auth_scheme("Negotiate") && is_auth_scheme("AWS4-HMAC-SHA256"));
        assert!(
            !is_auth_scheme("session=abc;"),
            "a cookie line is not an auth scheme"
        );
        // The point of all of it: the outcome is scrubbed of what the handler
        // actually carried.
        let masked = mask_echoes("token hunter2secret was rejected", &values);
        assert!(!masked.contains("hunter2secret"), "{masked}");
    }

    /// An RFC 7235 credential can be a comma-separated `name=value` list, and
    /// a handler extracts one field from it — a field that neither equals nor
    /// is contained in the whole credential string, so the list alone never
    /// matches what the handler held.
    ///
    /// The list mixes secrets with metadata and only the *names* tell them
    /// apart, which is the enumeration this code declines to make. Retaining
    /// every value is safe only because they are masked as whole tokens.
    #[test]
    fn auth_param_values_are_masked_only_as_whole_tokens() {
        let (_request, sigv4) = redact(
            Request::get("/").header(
                header::AUTHORIZATION,
                "AWS4-HMAC-SHA256 Credential=AKIA/20260815/us-east-1/s3/aws4_request, \
                 SignedHeaders=host;x-amz-date, Signature=abc123deadbeef",
            ),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            sigv4.contains(b"abc123deadbeef"),
            "the signature a handler extracts must be retained on its own"
        );
        assert_eq!(
            mask_echoes("signature abc123deadbeef rejected", &sigv4),
            format!("signature {FILTERED_PLACEHOLDER} rejected"),
            "an auth-param value quoted back must still be masked"
        );

        let (_request, digest) = redact(
            Request::get("/").header(
                header::AUTHORIZATION,
                r#"Digest username="alice", qop=auth, response="deadbeef""#,
            ),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            digest.contains(b"deadbeef"),
            "the digest response is retained"
        );
        assert_eq!(
            mask_echoes("authentication failed during reauthorization", &digest),
            "authentication failed during reauthorization",
            "`qop=auth` must not shred every later word containing `auth`"
        );
        assert_eq!(
            mask_echoes("scheme auth rejected", &digest),
            format!("scheme {FILTERED_PLACEHOLDER} rejected"),
            "it is still masked where it stands as a whole token"
        );
        // `-` and `_` sit *inside* identifiers, so they do not end a token:
        // `auth-error` is one word, and rewriting it would shred the static
        // messages replay compares against.
        assert_eq!(
            mask_echoes("auth-error and auth_error raised", &digest),
            "auth-error and auth_error raised",
            "identifier punctuation does not make a value stand alone"
        );
        // A dot joins only when flanked: `api.auth.error` is one dotted name,
        // but a trailing dot is a full stop and must not shield the secret.
        assert_eq!(
            mask_echoes("api.auth.error raised", &digest),
            "api.auth.error raised",
            "a dot between alphanumerics joins the name"
        );
        assert_eq!(
            mask_echoes("scheme was auth.", &digest),
            format!("scheme was {FILTERED_PLACEHOLDER}."),
            "a sentence-ending dot must not leave the secret unmasked"
        );
    }

    /// A `quoted-string` may hold the very characters the list is delimited
    /// by. Splitting on every comma keeps a fragment the handler never holds,
    /// which protects nothing.
    #[test]
    fn quoted_auth_param_values_survive_commas_and_escapes() {
        let (_request, values) = redact(
            Request::get("/").header(
                header::AUTHORIZATION,
                r#"Digest response="abc,def", opaque="gh\"ij", qop=auth"#,
            ),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            values.contains(b"abc,def"),
            "a comma inside a quoted value does not end the param"
        );
        assert!(
            !values.contains(b"abc"),
            "the fragment before the comma is not what the handler holds"
        );
        assert!(
            values.contains(br#"gh"ij"#),
            "a quoted-pair is resolved to the character the handler sees"
        );
        assert!(
            values.contains(b"auth"),
            "an unquoted param after a quoted one is still found"
        );

        // A value whose own first and last characters are escaped quotes. The
        // handler extracts `"hunter2"`, quotes included, so that is what a
        // bind will carry.
        let (_request, bounded) = redact(
            Request::get("/").header(header::AUTHORIZATION, r#"Digest response="\"hunter2\"""#),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            bounded.contains(br#""hunter2""#),
            "literal quotes at the value's boundary are part of the value"
        );
    }

    /// A `token68` credential is one blob whose trailing `=` is padding, not a
    /// `name=value` list. Reading it as one invents a bare `=` secret.
    #[test]
    fn token68_padding_is_not_read_as_an_auth_param() {
        let (_request, values) = redact(
            // base64("u:pw") — two padding characters.
            Request::get("/").header(header::AUTHORIZATION, "Basic dTpwdw=="),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            values.contains(b"dTpwdw=="),
            "the credential itself is still retained"
        );
        assert!(
            !values.contains(b"="),
            "padding must not become a secret of its own"
        );
        assert_eq!(
            mask_echoes("x = y", &values),
            "x = y",
            "an invented `=` secret would rewrite ordinary text"
        );
    }

    /// A header value is not required to be UTF-8: `obs-text` is legal inside
    /// a `quoted-string`, so one stray byte in a `Digest` username used to
    /// discard *every* component of the header — including parameter values
    /// that are plain ASCII and independently parseable. The handler extracts
    /// those, echoes them and binds them, and nothing in the echo set matched.
    #[test]
    fn a_non_utf8_header_byte_does_not_hide_the_ascii_components() {
        // `Digest username="<0xff>alice", response="deadbeef", nonce=cafebabe`
        let mut credential = br#"username=""#.to_vec();
        credential.push(0xFF);
        credential
            .extend_from_slice(br#"alice", response="deadbeef", nonce=cafebabe, qop=auth"#);
        let mut header = b"Digest ".to_vec();
        header.extend_from_slice(&credential);
        let (_request, values) = redact(
            Request::get("/").header(
                header::AUTHORIZATION,
                axum::http::HeaderValue::from_bytes(&header).expect("obs-text is legal"),
            ),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            values.contains(&credential),
            "the credential after the scheme is still retained whole, as bytes"
        );
        assert!(
            values.contains(b"deadbeef"),
            "an ASCII param value must survive a non-UTF-8 byte elsewhere in the header"
        );
        assert!(
            values.contains(b"cafebabe"),
            "an unquoted param value after the non-UTF-8 one is still found"
        );
        assert!(
            values.contains(b"auth"),
            "every remaining param value is recorded, as before"
        );
        assert!(
            values.contains(b"\xffalice".as_slice()),
            "the non-UTF-8 param value itself is retained as bytes, for bind masking"
        );
        assert!(
            !values.contains(b"username"),
            "param names are not values and must stay out of the echo set"
        );

        // The point of all of it: what the handler actually extracted is
        // masked out of a bind and out of the outcome text.
        let mut binds = vec![
            BindValue::Value(b"deadbeef".to_vec()),
            BindValue::Value(b"\xffalice".to_vec()),
            BindValue::Value(b"unrelated".to_vec()),
        ];
        mask_binds(&mut binds, &values);
        assert_eq!(
            binds.first(),
            Some(&BindValue::Masked),
            "a bind byte-equal to an ASCII param value is masked"
        );
        assert_eq!(
            binds.get(1),
            Some(&BindValue::Masked),
            "a bind byte-equal to the non-UTF-8 param value is masked too"
        );
        assert_eq!(
            binds.get(2),
            Some(&BindValue::Value(b"unrelated".to_vec())),
            "unrelated binds are untouched"
        );
        assert_eq!(
            mask_echoes("response deadbeef rejected", &values),
            format!("response {FILTERED_PLACEHOLDER} rejected"),
            "the extracted param value is scrubbed from the outcome"
        );

        // The same gap closed for cookies: one obs-text cookie must not take
        // the session token beside it down with it.
        let mut cookie = b"pref=".to_vec();
        cookie.push(0xFF);
        cookie.extend_from_slice(b"dark; session=sess-abcdef");
        let (_request, jar) = redact(
            Request::get("/").header(
                header::COOKIE,
                axum::http::HeaderValue::from_bytes(&cookie).expect("obs-text is legal"),
            ),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            jar.contains(b"sess-abcdef"),
            "an ASCII cookie value must survive a non-UTF-8 cookie beside it"
        );
        assert!(
            jar.contains(b"\xffdark".as_slice()),
            "the non-UTF-8 cookie value is retained as bytes"
        );
    }

    /// Percent-encoding a cookie value is a common convention — Autumn itself
    /// percent-decodes its own `autumn_time_zone` cookie before use — so a
    /// handler holds `abc/def` where the wire carried `abc%2Fdef`. Recording
    /// only the wire spelling matches neither the bind nor the echo.
    ///
    /// `mask_raw_urlencoded` already records both spellings for form and query
    /// values; cookie values follow it, staying whole-token-only.
    #[test]
    fn cookie_values_record_both_percent_spellings() {
        let (_request, values) = redact(
            Request::get("/").header(header::COOKIE, "session=abc%2Fdef; theme=dark"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            values.contains(b"abc%2Fdef"),
            "the on-the-wire spelling is still recorded"
        );
        assert!(
            values.contains(b"abc/def"),
            "the percent-decoded spelling is what a decoding handler holds"
        );
        assert!(
            values.contains(b"dark"),
            "a value with nothing to decode is recorded exactly once, unchanged"
        );

        let mut binds = vec![
            BindValue::Value(b"abc/def".to_vec()),
            BindValue::Value(b"abc%2Fdef".to_vec()),
        ];
        mask_binds(&mut binds, &values);
        assert_eq!(
            binds.first(),
            Some(&BindValue::Masked),
            "a bind carrying the decoded spelling is masked"
        );
        assert_eq!(
            binds.get(1),
            Some(&BindValue::Masked),
            "a bind carrying the raw spelling is masked"
        );
        assert_eq!(
            mask_echoes("cookie abc/def was rejected", &values),
            format!("cookie {FILTERED_PLACEHOLDER} was rejected"),
            "the decoded spelling is scrubbed where it stands as a whole token"
        );

        // Decoding stays whole-token-only, like every other cookie value: it
        // must not shred a word that merely contains it.
        assert_eq!(
            mask_echoes("darkness check failed", &values),
            "darkness check failed",
            "the decoded spellings inherit the whole-token classification"
        );

        // `Set-Cookie` carries one cookie then attributes, and the decoded
        // spelling follows the same one-pair rule.
        let (_request, set_cookie) = redact(
            Request::get("/").header("set-cookie", "session=a%2Fb; Path=%2Fadmin; Max-Age=0"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(set_cookie.contains(b"a/b"), "the cookie value decodes");
        assert!(
            !set_cookie.contains(b"/admin"),
            "an attribute value is not a secret, decoded or not"
        );

        // A percent-escape that decodes to bytes which are not UTF-8 is still
        // recorded: a bind carrying them is exactly the echo this exists for.
        let (_request, binary) = redact(
            Request::get("/").header(header::COOKIE, "session=raw%FFtail"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert!(
            binary.contains(b"raw\xfftail".as_slice()),
            "a decoded value that is not UTF-8 is still recorded, for bind masking"
        );
    }

    /// The same bytes can arrive both ways — a filtered body password the
    /// client also sent as a `Digest` parameter. The request carried it in its
    /// own right, so it needs full substring masking either way round.
    #[test]
    fn a_direct_insert_outranks_the_whole_token_classification() {
        for direct_first in [true, false] {
            let mut values = RedactedValues::default();
            if direct_first {
                values.insert(b"hunter2");
                values.insert_whole_token_only(b"hunter2");
            } else {
                values.insert_whole_token_only(b"hunter2");
                values.insert(b"hunter2");
            }

            assert_eq!(
                mask_echoes("token hunter2suffix rejected", &values),
                format!("token {FILTERED_PLACEHOLDER}suffix rejected"),
                "a directly captured secret is masked even mid-token (direct_first: {direct_first})"
            );
        }
    }

    /// Each needle is matched against the original text, never against output
    /// an earlier needle produced.
    #[test]
    fn masking_does_not_rewrite_the_placeholders_it_just_wrote() {
        let mut values = RedactedValues::default();
        values.insert(b"hunter2");
        // A secret that happens to be a substring of the placeholder itself.
        values.insert(b"FILTER");

        let masked = mask_echoes("login hunter2 failed", &values);

        assert_eq!(
            masked,
            format!("login {FILTERED_PLACEHOLDER} failed"),
            "the placeholder written for one secret must not be rewritten by another"
        );
    }

    /// A custom sensitive header carries whatever its application likes, so
    /// reading auth or cookie syntax into it invents secrets: `password: not
    /// valid` would put `valid` in the echo set, masking unrelated prose and
    /// blanking any SQL bind equal to it (which drops that bind from replay's
    /// comparison too).
    #[test]
    fn component_parsing_is_limited_to_headers_whose_syntax_is_known() {
        let (_request, values) = redact(
            Request::get("/").header("password", "not valid"),
            CapturedBody::Absent,
            &filter_with(&["password"]),
        );

        assert!(
            values.contains(b"not valid"),
            "the whole value is still retained, as for any masked header"
        );
        assert!(
            !values.contains(b"valid"),
            "a custom header's value must not be split as though it were `Authorization`"
        );
        assert_eq!(
            mask_echoes("the token was invalid", &values),
            "the token was invalid",
            "an invented component would have shredded this"
        );
    }

    #[test]
    fn authorization_and_cookie_headers_are_masked() {
        let (request, values) = redact(
            Request::get("/x")
                .header(header::AUTHORIZATION, "Bearer super-secret-token")
                .header(header::COOKIE, "session=abcdef123456")
                .header(header::ACCEPT, "application/json"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert_eq!(
            header_value(&request, "authorization"),
            FILTERED_PLACEHOLDER
        );
        assert_eq!(header_value(&request, "cookie"), FILTERED_PLACEHOLDER);
        assert_eq!(
            header_value(&request, "accept"),
            "application/json",
            "non-sensitive headers must survive verbatim"
        );
        assert!(
            request
                .redacted_keys
                .contains(&"header:authorization".to_owned()),
            "redacted_keys must name what was masked, got {:?}",
            request.redacted_keys
        );
        assert!(
            values.contains(b"Bearer super-secret-token"),
            "the pre-mask header value must be retained for bind masking"
        );
    }

    #[test]
    fn set_cookie_and_configured_sensitive_params_are_masked() {
        let (request, _) = redact(
            Request::get("/x")
                .header("set-cookie", "a=b")
                .header("x-tenant-pin", "4321"),
            CapturedBody::Absent,
            &filter_with(&["x-tenant-pin"]),
        );

        assert_eq!(header_value(&request, "set-cookie"), FILTERED_PLACEHOLDER);
        assert_eq!(
            header_value(&request, "x-tenant-pin"),
            FILTERED_PLACEHOLDER,
            "a key added via [log] filter_parameters must be masked here too"
        );
    }

    #[test]
    fn encrypted_column_names_are_masked() {
        // `router.rs` folds `registered_encrypted_column_names()` into the
        // filter it hands the capture layer; simulate that composition.
        let filter = filter_with(&["ssn_encrypted"]);
        let (request, _) = redact(
            Request::post("/x")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-ignored", "1"),
            CapturedBody::Buffered(Bytes::from_static(
                br#"{"ssn_encrypted":"123-45-6789","name":"Ada"}"#,
            )),
            &filter,
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            body.contains(FILTERED_PLACEHOLDER),
            "an encrypted column name must be masked in the body, got {body}"
        );
        assert!(!body.contains("123-45-6789"), "the value must be gone");
        assert!(body.contains("Ada"), "non-sensitive fields must survive");
    }

    #[test]
    fn encoded_query_value_forms_join_the_echo_set() {
        let (request, values) = redact(
            Request::get("/callback?token=a%2Fb%2Bc&page=2"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            !request.uri.contains("a%2Fb"),
            "the encoded value must be masked out of the capsule uri, got {}",
            request.uri
        );
        assert!(
            values.contains(b"a/b+c"),
            "the decoded value must be in the echo set"
        );
        assert!(
            values.contains(b"a%2Fb%2Bc"),
            "the on-the-wire encoded value must be in the echo set too — an error \
             that echoes the raw request target carries this spelling"
        );
        assert_eq!(
            mask_echoes("failed on /callback?token=a%2Fb%2Bc", &values),
            format!("failed on /callback?token={FILTERED_PLACEHOLDER}"),
            "an echoed raw request target must scrub"
        );
    }

    #[test]
    fn encoded_form_body_value_forms_join_the_echo_set() {
        let (request, values) = redact(
            Request::post("/callback")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"token=a%2Fb%2Bc&page=2")),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a form body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("a%2Fb"),
            "the encoded value must be masked out of the capsule body, got {body}"
        );
        assert!(
            values.contains(b"a/b+c"),
            "the decoded value must be in the echo set"
        );
        assert!(
            values.contains(b"a%2Fb%2Bc"),
            "the on-the-wire encoded value must be in the echo set too — an error \
             that echoes the raw form body carries this spelling"
        );
        assert_eq!(
            mask_echoes("bad form field token=a%2Fb%2Bc", &values),
            format!("bad form field token={FILTERED_PLACEHOLDER}"),
            "an echoed raw form body must scrub"
        );
    }

    /// A non-sensitive header whose value is valid HTTP bytes but not valid
    /// UTF-8 (`obs-text`) must keep its exact bytes — base64 in
    /// `binary_headers` — because a placeholder would hand the replayed
    /// handler different metadata than production saw. Sensitive headers stay
    /// masked whatever their encoding.
    #[test]
    fn non_utf8_header_bytes_are_preserved_not_placeholdered() {
        let mut headers = axum::http::HeaderMap::new();
        // Mixed values under one name: a UTF-8 value first, then obs-text.
        // The whole name must move to the binary list in original order, or
        // `get_all("x-meta")` would observe a different sequence on replay.
        headers.append(
            "x-meta",
            axum::http::HeaderValue::from_static("plain-text-first"),
        );
        headers.append(
            "x-meta",
            axum::http::HeaderValue::from_bytes(&[0x61, 0xFF, 0x62]).expect("obs-text is legal"),
        );
        headers.insert(
            "authorization",
            axum::http::HeaderValue::from_bytes(&[0x73, 0xFF]).expect("obs-text is legal"),
        );
        let raw = RawRequest {
            method: "GET".to_owned(),
            uri: "/meta".parse().expect("uri"),
            version: axum::http::Version::HTTP_11,
            headers,
            route: None,
        };
        let (request, _values, _notes) =
            redact_request(&raw, &CapturedBody::Absent, &filter_with(&[]));

        assert!(
            !request.headers.iter().any(|(name, _)| name == "x-meta"),
            "a name with any non-UTF-8 value moves wholesale out of the text list"
        );
        let meta_values: Vec<Vec<u8>> = request
            .binary_headers
            .iter()
            .filter(|(name, _)| name == "x-meta")
            .map(|(_, value)| STANDARD.decode(value).expect("valid base64"))
            .collect();
        assert_eq!(
            meta_values,
            vec![b"plain-text-first".to_vec(), vec![0x61, 0xFF, 0x62]],
            "all of the name's values live in binary_headers, exact bytes, original order"
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == FILTERED_PLACEHOLDER),
            "a sensitive header is masked regardless of its encoding"
        );
        assert!(
            !request
                .binary_headers
                .iter()
                .any(|(name, _)| name == "authorization"),
            "a sensitive header's bytes must never survive into binary_headers"
        );
    }

    /// A sensitive key whose value is an object or array masks the whole
    /// container — but a handler extracts, echoes, or binds the *leaves*, so
    /// each scalar beneath the matched key must join the echo set alongside
    /// the container's serialization.
    #[test]
    fn leaf_values_of_a_masked_json_container_join_the_echo_set() {
        let raw = br#"{"secret":{"value":"hunter2secret","attempts":42},"keep":"public"}"#;
        let (request, values) = redact(
            Request::post("/hook").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(raw)),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("hunter2secret"),
            "the nested value must be masked out of the body, got {body}"
        );
        assert!(body.contains("public"), "unmatched fields must survive");
        assert!(
            values.contains(b"hunter2secret"),
            "a string leaf under a masked container must be in the echo set"
        );
        assert!(
            values.contains(b"42"),
            "a numeric leaf under a masked container must be in the echo set"
        );
        assert!(
            values.contains(br#"{"value":"hunter2secret","attempts":42}"#)
                || values.contains(br#"{"attempts":42,"value":"hunter2secret"}"#),
            "the container's own serialization stays in the set too"
        );
        assert_eq!(
            mask_echoes("could not verify hunter2secret", &values),
            format!("could not verify {FILTERED_PLACEHOLDER}"),
            "an error echoing an extracted leaf must scrub"
        );
    }

    /// A masked JSON value the client spelled with escapes must scrub in
    /// *both* spellings: the decoded form (what a handler error usually
    /// quotes) and the on-wire escaped form (what an error echoing the raw
    /// body carries).
    #[test]
    fn escaped_json_value_spellings_join_the_echo_set() {
        let raw = br#"{"token":"line\nbreak!"}"#;
        let (request, values) = redact(
            Request::post("/hook").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(raw)),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("break"),
            "the value must be masked out of the body, got {body}"
        );
        assert!(
            values.contains(b"line\nbreak!"),
            "the decoded spelling must be in the echo set"
        );
        assert!(
            values.contains(br"line\nbreak!"),
            "the on-wire escaped spelling must be in the echo set too"
        );
        assert_eq!(
            mask_echoes(r#"rejected body {"token":"line\nbreak!"}"#, &values),
            format!(r#"rejected body {{"token":"{FILTERED_PLACEHOLDER}"}}"#),
            "an error echoing the raw body must scrub the escaped spelling"
        );
    }

    /// A signed webhook's handler verifies a signature over the exact bytes
    /// the client sent. When nothing needs masking, the capsule must carry
    /// those exact bytes — re-serializing would drift whitespace, number
    /// spellings and key order, and the replay would flunk a signature check
    /// the production request passed.
    #[test]
    fn an_unredacted_json_body_keeps_its_exact_bytes() {
        let raw = br#"{ "b": 1.50,  "a": "x" }"#;
        let (request, _values) = redact(
            Request::post("/webhook").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(raw)),
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Text(String::from_utf8(raw.to_vec()).expect("utf8")),
            "a JSON body with nothing to mask must be preserved byte for byte"
        );
    }

    /// Decoding and re-encoding a query string canonicalizes spellings —
    /// `%2f` → `%2F`, a bare `flag` → `flag=` — and routes that inspect the
    /// raw query (or sign the request target) would diverge on replay. Only
    /// matched pairs may be rewritten; everything else keeps its bytes.
    #[test]
    fn unredacted_query_pairs_keep_their_raw_spelling() {
        // No sensitive pair at all: the whole target is untouched.
        let (request, _values) = redact(
            Request::get("/hook?path=a%2fb&flag&q=1+2"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(
            request.uri, "/hook?path=a%2fb&flag&q=1+2",
            "an unredacted query must be preserved byte for byte"
        );

        // A sensitive pair elsewhere: its neighbours still keep their bytes.
        let (request, values) = redact(
            Request::get("/hook?path=a%2fb&token=s3cret&flag"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(
            request.uri, "/hook?path=a%2fb&token=%5BFILTERED%5D&flag",
            "only the matched pair may be rewritten"
        );
        assert!(values.contains(b"s3cret"));
    }

    /// An absolute-form request target (the shape a proxy-style route sees)
    /// keeps its scheme and authority through masking: the rewritten query is
    /// spliced onto the original prefix, never rebuilt from the path alone.
    #[test]
    fn an_absolute_form_target_keeps_its_scheme_and_authority() {
        let (request, _values) = redact(
            Request::get("https://api.example/items?token=s3cret&keep=a%2f"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(
            request.uri, "https://api.example/items?token=%5BFILTERED%5D&keep=a%2f",
            "scheme and authority must survive query masking"
        );

        // And untouched absolute-form targets are preserved byte for byte.
        let (request, _values) = redact(
            Request::get("https://api.example/items?keep=a%2f"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(request.uri, "https://api.example/items?keep=a%2f");
    }

    /// The form-body twin of the raw-spelling guarantee: an untouched form —
    /// and the untouched neighbours of a masked field — keep the exact bytes
    /// the client sent.
    #[test]
    fn unredacted_form_pairs_keep_their_raw_spelling() {
        let (request, _values) = redact(
            Request::post("/hook")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"path=a%2fb&q=1+2")),
            &filter_with(&[]),
        );
        assert_eq!(
            request.body,
            CapsuleBody::Text("path=a%2fb&q=1+2".to_owned()),
            "an unredacted form body must be preserved byte for byte"
        );

        let (request, values) = redact(
            Request::post("/hook")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"path=a%2fb&token=s3cret")),
            &filter_with(&[]),
        );
        assert_eq!(
            request.body,
            CapsuleBody::Text("path=a%2fb&token=%5BFILTERED%5D".to_owned()),
            "only the matched form field may be rewritten"
        );
        assert!(values.contains(b"s3cret"));
    }

    #[test]
    fn query_string_params_are_masked() {
        let (request, values) = redact(
            Request::get("/search?q=cats&api_key=abcdef123456&page=2"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            request.uri.contains("q=cats") && request.uri.contains("page=2"),
            "harmless query parameters must survive, got {}",
            request.uri
        );
        assert!(
            !request.uri.contains("abcdef123456"),
            "a sensitive query value must never be written to the capsule, got {}",
            request.uri
        );
        assert!(
            request.uri.contains("api_key=%5BFILTERED%5D")
                || request.uri.contains("api_key=[FILTERED]"),
            "the masked parameter must remain present as a placeholder, got {}",
            request.uri
        );
        assert!(values.contains(b"abcdef123456"));
    }

    #[test]
    fn form_body_bracket_keys_are_masked() {
        let (request, values) = redact(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(
                b"user%5Bemail%5D=ada%40example.com&user%5Bpassword%5D=hunter2secret",
            )),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a form body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("hunter2secret"),
            "a bracket-notation password must be masked, got {body}"
        );
        assert!(
            body.contains("ada%40example.com") || body.contains("ada@example.com"),
            "non-sensitive form fields must survive, got {body}"
        );
        assert!(
            request
                .redacted_keys
                .iter()
                .any(|key| key.contains("password")),
            "redacted_keys must name the masked form key, got {:?}",
            request.redacted_keys
        );
        assert!(values.contains(b"hunter2secret"));
    }

    #[test]
    fn json_body_is_masked_recursively() {
        let (request, values) = redact(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(
                br#"{"a":{"b":[{"token":"deep-secret-value"}]},"keep":"visible"}"#,
            )),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("deep-secret-value"),
            "redaction must recurse through objects and arrays, got {body}"
        );
        assert!(body.contains("visible"));
        assert!(values.contains(b"deep-secret-value"));
    }

    #[test]
    fn malformed_json_body_is_masked_not_copied_verbatim() {
        let (request, values, notes) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"password":"hunter2secret", oops"#)),
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Skipped {
                declared_len: Some(33)
            },
            "a body that declared JSON but did not parse must never be copied verbatim, got {:?}",
            request.body
        );
        assert!(
            values.contains(b"hunter2secret"),
            "a skipped body must still seed the echo set, so an outcome quoting it is scrubbed"
        );
        assert!(
            !values.contains(b"password"),
            "field names stay out of the echo set — masking them would shred outcome prose"
        );
        assert!(
            request
                .redacted_keys
                .contains(&UNPARSEABLE_BODY_KEY.to_owned()),
            "redacted_keys must record that the body was masked, got {:?}",
            request.redacted_keys
        );
        assert_eq!(notes, vec![UNPARSEABLE_JSON_NOTE.to_owned()]);
    }

    #[test]
    fn truncated_json_prefix_is_masked() {
        // The body tap copies bytes as the handler reads them, so a request
        // that fails mid-stream leaves a prefix that cannot parse.
        let (request, _, notes) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json; charset=utf-8"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"token":"deep-secret-value","#)),
            &filter_with(&[]),
        );

        assert!(
            matches!(request.body, CapsuleBody::Skipped { .. }),
            "a truncated JSON prefix must be masked, got {:?}",
            request.body
        );
        assert_eq!(notes, vec![UNPARSEABLE_JSON_NOTE.to_owned()]);
    }

    /// A skipped body never reaches the capsule, but a handler that already
    /// read it can quote it — whole, or one value at a time — into the 5xx
    /// message or panic payload that *does*. The echo set has to know the
    /// body's values even though no filter key ever matched.
    #[test]
    fn a_skipped_malformed_body_still_masks_echoed_values() {
        let (request, values, _) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"password":"hunter2secret","#)),
            &filter_with(&[]),
        );
        assert!(matches!(request.body, CapsuleBody::Skipped { .. }));

        // A handler quoting the offending value...
        let masked = mask_echoes("could not store credential hunter2secret", &values);
        assert!(
            !masked.contains("hunter2secret"),
            "an echoed body value must be masked out of the outcome: {masked}"
        );
        // ...or the entire raw body it read.
        let masked = mask_echoes(r#"bad payload: {"password":"hunter2secret","#, &values);
        assert!(
            !masked.contains("hunter2secret"),
            "an echoed raw body must be masked out of the outcome: {masked}"
        );
        // Ordinary prose sharing a word with a field name stays readable.
        assert_eq!(
            mask_echoes("password rejected", &values),
            "password rejected"
        );
    }

    /// The truncated-tap shape: capture stopped mid-literal, so there is no
    /// closing quote. The unterminated remainder is exactly where the secret
    /// sits and must join the echo set.
    #[test]
    fn a_body_truncated_inside_a_literal_still_masks_the_remainder() {
        let (_, values, _) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"token":"deep-secret-value"#)),
            &filter_with(&[]),
        );
        let masked = mask_echoes("refused: deep-secret-value", &values);
        assert!(
            !masked.contains("deep-secret-value"),
            "the unterminated literal's remainder must be masked: {masked}"
        );
    }

    #[test]
    fn body_that_is_not_a_form_under_a_form_content_type_is_masked() {
        // The urlencoded parser is lossy: it accepts anything, turning a JSON
        // document into one giant key that no filter can match.
        let (request, _, notes) = redact_with_notes(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"password":"hunter2secret"}"#)),
            &filter_with(&[]),
        );

        assert!(
            matches!(request.body, CapsuleBody::Skipped { .. }),
            "a body that declared a form but did not parse as one must be masked, got {:?}",
            request.body
        );
        assert_eq!(notes, vec![UNPARSEABLE_FORM_NOTE.to_owned()]);
    }

    #[test]
    fn valid_structured_bodies_still_parse_without_a_note() {
        let (request, values, notes) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(
                br#"{"password":"hunter2secret","keep":"visible"}"#,
            )),
            &filter_with(&[]),
        );
        let CapsuleBody::Text(body) = &request.body else {
            panic!("valid JSON must still be parsed and scrubbed, got {request:?}");
        };
        assert!(!body.contains("hunter2secret") && body.contains("visible"));
        assert!(values.contains(b"hunter2secret"));
        assert!(
            notes.is_empty(),
            "a parsed body needs no note, got {notes:?}"
        );

        let (request, _, notes) = redact_with_notes(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"email=ada%40example.com&flag=&page=2")),
            &filter_with(&[]),
        );
        assert!(
            matches!(&request.body, CapsuleBody::Text(body) if body.contains("page=2")),
            "an ordinary form body — empty values included — must still parse, got {:?}",
            request.body
        );
        assert!(
            notes.is_empty(),
            "a parsed body needs no note, got {notes:?}"
        );
    }

    #[test]
    fn unstructured_bodies_are_unaffected() {
        // No declared structure means nothing claimed to parse, so the existing
        // verbatim copy stands.
        let (request, _, notes) = redact_with_notes(
            Request::post("/notes").header(header::CONTENT_TYPE, "text/plain"),
            CapturedBody::Buffered(Bytes::from_static(b"{not json, not a form")),
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Text("{not json, not a form".to_owned())
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn oversized_body_is_skipped_not_consumed() {
        let (request, _) = redact(
            Request::post("/upload").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Skipped {
                declared_len: Some(9_000_000),
            },
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Skipped {
                declared_len: Some(9_000_000)
            },
            "an oversized body must be recorded as skipped, never partially copied"
        );
    }

    #[test]
    fn binary_body_is_base64_encoded() {
        let (request, _) = redact(
            Request::post("/upload").header(header::CONTENT_TYPE, "application/octet-stream"),
            CapturedBody::Buffered(Bytes::from_static(&[0xFF, 0xFE, 0x00, 0x01])),
            &filter_with(&[]),
        );

        let CapsuleBody::Base64(encoded) = &request.body else {
            panic!(
                "a non-UTF-8 body must be base64-encoded, got {:?}",
                request.body
            );
        };
        assert_eq!(encoded, "//4AAQ==");
    }

    #[test]
    fn outcome_text_echoing_a_redacted_value_is_masked() {
        // A handler that fails while talking about what it was given hands the
        // secret straight back: `redact_request` masked it out of the body, so
        // the outcome must not smuggle it into the capsule.
        let (_, values) = redact(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"email=ada&password=hunter2secret")),
            &filter_with(&[]),
        );

        let masked = mask_echoes("could not store password=hunter2secret for ada", &values);

        assert!(
            !masked.contains("hunter2secret"),
            "a value redaction removed must not reappear in the outcome, got {masked}"
        );
        assert!(
            masked.contains(FILTERED_PLACEHOLDER),
            "the echo must be replaced by the placeholder, got {masked}"
        );
        assert!(
            masked.contains("for ada"),
            "the rest of the message must survive, got {masked}"
        );
    }

    #[test]
    fn mask_echoes_prefers_the_longest_match() {
        let mut values = RedactedValues::default();
        values.insert(b"hunter2secret");
        values.insert(b"hunter2");

        let masked = mask_echoes("tried hunter2secret twice", &values);

        assert_eq!(masked, format!("tried {FILTERED_PLACEHOLDER} twice"));
    }

    /// A short secret — a CVV, a PIN — must not reach disk just because the
    /// failure quoted it, but masking it *everywhere* would shred timestamps,
    /// identifiers and ordinary words. It is masked exactly where it stands
    /// as a token of its own.
    #[test]
    fn mask_echoes_masks_short_values_only_as_whole_tokens() {
        let mut values = RedactedValues::default();
        values.insert(b"123");

        assert_eq!(
            mask_echoes("CVV 123 rejected", &values),
            format!("CVV {FILTERED_PLACEHOLDER} rejected"),
            "a short secret quoted in the failure must be masked"
        );
        assert_eq!(
            mask_echoes("cvv=123&ok=1", &values),
            format!("cvv={FILTERED_PLACEHOLDER}&ok=1"),
            "punctuation delimits a token just as whitespace does"
        );
        // Everything a naive substring replacement would have shredded.
        assert_eq!(
            mask_echoes("request 1234 took 5123ms at 12:31:23", &values),
            "request 1234 took 5123ms at 12:31:23",
            "a short value inside a longer run is not a secret occurrence"
        );
        // Back-to-back occurrences are a longer run, not two tokens: the
        // neighbour of each is the needle itself.
        assert_eq!(
            mask_echoes("request 123123 failed", &values),
            "request 123123 failed",
            "adjacent occurrences form one alphanumeric run, not whole tokens"
        );
        assert_eq!(
            mask_echoes("123", &values),
            FILTERED_PLACEHOLDER,
            "a value that is the entire text is a whole token"
        );
    }

    #[test]
    fn mask_echoes_is_a_no_op_without_redactions() {
        let values = RedactedValues::default();
        assert_eq!(mask_echoes("nothing to hide", &values), "nothing to hide");
    }

    #[test]
    fn bind_matching_a_redacted_value_is_masked() {
        let mut values = RedactedValues::default();
        values.insert(b"hunter2secret");

        let mut binds = vec![
            BindValue::Value(b"hunter2secret".to_vec()),
            BindValue::Value(b"ada@example.com".to_vec()),
            BindValue::Null,
        ];
        mask_binds(&mut binds, &values);

        assert_eq!(
            binds,
            vec![
                BindValue::Masked,
                BindValue::Value(b"ada@example.com".to_vec()),
                BindValue::Null,
            ],
            "a bind echoing a redacted value must be masked; others must survive"
        );
    }
}
