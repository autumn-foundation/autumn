//! `autumn check` — code and accessibility checks for Autumn projects.
//!
//! `autumn check --a11y` runs a pure-Rust static HTML analysis pass over one
//! or more HTML documents and reports WCAG 2.1 AA violations at the Serious or
//! Critical level. No Node.js or external tooling required.

/// Severity of an accessibility violation (mirrors axe-core's taxonomy).
///
/// Variants are ordered from least to most severe so that `Critical > Serious > Moderate`
/// holds for comparisons (derived `Ord` compares by discriminant index).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Degrades experience — worth addressing.
    Moderate,
    /// Causes significant barriers — should fix.
    Serious,
    /// Blocks access for some users — must fix.
    Critical,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Critical => write!(f, "Critical"),
            Self::Serious => write!(f, "Serious"),
            Self::Moderate => write!(f, "Moderate"),
        }
    }
}

/// A single accessibility violation found in an HTML document.
#[derive(Debug, Clone)]
pub struct A11yViolation {
    /// Short rule identifier (mirrors axe-core rule IDs where applicable).
    pub rule_id: &'static str,
    pub severity: Severity,
    /// Human-readable description of what is wrong.
    pub description: String,
    /// Remediation hint.
    pub help: &'static str,
}

/// Options for the `autumn check --a11y` subcommand.
#[derive(Debug, Clone)]
pub struct A11yCheckOptions {
    /// URL of a running Autumn app. When set, fetches HTML from the root.
    pub url: Option<String>,
    /// Inline HTML to analyse directly (used in tests and CI pre-render mode).
    pub html: Option<String>,
}

/// Run the config check on autumn.toml and profiles.
pub fn run_config_check() -> Result<(), String> {
    run_config_check_impl(std::path::Path::new("."))
}

