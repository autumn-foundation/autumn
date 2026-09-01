use reqwest::blocking::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub fn run(url: &str, duration: Duration, concurrency: usize) -> Result<(), String> {
    if concurrency == 0 {
        return Err("Concurrency must be > 0".into());
    }

    let url = url.to_owned();
    let requests_sent = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();

    let mut handles = Vec::with_capacity(concurrency);

    for _ in 0..concurrency {
        let url = url.clone();
        let requests_sent = Arc::clone(&requests_sent);

        let handle = thread::spawn(move || {
            let client = Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| Client::new());

            while start.elapsed() < duration {
                // Ignore the result of the request, we just want to generate load
                let _ = client.get(&url).send();
                requests_sent.fetch_add(1, Ordering::Relaxed);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }

    let total = requests_sent.load(Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let rps = total as f64 / elapsed;

    println!("Simulation complete.");
    println!("Sent {total} requests in {elapsed:.2}s ({rps:.2} req/s).");

    Ok(())
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
        let result = run(&url, Duration::from_millis(50), 2);
        assert!(
            result.is_ok(),
            "Expected run to succeed, but got {:?}",
            result.err()
        );
    }

    #[test]
    fn test_simulate_zero_concurrency_fails() {
        let result = run("http://localhost:3000", Duration::from_millis(10), 0);
        assert!(result.is_err(), "Expected run to fail with concurrency 0");
    }
}
