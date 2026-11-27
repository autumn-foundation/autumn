//! `autumn schema-org audit`: check the JSON-LD a page publishes.
//!
//! The audit reads each `<script type="application/ld+json">` block of a page
//! and checks its nodes against the vocabulary in [`super::vocab`].
//!
//! | Rule | Severity | What it means |
//! |---|---|---|
//! | `unreadable` | error | The page could not be fetched, or the file read as UTF-8. |
//! | `invalid-json` | error | The block is not JSON. |
//! | `missing-context` | error | A top-level node has no schema.org `@context`. |
//! | `missing-type` | error | A top-level node has no `@type`. |
//! | `missing-required` | error | Google requires a property the node lacks. |
//! | `unknown-property` | warning | The type has no such property. |
//! | `wrong-value` | warning | The value does not fit the property's type. |
//! | `empty-value` | warning | The value is an empty string or list. |
//! | `missing-recommended` | warning | Google recommends a property a top-level node lacks. |
//! | `unknown-type` | notice | The type is not in the CLI's vocabulary, so it is not checked. |
//! | `no-json-ld` | notice | The page has no JSON-LD block. |
//!
//! An error fails the audit. With `--strict` a warning fails it too. A
//! notice never does.
//!
//! The vocabulary is a curated slice of schema.org, so an unknown type is a
//! notice, not an error: it may be a real type the CLI does not know. A type
//! close to a known one is a warning with a "did you mean" hint.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::vocab::{self, SchemaType};

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing. Never fails the audit.
    Notice,
    /// Fails the audit with `--strict`.
    Warning,
    /// Fails the audit.
    Error,
}

impl Severity {
    /// `error`, `warning` or `notice`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Notice => "notice",
        }
    }
}

/// One problem in one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The rule id (`missing-required`).
    pub rule: &'static str,
    /// How bad it is.
    pub severity: Severity,
    /// The block, from 1, in page order. `0` for a page-level finding.
    pub block: usize,
    /// Where in the block: `$`, `$.offers.price`, `$["@graph"][1]`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

/// The audit of one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageReport {
    /// The URL or file.
    pub source: String,
    /// How many JSON-LD blocks the page has.
    pub blocks: usize,
    /// The `@type` of each top-level node, in order.
    pub types: Vec<String>,
    /// What is wrong.
    pub findings: Vec<Finding>,
}

impl PageReport {
    /// How many findings of `severity` the page has.
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    }
}

/// Audit the HTML of one page.
#[must_use]
pub fn audit_html(source: &str, html: &str) -> PageReport {
    let blocks = extract_blocks(html);
    let mut report = PageReport {
        source: source.to_owned(),
        blocks: blocks.len(),
        types: Vec::new(),
        findings: Vec::new(),
    };
    if blocks.is_empty() {
        report.findings.push(Finding {
            rule: "no-json-ld",
            severity: Severity::Notice,
            block: 0,
            path: String::new(),
            message: "the page has no <script type=\"application/ld+json\"> block".to_owned(),
        });
    }
    for (i, text) in blocks.iter().enumerate() {
        let mut ctx = Ctx {
            block: i + 1,
            findings: &mut report.findings,
        };
        match serde_json::from_str::<Value>(text) {
            Ok(value) => audit_block(&value, &mut ctx, &mut report.types),
            Err(err) => ctx.push(
                "invalid-json",
                Severity::Error,
                "$",
                format!("the block is not JSON: {err}"),
            ),
        }
    }
    report
}

/// The text of each `<script type="application/ld+json">` element of `html`,
/// in order, trimmed. The type may carry parameters
/// (`application/ld+json; charset=utf-8`) and matches in any ASCII case.
///
/// The page goes through the WHATWG HTML tokenizer, with the same raw-text
/// switches the tree builder makes: `<script>` text is script data, and
/// `<textarea>`/`<title>` (RCDATA) and `<style>`, `<xmp>`, `<iframe>`,
/// `<noembed>`, `<noframes>` (raw text) hold no tags. So a `<script>` inside a
/// comment, an attribute value or a `<textarea>` is text, and only a real
/// `</script>` end tag ends a block.
#[must_use]
pub fn extract_blocks(html: &str) -> Vec<String> {
    use html5ever::tokenizer::{BufferQueue, Tokenizer, TokenizerOpts};
    let queue = BufferQueue::default();
    queue.push_back(html5ever::tendril::StrTendril::from_slice(html));
    let tokenizer = Tokenizer::new(JsonLdSink::default(), TokenizerOpts::default());
    let _ = tokenizer.feed(&queue);
    tokenizer.end();
    tokenizer.sink.blocks.take()
}

