//! MCP protocol revision `2026-07-28` (the modern era).
//!
//! A modern request carries its protocol version and client capabilities in
//! `params._meta`. There is no `initialize` handshake. The server checks each
//! request on its own. See
//! <https://modelcontextprotocol.io/specification/2026-07-28>.
//!
//! Autumn is dual-era: a request with the modern `_meta` fields gets modern
//! semantics, and an `initialize` request selects the legacy flow.

use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use serde_json::{Value, json};

/// The modern protocol versions this server serves.
pub const VERSIONS: &[&str] = &["2026-07-28"];

/// The `_meta` key that holds the request's protocol version.
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// The `_meta` key that holds the request's client capabilities.
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// The `_meta` key that identifies the server on a result.
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// JSON-RPC error code: a header does not match the body, or is missing.
const HEADER_MISMATCH: i64 = -32020;

/// JSON-RPC error code: the server does not serve the requested version.
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// Freshness hint for `server/discover` and `tools/list`, in milliseconds.
/// The catalog is fixed while the process runs. A deploy can change it, so
/// the hint is short.
pub const CATALOG_TTL_MS: u64 = 300_000;

/// A rejected request: the HTTP status and the JSON-RPC error message.
pub struct Rejection {
    pub status: StatusCode,
    pub body: Value,
}

fn reject(
    status: StatusCode,
    id: Value,
    code: i64,
    message: &str,
    data: Option<Value>,
) -> Rejection {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    let mut body = serde_json::Map::new();
    body.insert("jsonrpc".to_owned(), json!("2.0"));
    // The 2026 schema allows no `null` id: an error with no known id omits it.
    if !id.is_null() {
        body.insert("id".to_owned(), id);
    }
    body.insert("error".to_owned(), error);
    Rejection {
        status,
        body: Value::Object(body),
    }
}

/// The value of a header that occurs once. A repeated header gives `None`:
/// a proxy can read another copy than the server, so the server must not
/// trust either.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
}

fn meta(msg: &Value) -> Option<&Value> {
    msg.get("params").and_then(|p| p.get("_meta"))
}

/// Whether one message uses the modern era: its `_meta` names a protocol
/// version.
fn message_is_modern(msg: &Value) -> bool {
    meta(msg).is_some_and(|m| m.get(META_PROTOCOL_VERSION).is_some())
}

/// Whether the request body uses the modern era.
///
/// True when a message carries the modern `_meta` field, when the
/// `MCP-Protocol-Version` header names a modern version, or for
/// `server/discover`, which only the modern era defines. `initialize`
/// always selects the legacy era.
pub fn wants_modern(headers: &HeaderMap, parsed: &Value) -> bool {
    if parsed.get("method").and_then(Value::as_str) == Some("initialize") {
        return false;
    }
    if header_is_modern(headers) {
        return true;
    }
    match parsed {
        Value::Array(batch) => batch.iter().any(message_is_modern),
        Value::Object(_) => {
            message_is_modern(parsed)
                || parsed.get("method").and_then(Value::as_str) == Some("server/discover")
        }
        _ => false,
    }
}

/// Decode a header value that can use the Base64 sentinel
/// (`=?base64?…?=`). Returns `None` for a bad encoding.
fn decode_header_value(value: &str) -> Option<String> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    else {
        return Some(value.to_owned());
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// The body value that `Mcp-Name` must mirror, if the method has one.
fn mcp_name_source<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    let key = match method {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        // The Tasks extension routes on the task ID.
        "tasks/get" | "tasks/update" | "tasks/cancel" => "taskId",
        _ => return None,
    };
    params.get(key).and_then(Value::as_str)
}

/// Whether the `MCP-Protocol-Version` header names a modern version.
pub fn header_is_modern(headers: &HeaderMap) -> bool {
    header(headers, "mcp-protocol-version").is_some_and(|v| VERSIONS.contains(&v))
}

/// The rejection for a body that is not JSON.
pub fn parse_error(message: &str) -> Rejection {
    reject(
        StatusCode::BAD_REQUEST,
        Value::Null,
        -32700,
        &format!("parse error: {message}"),
        None,
    )
}

