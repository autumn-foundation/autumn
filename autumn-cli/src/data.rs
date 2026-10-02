//! `autumn data export` and `autumn data import` — CSV data utilities.
//!
//! Both commands delegate to the running application's admin HTTP layer.
//! The admin plugin must be mounted at `/admin` (or the URL base you
//! specify) and the model must have CSV export/import enabled.

use std::fs;
use std::time::Duration;

use reqwest::blocking::{Client, multipart};

fn make_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(60))
        .no_proxy()
        .cookie_store(true)
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

/// `autumn data export <model> [options]`
///
/// Calls `GET {url}/{model}/export.csv` and writes the response body to
/// `out` (or `{model}.csv` when omitted).
pub fn run_export(
    model: &str,
    base_url: &str,
    out: Option<&str>,
    filter: Option<&str>,
    cookie: Option<&str>,
) {
    if let Err(e) = run_export_inner(model, base_url, out, filter, cookie) {
        eprintln!("autumn data export: {e}");
        std::process::exit(1);
    }
}

fn run_export_inner(
    model: &str,
    base_url: &str,
    out: Option<&str>,
    filter: Option<&str>,
    cookie: Option<&str>,
) -> Result<(), String> {
    let client = make_client()?;
    let base = base_url.trim_end_matches('/');
    let mut url = format!("{base}/{model}/export.csv");

    if let Some(q) = filter {
        let encoded = percent_encode(q);
        url.push_str("?q=");
        url.push_str(&encoded);
    }

    println!("Exporting {model} from {url}");

    let mut req = client.get(&url);
    if let Some(c) = cookie {
        req = req.header("Cookie", c);
    }
    let response = req.send().map_err(|e| format!("Request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Server returned HTTP {} for {url}",
            response.status()
        ));
    }

    let bytes = response
        .bytes()
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let output_path = out.map_or_else(|| format!("{model}.csv"), str::to_owned);

    fs::write(&output_path, &bytes).map_err(|e| format!("Failed to write '{output_path}': {e}"))?;

    #[allow(clippy::naive_bytecount)]
    let row_count = bytes
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        .saturating_sub(1);
    println!("Exported {row_count} rows to {output_path}");
    Ok(())
}

/// `autumn data import <model> --in <file> [options]`
///
/// Uploads `input` as a multipart CSV to `POST {url}/{model}/import`.
/// Prints the resulting `ImportReport` summary to stdout.
pub fn run_import(
    model: &str,
    base_url: &str,
    input: &str,
    dry_run: bool,
    upsert_by: Option<&str>,
    cookie: Option<&str>,
) {
    if upsert_by.is_some() {
        eprintln!(
            "autumn data import: --upsert-by is not yet supported via the admin HTTP API; \
             omit the flag to use insert mode, or implement upsert in a custom migration script"
        );
        std::process::exit(1);
    }
    if let Err(e) = run_import_inner(model, base_url, input, dry_run, cookie) {
        eprintln!("autumn data import: {e}");
        std::process::exit(1);
    }
}

fn run_import_inner(
    model: &str,
    base_url: &str,
    input: &str,
    dry_run: bool,
    cookie: Option<&str>,
) -> Result<(), String> {
    let client = make_client()?;
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/{model}/import");

    let csv_bytes = fs::read(input).map_err(|e| format!("Failed to read '{input}': {e}"))?;

    let mode_value = if dry_run { "dry_run" } else { "insert" };
    let label = if dry_run { "Dry run" } else { "Import" };

    let mut get_req = client.get(&url);
    if let Some(c) = cookie {
        get_req = get_req.header("Cookie", c);
    }
    let get_resp = get_req
        .send()
        .map_err(|e| format!("Failed to fetch import form: {e}"))?;

    // Collect cookies issued by the server during the GET (the CSRF cookie) so we
    // can merge them with the user-supplied session cookie on the POST. Setting an
    // explicit Cookie header suppresses reqwest's internal cookie jar, so we build
    // the merged cookie string ourselves when --cookie is provided.
    let server_cookies: Vec<String> = get_resp
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let form_html = get_resp
        .text()
        .map_err(|e| format!("Failed to read import form: {e}"))?;

    let csrf_token = extract_csrf_token(&form_html).unwrap_or_default();
    let csrf_header =
        extract_csrf_header_name(&form_html).unwrap_or_else(|| "X-CSRF-Token".to_owned());

    println!("{label}ing {model} from {input} → {url}");

    let form = multipart::Form::new()
        .part(
            "file",
            multipart::Part::bytes(csv_bytes)
                .file_name(
                    std::path::Path::new(input)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("import.csv")
                        .to_owned(),
                )
                .mime_str("text/csv")
                .map_err(|e| format!("MIME type error: {e}"))?,
        )
        .text("mode", mode_value.to_owned());

    let mut post_req = client.post(&url).multipart(form);
    if let Some(c) = cookie {
        // Merge the user-supplied session cookie with server-issued cookies (the
        // CSRF cookie). We must do this explicitly because setting a Cookie header
        // prevents reqwest from appending its jar cookies automatically.
        let mut parts = vec![c];
        let sc_refs: Vec<&str> = server_cookies.iter().map(String::as_str).collect();
        parts.extend_from_slice(&sc_refs);
        post_req = post_req.header("Cookie", parts.join("; "));
    }
    if !csrf_token.is_empty() {
        post_req = post_req.header(csrf_header.as_str(), csrf_token.as_str());
    }
    let response = post_req
        .send()
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
        return Err(format!("Server returned HTTP {status}:\n{body}"));
    }

    // The admin plugin returns HTML. Parse out the key numbers from the
    // import result page if we can; otherwise just print the status.
    print_import_summary(&body, dry_run);
    Ok(())
}