/// Collects the text of JSON-LD script elements as the tokenizer emits it.
#[derive(Default)]
struct JsonLdSink {
    /// The finished blocks.
    blocks: std::cell::RefCell<Vec<String>>,
    /// The text of the JSON-LD script being read, if any.
    current: std::cell::RefCell<Option<String>>,
}

impl html5ever::tokenizer::TokenSink for JsonLdSink {
    type Handle = ();

    fn process_token(
        &self,
        token: html5ever::tokenizer::Token,
        _line_number: u64,
    ) -> html5ever::tokenizer::TokenSinkResult<()> {
        use html5ever::tokenizer::states::RawKind;
        use html5ever::tokenizer::{TagKind, Token, TokenSinkResult};
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => match &*tag.name {
                "script" => {
                    let json_ld = tag.attrs.iter().any(|a| {
                        &*a.name.local == "type"
                            && a.value.split(';').next().is_some_and(|m| {
                                m.trim().eq_ignore_ascii_case("application/ld+json")
                            })
                    });
                    *self.current.borrow_mut() = json_ld.then(String::new);
                    TokenSinkResult::RawData(RawKind::ScriptData)
                }
                "textarea" | "title" => TokenSinkResult::RawData(RawKind::Rcdata),
                "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                    TokenSinkResult::RawData(RawKind::Rawtext)
                }
                "plaintext" => TokenSinkResult::Plaintext,
                _ => TokenSinkResult::Continue,
            },
            Token::TagToken(tag) if tag.kind == TagKind::EndTag && &*tag.name == "script" => {
                if let Some(text) = self.current.borrow_mut().take() {
                    self.blocks.borrow_mut().push(text.trim().to_owned());
                }
                TokenSinkResult::Continue
            }
            Token::CharacterTokens(text) => {
                if let Some(current) = self.current.borrow_mut().as_mut() {
                    current.push_str(&text);
                }
                TokenSinkResult::Continue
            }
            _ => TokenSinkResult::Continue,
        }
    }
}

struct Ctx<'a> {
    block: usize,
    findings: &'a mut Vec<Finding>,
}

impl Ctx<'_> {
    fn push(&mut self, rule: &'static str, severity: Severity, path: &str, message: String) {
        self.findings.push(Finding {
            rule,
            severity,
            block: self.block,
            path: path.to_owned(),
            message,
        });
    }
}

fn audit_block(value: &Value, ctx: &mut Ctx<'_>, types: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                audit_top(item, &format!("$[{i}]"), ctx, types);
            }
        }
        other => audit_top(other, "$", ctx, types),
    }
}

fn audit_top(value: &Value, path: &str, ctx: &mut Ctx<'_>, types: &mut Vec<String>) {
    let Value::Object(object) = value else {
        ctx.push(
            "missing-type",
            Severity::Error,
            path,
            "a top-level JSON-LD value must be an object".to_owned(),
        );
        return;
    };
    if !object.get("@context").is_some_and(is_schema_org_context) {
        ctx.push(
            "missing-context",
            Severity::Error,
            path,
            "no schema.org @context: add \"@context\": \"https://schema.org\"".to_owned(),
        );
    }
    if let Some(graph) = object.get("@graph") {
        let Value::Array(nodes) = graph else {
            ctx.push(
                "wrong-value",
                Severity::Warning,
                &format!("{path}[\"@graph\"]"),
                "@graph must be a list of nodes".to_owned(),
            );
            return;
        };
        for (i, node) in nodes.iter().enumerate() {
            let node_path = format!("{path}[\"@graph\"][{i}]");
            if let Value::Object(node_obj) = node {
                record_types(node_obj, types);
                audit_node(node_obj, &node_path, ctx, true, false);
            } else {
                ctx.push(
                    "missing-type",
                    Severity::Error,
                    &node_path,
                    "a @graph entry must be an object".to_owned(),
                );
            }
        }
        return;
    }
    record_types(object, types);
    audit_node(object, path, ctx, true, false);
}

fn record_types(object: &Map<String, Value>, types: &mut Vec<String>) {
    let names = type_names(object);
    if names.is_empty() {
        types.push("(none)".to_owned());
    } else {
        types.push(names.join(", "));
    }
}

