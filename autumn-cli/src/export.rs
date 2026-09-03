use std::fs::File;
use std::io::Write;
use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{Value, json};

pub fn run(url: &str, output: &str) {
    if let Err(e) = run_inner(url, output) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

pub fn run_inner(url: &str, output: &str) -> Result<(), String> {
    let base_url = url.trim_end_matches('/');
    println!("Exporting diagnostics from {base_url}");

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let health = fetch_endpoint(&client, base_url, "/actuator/health");
    let metrics = fetch_endpoint(&client, base_url, "/actuator/metrics");
    let tasks = fetch_endpoint(&client, base_url, "/actuator/tasks");
    let loggers = fetch_endpoint(&client, base_url, "/actuator/loggers");

    // If any endpoint returns an error, fail the export
    for (name, val) in [
        ("health", &health),
        ("metrics", &metrics),
        ("tasks", &tasks),
        ("loggers", &loggers),
    ] {
        if let Some(err) = val.get("error") {
            return Err(format!(
                "Failed to fetch {} from {base_url}: {}",
                name,
                err.as_str().unwrap_or("Unknown error")
            ));
        }
    }

    // Use std::time for a simple timestamp as chrono is not available
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let snapshot = json!({
        "timestamp": timestamp,
        "url": base_url,
        "health": health,
        "metrics": metrics,
        "tasks": tasks,
        "loggers": loggers,
    });

    match File::create(output) {
        Ok(mut file) => {
            let json_str = serde_json::to_string_pretty(&snapshot).unwrap();
            if let Err(e) = file.write_all(json_str.as_bytes()) {
                return Err(format!("Failed to write to file '{output}': {e}"));
            }
            println!("Successfully exported diagnostics to {output}");
            Ok(())
        }
        Err(e) => Err(format!("Failed to create file '{output}': {e}")),
    }
}

fn fetch_endpoint(client: &Client, base_url: &str, path: &str) -> Value {
    let full_url = format!("{base_url}{path}");
    match client.get(&full_url).send() {
        Ok(response) => {
            if response.status().is_success() || response.status().as_u16() == 503 {
                response
                    .json::<Value>()
                    .unwrap_or_else(|e| json!({ "error": format!("Failed to parse JSON: {}", e) }))
            } else {
                json!({ "error": format!("HTTP {}", response.status()) })
            }
        }
        Err(e) => {
            json!({ "error": format!("Request failed: {}", e) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    fn test_client() -> Client {
        Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    fn spawn_mock_server(status_line: &str, body: &str, num_requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let status_line = status_line.to_string();
        let body = body.to_string();

        thread::spawn(move || {
            // Count requests actually served, not accept() attempts: the
            // original `for _ in 0..num_requests { if let Ok(..) =
            // listener.accept() { .. } }` spent one of its num_requests
            // "slots" on every accept() call, success or not, so a single
            // transient accept() error (observed on windows-latest CI —
            // loopback connections there are occasionally intercepted by
            // Defender/firewall and reset before the OS hands them to
            // accept()) silently starved the client of a response it was
            // always going to get otherwise, until its own timeout expired.
            // Retry on a transient accept()/read failure instead of
            // treating it as one of the num_requests attempts used up.
            let mut served = 0;
            while served < num_requests {
                let Ok((mut stream, _)) = listener.accept() else {
                    continue;
                };
                let mut reader = BufReader::new(&mut stream);
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_err() || req_line.is_empty() {
                    continue;
                }

                loop {
                    let mut header_line = String::new();
                    if reader.read_line(&mut header_line).is_err()
                        || header_line == "\r\n"
                        || header_line.trim().is_empty()
                    {
                        break;
                    }
                }

                let response = if body.is_empty() {
                    format!("{status_line}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                } else {
                    format!(
                        "{status_line}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                served += 1;
            }
        });

        url
    }

    #[test]
    fn test_fetch_endpoint_success() {
        let url = spawn_mock_server("HTTP/1.1 200 OK", r#"{"status": "ok"}"#, 1);

        let client = test_client();
        let val = fetch_endpoint(&client, &url, "/test");
        assert_eq!(val["status"], "ok");
    }

    #[test]
    fn test_fetch_endpoint_failure() {
        // Deliberately does NOT bind an ephemeral port and drop it to
        // "guarantee" the port is closed: every other test in this module
        // runs concurrently (the default test harness) and independently
        // calls `TcpListener::bind("127.0.0.1:0")` via `spawn_mock_server`,
        // so a just-freed ephemeral port can be reallocated by one of
        // THOSE binds before this test's client connects — observed on
        // windows-latest CI as this assertion failing because the request
        // landed on a live, unrelated mock server instead of getting
        // refused. Port 1 (tcpmux) is never bound by anything in this
        // suite or a CI sandbox and needs no privilege to *connect* to
        // (only to bind), so there is nothing to race against.
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let val = fetch_endpoint(&client, "http://127.0.0.1:1", "/test");
        assert!(val.get("error").is_some());
    }

    #[test]
    fn test_fetch_endpoint_404() {
        let url = spawn_mock_server("HTTP/1.1 404 NOT FOUND", "", 1);

        let client = test_client();
        let val = fetch_endpoint(&client, &url, "/test");
        assert!(val["error"].as_str().unwrap_or("").contains("HTTP 404"));
    }

    #[test]
    fn test_run_inner_success() {
        let url = spawn_mock_server("HTTP/1.1 200 OK", r#"{"status": "ok"}"#, 4);

        let output_file = tempfile::NamedTempFile::new().unwrap();
        let output_path = output_file.path().to_str().unwrap();

        let result = run_inner(&url, output_path);
        assert!(result.is_ok());

        let contents = std::fs::read_to_string(output_path).unwrap();
        let json: Value = serde_json::from_str(&contents).unwrap();

        assert_eq!(json["url"], url);
        assert_eq!(json["health"]["status"], "ok");
        assert_eq!(json["metrics"]["status"], "ok");
        assert_eq!(json["tasks"]["status"], "ok");
        assert_eq!(json["loggers"]["status"], "ok");
    }

    #[test]
    fn test_fetch_endpoint_invalid_json() {
        let url = spawn_mock_server("HTTP/1.1 200 OK", r#"{"status": "ok", "#, 1); // Invalid JSON

        let client = test_client();
        let val = fetch_endpoint(&client, &url, "/test");
        let error = val["error"].as_str().unwrap_or("");
        assert!(
            error.contains("Failed to parse JSON"),
            "expected JSON parse error, got {val}"
        );
    }

    #[test]
    fn test_run_inner_file_creation_error() {
        let url = spawn_mock_server("HTTP/1.1 200 OK", r#"{"status": "ok"}"#, 4);

        // Use an invalid path that cannot be created
        let result = run_inner(&url, "/invalid/path/that/does/not/exist/diag.json");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to create file"));
    }
}
