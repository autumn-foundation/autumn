//! Maud form helpers for direct browser-to-storage uploads.
//!
//! With JavaScript, `autumn-widgets.js` gets a presigned URL from your app,
//! sends the file straight to storage, and adds a hidden field with the blob
//! key. Without JavaScript, the input is a plain file field. It submits with
//! the form (use `enctype="multipart/form-data"`).

use maud::{Markup, html};

/// Options for [`direct_upload_field`].
///
/// ```rust
/// use autumn_web::storage::form_helper::DirectUploadConfig;
///
/// let config = DirectUploadConfig::new()
///     .label("Photos")
///     .accept("image/*")
///     .multiple(true);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectUploadConfig<'a> {
    label: Option<&'a str>,
    id: Option<&'a str>,
    accept: Option<&'a str>,
    multiple: bool,
}

impl<'a> DirectUploadConfig<'a> {
    /// Make a config. The label comes from the field name. It takes one file.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            label: None,
            id: None,
            accept: None,
            multiple: false,
        }
    }

    /// The visible label text. Default: the field name, for example
    /// `cover_image` becomes "Cover image".
    #[must_use]
    pub const fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// The `id` of the input. Default: `upload-<name>`.
    #[must_use]
    pub const fn id(mut self, id: &'a str) -> Self {
        self.id = Some(id);
        self
    }

    /// The `accept` filter, for example `image/*` or `.pdf,image/png`. The
    /// script also checks it on a drop. Your server must still validate.
    #[must_use]
    pub const fn accept(mut self, accept: &'a str) -> Self {
        self.accept = Some(accept);
        self
    }

    /// Allow more than one file. Each file gets its own hidden field.
    #[must_use]
    pub const fn multiple(mut self, multiple: bool) -> Self {
        self.multiple = multiple;
        self
    }
}