/// Whether `value` is a schema.org context: the IRI with either scheme and
/// with or without a trailing `/`, or a list or object holding it.
fn is_schema_org_context(value: &Value) -> bool {
    match value {
        Value::String(s) => {
            let s = s.trim_end_matches('/');
            s == "https://schema.org" || s == "http://schema.org"
        }
        Value::Array(items) => items.iter().any(is_schema_org_context),
        Value::Object(map) => map.get("@vocab").is_some_and(is_schema_org_context),
        _ => false,
    }
}

/// The `@type` names of a node, without a `schema:` or schema.org prefix.
fn type_names(object: &Map<String, Value>) -> Vec<String> {
    match object.get("@type") {
        Some(Value::String(s)) => vec![vocab::strip_schema_prefix(s).to_owned()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| vocab::strip_schema_prefix(s).to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Check one node. `top` is a top-level or `@graph` node. `nested` is a node
/// that is the value of a property.
fn audit_node(object: &Map<String, Value>, path: &str, ctx: &mut Ctx<'_>, top: bool, nested: bool) {
    let names = type_names(object);
    if names.is_empty() {
        // A reference to a node elsewhere (`{"@id": …}`) needs no type.
        if object.keys().all(|k| k == "@id" || k == "@context") {
            return;
        }
        if top {
            ctx.push(
                "missing-type",
                Severity::Error,
                path,
                "the node has no @type".to_owned(),
            );
        } else {
            ctx.push(
                "missing-type",
                Severity::Warning,
                path,
                "the nested node has no @type, so a reader cannot tell what it is".to_owned(),
            );
        }
        return;
    }
    let known: Vec<&'static SchemaType> = names.iter().filter_map(|n| vocab::find(n)).collect();
    for name in names.iter().filter(|n| vocab::find(n).is_none()) {
        match vocab::suggest_type(name) {
            Some(s) => ctx.push(
                "unknown-type",
                Severity::Warning,
                path,
                format!("`{name}` is not a schema.org type this CLI knows. Did you mean `{s}`?"),
            ),
            None => ctx.push(
                "unknown-type",
                Severity::Notice,
                path,
                format!("`{name}` is not in the CLI's schema.org vocabulary, so it is not checked"),
            ),
        }
    }
    if known.is_empty() {
        return;
    }
    for (key, value) in object {
        if key.starts_with('@') {
            continue;
        }
        let prop_path = format!("{path}.{key}");
        let resolved = known.iter().find_map(|t| vocab::property(t, key));
        let Some(resolved) = resolved else {
            let hint = known
                .iter()
                .find_map(|t| vocab::suggest_property(t, key))
                .map(|s| format!(" Did you mean `{s}`?"))
                .unwrap_or_default();
            ctx.push(
                "unknown-property",
                Severity::Warning,
                &prop_path,
                format!("`{}` has no property `{key}`.{hint}", names.join(", ")),
            );
            continue;
        };
        audit_value(resolved.property, value, &prop_path, ctx);
    }
    for t in &known {
        check_presence(t, object, path, nested, top, ctx);
    }
}

fn check_presence(
    t: &'static SchemaType,
    object: &Map<String, Value>,
    path: &str,
    nested: bool,
    top: bool,
    ctx: &mut Ctx<'_>,
) {
    let has = |name: &str| object.get(name).is_some_and(|v| !is_empty(v));
    for name in t.required {
        // `itemReviewed` is implied when the review or rating is nested in
        // the item it rates.
        if nested && *name == "itemReviewed" {
            continue;
        }
        if !has(name) {
            ctx.push(
                "missing-required",
                Severity::Error,
                path,
                format!("Google requires `{name}` on `{}`", t.name),
            );
        }
    }
    for group in t.required_any {
        if !group.iter().any(|name| has(name)) {
            ctx.push(
                "missing-required",
                Severity::Error,
                path,
                format!(
                    "Google requires one of `{}` on `{}`",
                    group.join("`, `"),
                    t.name
                ),
            );
        }
    }
    // Recommendations are for the page's own subject. A nested author or
    // offer is judged by what Google requires of it.
    for name in t.recommended.iter().filter(|_| top) {
        if !has(name) {
            ctx.push(
                "missing-recommended",
                Severity::Warning,
                path,
                format!("Google recommends `{name}` on `{}`", t.name),
            );
        }
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

fn audit_value(property: &'static vocab::Property, value: &Value, path: &str, ctx: &mut Ctx<'_>) {
    if is_empty(value) {
        ctx.push(
            "empty-value",
            Severity::Warning,
            path,
            format!("`{}` is empty", property.name),
        );
        return;
    }
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                audit_value(property, item, &format!("{path}[{i}]"), ctx);
            }
        }
        Value::Object(object) => {
            let names = type_names(object);
            let fits = names.is_empty()
                || names.iter().any(|n| {
                    vocab::find(n).is_none_or(|t| property.range.iter().any(|r| vocab::is_a(t, r)))
                });
            if !fits {
                ctx.push(
                    "wrong-value",
                    Severity::Warning,
                    path,
                    format!(
                        "`{}` expects {}, not `{}`",
                        property.name,
                        property.range.join(" or "),
                        names.join(", ")
                    ),
                );
            }
            audit_node(object, path, ctx, false, true);
        }
        scalar => {
            if !scalar_fits(property, scalar) {
                ctx.push(
                    "wrong-value",
                    Severity::Warning,
                    path,
                    format!(
                        "`{}` expects {}, not {}",
                        property.name,
                        property.range.join(" or "),
                        describe(scalar)
                    ),
                );
            }
        }
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::String(s) if s.chars().count() > 40 => {
            format!("\"{}…\"", s.chars().take(40).collect::<String>())
        }
        other => other.to_string(),
    }
}

/// Whether a plain JSON value fits any entry of the property's range.
fn scalar_fits(property: &vocab::Property, value: &Value) -> bool {
    property.range.iter().any(|r| match (*r, value) {
        ("Text" | "Duration" | "Energy" | "Mass", Value::String(_))
        | ("Number", Value::Number(_))
        | ("Boolean", Value::Bool(_)) => true,
        ("URL", Value::String(s)) => is_url(s),
        ("Date", Value::String(s)) => is_iso_date(s),
        ("DateTime", Value::String(s)) => is_iso_date_time(s) || is_iso_date(s),
        ("Time", Value::String(s)) => is_iso_time(s),
        ("Number", Value::String(s)) => s.trim().parse::<f64>().is_ok(),
        ("Integer", Value::Number(n)) => n.is_i64() || n.is_u64(),
        ("Integer", Value::String(s)) => s.trim().parse::<i64>().is_ok(),
        ("Boolean", Value::String(s)) => matches!(s.as_str(), "true" | "false" | "True" | "False"),
        // A node type also takes a string: its URL or, often, its name.
        (other, Value::String(s)) if !vocab::is_primitive(other) => {
            vocab::find(other).is_none_or(|t| !t.is_enumeration() || is_member(t, s))
        }
        _ => false,
    })
}

/// Whether `s` names a member of the enumeration `t`, by short name or IRI.
fn is_member(t: &SchemaType, s: &str) -> bool {
    t.members.contains(&vocab::strip_schema_prefix(s))
}

fn is_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("https://") || s.starts_with("http://") || s.starts_with('/')
}

fn digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

/// `YYYY-MM-DD`.
fn is_iso_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3 && digits(parts[0], 4) && digits(parts[1], 2) && digits(parts[2], 2)
}

