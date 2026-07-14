//! Maud form helpers for direct browser-to-storage uploads.
//!
//! These helpers render the HTML that wires an `<input type="file">` for the
//! direct upload flow. When JavaScript is available a data-attribute-driven
//! script can intercept the submit, obtain the presign envelope, PUT the bytes
//! directly to storage, and signal completion. When JavaScript is disabled the
//! input degrades to a standard file field that submits through the Autumn app
//! server (the existing through-app upload path).

use maud::{Markup, html};

/// Render an `<input type="file">` wired for direct-to-storage uploads.
///
/// The rendered markup includes:
///
/// - A container `<div>` with `data-controller="direct-upload"` and the
///   presign endpoint URL. A JS controller (e.g. a Stimulus controller or
///   custom htmx extension) hooks into these attributes.
/// - The file input itself, with optional MIME type restrictions via `accept`.
/// - A progress bar placeholder (hidden until JS shows it).
/// - A `<noscript>` fallback that informs the user about the JS dependency.
///
/// # Arguments
///
/// - `name`: The HTML `name` attribute of the `<input>` field. Must match the
///   field name expected by the completion route.
/// - `presign_url`: The app route that returns a `PresignPutResult` JSON
///   envelope. The JS controller POSTs to this URL before starting the upload.
/// - `accept`: Optional MIME type filter (e.g., `"image/*"` or
///   `"image/png,image/jpeg"`). Passed directly to `accept=""` on the input.
///
/// # Degradation
///
/// When JavaScript is disabled the `<noscript>` block explains the dependency.
/// If you want a full fallback to through-app upload, wrap the form in a
/// `<noscript>` version that uses `enctype="multipart/form-data"` and a
/// standard handler.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::storage::form_helper::direct_upload_input;
///
/// html! {
///     form method="post" action="/posts/new" {
///         (direct_upload_input("cover_image", "/uploads/presign", Some("image/*")))
///         button type="submit" { "Create Post" }
///     }
/// }
/// ```
#[must_use]
pub fn direct_upload_input(name: &str, presign_url: &str, accept: Option<&str>) -> Markup {
    html! {
        div
            data-controller="direct-upload"
            data-direct-upload-url=(presign_url)
            class="autumn-direct-upload" {
            input
                type="file"
                name=(name)
                aria-label="File upload"
                data-direct-upload-target="input"
                accept=[accept] {}
            div
                class="autumn-upload-progress"
                hidden="" {
                div class="autumn-upload-bar" {}
            }
            noscript {
                p class="autumn-upload-noscript" {
                    "Direct upload requires JavaScript. "
                    "Please enable JavaScript to upload files directly to storage."
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_upload_input_contains_required_attributes() {
        let markup = direct_upload_input("cover_image", "/uploads/presign", Some("image/*"));
        let html = markup.into_string();
        assert!(html.contains("data-controller=\"direct-upload\""));
        assert!(html.contains("data-direct-upload-url=\"/uploads/presign\""));
        assert!(html.contains("name=\"cover_image\""));
        assert!(html.contains("type=\"file\""));
        assert!(html.contains("aria-label=\"File upload\""));
        assert!(html.contains("accept=\"image/*\""));
        assert!(html.contains("autumn-upload-progress"));
        assert!(
            html.contains("direct upload requires javascript")
                || html.contains("Direct upload requires JavaScript")
        );
    }

    #[test]
    fn direct_upload_input_without_accept() {
        let markup = direct_upload_input("avatar", "/presign", None);
        let html = markup.into_string();
        assert!(!html.contains("accept="));
        assert!(html.contains("name=\"avatar\""));
    }

    #[test]
    fn direct_upload_input_includes_noscript_fallback() {
        let markup = direct_upload_input("file", "/presign", None);
        let html = markup.into_string();
        assert!(html.contains("<noscript>") || html.contains("noscript"));
    }
}