/// Extract inserted/updated/skipped/error counts from the HTML result page
/// and print a concise summary.  Falls back gracefully if the page structure
/// changes.
fn print_import_summary(html: &str, dry_run: bool) {
    // Simple heuristic: look for digit-only content in the summary grid cells.
    let counts: Vec<u64> = html
        .split("font-size: 1.5rem")
        .skip(1)
        .take(4)
        .filter_map(|chunk| {
            chunk
                .find('>')
                .and_then(|start| {
                    chunk[start + 1..]
                        .find('<')
                        .map(|end| (start + 1, start + 1 + end))
                })
                .and_then(|(s, e)| chunk[s..e].trim().parse::<u64>().ok())
        })
        .collect();

    if counts.len() == 4 {
        let prefix = if dry_run { "(dry run) " } else { "" };
        println!(
            "{prefix}inserted={} updated={} skipped={} errors={}",
            counts[0], counts[1], counts[2], counts[3]
        );
        if counts[3] > 0 {
            eprintln!(
                "Import completed with {} errors. Check the admin UI for details.",
                counts[3]
            );
            std::process::exit(1);
        }
        println!("Import completed successfully.");
    } else {
        println!("Import request accepted. Check the admin UI for details.");
    }
}

/// Extract the CSRF token from `<meta name="csrf-token" content="...">`.
fn extract_csrf_token(html: &str) -> Option<String> {
    let meta_pos = html.find("name=\"csrf-token\"")?;
    let after_meta = &html[meta_pos..];
    let val_start = after_meta.find("content=\"")? + 9;
    let val_end = after_meta[val_start..].find('"')?;
    Some(after_meta[val_start..val_start + val_end].to_owned())
}

/// Extract the configured CSRF header name from `<meta name="csrf-token" data-header="...">`.
fn extract_csrf_header_name(html: &str) -> Option<String> {
    let meta_pos = html.find("name=\"csrf-token\"")?;
    let after_meta = &html[meta_pos..];
    let val_start = after_meta.find("data-header=\"")? + 13;
    let val_end = after_meta[val_start..].find('"')?;
    let header = after_meta[val_start..val_start + val_end].to_owned();
    if header.is_empty() {
        None
    } else {
        Some(header)
    }
}

fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            b => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_encodes_special_chars() {
        assert_eq!(percent_encode("hello world"), "hello+world");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode("safe-chars_123"), "safe-chars_123");
    }

    #[test]
    fn run_export_prints_error_and_exits_on_bad_url() {
        // We cannot connect to localhost:9 (IANA discard port)
        // so the function should fail gracefully. We test that the
        // inner function returns Err rather than testing that it exits.
        let result = run_export_inner("posts", "http://127.0.0.1:9", None, None, None);
        assert!(result.is_err(), "should fail with unreachable URL");
    }

    #[test]
    fn run_import_prints_error_on_missing_file() {
        let result = run_import_inner(
            "posts",
            "http://127.0.0.1:3000",
            "/tmp/nonexistent_autumn_csv_import_test.csv",
            false,
            None,
        );
        assert!(result.is_err(), "should fail when file doesn't exist");
        assert!(
            result.unwrap_err().contains("Failed to read"),
            "error should mention file read failure"
        );
    }

    #[test]
    fn print_import_summary_handles_incomplete_html_gracefully() {
        // Should not panic on HTML that doesn't have the expected grid structure
        print_import_summary("<html>Something went wrong</html>", false);
    }

    #[test]
    fn extract_csrf_token_reads_from_meta_tag() {
        let html = r#"<meta name="csrf-token" content="tok-abc" data-header="X-CSRF-Token">"#;
        assert_eq!(extract_csrf_token(html), Some("tok-abc".to_owned()));
    }

    #[test]
    fn extract_csrf_token_returns_none_when_meta_absent() {
        assert_eq!(extract_csrf_token("<html></html>"), None);
    }

    #[test]
    fn extract_csrf_header_name_reads_data_header() {
        let html = r#"<meta name="csrf-token" content="tok" data-header="X-Custom-CSRF">"#;
        assert_eq!(
            extract_csrf_header_name(html),
            Some("X-Custom-CSRF".to_owned())
        );
    }

    #[test]
    fn extract_csrf_header_name_returns_none_when_absent() {
        let html = r#"<meta name="csrf-token" content="tok">"#;
        assert_eq!(extract_csrf_header_name(html), None);
    }
}