/// `HH:MM`, `HH:MM:SS`, with optional fraction and offset.
fn is_iso_time(s: &str) -> bool {
    let core = s
        .trim_end_matches('Z')
        .split(['+', '-'])
        .next()
        .unwrap_or("");
    let core = core.split('.').next().unwrap_or("");
    let parts: Vec<&str> = core.split(':').collect();
    (parts.len() == 2 || parts.len() == 3) && parts.iter().all(|p| digits(p, 2))
}

/// `YYYY-MM-DDTHH:MM[:SS][.fff][Z|±HH:MM]`.
fn is_iso_date_time(s: &str) -> bool {
    s.split_once(['T', ' '])
        .is_some_and(|(date, time)| is_iso_date(date) && is_iso_time(time))
}

/// Where the HTML of a target comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An http(s) URL to fetch.
    Url(String),
    /// An HTML file.
    File(PathBuf),
}

impl Target {
    /// The target as the report names it.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Url(u) => u.clone(),
            Self::File(p) => p.display().to_string(),
        }
    }
}

/// Expand the command-line arguments into targets. A directory becomes each
/// `.html` file under it, sorted.
///
/// # Errors
///
/// A message for a path that does not exist.
pub fn expand_targets(args: &[String]) -> Result<Vec<Target>, String> {
    let mut out = Vec::new();
    for arg in args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            out.push(Target::Url(arg.clone()));
            continue;
        }
        let path = Path::new(arg);
        if path.is_dir() {
            let mut files = Vec::new();
            collect_html(path, &mut files).map_err(|e| format!("{arg}: {e}"))?;
            files.sort();
            if files.is_empty() {
                return Err(format!("{arg}: no .html files under this directory"));
            }
            out.extend(files.into_iter().map(Target::File));
        } else if path.is_file() {
            out.push(Target::File(path.to_path_buf()));
        } else {
            return Err(format!("{arg}: no such file, directory or http(s) URL"));
        }
    }
    Ok(out)
}

