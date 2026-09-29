use reqwest::blocking::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on a single request, so one slow response cannot stall a
/// worker thread for the whole run.
const PER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Totals from one [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationReport {
    /// Requests that got an HTTP response (any status).
    pub responses: usize,
    /// Requests that failed before a response: connection refused, DNS
    /// failure, timeout.
    pub errors: usize,
}

/// Drive GET requests at `url` from `concurrency` threads for `duration`.
///
/// No request is started after the deadline, and each request's timeout is
/// capped at the time left, so a hanging endpoint cannot stretch the run far
/// past `duration`.
pub fn run(url: &str, duration: Duration, concurrency: usize) -> Result<SimulationReport, String> {
    if concurrency == 0 {
        return Err("Concurrency must be > 0".into());
    }

    let url = url.to_owned();
    let responses = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let deadline = start + duration;

    let mut handles = Vec::with_capacity(concurrency);

    for _ in 0..concurrency {
        let url = url.clone();
        let responses = Arc::clone(&responses);
        let errors = Arc::clone(&errors);

        let handle = thread::spawn(move || {
            let client = Client::builder()
                .timeout(PER_REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| Client::new());

            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let outcome = client
                    .get(&url)
                    .timeout(remaining.min(PER_REQUEST_TIMEOUT))
                    .send();
                if outcome.is_ok() {
                    responses.fetch_add(1, Ordering::Relaxed);
                } else {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }

    let report = SimulationReport {
        responses: responses.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
    };
    let elapsed = start.elapsed().as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let rps = if elapsed > 0.0 {
        report.responses as f64 / elapsed
    } else {
        0.0
    };

    println!("Simulation complete.");
    println!(
        "{} responses in {elapsed:.2}s ({rps:.2} req/s), {} failed requests.",
        report.responses, report.errors
    );
    if report.responses == 0 && report.errors > 0 {
        return Err(format!(
            "no request to {url} got a response ({} failed); is the app running?",
            report.errors
        ));
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn spawn_mock_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
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

                let response = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        url
    }

    #[test]
    fn test_simulate_succeeds() {
        let url = spawn_mock_server();
        // A very short duration to keep the test fast
        let report = run(&url, Duration::from_millis(50), 2).expect("run succeeds");
        assert!(report.responses > 0, "{report:?}");
    }

    #[test]
    fn test_simulate_unreachable_url_reports_failures_not_traffic() {
        // Bind then drop a listener so the port is closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let result = run(
            &format!("http://127.0.0.1:{port}"),
            Duration::from_millis(50),
            1,
        );
        let err = result.expect_err("a closed port must not count as traffic");
        assert!(err.contains("got a response"), "{err}");
    }

    #[test]
    fn test_simulate_honours_deadline_against_hanging_endpoint() {
        // Accept connections but never answer.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            // Leak each accepted stream so the connection stays open and silent.
            for stream in listener.incoming().flatten() {
                std::mem::forget(stream);
            }
        });
        let start = Instant::now();
        let _ = run(
            &format!("http://127.0.0.1:{port}"),
            Duration::from_millis(200),
            1,
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "run overran its duration: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_simulate_zero_concurrency_fails() {
        let result = run("http://localhost:3000", Duration::from_millis(10), 0);
        assert!(result.is_err(), "Expected run to fail with concurrency 0");
    }
}