/// The label text for a field name: `cover_image` becomes "Cover image".
fn humanize(name: &str) -> String {
    if name.is_empty() {
        return "File upload".to_owned();
    }
    let spaced = name.replace('_', " ");
    let mut chars = spaced.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// Render a direct-to-storage upload field.
///
/// The markup has a visible `<label>`, a file input, a progress bar, a polite
/// status line and an assertive error line. The whole widget is the dropzone.
///
/// # Script contract
///
/// `autumn-widgets.js` does the following for each file:
///
/// 1. `POST`s JSON `{filename, content_type, size}` to `presign_url`, with the
///    CSRF token from `<meta name="csrf-token">`.
/// 2. Expects JSON `{url, method, headers, key}` back. `url`, `method` and
///    `headers` come from [`PresignPutResult`](super::PresignPutResult).
///    Your app chooses `key`.
/// 3. Sends the bytes to `url` and shows the progress.
/// 4. Adds `<input type="hidden" name="<name>" value="<key>">`.
///
/// Your form handler then calls
/// [`complete_direct_upload`](super::complete_direct_upload) with the key.
/// With the script, the form sends no file bytes. A failed upload adds no
/// key, so check the field on the server.
///
/// The script fires `autumn:upload` (`detail.key`, `detail.file`) and
/// `autumn:upload-error` (`detail.file`) on the widget.
///
/// # Without JavaScript
///
/// The input keeps its `name` and submits the file with the form. The form
/// needs `enctype="multipart/form-data"` and a multipart handler.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::storage::form_helper::{DirectUploadConfig, direct_upload_field};
///
/// html! {
///     form method="post" action="/posts" enctype="multipart/form-data" {
///         (direct_upload_field(
///             "cover_image",
///             "/uploads/presign",
///             &DirectUploadConfig::new().label("Cover image").accept("image/*"),
///         ))
///         button type="submit" { "Create post" }
///     }
/// }
/// ```
#[must_use]
pub fn direct_upload_field(
    name: &str,
    presign_url: &str,
    config: &DirectUploadConfig<'_>,
) -> Markup {
    let id = config.id.map_or_else(
        || format!("upload-{}", if name.is_empty() { "file" } else { name }),
        str::to_owned,
    );
    let label_id = format!("{id}-label");
    let label = config.label.map_or_else(|| humanize(name), str::to_owned);
    html! {
        div
            class="autumn-direct-upload"
            data-autumn-direct-upload
            data-autumn-upload-url=(presign_url)
            data-autumn-upload-field=(name)
            data-controller="direct-upload"
            data-direct-upload-url=(presign_url) {
            label for=(id) id=(label_id) class="autumn-upload-label" { (label) }
            input
                type="file"
                id=(id)
                name=(name)
                accept=[config.accept]
                multiple[config.multiple]
                aria-describedby=(format!("{id}-status {id}-error"))
                data-direct-upload-target="input" {}
            span class="autumn-upload-hint" data-autumn-upload-hint hidden="" {
                @if config.multiple { "Or drop files here." } @else { "Or drop a file here." }
            }
            div
                class="autumn-upload-progress"
                role="progressbar"
                aria-labelledby=(label_id)
                aria-valuemin="0"
                aria-valuemax="100"
                aria-valuenow="0"
                hidden="" {
                div class="autumn-upload-bar" {}
            }
            p class="autumn-upload-status" id=(format!("{id}-status")) role="status" {}
            p class="autumn-upload-error" id=(format!("{id}-error")) role="alert" {}
            div data-autumn-upload-fields {}
            noscript {
                p class="autumn-upload-noscript" {
                    "JavaScript is off. The file uploads with the form."
                }
            }
        }
    }
}

/// Render a direct-to-storage upload field with the default options.
///
/// This is [`direct_upload_field`] with an optional `accept` filter. See that
/// function for the script contract and the no-JavaScript path.
#[must_use]
pub fn direct_upload_input(name: &str, presign_url: &str, accept: Option<&str>) -> Markup {
    let config = DirectUploadConfig {
        accept,
        ..DirectUploadConfig::new()
    };
    direct_upload_field(name, presign_url, &config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(config: &DirectUploadConfig<'_>) -> String {
        direct_upload_field("cover_image", "/uploads/presign", config).into_string()
    }

    #[test]
    fn direct_upload_input_contains_required_attributes() {
        let html =
            direct_upload_input("cover_image", "/uploads/presign", Some("image/*")).into_string();
        assert!(html.contains("data-autumn-direct-upload"), "{html}");
        assert!(html.contains("data-autumn-upload-url=\"/uploads/presign\""));
        assert!(html.contains("data-autumn-upload-field=\"cover_image\""));
        assert!(html.contains("name=\"cover_image\""));
        assert!(html.contains("type=\"file\""));
        assert!(html.contains("accept=\"image/*\""));
        assert!(html.contains("autumn-upload-progress"));
    }

    #[test]
    fn direct_upload_input_without_accept() {
        let html = direct_upload_input("avatar", "/presign", None).into_string();
        assert!(!html.contains("accept="));
        assert!(html.contains("name=\"avatar\""));
    }

    #[test]
    fn the_input_has_a_visible_label_and_no_aria_label() {
        let html = direct_upload_input("cover_image", "/presign", None).into_string();
        assert!(html.contains("<label for=\"upload-cover_image\""), "{html}");
        assert!(html.contains(">Cover image</label>"), "{html}");
        assert!(html.contains("id=\"upload-cover_image\""), "{html}");
        assert!(!html.contains("aria-label="), "{html}");
    }

    #[test]
    fn an_empty_name_still_gets_a_label() {
        let html = direct_upload_input("", "/presign", None).into_string();
        assert!(html.contains(">File upload</label>"), "{html}");
    }

    #[test]
    fn config_sets_label_id_and_multiple() {
        let config = DirectUploadConfig::new()
            .label("Photos")
            .id("photos")
            .multiple(true);
        let html = render(&config);
        assert!(html.contains("<label for=\"photos\""), "{html}");
        assert!(html.contains(">Photos</label>"), "{html}");
        assert!(html.contains("id=\"photos\""), "{html}");
        assert!(html.contains("multiple"), "{html}");
    }

    #[test]
    fn the_label_text_is_escaped() {
        let html = render(&DirectUploadConfig::new().label("<b>x</b>"));
        assert!(!html.contains("<b>x</b>"), "{html}");
    }

    #[test]
    fn progress_status_and_error_are_announced() {
        let html = direct_upload_input("file", "/presign", None).into_string();
        assert!(html.contains("role=\"progressbar\""), "{html}");
        assert!(html.contains("aria-valuemin=\"0\""), "{html}");
        assert!(html.contains("aria-valuemax=\"100\""), "{html}");
        assert!(html.contains("role=\"status\""), "{html}");
        assert!(html.contains("role=\"alert\""), "{html}");
        assert!(html.contains("data-autumn-upload-fields"), "{html}");
    }

    #[test]
    fn the_progress_bar_is_named_by_the_label() {
        let html = direct_upload_input("file", "/presign", None).into_string();
        assert!(
            html.contains("aria-labelledby=\"upload-file-label\""),
            "{html}"
        );
        assert!(html.contains("id=\"upload-file-label\""), "{html}");
    }

    #[test]
    fn the_dropzone_hint_is_hidden_until_the_script_runs() {
        let html = direct_upload_input("file", "/presign", None).into_string();
        assert!(html.contains("data-autumn-upload-hint"), "{html}");
        let hint = html.split("data-autumn-upload-hint").nth(1).unwrap_or("");
        assert!(
            hint.split('>').next().unwrap_or("").contains("hidden"),
            "{html}"
        );
    }

    #[test]
    fn without_script_the_input_is_a_plain_named_file_field() {
        let html = direct_upload_input("file", "/presign", None).into_string();
        let input = html
            .split("<input")
            .nth(1)
            .and_then(|t| t.split('>').next())
            .unwrap_or("");
        assert!(input.contains("name=\"file\""), "{input}");
        assert!(!input.contains("disabled"), "{input}");
        assert!(!input.contains("hidden"), "{input}");
    }

    #[test]
    fn the_noscript_note_says_the_file_goes_with_the_form() {
        let html = direct_upload_input("file", "/presign", None).into_string();
        assert!(html.contains("<noscript>"), "{html}");
        assert!(html.contains("with the form"), "{html}");
        assert!(!html.contains("requires JavaScript"), "{html}");
    }
}
