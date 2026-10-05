//! The offline HTML viewer of a capsule.
//!
//! The viewer uses the `static_gen` layout: route `/` is `viewer/index.html`
//! and route `/<table>` is `viewer/<table>/index.html`, with a
//! `viewer/manifest.json` [`StaticManifest`]. A browser opens the pages from
//! the disk. The pages have no script and load nothing from a network.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use crate::static_gen::{ManifestEntry, StaticManifest, url_to_file_path};

use super::model::{CapsuleManifest, ModelManifest, Record, value_key};

const HTML_TYPE: &str = "text/html; charset=utf-8";
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'";
const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;margin:2rem;max-width:72rem}\
table{border-collapse:collapse;margin:.5rem 0 1.5rem}\
th,td{border:1px solid #ccc;padding:.25rem .5rem;text-align:left;vertical-align:top}\
th{background:#f4f4f4}section{margin-bottom:2rem}em{color:#888}\
@media(prefers-color-scheme:dark){body{background:#111;color:#eee}th{background:#222}\
a{color:#8cf}}";

/// Make the viewer files: `(path relative to the capsule root, bytes)`.
pub(super) fn render(
    manifest: &CapsuleManifest,
    records: &BTreeMap<String, Vec<Record>>,
) -> Vec<(String, Vec<u8>)> {
    let index = KeyIndex::new(manifest, records);
    let mut files = Vec::new();
    let mut routes = HashMap::new();

    let mut page = |route: String, html: String| {
        let file = url_to_file_path(&route);
        routes.insert(
            route,
            ManifestEntry::new(file.clone()).with_content_type(Some(HTML_TYPE.to_owned())),
        );
        files.push((format!("viewer/{file}"), html.into_bytes()));
    };
    page("/".to_owned(), render_index(manifest));
    for model in &manifest.models {
        let rows = records.get(&model.table).map_or(&[][..], Vec::as_slice);
        page(
            format!("/{}", model.table),
            render_model(manifest, model, rows, &index, records),
        );
    }

    let static_manifest = StaticManifest::new(routes).with_generated_at(&manifest.generated_at);
    let json = serde_json::to_vec_pretty(&static_manifest).unwrap_or_default();
    files.push(("viewer/manifest.json".to_owned(), json));
    files
}

/// The primary-key values of each table, for links that only point at rows
/// the capsule has.
struct KeyIndex(HashMap<String, BTreeSet<String>>);

impl KeyIndex {
    fn new(manifest: &CapsuleManifest, records: &BTreeMap<String, Vec<Record>>) -> Self {
        let map = manifest
            .models
            .iter()
            .map(|m| {
                let keys = records
                    .get(&m.table)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.get(&m.primary_key).and_then(value_key))
                    .collect();
                (m.table.clone(), keys)
            })
            .collect();
        Self(map)
    }

    fn has(&self, table: &str, key: &str) -> bool {
        self.0.get(table).is_some_and(|keys| keys.contains(key))
    }
}

/// Escape text for HTML content and attribute values.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The anchor id of a record key. The same function makes ids and links.
fn anchor(key: &str) -> String {
    let mut out = String::from("r-");
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "_{b:02x}");
        }
    }
    out
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>\n",
        esc(title)
    )
}

fn render_index(manifest: &CapsuleManifest) -> String {
    let mut body = format!(
        "<h1>Data capsule</h1><p>Subject: <strong>{}</strong><br>Exported: {}<br>\
Autumn {}</p><table><tr><th>Model</th><th>Records</th></tr>",
        esc(&manifest.subject),
        esc(&manifest.generated_at),
        esc(&manifest.framework_version),
    );
    for model in &manifest.models {
        let _ = write!(
            body,
            "<tr><td><a href=\"{}/index.html\">{}</a></td><td>{}</td></tr>",
            esc(&model.table),
            esc(&model.table),
            model.record_count
        );
    }
    body.push_str("</table>");
    shell("Data capsule", &body)
}

fn display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "<em>null</em>".to_owned(),
        serde_json::Value::String(s) => esc(s),
        other => esc(&other.to_string()),
    }
}

/// The blob key in a blob column: a `storage::Blob` object or
/// a plain key string.
pub(super) fn blob_key(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Object(o) => o.get("key").and_then(serde_json::Value::as_str),
        _ => None,
    }
}

