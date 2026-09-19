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
/// Three attempts with a short backoff resolve almost all of them before the
/// user ever sees an error.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Download `url` with a progress bar and return the response body as bytes.
/// Retries transient failures (see [`MAX_ATTEMPTS`]) before giving up.
///
/// Returns `Err(reqwest::Error)` so callers can convert with `?` into their
/// own error type (both [`crate::setup::SetupError`] and
/// [`crate::assets::AssetsError`] wrap `reqwest::Error` via `#[from]`).
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>, reqwest::Error> {
    retry_transient(
        MAX_ATTEMPTS,
        RETRY_DELAY,
        |e: &reqwest::Error| !e.is_status(),
        || fetch_bytes_once(url),
    )
}

/// Fetch `url` as UTF-8 text (used for the small Tailwind checksums
/// manifest). Retries transient failures the same as [`fetch_bytes`].
pub fn fetch_text(url: &str) -> Result<String, reqwest::Error> {
    retry_transient(
        MAX_ATTEMPTS,
        RETRY_DELAY,
        |e: &reqwest::Error| !e.is_status(),
        || fetch_text_once(url),
    )
}

fn fetch_bytes_once(url: &str) -> Result<Vec<u8>, reqwest::Error> {
    let response = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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

/// Retry `attempt` up to `max_attempts` times, waiting `delay` between tries,
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
                    eprintln!("  download attempt {n}/{max_attempts} failed ({e}), retrying...");
                    sleep(delay);
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
}