fn collect_html(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_html(&path, out)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Read a target's HTML.
///
/// # Errors
///
/// A message when the file cannot be read or the fetch fails.
pub fn load(target: &Target) -> Result<String, String> {
    match target {
        Target::File(path) => {
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
        }
        Target::Url(url) => {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|e| format!("failed to build HTTP client: {e}"))?;
            client
                .get(url)
                .header("accept", "text/html")
                .send()
                .map_err(|e| format!("failed to fetch {url}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("{url}: {e}"))?
                .text()
                .map_err(|e| format!("{url}: failed to read the body: {e}"))
        }
    }
}

/// Whether the reports fail the audit.
#[must_use]
pub fn failed(reports: &[PageReport], strict: bool) -> bool {
    reports
        .iter()
        .any(|r| r.count(Severity::Error) > 0 || (strict && r.count(Severity::Warning) > 0))
}

/// The text report.
#[must_use]
pub fn render_text(reports: &[PageReport], strict: bool) -> String {
    let mut out = String::new();
    for r in reports {
        let types = if r.types.is_empty() {
            String::new()
        } else {
            format!(": {}", r.types.join("; "))
        };
        let _ = writeln!(
            out,
            "{} — {} JSON-LD block{}{types}",
            r.source,
            r.blocks,
            if r.blocks == 1 { "" } else { "s" }
        );
        let mut findings = r.findings.clone();
        findings.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.block.cmp(&b.block)));
        for f in &findings {
            let place = if f.block == 0 {
                String::new()
            } else {
                format!(" block {} {}", f.block, f.path)
            };
            let _ = writeln!(
                out,
                "  {:7} {}{place}: {}",
                f.severity.as_str(),
                f.rule,
                f.message
            );
        }
    }
    let total = |s: Severity| reports.iter().map(|r| r.count(s)).sum::<usize>();
    let _ = writeln!(
        out,
        "\n{} page{}: {} error{}, {} warning{}, {} notice{}.{}",
        reports.len(),
        plural(reports.len()),
        total(Severity::Error),
        plural(total(Severity::Error)),
        total(Severity::Warning),
        plural(total(Severity::Warning)),
        total(Severity::Notice),
        plural(total(Severity::Notice)),
        if failed(reports, strict) {
            " The audit failed."
        } else {
            ""
        }
    );
    out
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The JSON report.
#[must_use]
pub fn render_json(reports: &[PageReport], strict: bool) -> String {
    let pages: Vec<Value> = reports
        .iter()
        .map(|r| {
            json!({
                "source": r.source,
                "blocks": r.blocks,
                "types": r.types,
                "findings": r.findings.iter().map(|f| json!({
                    "rule": f.rule,
                    "severity": f.severity.as_str(),
                    "block": f.block,
                    "path": f.path,
                    "message": f.message,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let total = |s: Severity| reports.iter().map(|r| r.count(s)).sum::<usize>();
    format!(
        "{}\n",
        json!({
            "pages": pages,
            "errors": total(Severity::Error),
            "warnings": total(Severity::Warning),
            "notices": total(Severity::Notice),
            "failed": failed(reports, strict),
        })
    )
}

/// The report for a target that could not be read: one `unreadable` error,
/// so the report, text or JSON, fails like the exit code does.
#[must_use]
pub fn unreadable_report(source: &str, error: String) -> PageReport {
    PageReport {
        source: source.to_owned(),
        blocks: 0,
        types: Vec::new(),
        findings: vec![Finding {
            rule: "unreadable",
            severity: Severity::Error,
            block: 0,
            path: String::new(),
            message: error,
        }],
    }
}

/// Run `autumn schema-org audit`. Exits 1 when the audit fails or a target
/// cannot be read.
pub fn run(targets: &[String], strict: bool, format: super::Format) {
    let targets = match expand_targets(targets) {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => {
            eprintln!("error: give a URL, an HTML file or a directory to audit");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let reports: Vec<PageReport> = targets
        .iter()
        .map(|target| match load(target) {
            Ok(html) => audit_html(&target.label(), &html),
            Err(e) => unreadable_report(&target.label(), e),
        })
        .collect();
    match format {
        super::Format::Text => print!("{}", render_text(&reports, strict)),
        super::Format::Json => print!("{}", render_json(&reports, strict)),
    }
    if failed(&reports, strict) {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(json: &str) -> String {
        format!(
            "<html><head><script type=\"application/ld+json\">{json}</script></head><body></body></html>"
        )
    }

    fn rules(report: &PageReport) -> Vec<(&'static str, Severity, String)> {
        report
            .findings
            .iter()
            .map(|f| (f.rule, f.severity, f.path.clone()))
            .collect()
    }

    const GOOD_RECIPE: &str = r#"{
        "@context": "https://schema.org",
        "@type": "Recipe",
        "name": "Pancakes",
        "image": "https://example.com/p.jpg",
        "author": {"@type": "Person", "name": "Ada"},
        "datePublished": "2026-11-27",
        "description": "Fluffy.",
        "prepTime": "PT10M",
        "cookTime": "PT20M",
        "totalTime": "PT30M",
        "keywords": "breakfast",
        "recipeYield": "4 servings",
        "recipeCategory": "Breakfast",
        "recipeCuisine": "American",
        "recipeIngredient": ["2 eggs", "1 cup flour"],
        "recipeInstructions": [{"@type": "HowToStep", "text": "Mix."}],
        "nutrition": {"@type": "NutritionInformation", "calories": "270 calories"}
    }"#;

    #[test]
    fn a_complete_recipe_has_no_findings() {
        let report = audit_html("page", &page(GOOD_RECIPE));
        assert_eq!(report.blocks, 1);
        assert_eq!(report.types, vec!["Recipe".to_owned()]);
        assert!(report.findings.is_empty(), "{:?}", rules(&report));
    }

    #[test]
    fn missing_required_and_recommended_are_reported() {
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org","@type":"Recipe","name":"x"}"#),
        );
        let r = rules(&report);
        assert!(
            r.contains(&("missing-required", Severity::Error, "$".to_owned())),
            "{r:?}"
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("`image`"))
        );
        assert!(
            r.iter()
                .any(|(rule, s, _)| *rule == "missing-recommended" && *s == Severity::Warning)
        );
        assert!(failed(std::slice::from_ref(&report), false));
    }

    #[test]
    fn invalid_json_context_and_type_are_errors() {
        let report = audit_html("page", &page("{not json"));
        assert_eq!(report.findings[0].rule, "invalid-json");
        let report = audit_html("page", &page(r#"{"@type":"Thing","name":"x"}"#));
        assert_eq!(report.findings[0].rule, "missing-context");
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org/","name":"x"}"#),
        );
        assert_eq!(
            rules(&report),
            vec![("missing-type", Severity::Error, "$".to_owned())]
        );
    }

    #[test]
    fn unknown_types_and_properties_get_hints() {
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org","@type":"Recipie","name":"x"}"#),
        );
        assert_eq!(report.findings[0].rule, "unknown-type");
        assert_eq!(report.findings[0].severity, Severity::Warning);
        assert!(report.findings[0].message.contains("Did you mean `Recipe`"));
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org","@type":"Zeppelin","name":"x"}"#),
        );
        assert_eq!(report.findings[0].severity, Severity::Notice);
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org","@type":"Thing","nmae":"x"}"#),
        );
        assert!(report.findings.iter().any(|f| f.rule == "unknown-property"
            && f.path == "$.nmae"
            && f.message.contains("Did you mean `name`")));
    }

    #[test]
    fn wrong_values_are_warnings() {
        let report = audit_html(
            "page",
            &page(
                r#"{"@context":"https://schema.org","@type":"Event","name":"x",
                "startDate":"next tuesday","location":{"@type":"Recipe"},
                "url":"not a url","maximumAttendeeCapacity":"lots",
                "eventStatus":"https://schema.org/EventCancelled"}"#,
            ),
        );
        let wrong: Vec<&str> = report
            .findings
            .iter()
            .filter(|f| f.rule == "wrong-value")
            .map(|f| f.path.as_str())
            .collect();
        assert!(wrong.contains(&"$.startDate"), "{wrong:?}");
        assert!(wrong.contains(&"$.location"), "{wrong:?}");
        assert!(wrong.contains(&"$.url"), "{wrong:?}");
        assert!(wrong.contains(&"$.maximumAttendeeCapacity"), "{wrong:?}");
        assert!(!wrong.contains(&"$.eventStatus"), "{wrong:?}");
    }

    #[test]
    fn enumeration_members_are_checked() {
        let report = audit_html(
            "page",
            &page(
                r#"{"@context":"https://schema.org","@type":"Offer","price":"3.00",
                "priceCurrency":"EUR","availability":"InStok"}"#,
            ),
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule == "wrong-value" && f.path == "$.availability")
        );
    }

    #[test]
    fn nested_nodes_are_checked_and_item_reviewed_is_implied() {
        let report = audit_html(
            "page",
            &page(
                r#"{"@context":"https://schema.org","@type":"Product","name":"Mug",
                "offers":{"@type":"Offer","priceCurrency":"EUR"},
                "review":{"@type":"Review","author":{"@type":"Person","name":"A"},
                  "reviewRating":{"@type":"Rating","ratingValue":4}}}"#,
            ),
        );
        let errors: Vec<(&str, String)> = report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .map(|f| (f.rule, f.path.clone()))
            .collect();
        assert_eq!(
            errors,
            vec![("missing-required", "$.offers".to_owned())],
            "{errors:?}"
        );
    }

    #[test]
    fn graph_nodes_are_each_checked() {
        let report = audit_html(
            "page",
            &page(
                r#"{"@context":"https://schema.org","@graph":[
                {"@type":"Organization","name":"Acme","url":"https://acme.test"},
                {"@type":"Person"}]}"#,
            ),
        );
        assert_eq!(
            report.types,
            vec!["Organization".to_owned(), "Person".to_owned()]
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule == "missing-required" && f.path == "$[\"@graph\"][1]")
        );
    }

    #[test]
    fn a_reference_node_needs_no_type() {
        let report = audit_html(
            "page",
            &page(
                r##"{"@context":"https://schema.org","@type":"CreativeWork","about":{"@id":"#acme"}}"##,
            ),
        );
        assert!(report.findings.is_empty(), "{:?}", rules(&report));
    }

    #[test]
    fn empty_values_are_warnings() {
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org","@type":"Thing","name":"  ","sameAs":[]}"#),
        );
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.rule == "empty-value")
                .count(),
            2
        );
    }

    #[test]
    fn a_page_without_json_ld_is_a_notice() {
        let report = audit_html("page", "<html><script>var x = 1;</script></html>");
        assert_eq!(report.blocks, 0);
        assert_eq!(
            rules(&report),
            vec![("no-json-ld", Severity::Notice, String::new())]
        );
        assert!(!failed(&[report], true));
    }

    #[test]
    fn extracts_only_ld_json_scripts() {
        let html = "<SCRIPT TYPE='application/ld+json; charset=utf-8'> {\"a\":1} </SCRIPT>\
                    <script>{\"b\":2}</script>\
                    <scripts type=\"application/ld+json\">no</scripts>\
                    <script type=application/ld+json>{\"c\":3}</script>\
                    <script src=\"x.js\" type=\"module\"></script>";
        assert_eq!(extract_blocks(html), vec!["{\"a\":1}", "{\"c\":3}"]);
    }

    #[test]
    fn a_gt_inside_a_quoted_attribute_does_not_end_the_tag() {
        let html = "<script data-note=\"a > b\" type=\"application/ld+json\">{\"a\":1}</script>\
                    <script data-x='>' type=application/ld+json>{\"b\":2}</script>";
        assert_eq!(extract_blocks(html), vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn an_unreadable_target_fails_the_json_report() {
        let reports = vec![
            audit_html("a.html", &page(GOOD_RECIPE)),
            unreadable_report(
                "https://down.test/",
                "failed to fetch https://down.test/".to_owned(),
            ),
        ];
        assert!(failed(&reports, false));
        let json: Value = serde_json::from_str(&render_json(&reports, false)).expect("json");
        assert_eq!(json["failed"], true);
        assert_eq!(json["errors"], 1);
        assert_eq!(json["pages"][1]["findings"][0]["rule"], "unreadable");
        let text = render_text(&reports, false);
        assert!(
            text.contains("error   unreadable: failed to fetch"),
            "{text}"
        );
    }

    #[test]
    fn a_script_inside_a_comment_is_skipped() {
        let html = "<!-- <script> removed --> <script type=\"application/ld+json\">{bad json}</script>\
                    <!-- <script type=\"application/ld+json\">{\"old\":1}</script> -->";
        assert_eq!(extract_blocks(html), vec!["{bad json}"]);
        let report = audit_html("page", html);
        assert_eq!(report.findings[0].rule, "invalid-json");
    }

    #[test]
    fn a_longer_tag_name_does_not_close_the_script() {
        let json = r#"{"@context":"https://schema.org","@type":"Thing","name":"</scripture>"}"#;
        let html = format!("<script type=\"application/ld+json\">{json}</script >");
        assert_eq!(extract_blocks(&html), vec![json]);
        assert!(audit_html("page", &html).findings.is_empty());
        // `<scripts>` is not a script element either.
        assert!(extract_blocks("<scripts type=\"application/ld+json\">{}</scripts>").is_empty());
    }

    #[test]
    fn a_script_inside_rcdata_rawtext_or_an_attribute_is_text() {
        let html = "<textarea><script></textarea>\
                    <title><script></title>\
                    <style>p::after{content:\"<script>\"}</style>\
                    <a title=\"<script>\">x</a>\
                    <script type=\"application/ld+json\">{bad json}</script>";
        assert_eq!(extract_blocks(html), vec!["{bad json}"]);
        assert_eq!(audit_html("page", html).findings[0].rule, "invalid-json");
    }

    #[test]
    fn a_json_ld_type_attribute_in_another_tag_is_not_a_block() {
        let html = "<div type=\"application/ld+json\">{}</div><textarea><script type=\"application/ld+json\">{}</script></textarea>";
        assert!(extract_blocks(html).is_empty());
    }

    #[test]
    fn extract_survives_an_unclosed_script() {
        assert!(extract_blocks("<script type=\"application/ld+json\">{").is_empty());
        assert!(extract_blocks("<script").is_empty());
    }

    #[test]
    fn date_and_time_shapes() {
        assert!(is_iso_date("2026-11-27"));
        assert!(!is_iso_date("27/11/2026"));
        assert!(is_iso_date_time("2026-11-27T10:00:00Z"));
        assert!(is_iso_date_time("2026-11-27T10:00:00.123+01:00"));
        assert!(is_iso_date_time("2026-11-27T10:00"));
        assert!(!is_iso_date_time("2026-11-27T"));
        assert!(is_iso_time("19:30"));
        assert!(!is_iso_time("7pm"));
    }

    #[test]
    fn strict_fails_on_warnings_only() {
        let report = audit_html(
            "page",
            &page(r#"{"@context":"https://schema.org","@type":"Thing","nmae":"x"}"#),
        );
        assert!(!failed(std::slice::from_ref(&report), false));
        assert!(failed(&[report], true));
    }

    #[test]
    fn text_and_json_reports_summarize() {
        let reports = vec![
            audit_html("a.html", &page(GOOD_RECIPE)),
            audit_html("b.html", "<p>"),
        ];
        let text = render_text(&reports, false);
        assert!(text.contains("a.html — 1 JSON-LD block: Recipe"), "{text}");
        assert!(text.contains("notice  no-json-ld"), "{text}");
        assert!(
            text.contains("2 pages: 0 errors, 0 warnings, 1 notice."),
            "{text}"
        );
        let json: Value = serde_json::from_str(&render_json(&reports, false)).expect("json");
        assert_eq!(json["notices"], 1);
        assert_eq!(json["failed"], false);
        assert_eq!(json["pages"][0]["types"][0], "Recipe");
    }

    #[test]
    fn expand_targets_walks_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("posts")).expect("mkdir");
        std::fs::write(dir.path().join("index.html"), "<p>").expect("write");
        std::fs::write(dir.path().join("posts/a.html"), "<p>").expect("write");
        std::fs::write(dir.path().join("style.css"), "p{}").expect("write");
        let arg = dir.path().display().to_string();
        let targets = expand_targets(&[arg, "https://example.com/".to_owned()]).expect("expand");
        assert_eq!(targets.len(), 3, "{targets:?}");
        assert!(matches!(targets[2], Target::Url(_)));
        assert!(expand_targets(&["/no/such/path".to_owned()]).is_err());
    }
}