/// Check one modern request: the `_meta` fields and the request headers.
///
/// The checks run in this order: the message shape, the protocol version
/// (present, same as the header, served), the client capabilities, then the
/// `Mcp-Method` and `Mcp-Name` headers. `served` tells whether the server
/// serves the method. `Mcp-Name` is checked only for a served method, so an
/// unknown method still gets `-32601`.
///
/// The caller handles batches and notifications before this.
pub fn validate(
    headers: &HeaderMap,
    msg: &Value,
    served: impl Fn(&str) -> bool,
) -> Result<(), Rejection> {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    // A request id is a string or an integer.
    let id_ok = id.is_string() || id.is_i64() || id.is_u64();
    let method = msg.get("method").and_then(Value::as_str);
    let (true, Some(method), true) = (
        msg.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
        method,
        id_ok,
    ) else {
        let err_id = if id_ok { id } else { Value::Null };
        return Err(reject(
            StatusCode::BAD_REQUEST,
            err_id,
            -32600,
            "Invalid Request",
            None,
        ));
    };

    // The required `_meta` fields.
    let meta = meta(msg);
    let Some(version) = meta
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str)
    else {
        return Err(reject(
            StatusCode::BAD_REQUEST,
            id,
            -32602,
            "Invalid params: `_meta` must have `io.modelcontextprotocol/protocolVersion` (a string)",
            None,
        ));
    };

    // The standard headers must be present and match the body.
    let mismatch = |message: &str| {
        reject(
            StatusCode::BAD_REQUEST,
            id.clone(),
            HEADER_MISMATCH,
            message,
            None,
        )
    };
    if header(headers, "mcp-protocol-version") != Some(version) {
        return Err(mismatch(
            "Header mismatch: MCP-Protocol-Version must match `_meta` protocolVersion",
        ));
    }
    if !VERSIONS.contains(&version) {
        return Err(reject(
            StatusCode::BAD_REQUEST,
            id,
            UNSUPPORTED_PROTOCOL_VERSION,
            "Unsupported protocol version",
            Some(json!({ "supported": VERSIONS, "requested": version })),
        ));
    }
    if !meta
        .and_then(|m| m.get(META_CLIENT_CAPABILITIES))
        .is_some_and(Value::is_object)
    {
        return Err(reject(
            StatusCode::BAD_REQUEST,
            id.clone(),
            -32602,
            "Invalid params: `_meta` must have `io.modelcontextprotocol/clientCapabilities` (an object)",
            None,
        ));
    }
    if header(headers, "mcp-method") != Some(method) {
        return Err(mismatch(
            "Header mismatch: Mcp-Method must match the method",
        ));
    }
    let params = msg.get("params").unwrap_or(&Value::Null);
    if served(method)
        && let Some(expected) = mcp_name_source(method, params)
    {
        let sent = header(headers, "mcp-name").and_then(decode_header_value);
        if sent.as_deref() != Some(expected) {
            return Err(mismatch("Header mismatch: Mcp-Name must match the body"));
        }
    }
    Ok(())
}

/// Whether `msg` is a well-formed notification: JSON-RPC 2.0, a string
/// `method`, and no `id`.
pub fn is_notification(msg: &Value) -> bool {
    msg.get("id").is_none()
        && msg.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && msg.get("method").is_some_and(Value::is_string)
}

/// The rejection for a modern batch. A modern POST carries one message.
pub fn batch_rejection() -> Rejection {
    reject(
        StatusCode::BAD_REQUEST,
        Value::Null,
        -32600,
        "Invalid Request: protocol 2026-07-28 does not allow JSON-RPC batches",
        None,
    )
}

/// The rejection for an unknown method: HTTP 404 and `-32601`.
pub fn method_not_found(id: Value, method: &str) -> Rejection {
    reject(
        StatusCode::NOT_FOUND,
        id,
        -32601,
        &format!("method not found: {method}"),
        None,
    )
}

