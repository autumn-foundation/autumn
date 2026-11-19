//! Shared HTTP download helpers used by `autumn setup` and `autumn assets`.

use std::fmt;
use std::thread::sleep;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// Retry budget for transient network failures during `autumn setup` /
/// `autumn assets`.
///
/// A single dropped byte mid-transfer from a release CDN used to abort the
/// whole documented quickstart with a bare `download failed: error decoding
/// response body` and no second chance — 2 of 3 `quickstart-gate.yml` runs on
/// 2026-09-03 hit exactly this on the `autumn setup` step
/// (runs 33784307158, 33805758766; run 33797353571 in between passed with no
/// code change in between, confirming it's transient, not a real break).
/// A release host can also answer 5xx, or drop connections, for a minute or
/// more. Five attempts with a doubling wait (2, 4, 8, 16 s) ride that out.
const MAX_ATTEMPTS: u32 = 5;
/// The first wait. Each next wait doubles.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Download `url` with a progress bar and return the response body as bytes.
/// Retries transient failures (see [`MAX_ATTEMPTS`]) before giving up.
///
/// Returns `Err(reqwest::Error)` so callers can convert with `?` into their
/// own error type (both [`crate::setup::SetupError`] and
/// [`crate::assets::AssetsError`] wrap `reqwest::Error` via `#[from]`).
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>, reqwest::Error> {
    retry_transient(MAX_ATTEMPTS, RETRY_DELAY, is_retryable, || {
        fetch_bytes_once(url, false)
    })
}

/// Download `url` like [`fetch_bytes`], but over `https` only.
///
/// The client rejects an `http` URL and an `http` redirect target. Thus an
/// `https` source cannot redirect the download to plain `http`.
pub fn fetch_bytes_https_only(url: &str) -> Result<Vec<u8>, reqwest::Error> {
    retry_transient(MAX_ATTEMPTS, RETRY_DELAY, is_retryable, || {
        fetch_bytes_once(url, true)
    })
}

/// Whether a failed download is worth another try. A bad URL or a refused
/// redirect is not transient.
fn is_retryable(e: &reqwest::Error) -> bool {
    !e.is_builder() && !e.is_redirect() && retryable_status(e.status())
}

/// Fetch `url` as UTF-8 text (used for the small Tailwind checksums
/// manifest). Retries transient failures the same as [`fetch_bytes`].
pub fn fetch_text(url: &str) -> Result<String, reqwest::Error> {
    retry_transient(MAX_ATTEMPTS, RETRY_DELAY, is_retryable, || {
        fetch_text_once(url)
    })
}

fn fetch_bytes_once(url: &str, https_only: bool) -> Result<Vec<u8>, reqwest::Error> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .https_only(https_only)
        .build()?
        .get(url)
        .send()?
        .error_for_status()?;

    let total = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .expect("valid progress template")
            .progress_chars("=> "),
    );

    let bytes = response.bytes()?;
    pb.set_length(bytes.len() as u64);
    pb.finish_and_clear();
    Ok(bytes.to_vec())
}

fn fetch_text_once(url: &str) -> Result<String, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(url)
        .send()?
        .error_for_status()?
        .text()
}

/// Whether a failed download is worth another try: no status (the request
/// did not complete), a server error, or a rate limit.
fn retryable_status(status: Option<reqwest::StatusCode>) -> bool {
    status.is_none_or(|s| s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS)
}

/// The wait before retry `n` (1-based): `base`, then double each time.
fn backoff(base: Duration, n: u32) -> Duration {
    base.saturating_mul(1 << n.saturating_sub(1).min(16))
}

/// Retry `attempt` up to `max_attempts` times, waiting [`backoff`] of `delay`
/// between tries,
/// as long as `is_retryable` says the error is worth retrying. Returns the
/// last error once attempts (or retryability) are exhausted.
///
/// Generic over the error type and injected purely so the retry/backoff
/// bookkeeping can be exercised in tests without any real networking.
fn retry_transient<T, E: fmt::Display>(
    max_attempts: u32,
    delay: Duration,
    is_retryable: impl Fn(&E) -> bool,
    mut attempt: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut last_err = None;
    for n in 1..=max_attempts {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let will_retry = n < max_attempts && is_retryable(&e);
                if will_retry {
                    let wait = backoff(delay, n);
                    eprintln!(
                        "  download attempt {n}/{max_attempts} failed ({e}), retrying in {}s...",
                        wait.as_secs()
                    );
                    sleep(wait);
                }
                last_err = Some(e);
                if !will_retry {
                    break;
                }
            }
        }
    }
    Err(last_err.expect("max_attempts >= 1, so the loop runs at least once"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn server_errors_and_rate_limits_are_retried() {
        use reqwest::StatusCode;
        assert!(retryable_status(None), "a connection error is retried");
        for code in [500, 502, 503, 504, 429] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(retryable_status(Some(status)), "{code} is retried");
        }
        for code in [400, 401, 403, 404, 410] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(!retryable_status(Some(status)), "{code} is not retried");
        }
    }

    #[test]
    fn backoff_doubles_each_attempt() {
        let base = Duration::from_secs(2);
        let waits: Vec<u64> = (1..MAX_ATTEMPTS)
            .map(|n| backoff(base, n).as_secs())
            .collect();
        assert_eq!(waits, [2, 4, 8, 16]);
    }

    #[test]
    fn retry_transient_recovers_after_transient_failures() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_transient(
            3,
            Duration::from_millis(1),
            |_| true,
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err("transient")
                } else {
                    Ok(42)
                }
            },
        );
        assert_eq!(result, Ok(42));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn retry_transient_gives_up_after_max_attempts() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_transient(
            3,
            Duration::from_millis(1),
            |_| true,
            || {
                calls.set(calls.get() + 1);
                Err("still failing")
            },
        );
        assert_eq!(result, Err("still failing"));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn retry_transient_does_not_retry_non_retryable_errors() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_transient(
            3,
            Duration::from_millis(1),
            |_| false,
            || {
                calls.set(calls.get() + 1);
                Err("404 not found")
            },
        );
        assert_eq!(result, Err("404 not found"));
        assert_eq!(calls.get(), 1, "a non-retryable error must not be retried");
    }

    #[test]
    fn retry_transient_succeeds_on_first_try_without_delay() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_transient(
            3,
            Duration::from_secs(60),
            |_| true,
            || {
                calls.set(calls.get() + 1);
                Ok(7)
            },
        );
        assert_eq!(result, Ok(7));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn https_only_fetch_rejects_http_without_a_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/x.js", listener.local_addr().unwrap());
        let err = fetch_bytes_https_only(&url).expect_err("http must be rejected");
        assert!(
            !is_retryable(&err),
            "a scheme error is not transient: {err}"
        );
        let accepted = listener.accept();
        assert!(
            matches!(&accepted, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "no connection may be made: {accepted:?}"
        );
    }
}
