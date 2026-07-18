//! Shared HTTP download helper used by `autumn setup` and `autumn assets`.

use indicatif::{ProgressBar, ProgressStyle};

/// Download `url` with a progress bar and return the response body as bytes.
///
/// Returns `Err(reqwest::Error)` so callers can convert with `?` into their
/// own error type (both [`crate::setup::SetupError`] and
/// [`crate::assets::AssetsError`] wrap `reqwest::Error` via `#[from]`).
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>, reqwest::Error> {
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