/// The HTTP status for a modern JSON-RPC error message.
///
/// `-32021` (missing capability) is 400 and `-32601` (method not found) is
/// 404, as the transport requires. Other errors keep 200.
pub fn status_for(message: &Value) -> StatusCode {
    match message
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
    {
        Some(-32021) => StatusCode::BAD_REQUEST,
        Some(-32601) => StatusCode::NOT_FOUND,
        _ => StatusCode::OK,
    }
}

/// Add the modern result fields to a JSON-RPC result message: `resultType`
/// (`"complete"` unless set) and `_meta.serverInfo`. An error message does
/// not change.
pub fn stamp(message: &mut Value, server_info: &Value) {
    let Some(result) = message.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    result
        .entry("resultType")
        .or_insert_with(|| json!("complete"));
    let meta = result.entry("_meta").or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(META_SERVER_INFO.to_owned(), server_info.clone());
    }
}

/// Add the caching hints (`ttlMs`, `cacheScope`) to a result object.
///
/// The scope is always `"private"`. The catalog does not vary by caller, but
/// the endpoint can sit behind auth (`secure_mcp`, mTLS, a proxy) that this
/// module cannot see. A shared cache must not serve it to another caller.
pub fn add_cache_hints(result: &mut Value) {
    result["ttlMs"] = json!(CATALOG_TTL_MS);
    result["cacheScope"] = json!("private");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: &Value) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
    }

    fn modern_meta() -> Value {
        json!({ META_PROTOCOL_VERSION: "2026-07-28", META_CLIENT_CAPABILITIES: {} })
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn base64_sentinel_decodes() {
        assert_eq!(decode_header_value("plain").as_deref(), Some("plain"));
        assert_eq!(
            decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(),
            Some("Hello, 世界")
        );
        assert_eq!(decode_header_value("=?base64?!!!?="), None);
    }

    #[test]
    fn era_detection() {
        let none = HeaderMap::new();
        assert!(wants_modern(
            &none,
            &request("tools/list", &json!({"_meta": modern_meta()}))
        ));
        assert!(wants_modern(&none, &request("server/discover", &json!({}))));
        assert!(!wants_modern(&none, &request("tools/list", &json!({}))));
        let modern_header = headers(&[("mcp-protocol-version", "2026-07-28")]);
        assert!(wants_modern(
            &modern_header,
            &request("tools/list", &json!({}))
        ));
        let legacy_header = headers(&[("mcp-protocol-version", "2025-06-18")]);
        assert!(!wants_modern(
            &legacy_header,
            &request("initialize", &json!({}))
        ));
    }

    #[test]
    fn validate_accepts_a_conforming_request() {
        let msg = request("tools/call", &json!({"name": "t", "_meta": modern_meta()}));
        let ok = headers(&[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/call"),
            ("mcp-name", "t"),
        ]);
        assert!(validate(&ok, &msg, |_| true).is_ok());
    }

    #[test]
    fn validate_rejects_a_null_id() {
        let mut msg = request("tools/list", &json!({"_meta": modern_meta()}));
        msg["id"] = Value::Null;
        let err = validate(&HeaderMap::new(), &msg, |_| true).err().unwrap();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.body["error"]["code"], -32600);
    }

    #[test]
    fn stamp_keeps_a_task_result_type() {
        let info = json!({"name": "s", "version": "1"});
        let mut task = json!({"id": 1, "result": {"resultType": "task"}});
        stamp(&mut task, &info);
        assert_eq!(task["result"]["resultType"], "task");
        assert_eq!(task["result"]["_meta"][META_SERVER_INFO], info);
        let mut failure = json!({"id": 1, "error": {"code": -1}});
        stamp(&mut failure, &info);
        assert!(failure.get("result").is_none());
    }

    #[test]
    fn status_mapping() {
        assert_eq!(
            status_for(&json!({"error": {"code": -32021}})),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_for(&json!({"error": {"code": -32601}})),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_for(&json!({"error": {"code": -32602}})),
            StatusCode::OK
        );
        assert_eq!(status_for(&json!({"result": {}})), StatusCode::OK);
    }
}