/// Helper for `run_config_check` allowing path injection for testing.
pub fn run_config_check_impl(dir: &std::path::Path) -> Result<(), String> {
    let toml_path = dir.join("autumn.toml");
    if !toml_path.exists() {
        return Err("autumn.toml not found".to_string());
    }

    let toml_content = std::fs::read_to_string(&toml_path)
        .map_err(|e| format!("failed to read autumn.toml: {e}"))?;

    let schema = autumn_web::config::AutumnConfig::get_schema_keys();
    let mut errors = Vec::new();

    // 1. Validate base autumn.toml
    let base_errors = crate::doctor::validate_toml_content(&toml_content, &schema);
    for (path, sug) in base_errors {
        errors.push(format!(
            "autumn.toml: unknown key \"{path}\"{}",
            sug.map(|s| format!(" — did you mean \"{s}\"?"))
                .unwrap_or_default()
        ));
    }

    // 2. Validate any autumn-*.toml profile files
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                let is_profile = path.is_file()
                    && filename.starts_with("autumn-")
                    && std::path::Path::new(filename)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
                if is_profile {
                    let read_result = std::fs::read_to_string(&path);
                    if let Ok(file_content) = read_result {
                        let profile_errors =
                            crate::doctor::validate_toml_content(&file_content, &schema);
                        for (p_path, sug) in profile_errors {
                            errors.push(format!(
                                "{filename}: unknown key \"{p_path}\"{}",
                                sug.map(|s| format!(" — did you mean \"{s}\"?"))
                                    .unwrap_or_default()
                            ));
                        }
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Run the a11y audit and return all violations found.
///
/// Returns `Err` only on I/O failures (e.g. cannot connect to `url`); an
/// empty `Vec` means no violations were detected.
///
/// # Errors
///
/// Returns an error string when the HTTP fetch fails.
pub fn run_a11y_check(opts: &A11yCheckOptions) -> Result<Vec<A11yViolation>, String> {
    let html = if let Some(ref inline) = opts.html {
        inline.clone()
    } else if let Some(ref url) = opts.url {
        fetch_html(url)?
    } else {
        return Err("supply either --url or inline HTML".into());
    };

    Ok(analyse_html(&html))
}

/// Fetch the HTML body from `url` using a blocking HTTP GET (10 s timeout).
fn fetch_html(url: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    client
        .get(url)
        .send()
        .map_err(|e| format!("failed to fetch {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("server returned error status for {url}: {e}"))?
        .text()
        .map_err(|e| format!("failed to read response body: {e}"))
}

// ── HTML static analysis ────────────────────────────────────────────

/// Analyse a raw HTML string and return all detected violations.
pub fn analyse_html(html: &str) -> Vec<A11yViolation> {
    let html = strip_html_comments(html).to_ascii_lowercase();
    let mut violations = Vec::new();
    check_lang(&html, &mut violations);
    check_skip_link(&html, &mut violations);
    check_landmark_main(&html, &mut violations);
    check_images_alt(&html, &mut violations);
    check_inputs_have_labels(&html, &mut violations);
    check_buttons_accessible_name(&html, &mut violations);
    violations
}

/// Check: `<html>` element must carry a non-empty `lang` attribute.
///
/// WCAG 2.1 SC 3.1.1 (Level A) — axe-core rule `html-has-lang`.
fn check_lang(html: &str, out: &mut Vec<A11yViolation>) {
    // Verify the value is non-empty: lang="" fails WCAG just as much as no lang.
    let has_lang = html.find("<html").is_some_and(|start| {
        let tag_end = html[start..].find('>').unwrap_or(0);
        let tag = &html[start..start + tag_end];
        attr_value(tag, "lang").is_some_and(|val| !val.is_empty())
    });

    if !has_lang {
        out.push(A11yViolation {
            rule_id: "html-has-lang",
            severity: Severity::Serious,
            description: "<html> element does not have a lang attribute".into(),
            help: "Add lang=\"en\" (or the appropriate BCP 47 tag) to the <html> element",
        });
    }
}

/// Check: page must have a skip-to-content link as the first focusable element.
///
/// WCAG 2.1 SC 2.4.1 (Level A) — axe-core rule `bypass`.
fn check_skip_link(html: &str, out: &mut Vec<A11yViolation>) {
    let body_start = html.find("<body").unwrap_or(0);
    let body_content = &html[body_start..];

    // The skip link must be the FIRST focusable link in the document; a skip
    // link that comes after navigation links doesn't help keyboard users.
    // Verify by finding the first <a > and checking it is the skip link.
    let has_skip_link = body_content.find("<a ").is_some_and(|a_pos| {
        let rest = &body_content[a_pos..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        let href = attr_value(tag, "href");
        tag.contains("skip-link")
            || href.as_deref() == Some("#main-content")
            || href.as_deref() == Some("#main")
    });

    if !has_skip_link {
        out.push(A11yViolation {
            rule_id: "bypass",
            severity: Severity::Serious,
            description: "Page does not have a skip-to-content link".into(),
            help: r##"Add a visually-hidden <a href="#main-content" class="skip-link"> as the first element inside <body>"##,
        });
    }
}

/// Check: page must contain exactly one `<main>` landmark region.
///
/// WCAG 2.1 SC 1.3.6 (Level AAA) / best practice — axe-core rule `landmark-one-main`.
fn check_landmark_main(html: &str, out: &mut Vec<A11yViolation>) {
    let main_tag_count = count_opening_tags(html, "main");

    // Count role="main" only on non-<main> elements to avoid double-counting
    // the common pattern <main role="main">.
    let mut extra_role_main = 0usize;
    let mut offset = 0usize;
    while let Some(rel) = html[offset..].find('<') {
        let abs = offset + rel;
        let rest = &html[abs..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if !opening_tag_matches(tag, "main") && attr_value(tag, "role").as_deref() == Some("main") {
            extra_role_main += 1;
        }
        offset = abs + tag_end.saturating_add(1);
    }

    let main_count = main_tag_count + extra_role_main;
    match main_count {
        0 => {
            out.push(A11yViolation {
                rule_id: "landmark-one-main",
                severity: Severity::Moderate,
                description: "Page does not contain a <main> landmark region".into(),
                help: "Wrap your primary content in a <main> element",
            });
        }
        2.. => {
            out.push(A11yViolation {
                rule_id: "landmark-one-main",
                severity: Severity::Moderate,
                description: format!(
                    "Page has {main_count} <main> landmarks; exactly one is required"
                ),
                help: "Ensure only one <main> element or role=\"main\" exists per page",
            });
        }
        _ => {}
    }
}

/// Check: `<img>` elements must have an `alt` attribute.
///
/// WCAG 2.1 SC 1.1.1 (Level A) — axe-core rule `image-alt`.
fn check_images_alt(html: &str, out: &mut Vec<A11yViolation>) {
    let mut search = html;
    let mut count = 0u32;

    while let Some(img_pos) = search.find("<img") {
        let rest = &search[img_pos..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];

        if attr_value(tag, "alt").is_none() {
            count += 1;
        }
        search = &search[img_pos + 4..];
    }

    if count > 0 {
        out.push(A11yViolation {
            rule_id: "image-alt",
            severity: Severity::Critical,
            description: format!("{count} <img> element(s) are missing an alt attribute"),
            help: "Add alt=\"description\" to every <img>; use alt=\"\" for decorative images",
        });
    }
}

/// Check: interactive form controls must have an associated `<label>`.
///
/// Covers `<input>`, `<textarea>`, and `<select>`.
/// WCAG 2.1 SC 1.3.1 / 3.3.2 (Level A) — axe-core rule `label`.
fn check_inputs_have_labels(html: &str, out: &mut Vec<A11yViolation>) {
    let mut unlabelled = 0u32;

    // Collect all `for=` values from `<label` tags.
    let mut labelled_ids: Vec<String> = Vec::new();
    let mut lsearch = html;
    while let Some(pos) = lsearch.find("<label") {
        let rest = &lsearch[pos..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if let Some(id) = attr_value(tag, "for").filter(|id| !id.is_empty()) {
            labelled_ids.push(id);
        }
        lsearch = &lsearch[pos + 6..];
        if lsearch.is_empty() {
            break;
        }
    }

    // Collect byte ranges of wrapped <label>…</label> blocks so that controls
    // implicitly associated by containment (no for=/id pair needed) pass.
    let label_regions = label_regions(html);

    // Check <input>, <textarea>, and <select> — all require an accessible label.
    for (tag_name, skip_len) in &[("<input", 6usize), ("<textarea", 9), ("<select", 7)] {
        let mut offset = 0usize;
        while let Some(rel_pos) = html[offset..].find(tag_name) {
            let abs_pos = offset + rel_pos;
            let rest = &html[abs_pos..];
            let tag_end = rest.find('>').unwrap_or(rest.len());
            let tag = &rest[..tag_end];

            // For <input>, skip types that don't need a visible label.
            let skip = if *tag_name == "<input" {
                let input_type = attr_value(tag, "type").unwrap_or_default();
                ["hidden", "submit", "button", "reset", "image"].contains(&input_type.as_str())
            } else {
                false
            };

            if !skip {
                // Require non-empty aria-label values and resolve aria-labelledby IDs.
                let has_aria = attr_value(tag, "aria-label").is_some_and(|label| !label.is_empty())
                    || attr_value(tag, "aria-labelledby")
                        .is_some_and(|ids| !ids.is_empty() && aria_labelledby_resolves(html, &ids));
                let has_for = attr_value(tag, "id")
                    .is_some_and(|id_val| !id_val.is_empty() && labelled_ids.contains(&id_val));
                // A wrapping <label> only provides an accessible name when it
                // contains visible text; <label><input></label> still fails.
                let is_wrapped = label_regions.iter().any(|r| {
                    r.contains(&abs_pos) && !strip_html_tags(&html[r.clone()]).trim().is_empty()
                });

                if !has_aria && !has_for && !is_wrapped {
                    unlabelled += 1;
                }
            }

            offset = abs_pos + skip_len;
        }
    }

    if unlabelled > 0 {
        out.push(A11yViolation {
            rule_id: "label",
            severity: Severity::Critical,
            description: format!("{unlabelled} form control(s) are not associated with a <label>"),
            help: "Add <label for=\"id\"> or wrap the control in <label>, or use aria-label",
        });
    }
}

/// Return the byte ranges of all `<label>…</label>` blocks in `html`.
fn label_regions(html: &str) -> Vec<std::ops::Range<usize>> {
    let mut regions = Vec::new();
    let mut offset = 0usize;
    while let Some(rel_start) = html[offset..].find("<label") {
        let abs_start = offset + rel_start;
        if let Some(close_rel) = html[abs_start..].find("</label>") {
            let abs_end = abs_start + close_rel + 8; // 8 = len("</label>")
            regions.push(abs_start..abs_end);
            offset = abs_end;
        } else {
            offset = abs_start + 6;
        }
    }
    regions
}

/// Check: `<button>` elements must have an accessible name.
///
/// WCAG 2.1 SC 4.1.2 (Level A) — axe-core rule `button-name`.
fn check_buttons_accessible_name(html: &str, out: &mut Vec<A11yViolation>) {
    let mut nameless = 0u32;
    let mut search = html;

    while let Some(pos) = search.find("<button") {
        let rest = &search[pos..];
        let (close, btn_end) = rest
            .find("</button>")
            .map_or((rest.len(), rest.len()), |c| (c, c + 9));
        let button_html = &rest[..btn_end];
        let tag_end = rest.find('>').unwrap_or(0);
        let tag = &rest[..tag_end];

        // Require non-empty aria-label values and resolve aria-labelledby IDs.
        let has_aria_label = attr_value(tag, "aria-label").is_some_and(|label| !label.is_empty())
            || attr_value(tag, "aria-labelledby")
                .is_some_and(|ids| !ids.is_empty() && aria_labelledby_resolves(html, &ids));
        let inner = if close > tag_end {
            &button_html[tag_end + 1..close]
        } else {
            ""
        };
        // Strip HTML tags (e.g. <img>) before checking for visible text so that a
        // button containing only an image without alt text is not incorrectly named.
        // Also accept a non-empty alt on a child <img> — that alt contributes to
        // the button's accessible name per the AccName algorithm.
        let has_text = !strip_html_tags(inner).trim().is_empty() || img_alt_text(inner);

        if !has_aria_label && !has_text {
            nameless += 1;
        }

        search = &search[pos + 7..];
        if search.is_empty() {
            break;
        }
    }

    if nameless > 0 {
        out.push(A11yViolation {
            rule_id: "button-name",
            severity: Severity::Critical,
            description: format!("{nameless} button(s) have no accessible name"),
            help: "Add visible text inside <button> or use aria-label",
        });
    }
}

/// Returns `true` when `html` contains at least one `<img>` with a non-empty `alt`.
///
/// Used to recognise icon buttons whose accessible name comes from the child image.
fn img_alt_text(html: &str) -> bool {
    let mut search = html;
    while let Some(img_pos) = search.find("<img") {
        let rest = &search[img_pos..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if attr_value(tag, "alt").is_some_and(|alt| !alt.is_empty()) {
            return true;
        }
        search = &search[img_pos + 4..];
    }
    false
}

/// Check whether all IDs referenced by an `aria-labelledby` attribute resolve to
/// elements with usable text content.
///
/// `html` should already be lowercased. Splits `ids` on whitespace and verifies
/// that at least one referenced element exists and has non-empty text.
fn aria_labelledby_resolves(html: &str, ids: &str) -> bool {
    ids.split_whitespace().any(|id| {
        let mut offset = 0usize;
        while let Some(rel_pos) = html[offset..].find('<') {
            let tag_start = offset + rel_pos;
            let rest = &html[tag_start..];
            let tag_end_rel = rest.find('>').unwrap_or(rest.len());
            let tag_end = tag_start + tag_end_rel;
            let tag = &html[tag_start..tag_end];

            if attr_value(tag, "id").as_deref() == Some(id) {
                let after_tag = (tag_end + 1).min(html.len());
                let content_end = (after_tag + 500).min(html.len());
                let inner_raw = &html[after_tag..content_end];
                let inner = &inner_raw[..inner_raw.find("</").unwrap_or(inner_raw.len())];
                return !strip_html_tags(inner).trim().is_empty();
            }

            offset = tag_end.saturating_add(1);
        }
        false
    })
}

/// Remove all HTML tags from `s`, leaving only text nodes.
fn strip_html_tags(s: &str) -> String {
    let without_raw_text = strip_raw_text_element(&strip_raw_text_element(s, "script"), "style");
    let mut result = String::with_capacity(without_raw_text.len());
    let mut i = 0usize;

    while i < without_raw_text.len() {
        if without_raw_text.as_bytes()[i] == b'<'
            && looks_like_tag_start_at(&without_raw_text, i)
            && let Some(tag_end) = without_raw_text[i..].find('>')
        {
            i += tag_end + 1;
            continue;
        }

        if let Some(ch) = without_raw_text[i..].chars().next() {
            result.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    decode_basic_html_entities(&result)
}

fn strip_html_comments(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find("<!--") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 4..];
        if let Some(end) = after_open.find("-->") {
            rest = &after_open[end + 3..];
        } else {
            return result;
        }
    }

    result.push_str(rest);
    result
}

fn count_opening_tags(html: &str, name: &str) -> usize {
    let mut count = 0usize;
    let mut offset = 0usize;
    while let Some(rel_pos) = html[offset..].find('<') {
        let tag_start = offset + rel_pos;
        let rest = &html[tag_start..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if opening_tag_matches(tag, name) {
            count += 1;
        }
        offset = tag_start + tag_end.saturating_add(1);
    }
    count
}

fn opening_tag_matches(tag: &str, name: &str) -> bool {
    let bytes = tag.as_bytes();
    let name = name.as_bytes();
    if bytes.first() != Some(&b'<') || bytes.get(1) == Some(&b'/') {
        return false;
    }
    if bytes.get(1..1 + name.len()) != Some(name) {
        return false;
    }
    bytes
        .get(1 + name.len())
        .is_none_or(|b| b.is_ascii_whitespace() || matches!(*b, b'>' | b'/'))
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let attr = attr.as_bytes();
    let mut i = 0usize;

    while i + attr.len() <= bytes.len() {
        if bytes[i..].starts_with(attr)
            && attr_boundary_before(bytes, i)
            && attr_boundary_after(bytes, i + attr.len())
        {
            let mut pos = i + attr.len();
            while bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
                pos += 1;
            }
            if bytes.get(pos) != Some(&b'=') {
                i += 1;
                continue;
            }
            pos += 1;
            while bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
                pos += 1;
            }

            return Some(match bytes.get(pos) {
                Some(b'"' | b'\'') => {
                    let quote = bytes[pos];
                    let start = pos + 1;
                    let end = bytes[start..]
                        .iter()
                        .position(|b| *b == quote)
                        .map_or(bytes.len(), |rel| start + rel);
                    tag[start..end].to_owned()
                }
                Some(_) => {
                    let start = pos;
                    let end = bytes[start..]
                        .iter()
                        .position(|b| b.is_ascii_whitespace() || *b == b'>')
                        .map_or(bytes.len(), |rel| start + rel);
                    tag[start..end].to_owned()
                }
                None => String::new(),
            });
        }
        i += 1;
    }

    None
}

fn attr_boundary_before(bytes: &[u8], pos: usize) -> bool {
    pos == 0 || bytes[pos - 1].is_ascii_whitespace() || matches!(bytes[pos - 1], b'<' | b'/')
}

fn attr_boundary_after(bytes: &[u8], pos: usize) -> bool {
    bytes
        .get(pos)
        .is_none_or(|b| b.is_ascii_whitespace() || matches!(*b, b'=' | b'>' | b'/'))
}

fn strip_raw_text_element(html: &str, name: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut result = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while let Some(rel_start) = lower[cursor..].find(&open) {
        let start = cursor + rel_start;
        let rest = &lower[start..];
        let Some(tag_end_rel) = rest.find('>') else {
            break;
        };
        let tag_end = start + tag_end_rel + 1;
        if !opening_tag_matches(&lower[start..tag_end], name) {
            result.push_str(&html[cursor..=start]);
            cursor = start + 1;
            continue;
        }

        result.push_str(&html[cursor..start]);
        if let Some(close_rel) = lower[tag_end..].find(&close) {
            cursor = tag_end + close_rel + close.len();
        } else {
            cursor = html.len();
            break;
        }
    }

    result.push_str(&html[cursor..]);
    result
}

fn looks_like_tag_start_at(s: &str, pos: usize) -> bool {
    s.as_bytes()
        .get(pos + 1)
        .is_some_and(|b| b.is_ascii_alphabetic() || matches!(*b, b'/' | b'!' | b'?'))
}

fn decode_basic_html_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Extract the value of an attribute from HTML text starting just after `attr=`.
///
/// Handles both quoted (`attr="val"`, `attr='val'`) and unquoted forms.
#[cfg(test)]
fn extract_attr_value(s: &str) -> String {
    let s = s.trim_start();
    match s.chars().next() {
        Some('"') => s[1..].split('"').next().unwrap_or("").to_string(),
        Some('\'') => s[1..].split('\'').next().unwrap_or("").to_string(),
        _ => s
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_string(),
    }
}

// ── CLI output ──────────────────────────────────────────────────────

/// Print violations to stdout in a human-readable format.
///
/// Returns `true` when violations at or above the failure threshold are found.
/// With `critical_only = false` (default) both Critical and Serious violations
/// trigger a non-zero exit. With `critical_only = true` only Critical violations
/// do.
pub fn print_report(violations: &[A11yViolation], url: &str, critical_only: bool) -> bool {
    if violations.is_empty() {
        println!("✅  autumn check --a11y: no violations found in {url}");
        return false;
    }

    println!("autumn check --a11y: analysed {url}");
    println!();

    let mut fail = false;
    for v in violations {
        let icon = match v.severity {
            Severity::Critical => {
                fail = true;
                "❌"
            }
            Severity::Serious => {
                if !critical_only {
                    fail = true;
                }
                "❌"
            }
            Severity::Moderate => "⚠️ ",
        };
        println!("{icon} [{}] {} — {}", v.severity, v.rule_id, v.description);
        println!("   {}", v.help);
        println!();
    }

    let criticals = violations
        .iter()
        .filter(|v| v.severity == Severity::Critical)
        .count();
    let serious = violations
        .iter()
        .filter(|v| v.severity == Severity::Serious)
        .count();
    let moderate = violations
        .iter()
        .filter(|v| v.severity == Severity::Moderate)
        .count();

    println!("Found {criticals} Critical, {serious} Serious, {moderate} Moderate violation(s)");

    if fail {
        if critical_only {
            println!("❌  Fix Critical violations before shipping.");
        } else {
            println!("❌  Fix Critical and Serious violations before shipping.");
        }
    }

    fail
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_config_check_passes_on_valid_and_fails_on_typo() {
        let temp = tempfile::tempdir().unwrap();
        let toml_path = temp.path().join("autumn.toml");
        std::fs::write(
            &toml_path,
            "[database]\nprimary_url = \"postgres://localhost/db\"",
        )
        .unwrap();

        // Valid check passes
        if let Err(e) = run_config_check_impl(temp.path()) {
            panic!("Valid check failed: {e}");
        }

        // Typo check fails
        std::fs::write(
            &toml_path,
            "[database]\nprimry_url = \"postgres://localhost/db\"",
        )
        .unwrap();
        let res = run_config_check_impl(temp.path());
        assert!(res.is_err());
        assert!(res.err().unwrap().contains("primry_url"));

        // Inline profile with valid key passes
        std::fs::write(
            &toml_path,
            "[profile.dev.database]\nprimary_url = \"postgres://localhost/db\"",
        )
        .unwrap();
        if let Err(e) = run_config_check_impl(temp.path()) {
            panic!("Inline profile with valid key failed: {e}");
        }

        // Inline profile with typo fails
        std::fs::write(
            &toml_path,
            "[profile.dev.database]\nprimry_url = \"postgres://localhost/db\"",
        )
        .unwrap();
        let res = run_config_check_impl(temp.path());
        assert!(res.is_err());
        assert!(res.err().unwrap().contains("primry_url"));

        // Reset autumn.toml to be clean
        std::fs::write(
            &toml_path,
            "[database]\nprimary_url = \"postgres://localhost/db\"",
        )
        .unwrap();

        // Profile file (autumn-dev.toml) with typo fails
        let profile_toml_path = temp.path().join("autumn-dev.toml");
        std::fs::write(
            &profile_toml_path,
            "[database]\nprimry_url = \"postgres://localhost/db\"",
        )
        .unwrap();
        let res = run_config_check_impl(temp.path());
        assert!(res.is_err());
        assert!(res.err().unwrap().contains("primry_url"));

        // Profile file (autumn-dev.toml) valid passes
        std::fs::write(
            &profile_toml_path,
            "[database]\nprimary_url = \"postgres://localhost/db\"",
        )
        .unwrap();
        if let Err(e) = run_config_check_impl(temp.path()) {
            panic!("Profile file with valid key failed: {e}");
        }
    }

    fn violation_ids(html: &str) -> Vec<&'static str> {
        let mut ids: Vec<&'static str> =
            analyse_html(html).into_iter().map(|v| v.rule_id).collect();
        ids.sort_unstable();
        ids
    }

    // Helper: minimal clean base page (all green)
    fn clean_page(inner: &str) -> String {
        // Use concat + runtime format to avoid "# inside raw-string-literal issues.
        format!(
            concat!(
                r#"<!DOCTYPE html><html lang="en"><body>"#,
                r##"<a class="skip-link" href="#main">Skip</a>"##,
                r#"<main id="main">{}</main></body></html>"#,
            ),
            inner
        )
    }

    // ── html-has-lang ──────────────────────────────────────────────

    #[test]
    fn html_with_lang_passes() {
        let html = clean_page("");
        assert!(
            !violation_ids(&html).contains(&"html-has-lang"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn html_without_lang_fails() {
        let html = "<html><body><main></main></body></html>";
        assert!(violation_ids(html).contains(&"html-has-lang"));
    }

    #[test]
    fn html_data_lang_attribute_does_not_satisfy_lang_requirement() {
        let html = r#"<html data-lang="en"><body><main></main></body></html>"#;
        assert!(violation_ids(html).contains(&"html-has-lang"));
    }

    #[test]
    fn html_empty_lang_fails() {
        let html = r#"<html lang=""><body><main></main></body></html>"#;
        assert!(
            violation_ids(html).contains(&"html-has-lang"),
            "lang=\"\" must not satisfy the lang requirement"
        );
    }

    #[test]
    fn html_lang_allows_spaces_around_equals() {
        let html = r#"<html lang = "en"><body><main></main></body></html>"#;
        assert!(
            !violation_ids(html).contains(&"html-has-lang"),
            "valid attribute whitespace must not hide lang"
        );
    }

    // ── bypass (skip link) ─────────────────────────────────────────

    #[test]
    fn skip_link_class_passes() {
        let html = clean_page("");
        assert!(
            !violation_ids(&html).contains(&"bypass"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn skip_link_href_to_main_passes() {
        // Use r## so the inner "# sequences don't close the raw string.
        let html = r##"<html lang="en"><body><a href="#main">Skip</a><main></main></body></html>"##;
        assert!(
            !violation_ids(html).contains(&"bypass"),
            "{:?}",
            violation_ids(html)
        );
    }

    #[test]
    fn skip_link_href_to_main_content_passes() {
        let html = r##"<html lang="en"><body><a href="#main-content">Skip</a><main></main></body></html>"##;
        assert!(
            !violation_ids(html).contains(&"bypass"),
            "{:?}",
            violation_ids(html)
        );
    }

    #[test]
    fn missing_skip_link_fails() {
        let html = r#"<html lang="en"><body><nav>stuff</nav><main></main></body></html>"#;
        assert!(
            violation_ids(html).contains(&"bypass"),
            "{:?}",
            violation_ids(html)
        );
    }

    #[test]
    fn skip_link_after_nav_link_fails() {
        // The skip link must be the FIRST focusable link; a nav link before it
        // defeats the purpose for keyboard users.
        let html = r##"<html lang="en"><body><a href="/home">Home</a><a class="skip-link" href="#main">Skip</a><main></main></body></html>"##;
        assert!(
            violation_ids(html).contains(&"bypass"),
            "skip link placed after a nav link must fail"
        );
    }

    // ── landmark-one-main ──────────────────────────────────────────

    #[test]
    fn main_element_passes() {
        let html = r#"<html lang="en"><body><main></main></body></html>"#;
        assert!(!violation_ids(html).contains(&"landmark-one-main"));
    }

    #[test]
    fn role_main_passes() {
        let html = r#"<html lang="en"><body><div role="main"></div></body></html>"#;
        assert!(!violation_ids(html).contains(&"landmark-one-main"));
    }

    #[test]
    fn missing_main_landmark_fails() {
        let html = r#"<html lang="en"><body><div>content</div></body></html>"#;
        assert!(violation_ids(html).contains(&"landmark-one-main"));
    }

    #[test]
    fn landmark_in_comment_is_ignored() {
        let html = r##"<html lang="en"><body><a href="#main">Skip</a><!-- <main id="main"></main> --><div>content</div></body></html>"##;
        assert!(
            violation_ids(html).contains(&"landmark-one-main"),
            "commented-out landmarks must not satisfy the main-landmark rule"
        );
    }

    #[test]
    fn multiple_main_landmarks_fails() {
        let html = r#"<html lang="en"><body><main>A</main><main>B</main></body></html>"#;
        assert!(violation_ids(html).contains(&"landmark-one-main"));
    }

    #[test]
    fn main_with_role_main_does_not_double_count() {
        // <main role="main"> is the scaffold default; must not count as two landmarks.
        let html = r##"<html lang="en"><body><a class="skip-link" href="#main">Skip</a><main id="main" role="main">content</main></body></html>"##;
        assert!(
            !violation_ids(html).contains(&"landmark-one-main"),
            "<main role=\"main\"> should count as one landmark, not two"
        );
    }

    #[test]
    fn div_with_role_main_passes() {
        let html = r#"<html lang="en"><body><div role="main">content</div></body></html>"#;
        assert!(!violation_ids(html).contains(&"landmark-one-main"));
    }

    // ── image-alt ─────────────────────────────────────────────────

    #[test]
    fn image_with_alt_passes() {
        let html = clean_page(r#"<img src="x.png" alt="logo">"#);
        assert!(
            !violation_ids(&html).contains(&"image-alt"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn image_with_empty_alt_passes() {
        let html = clean_page(r#"<img src="deco.png" alt="">"#);
        assert!(!violation_ids(&html).contains(&"image-alt"));
    }

    #[test]
    fn image_without_alt_fails() {
        let html = r#"<html lang="en"><body><main><img src="x.png"></main></body></html>"#;
        assert!(violation_ids(html).contains(&"image-alt"));
    }

    #[test]
    fn image_with_data_alt_but_no_alt_fails() {
        let html =
            r#"<html lang="en"><body><main><img src="x.png" data-alt="logo"></main></body></html>"#;
        assert!(violation_ids(html).contains(&"image-alt"));
    }

    #[test]
    fn image_in_comment_is_ignored() {
        let html = clean_page(r#"<!-- <img src="x.png"> -->"#);
        assert!(
            !violation_ids(&html).contains(&"image-alt"),
            "commented-out images must not be analysed"
        );
    }

    // ── label ─────────────────────────────────────────────────────

    #[test]
    fn input_with_label_for_passes() {
        let html = clean_page(r#"<label for="name">Name</label><input type="text" id="name">"#);
        assert!(
            !violation_ids(&html).contains(&"label"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn input_label_for_allows_attribute_whitespace() {
        let html = clean_page(
            r#"<label
                for = "name">Name</label><input type = "text" id = "name">"#,
        );
        assert!(
            !violation_ids(&html).contains(&"label"),
            "valid whitespace around attributes must not break label association"
        );
    }

    #[test]
    fn input_with_empty_aria_label_fails() {
        let html = clean_page(r#"<input type="text" aria-label="">"#);
        assert!(
            violation_ids(&html).contains(&"label"),
            "aria-label=\"\" must not count as an accessible name"
        );
    }

    #[test]
    fn input_with_aria_label_passes() {
        let html = clean_page(r#"<input type="text" aria-label="Name">"#);
        assert!(
            !violation_ids(&html).contains(&"label"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn hidden_input_needs_no_label() {
        let html = clean_page(r#"<input type="hidden" name="_csrf" value="tok">"#);
        assert!(
            !violation_ids(&html).contains(&"label"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn submit_input_needs_no_label() {
        let html = clean_page(r#"<input type="submit" value="Go">"#);
        assert!(
            !violation_ids(&html).contains(&"label"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn unlabelled_text_input_fails() {
        let html =
            r#"<html lang="en"><body><main><input type="text" id="name"></main></body></html>"#;
        assert!(violation_ids(html).contains(&"label"));
    }

    #[test]
    fn input_aria_labelledby_resolving_id_passes() {
        let html = clean_page(
            r#"<span id="lbl">Username</span><input type="text" aria-labelledby="lbl">"#,
        );
        assert!(
            !violation_ids(&html).contains(&"label"),
            "aria-labelledby referencing existing element with text must pass"
        );
    }

    #[test]
    fn input_aria_labelledby_missing_id_fails() {
        let html = clean_page(r#"<input type="text" aria-labelledby="nonexistent">"#);
        assert!(
            violation_ids(&html).contains(&"label"),
            "aria-labelledby referencing a missing ID must fail"
        );
    }

    #[test]
    fn wrapped_label_input_passes() {
        let html = clean_page(r#"<label>Name <input type="text"></label>"#);
        assert!(
            !violation_ids(&html).contains(&"label"),
            "input wrapped in <label> must pass"
        );
    }

    #[test]
    fn empty_wrapped_label_fails() {
        let html = clean_page("<label><input type=\"text\"></label>");
        assert!(
            violation_ids(&html).contains(&"label"),
            "<label><input></label> with no label text must still fail"
        );
    }

    #[test]
    fn textarea_with_label_for_passes() {
        let html = clean_page(r#"<label for="bio">Bio</label><textarea id="bio"></textarea>"#);
        assert!(
            !violation_ids(&html).contains(&"label"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn unlabelled_textarea_fails() {
        let html =
            r#"<html lang="en"><body><main><textarea id="bio"></textarea></main></body></html>"#;
        assert!(
            violation_ids(html).contains(&"label"),
            "unlabelled <textarea> must fail"
        );
    }

    #[test]
    fn wrapped_label_textarea_passes() {
        let html = clean_page("<label>Bio <textarea></textarea></label>");
        assert!(
            !violation_ids(&html).contains(&"label"),
            "textarea wrapped in <label> must pass"
        );
    }

    #[test]
    fn select_with_label_for_passes() {
        let html = clean_page(
            r#"<label for="role">Role</label><select id="role"><option>Admin</option></select>"#,
        );
        assert!(
            !violation_ids(&html).contains(&"label"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn unlabelled_select_fails() {
        let html = r#"<html lang="en"><body><main><select id="role"><option>Admin</option></select></main></body></html>"#;
        assert!(
            violation_ids(html).contains(&"label"),
            "unlabelled <select> must fail"
        );
    }

    #[test]
    fn label_word_in_text_content_does_not_create_false_positive() {
        // "label" appears in text content but there is no <label> tag
        let html = clean_page(r#"<p>Enter your label here</p><input type="text" id="x">"#);
        assert!(violation_ids(&html).contains(&"label"));
    }

    // ── button-name ───────────────────────────────────────────────

    #[test]
    fn button_with_text_passes() {
        let html = clean_page("<button>Save</button>");
        assert!(
            !violation_ids(&html).contains(&"button-name"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn button_with_empty_aria_label_fails() {
        let html = clean_page(r#"<button aria-label=""></button>"#);
        assert!(
            violation_ids(&html).contains(&"button-name"),
            "aria-label=\"\" must not count as an accessible button name"
        );
    }

    #[test]
    fn button_with_aria_label_passes() {
        let html = clean_page(r#"<button aria-label="Close dialog"></button>"#);
        assert!(
            !violation_ids(&html).contains(&"button-name"),
            "{:?}",
            violation_ids(&html)
        );
    }

    #[test]
    fn empty_button_fails() {
        let html = r#"<html lang="en"><body><main><button></button></main></body></html>"#;
        assert!(violation_ids(html).contains(&"button-name"));
    }

    #[test]
    fn button_with_script_only_fails() {
        let html = clean_page("<button><script>save</script></button>");
        assert!(
            violation_ids(&html).contains(&"button-name"),
            "script contents must not count as an accessible button name"
        );
    }

    #[test]
    fn button_aria_labelledby_resolving_id_passes() {
        let html = clean_page(
            r#"<span id="close-label">Close</span><button aria-labelledby="close-label"></button>"#,
        );
        assert!(
            !violation_ids(&html).contains(&"button-name"),
            "aria-labelledby referencing existing element with text must pass"
        );
    }

    #[test]
    fn button_aria_labelledby_missing_id_fails() {
        let html = clean_page(r#"<button aria-labelledby="ghost"></button>"#);
        assert!(
            violation_ids(&html).contains(&"button-name"),
            "aria-labelledby referencing a missing ID must fail"
        );
    }

    #[test]
    fn button_with_only_img_no_alt_fails() {
        let html = r#"<html lang="en"><body><main><button><img src="icon.png"></button></main></body></html>"#;
        assert!(violation_ids(html).contains(&"button-name"));
    }

    #[test]
    fn button_with_img_with_alt_passes() {
        let html = r#"<html lang="en"><body><main><button><img src="delete.svg" alt="Delete"></button></main></body></html>"#;
        assert!(
            !violation_ids(html).contains(&"button-name"),
            "icon button with non-empty img alt should pass"
        );
    }

    #[test]
    fn button_with_img_empty_alt_fails() {
        let html = r#"<html lang="en"><body><main><button><img src="deco.png" alt=""></button></main></body></html>"#;
        assert!(
            violation_ids(html).contains(&"button-name"),
            "button with img alt=\"\" has no accessible name"
        );
    }

    #[test]
    fn unclosed_button_does_not_panic() {
        // Malformed HTML with no </button> must not panic — it should be flagged
        // as nameless (no visible text, no aria) rather than causing an index OOB.
        let html = r#"<html lang="en"><body><main><button></main></body></html>"#;
        // We only care that this returns without panicking; the violation may or may not fire.
        let _ = violation_ids(html);
    }

    // ── strip_html_tags ────────────────────────────────────────────

    #[test]
    fn strip_html_tags_removes_tags() {
        assert_eq!(strip_html_tags("<b>hello</b> <i>world</i>"), "hello world");
    }

    #[test]
    fn strip_html_tags_leaves_plain_text() {
        assert_eq!(strip_html_tags("just text"), "just text");
    }

    #[test]
    fn strip_html_tags_empty_tag_only() {
        assert_eq!(strip_html_tags("<img src=\"x.png\">").trim(), "");
    }

    #[test]
    fn strip_html_tags_removes_script_and_style_content() {
        let mut html = String::from("<style>.hidden");
        html.push('{');
        html.push_str("display:none");
        html.push('}');
        html.push_str("</style><script>alert('x')</script>Visible");
        assert_eq!(strip_html_tags(&html), "Visible");
    }

    #[test]
    fn strip_html_tags_decodes_basic_entities() {
        assert_eq!(
            strip_html_tags("Fish &amp; Chips &lt; Menu &gt;"),
            "Fish & Chips < Menu >"
        );
    }

    #[test]
    fn strip_html_tags_preserves_literal_angle_text() {
        assert_eq!(strip_html_tags("2 < 3 and 5 > 4"), "2 < 3 and 5 > 4");
    }

    // ── extract_attr_value ─────────────────────────────────────────

    #[test]
    fn extracts_double_quoted_value() {
        assert_eq!(extract_attr_value(r#""hello" rest"#), "hello");
    }

    #[test]
    fn extracts_single_quoted_value() {
        assert_eq!(extract_attr_value("'world' rest"), "world");
    }

    #[test]
    fn extracts_unquoted_value() {
        assert_eq!(extract_attr_value("main-content >"), "main-content");
    }

    // ── run_a11y_check inline ──────────────────────────────────────

    #[test]
    fn run_a11y_check_with_clean_html_returns_empty() {
        let clean = r##"<!DOCTYPE html>
<html lang="en">
<body>
  <a href="#main-content" class="skip-link">Skip to content</a>
  <header><nav aria-label="Main">Home</nav></header>
  <main id="main-content">
    <label for="q">Search</label>
    <input type="text" id="q">
    <button>Go</button>
  </main>
  <footer>Footer</footer>
</body>
</html>"##;

        let opts = A11yCheckOptions {
            url: None,
            html: Some(clean.into()),
        };
        let violations = run_a11y_check(&opts).unwrap();
        assert!(
            violations.is_empty(),
            "expected zero violations, got: {:#?}",
            violations.iter().map(|v| v.rule_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn run_a11y_check_reports_multiple_violations() {
        let bad = r#"<html><body><img src="x.png"><input type="text" id="f"><button></button></body></html>"#;
        let opts = A11yCheckOptions {
            url: None,
            html: Some(bad.into()),
        };
        let violations = run_a11y_check(&opts).unwrap();
        let ids: Vec<&str> = violations.iter().map(|v| v.rule_id).collect();
        assert!(ids.contains(&"html-has-lang"), "{ids:?}");
        assert!(ids.contains(&"image-alt"), "{ids:?}");
        assert!(ids.contains(&"button-name"), "{ids:?}");
    }

    // ── nav_bar full-page a11y (#1137) ──────────────────────────────

    #[test]
    fn nav_bar_full_page_passes_all_a11y_lints() {
        use autumn_web::widgets::{NavBarConfig, NavItem, NavMenu, nav_bar};

        let nav = nav_bar(
            "/",
            &NavBarConfig::new()
                .brand("Acme", "/")
                .item(NavItem::link("/", "Home"))
                .item(NavItem::menu(
                    NavMenu::new("Products").link("/widgets", "Widgets"),
                ))
                .trailing(NavItem::plain_link("/login", "Log in")),
        )
        .into_string();

        let html = format!(
            r##"<!DOCTYPE html>
<html lang="en">
<body>
  <a href="#main-content" class="skip-link">Skip to content</a>
  <header>{nav}</header>
  <main id="main-content">Content</main>
</body>
</html>"##
        );

        let violations = analyse_html(&html);
        assert!(
            violations.is_empty(),
            "nav_bar layout must pass autumn check --a11y: {:#?}",
            violations.iter().map(|v| v.rule_id).collect::<Vec<_>>()
        );
    }

    // ── severity ordering ──────────────────────────────────────────

    #[test]
    fn critical_severity_greater_than_serious() {
        assert!(Severity::Critical > Severity::Serious);
        assert!(Severity::Serious > Severity::Moderate);
    }

    // ── print_report ──────────────────────────────────────────────

    #[test]
    fn print_report_returns_true_when_serious_violation() {
        let violations = vec![A11yViolation {
            rule_id: "html-has-lang",
            severity: Severity::Serious,
            description: "missing lang".into(),
            help: "add lang",
        }];
        assert!(print_report(&violations, "test", false));
    }

    #[test]
    fn print_report_critical_only_does_not_fail_on_serious() {
        let violations = vec![A11yViolation {
            rule_id: "html-has-lang",
            severity: Severity::Serious,
            description: "missing lang".into(),
            help: "add lang",
        }];
        assert!(!print_report(&violations, "test", true));
    }

    #[test]
    fn print_report_critical_only_still_fails_on_critical() {
        let violations = vec![A11yViolation {
            rule_id: "image-alt",
            severity: Severity::Critical,
            description: "img missing alt".into(),
            help: "add alt",
        }];
        assert!(print_report(&violations, "test", true));
    }

    #[test]
    fn print_report_returns_false_when_no_violations() {
        assert!(!print_report(&[], "test", false));
    }
}