fn render_model(
    manifest: &CapsuleManifest,
    model: &ModelManifest,
    rows: &[Record],
    index: &KeyIndex,
    records: &BTreeMap<String, Vec<Record>>,
) -> String {
    let mut body = format!(
        "<p><a href=\"../index.html\">All models</a></p><h1>{}</h1>",
        esc(&model.table)
    );
    let columns: Vec<&str> = if model.fields.is_empty() {
        rows.first()
            .map(|r| r.keys().map(String::as_str).collect())
            .unwrap_or_default()
    } else {
        model.fields.iter().map(|f| f.name.as_str()).collect()
    };
    for row in rows {
        let key = row.get(&model.primary_key).and_then(value_key);
        let id = key.as_deref().map(anchor).unwrap_or_default();
        let _ = write!(
            body,
            "<section id=\"{}\"><h2>{} {}</h2><table>",
            esc(&id),
            esc(&model.table),
            esc(key.as_deref().unwrap_or("?"))
        );
        for column in &columns {
            let value = row.get(*column).unwrap_or(&serde_json::Value::Null);
            let cell = cell(manifest, model, column, value, index);
            let _ = write!(body, "<tr><th>{}</th><td>{cell}</td></tr>", esc(column));
        }
        body.push_str("</table>");
        if let Some(key) = key.as_deref() {
            body.push_str(&backlinks(manifest, model, key, records));
        }
        body.push_str("</section>");
    }
    shell(&model.table, &body)
}

fn cell(
    manifest: &CapsuleManifest,
    model: &ModelManifest,
    column: &str,
    value: &serde_json::Value,
    index: &KeyIndex,
) -> String {
    if model.blob_columns.iter().any(|c| c == column)
        && let Some(blob) = blob_key(value).and_then(|k| manifest.blob(k))
    {
        return format!(
            "<a href=\"../../{}\">{}</a> ({}, {} bytes)",
            esc(&blob.file()),
            esc(&blob.key),
            esc(&blob.content_type),
            blob.byte_size
        );
    }
    let link = model
        .relationships
        .iter()
        .filter(|r| r.column == column)
        .find_map(|r| {
            let key = value_key(value)?;
            let target = manifest.model(&r.target)?;
            (target.primary_key == r.target_column && index.has(&r.target, &key))
                .then_some((r.target.as_str(), key))
        });
    match link {
        Some((target, key)) => format!(
            "<a href=\"../{}/index.html#{}\">{}</a>",
            esc(target),
            esc(&anchor(&key)),
            display(value)
        ),
        None => display(value),
    }
}

/// Links to the records of other models that point at this record.
fn backlinks(
    manifest: &CapsuleManifest,
    model: &ModelManifest,
    key: &str,
    records: &BTreeMap<String, Vec<Record>>,
) -> String {
    let mut links = Vec::new();
    for other in &manifest.models {
        for rel in other
            .relationships
            .iter()
            .filter(|r| r.target == model.table && r.target_column == model.primary_key)
        {
            for row in records.get(&other.table).into_iter().flatten() {
                if row.get(&rel.column).and_then(value_key).as_deref() == Some(key)
                    && let Some(other_key) = row.get(&other.primary_key).and_then(value_key)
                {
                    links.push(format!(
                        "<li><a href=\"../{}/index.html#{}\">{} {}</a> ({})</li>",
                        esc(&other.table),
                        esc(&anchor(&other_key)),
                        esc(&other.table),
                        esc(&other_key),
                        esc(&rel.column)
                    ));
                }
            }
        }
    }
    if links.is_empty() {
        String::new()
    } else {
        format!("<p>Referenced by:</p><ul>{}</ul>", links.concat())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_escapes_markup_and_quotes() {
        assert_eq!(
            esc("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn anchor_is_safe_for_ids_and_fragments() {
        assert_eq!(anchor("10"), "r-10");
        assert_eq!(anchor("a b\"<"), "r-a_20b_22_3c");
        assert_eq!(anchor("é"), "r-_c3_a9");
    }

    #[test]
    fn blob_key_reads_objects_and_strings() {
        assert_eq!(blob_key(&serde_json::json!("k/1")), Some("k/1"));
        assert_eq!(blob_key(&serde_json::json!({"key": "k/2"})), Some("k/2"));
        assert_eq!(blob_key(&serde_json::json!(3)), None);
    }
}
