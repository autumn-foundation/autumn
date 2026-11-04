//! CSV import and export for Autumn repository models.
//!
//! This module provides a [`CsvSchema`] trait, an [`ImportReport`] result type,
//! and the [`export_csv`] / [`import_csv`] free functions that drive the
//! streaming CSV pipeline.
//!
//! # Feature flag
//!
//! Everything in this module is gated behind the `csv` Cargo feature:
//!
//! ```toml
//! autumn-web = { version = "0.4", features = ["csv"] }
//! ```
//!
//! # Example — export
//!
//! ```rust,no_run
//! use autumn_web::data::csv::{CsvSchema, export_csv};
//! use std::io;
//!
//! struct Post { id: i64, title: String, published: bool }
//!
//! impl CsvSchema for Post {
//!     fn csv_columns() -> &'static [&'static str] { &["id", "title", "published"] }
//!     fn to_csv_record(&self) -> Vec<String> {
//!         vec![self.id.to_string(), self.title.clone(), self.published.to_string()]
//!     }
//! }
//!
//! let posts = vec![Post { id: 1, title: "Hello".into(), published: true }];
//! let mut out = Vec::<u8>::new();
//! export_csv(posts, &mut out).unwrap();
//! ```
//!
//! # Example — import
//!
//! ```rust,no_run
//! use autumn_web::data::csv::{ImportOptions, ImportRowResult, import_csv};
//! use std::collections::HashMap;
//!
//! let csv_data = b"title,published\nHello,true\nWorld,false\n";
//! let report = import_csv(
//!     csv_data.as_ref(),
//!     &ImportOptions::default(),
//!     |_line, row, _mode| {
//!         println!("Importing: {:?}", row);
//!         ImportRowResult::Inserted
//!     },
//! );
//! println!("{} inserted, {} errors", report.inserted, report.errors.len());
//! ```

use std::collections::HashMap;
use std::io;

use axum::response::{IntoResponse, Response};

// ── Row-level error ───────────────────────────────────────────────────────────

/// A parse or validation error for a single CSV row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRowError {
    /// 1-based line number in the source CSV file (header counts as line 1).
    pub line: u64,
    /// Column name where the error was detected, if known.
    pub column: Option<String>,
    /// Human-readable error description.
    pub message: String,
}

impl CsvRowError {
    /// Construct a row-level error without a column name.
    #[must_use]
    pub fn row(line: u64, message: impl Into<String>) -> Self {
        Self {
            line,
            column: None,
            message: message.into(),
        }
    }

    /// Construct a field-level error with a column name.
    #[must_use]
    pub fn field(line: u64, column: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            line,
            column: Some(column.into()),
            message: message.into(),
        }
    }
}

// ── Import report ─────────────────────────────────────────────────────────────

/// Structured summary returned by [`import_csv`].
///
/// `inserted + updated + skipped + errors.len()` equals the total number of
/// data rows (non-header rows) in the CSV.
#[non_exhaustive]
#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    /// Rows that were inserted as new records.
    pub inserted: u64,
    /// Rows that matched an existing record and were updated.
    pub updated: u64,
    /// Rows that were intentionally skipped (e.g. no-op upsert).
    pub skipped: u64,
    /// Rows that failed parsing or validation.
    pub errors: Vec<CsvRowError>,
}

impl ImportReport {
    /// Total data rows processed (inserted + updated + skipped + errors).
    #[must_use]
    pub const fn total_rows(&self) -> u64 {
        self.inserted + self.updated + self.skipped + self.errors.len() as u64
    }

    /// `true` if no errors were recorded.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

// ── Import mode ───────────────────────────────────────────────────────────────

/// Controls how imported rows are written to the database.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ImportMode {
    /// Every row is inserted as a new record. Duplicate key errors are
    /// surfaced as row-level errors in [`ImportReport::errors`].
    Insert,
    /// Rows are inserted or updated depending on whether a record with
    /// matching values in the `by` columns already exists.
    Upsert {
        /// Column names used to identify existing records.
        by: Vec<String>,
    },
    /// Parse and validate every row but do not write anything.
    /// The returned [`ImportReport`] reflects what *would* have happened.
    DryRun,
}

// ── Import options ────────────────────────────────────────────────────────────

/// Configuration knobs passed to [`import_csv`].
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// How rows should be written (or not written, for dry-run).
    pub mode: ImportMode,
    /// Number of rows passed to the handler per batch.
    ///
    /// The handler is still called once per row; this knob controls the
    /// chunking that callers use for transactional batching.
    pub batch_size: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            mode: ImportMode::Insert,
            batch_size: 500,
        }
    }
}

// ── Row outcome ───────────────────────────────────────────────────────────────

/// The outcome of processing a single imported row, returned by the handler
/// passed to [`import_csv`].
pub enum ImportRowResult {
    /// The row was inserted as a new record.
    Inserted,
    /// The row updated an existing record.
    Updated,
    /// The row was intentionally skipped (no-op).
    Skipped,
    /// A row-level error occurred.
    RowError(String),
    /// A field-level error occurred (column name + message).
    FieldError { column: String, message: String },
}

// ── CsvSchema trait ───────────────────────────────────────────────────────────

/// A type that knows its own CSV column schema.
///
/// Implement this manually or derive it with `#[derive(CsvSchema)]`
/// (provided by the `autumn-macros` crate).
///
/// # PII redaction and sensitive columns
///
/// Mark sensitive columns by omitting them from [`csv_columns`] or by
/// returning a redacted string (e.g. `"[REDACTED]"`) from [`to_csv_record`].
/// The derive macro honours `#[csv(skip)]` for this purpose.
///
/// # Custom column override
///
/// If you need a computed column (e.g. a joined display value), implement
/// [`CsvSchema`] manually and return the computed value from [`to_csv_record`].
/// The column name you add to [`csv_columns`] is used as the header.
///
/// [`csv_columns`]: CsvSchema::csv_columns
/// [`to_csv_record`]: CsvSchema::to_csv_record
pub trait CsvSchema {
    /// Ordered list of CSV column headers.
    ///
    /// The order here determines column order in the exported file and must
    /// match the order of values returned by [`CsvSchema::to_csv_record`].
    fn csv_columns() -> &'static [&'static str]
    where
        Self: Sized;

    /// Serialize this record into a list of string values.
    ///
    /// The number and order of values must match [`csv_columns`].
    ///
    /// [`csv_columns`]: CsvSchema::csv_columns
    fn to_csv_record(&self) -> Vec<String>;
}

// ── export_csv ────────────────────────────────────────────────────────────────

/// Stream `records` as RFC 4180 CSV into `writer`.
///
/// The first row is a header row derived from `T::csv_columns()`.
/// Subsequent rows come from `T::to_csv_record()`.
///
/// Memory usage is bounded by a single row at a time: the iterator is
/// consumed lazily and each row is flushed before the next is read.
///
/// # Errors
///
/// Returns an [`io::Error`] if writing to `writer` fails.
pub fn export_csv<T, W>(records: impl IntoIterator<Item = T>, mut writer: W) -> io::Result<()>
where
    T: CsvSchema,
    W: io::Write,
{
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(true)
        .from_writer(&mut writer);

    wtr.write_record(T::csv_columns())?;

    for record in records {
        wtr.write_record(record.to_csv_record())?;
    }

    wtr.flush()?;
    Ok(())
}

/// Axum response wrapper that serializes an iterator of [`CsvSchema`] records
/// into a downloaded CSV file (`Content-Disposition: attachment`).
///
/// The records are buffered into memory before the response is sent.
///
/// # Example
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::data::csv::{CsvSchema, CsvExport};
///
/// struct Post { id: i64, title: String }
/// impl CsvSchema for Post {
///     fn csv_columns() -> &'static [&'static str] { &["id", "title"] }
///     fn to_csv_record(&self) -> Vec<String> {
///         vec![self.id.to_string(), self.title.clone()]
///     }
/// }
///
/// #[get("/posts.csv")]
/// async fn export() -> impl axum::response::IntoResponse {
///     let posts = vec![Post { id: 1, title: "Hello".into() }];
///     CsvExport("posts.csv".to_owned(), posts)
/// }
/// ```
pub struct CsvExport<I>(pub String, pub I);

impl<I, T> IntoResponse for CsvExport<I>
where
    I: IntoIterator<Item = T>,
    T: CsvSchema,
{
    fn into_response(self) -> Response {
        let mut out = Vec::new();
        if export_csv(self.1, &mut out).is_err() {
            return (
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to generate CSV export",
            )
                .into_response();
        }

        // `Download` owns the `Content-Disposition` encoding: it strips control
        // characters, reduces the name to its basename, and quotes/escapes it,
        // so a caller-supplied filename cannot inject header directives.
        crate::download::Download::from_bytes(out)
            .filename(self.0)
            .content_type("text/csv; charset=utf-8")
            .into_response()
    }
}

// ── import_csv ────────────────────────────────────────────────────────────────

/// Count the data rows in a CSV source without retaining any of them.
///
/// The header is not counted; a malformed row IS (it is a row the file has, and
/// a caller bounding work has to see it). Nothing from the file is kept — each
/// record is parsed and dropped — so the memory cost is one record at a time
/// regardless of how long or how broken the input is.
///
/// This exists for callers that must bound how much work an untrusted upload can
/// ask for BEFORE handing it to [`import_csv`]: `import_csv` records a malformed
/// row as a [`CsvRowError`] and moves on *without* calling the row handler, so a
/// counter inside that handler cannot see the very file that costs the most. A
/// cheap counting pass first — over bytes the caller has already size-capped —
/// closes that gap for every shape of input.
///
/// ```rust,no_run
/// use autumn_web::data::csv::count_data_rows;
///
/// let file = b"title\nHello\nWorld\n";
/// assert_eq!(count_data_rows(file.as_ref()), 2);
/// ```
pub fn count_data_rows<R>(reader: R) -> u64
where
    R: io::Read,
{
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(reader);
    // `byte_records` avoids the UTF-8 validation `records` performs: this pass
    // only counts, and a row that is not valid UTF-8 is still a row.
    rdr.byte_records().count() as u64
}

/// The column names in a CSV source's header row, in file order.
///
/// Returns an empty `Vec` for an empty source or one whose header cannot be
/// parsed — "no columns" and "a header we could not read" are the same thing to
/// a caller deciding whether a file is the right shape.
///
/// This exists so a caller can reject a file that is simply the WRONG FILE
/// before importing any of it. [`import_csv`] decodes each row by column name
/// and a decoder can legitimately default an absent field (an unchecked
/// checkbox's `bool`, an optional column), so a spreadsheet whose header shares
/// none of the expected names can otherwise parse cleanly into a run of blank
/// records. Comparing the header against the columns you require turns that
/// into one file-level refusal — which is also what it actually is, rather than
/// the same error repeated per row.
///
/// ```rust,no_run
/// use autumn_web::data::csv::read_header;
///
/// let file = b"title,published\nHello,true\n";
/// assert_eq!(read_header(file.as_ref()), vec!["title", "published"]);
/// ```
pub fn read_header<R>(reader: R) -> Vec<String>
where
    R: io::Read,
{
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(reader);
    rdr.headers()
        .map(|header| header.iter().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Strip the `(line: N, byte: N)` fragment the CSV parser embeds in its own
/// error text.
///
/// The parser's line number is the UNCALIBRATED one (see the calibration in
/// [`import_csv`]), so on a CRLF file it disagrees with the `line` field of the
/// [`CsvRowError`] the message is attached to — and a report that shows a reader
/// two different line numbers for the same row is worse than one that shows
/// none. The `line` field is the answer; this removes the contradicting copy.
///
/// A message without the fragment is returned unchanged, so this degrades to a
/// no-op if the parser's wording ever changes.
fn without_parser_position(message: &str) -> String {
    const OPEN: &str = " (line: ";
    let Some(start) = message.find(OPEN) else {
        return message.to_owned();
    };
    let Some(len) = message[start..].find(')') else {
        return message.to_owned();
    };
    let mut out = String::with_capacity(message.len());
    out.push_str(&message[..start]);
    out.push_str(&message[start + len + 1..]);
    out
}

/// Count the line feeds in `bytes`.
fn newline_count(bytes: &[u8]) -> u64 {
    bytes.iter().map(|&b| u64::from(b == b'\n')).sum()
}

/// Reader that watches the two ends of the stream for the line count.
///
/// The parser skips blank lines before the header, but counts them. It also
/// counts the last line break of a record, so a final record with no break is
/// one line short on an LF file. A CRLF file counts that break late, so the
/// calibration offset already covers it.
struct EdgeProbe<R> {
    inner: R,
    /// Line feeds before the first byte that is not a line break.
    leading_breaks: u64,
    /// `true` until that first byte arrives.
    in_lead: bool,
    /// Last byte read so far.
    last: Option<u8>,
    /// `true` once the inner reader has no more bytes.
    at_eof: bool,
}

impl<R> EdgeProbe<R> {
    const fn new(inner: R) -> Self {
        Self {
            inner,
            leading_breaks: 0,
            in_lead: true,
            last: None,
            at_eof: false,
        }
    }

    /// `true` when the whole stream was read and its last byte is no line break.
    fn ends_open(&self) -> bool {
        self.at_eof && self.last.is_some_and(|b| b != b'\n' && b != b'\r')
    }
}

impl<R: io::Read> io::Read for EdgeProbe<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let read = &buf[..n];
        self.at_eof = n == 0;
        if let Some(&last) = read.last() {
            self.last = Some(last);
        }
        if self.in_lead {
            for &b in read {
                match b {
                    b'\n' => self.leading_breaks += 1,
                    b'\r' => {}
                    _ => {
                        self.in_lead = false;
                        break;
                    }
                }
            }
        }
        Ok(n)
    }
}

/// Parse CSV from `reader`, validate rows, and drive import via `handler`.
///
/// The `handler` closure receives the **1-based CSV line number** and a
/// `HashMap<String, String>` mapping column name → value for every data row.
/// It returns an [`ImportRowResult`] that is folded into the returned
/// [`ImportReport`].
///
/// The line number is the line where the row starts. Blank lines count as
/// lines. A quoted field with line breaks spans several lines. A file with
/// only CR line endings has no line numbers: each row gets 2.
///
/// In [`ImportMode::DryRun`], the handler is invoked but any
/// `Inserted` / `Updated` results are counted but **not written**.
/// The returned report reflects what *would* happen in a real run.
///
/// # Column ordering
///
/// Column names come from the CSV header row (the first row).  Field names
/// must match the schema (or a `#[serde(rename)]` mapping) for the handler to
/// find them by name.
///
/// # Errors
///
/// CSV parse errors (malformed quoting, wrong number of fields) are captured
/// as [`CsvRowError`] entries rather than bubbling as a `Result` so that a
/// single bad row does not abort the entire import. Their `line` follows the
/// same rule as the handler line.
pub fn import_csv<R, F>(reader: R, opts: &ImportOptions, mut handler: F) -> ImportReport
where
    R: io::Read,
    F: FnMut(u64, HashMap<String, String>, &ImportMode) -> ImportRowResult,
{
    let mut report = ImportReport::default();

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(EdgeProbe::new(reader));

    let headers: Vec<String> = match rdr.headers() {
        Ok(h) => h.iter().map(str::to_owned).collect(),
        Err(e) => {
            report
                .errors
                .push(CsvRowError::row(1, format!("CSV header error: {e}")));
            return report;
        }
    };

    // Calibrate the reported line numbers against the header, whose true span
    // we know: it is line 1, so a reader that has just consumed it sits at line
    // 2. The underlying `csv` parser's own counter does NOT agree with that on
    // every dialect — on a CRLF file (what Excel and most Windows tools write)
    // it runs exactly one behind the LF reading of the same rows, so a row a
    // spreadsheet shows on line 7 would be reported as line 6, and a
    // byte-identical LF copy of the same file would report 7. A row number that
    // depends on the line endings is worse than useless in an error report
    // someone is meant to act on, so measure the parser's convention once here
    // and shift every position below by the same amount. On a dialect the
    // parser does not count lines for at all (bare-CR, the classic-Mac
    // dialect), the shift still puts the first data row at 2 — the rows after
    // it share that number rather than climbing, which is the parser's limit,
    // not this offset's.
    //
    // The shift is measured ONCE, so it assumes the file's terminators are
    // internally consistent. The one input it cannot serve is a file that MIXES
    // them — an editor that appended an LF row to a CRLF export, or two exports
    // concatenated — where the rows after the switch drift by one. That is rare
    // next to "the file came out of Excel", and the alternative (trusting a
    // counter that disagrees with itself across dialects) is wrong on the common
    // case instead of the exotic one.
    // The header's true span, so the shift is the dialect's alone. Count the
    // blank lines before the header and the line breaks inside its fields.
    // Assuming one line would move every row number.
    let header_lines: u64 = 1
        + rdr.get_ref().leading_breaks
        + headers
            .iter()
            .map(|field| newline_count(field.as_bytes()))
            .sum::<u64>();
    // The first data row therefore starts at `header_lines + 1`; whatever the
    // parser calls that row is the convention to correct for.
    let line_offset: i64 = i64::try_from(header_lines.saturating_add(1)).unwrap_or(2)
        - i64::try_from(rdr.position().line()).unwrap_or(2);
    let absolute_line = move |raw: u64| -> u64 {
        let raw = i64::try_from(raw).unwrap_or(i64::MAX);
        u64::try_from(raw.saturating_add(line_offset)).unwrap_or(0)
    };

    // The parser sets the start line of a record before it skips blank lines,
    // so that line is too small by the number of blank lines. The end line is
    // exact. Subtract the line breaks of the record to get its true start.
    // Never report a line before the parser start.
    let lf_file = line_offset == 0;
    let start_line = |rdr_end: u64, raw: &csv::ByteRecord, parser: u64, open: bool| -> u64 {
        let end_break = u64::from(!(open && lf_file));
        let inner_breaks: u64 = raw.iter().map(newline_count).sum();
        absolute_line(rdr_end)
            .saturating_sub(inner_breaks + end_break)
            .max(parser)
    };
    let mut raw = csv::ByteRecord::new();
    loop {
        let read = rdr.read_byte_record(&mut raw);
        let end = rdr.position().line();
        let open = rdr.get_ref().ends_open();
        let line = match &read {
            Ok(false) => break,
            Ok(true) => {
                let parser = absolute_line(raw.position().map_or(0, csv::Position::line));
                start_line(end, &raw, parser, open)
            }
            Err(e) => {
                let parser = absolute_line(e.position().map_or(0, csv::Position::line));
                // After a length error, `raw` holds the bad row.
                let line = if matches!(e.kind(), csv::ErrorKind::UnequalLengths { .. }) {
                    start_line(end, &raw, parser, open)
                } else {
                    parser
                };
                report.errors.push(CsvRowError::row(
                    line,
                    format!(
                        "CSV parse error: {}",
                        without_parser_position(&e.to_string())
                    ),
                ));
                if matches!(e.kind(), csv::ErrorKind::Io(_)) {
                    break;
                }
                continue;
            }
        };

        let row: Result<HashMap<String, String>, _> = headers
            .iter()
            .zip(raw.iter())
            .map(|(k, v)| std::str::from_utf8(v).map(|v| (k.clone(), v.to_owned())))
            .collect();
        let row = match row {
            Ok(row) => row,
            Err(e) => {
                report.errors.push(CsvRowError::row(
                    line,
                    format!("CSV parse error: invalid utf-8: {e}"),
                ));
                continue;
            }
        };

        let outcome = handler(line, row, &opts.mode);

        match outcome {
            ImportRowResult::Inserted => report.inserted += 1,
            ImportRowResult::Updated => report.updated += 1,
            ImportRowResult::Skipped => report.skipped += 1,
            ImportRowResult::RowError(msg) => {
                report.errors.push(CsvRowError::row(line, msg));
            }
            ImportRowResult::FieldError { column, message } => {
                report
                    .errors
                    .push(CsvRowError::field(line, column, message));
            }
        }
    }

    report
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    struct Post {
        id: i64,
        title: String,
        published: bool,
    }

    impl CsvSchema for Post {
        fn csv_columns() -> &'static [&'static str] {
            &["id", "title", "published"]
        }
        fn to_csv_record(&self) -> Vec<String> {
            vec![
                self.id.to_string(),
                self.title.clone(),
                self.published.to_string(),
            ]
        }
    }

    fn sample_posts() -> Vec<Post> {
        vec![
            Post {
                id: 1,
                title: "Hello, World".to_string(),
                published: true,
            },
            Post {
                id: 2,
                title: "Goodbye cruel \"world\"".to_string(),
                published: false,
            },
        ]
    }

    // ── CsvRowError tests ─────────────────────────────────────────────────────

    #[test]
    fn csv_row_error_row_constructor() {
        let e = CsvRowError::row(42, "bad value");
        assert_eq!(e.line, 42);
        assert!(e.column.is_none());
        assert_eq!(e.message, "bad value");
    }

    #[test]
    fn csv_row_error_field_constructor() {
        let e = CsvRowError::field(5, "email", "invalid email");
        assert_eq!(e.line, 5);
        assert_eq!(e.column.as_deref(), Some("email"));
        assert_eq!(e.message, "invalid email");
    }

    // ── ImportReport tests ────────────────────────────────────────────────────

    #[test]
    fn import_report_default_is_zero() {
        let r = ImportReport::default();
        assert_eq!(r.inserted, 0);
        assert_eq!(r.updated, 0);
        assert_eq!(r.skipped, 0);
        assert!(r.errors.is_empty());
        assert_eq!(r.total_rows(), 0);
        assert!(r.is_ok());
    }

    #[test]
    fn import_report_total_rows_sums_all_buckets() {
        let r = ImportReport {
            inserted: 3,
            updated: 2,
            skipped: 1,
            errors: vec![CsvRowError::row(10, "oops")],
        };
        assert_eq!(r.total_rows(), 7);
        assert!(!r.is_ok());
    }

    // ── export_csv tests ──────────────────────────────────────────────────────

    #[test]
    fn export_csv_writes_header_and_rows() {
        let mut out = Vec::new();
        export_csv(sample_posts(), &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        let mut lines = s.lines();

        assert_eq!(lines.next().unwrap(), "id,title,published");
        assert_eq!(lines.next().unwrap(), "1,\"Hello, World\",true");
    }

    #[test]
    fn export_csv_applies_rfc4180_quoting_for_commas() {
        let mut out = Vec::new();
        export_csv(sample_posts(), &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("\"Hello, World\""),
            "comma in title should be quoted: {s}"
        );
    }

    #[test]
    fn export_csv_applies_rfc4180_quoting_for_double_quotes() {
        let mut out = Vec::new();
        export_csv(sample_posts(), &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        // RFC 4180: embedded quote → double-quote escape
        assert!(
            s.contains("\"Goodbye cruel \"\"world\"\"\""),
            "embedded quotes should be doubled: {s}"
        );
    }

    #[test]
    fn export_csv_empty_iterator_writes_header_only() {
        let mut out = Vec::new();
        export_csv(Vec::<Post>::new(), &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines, vec!["id,title,published"]);
    }

    #[test]
    fn export_csv_stable_column_ordering() {
        let mut out = Vec::new();
        export_csv(sample_posts(), &mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        let header = s.lines().next().unwrap();
        assert_eq!(header, "id,title,published");
    }

    // ── CsvExport tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn csv_export_sets_csv_headers_and_body() {
        let resp = CsvExport("posts.csv".to_owned(), sample_posts()).into_response();
        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(
            resp.headers()[http::header::CONTENT_TYPE],
            "text/csv; charset=utf-8"
        );
        assert_eq!(
            resp.headers()[http::header::CONTENT_DISPOSITION],
            "attachment; filename=\"posts.csv\""
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.starts_with("id,title,published\n"), "{body}");
        assert!(body.contains("\"Hello, World\""), "{body}");
    }

    #[test]
    fn csv_export_filename_cannot_inject_header_directives() {
        let resp = CsvExport(
            "../x\"; filename=evil.exe\r\nSet-Cookie: a=b".to_owned(),
            sample_posts(),
        )
        .into_response();
        assert_eq!(resp.status(), http::StatusCode::OK);
        assert!(resp.headers().get(http::header::SET_COOKIE).is_none());
        let disposition = resp.headers()[http::header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap();
        assert_eq!(
            disposition,
            "attachment; filename=\"x\\\"; filename=evil.exeSet-Cookie: a=b\""
        );
    }

    // ── import_csv tests ──────────────────────────────────────────────────────

    #[test]
    fn import_csv_insert_mode_counts_inserted() {
        let csv = b"id,title,published\n1,Hello,true\n2,World,false\n";
        let report = import_csv(
            csv.as_ref(),
            &ImportOptions::default(),
            |_line, _row, _mode| ImportRowResult::Inserted,
        );
        assert_eq!(report.inserted, 2);
        assert_eq!(report.updated, 0);
        assert_eq!(report.skipped, 0);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn import_csv_handler_receives_column_values_as_map() {
        let csv = b"title,published\nHello,true\n";
        let mut seen: Option<HashMap<String, String>> = None;
        import_csv(
            csv.as_ref(),
            &ImportOptions::default(),
            |_line, row, _mode| {
                seen = Some(row);
                ImportRowResult::Inserted
            },
        );
        let row = seen.unwrap();
        assert_eq!(row.get("title").map(String::as_str), Some("Hello"));
        assert_eq!(row.get("published").map(String::as_str), Some("true"));
    }

    #[test]
    fn import_csv_row_error_is_captured_with_line_number() {
        let csv = b"title\nGood row\nBad row\nAnother good\n";
        let report = import_csv(
            csv.as_ref(),
            &ImportOptions::default(),
            |line, row, _mode| {
                if row.get("title").map(String::as_str) == Some("Bad row") {
                    ImportRowResult::RowError("title must not be 'Bad row'".into())
                } else {
                    let _ = line;
                    ImportRowResult::Inserted
                }
            },
        );
        assert_eq!(report.inserted, 2);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].message, "title must not be 'Bad row'");
        // "Bad row" is the THIRD line of the file (the header is line 1), and
        // that is the number an operator has to be able to find in their
        // spreadsheet.
        assert_eq!(report.errors[0].line, 3);
    }

    /// The same file with CRLF terminators — what Excel and most Windows tools
    /// write — must report the SAME line numbers. The underlying parser's own
    /// counter runs one behind on that dialect, so this is what pins the
    /// calibration in `import_csv`; without it a byte-identical CRLF copy of the
    /// file above blames line 2 for the row on line 3.
    #[test]
    fn import_csv_line_numbers_are_the_same_for_crlf_and_lf() {
        let lines = |csv: &[u8]| -> Vec<u64> {
            let mut seen = Vec::new();
            let report = import_csv(csv, &ImportOptions::default(), |line, row, _mode| {
                seen.push(line);
                if row.get("title").map(String::as_str) == Some("Bad row") {
                    ImportRowResult::RowError("bad".into())
                } else {
                    ImportRowResult::Inserted
                }
            });
            assert_eq!(report.errors.len(), 1, "one bad row per fixture");
            assert_eq!(
                report.errors[0].line, 3,
                "the bad row is on line 3 of both fixtures"
            );
            seen
        };
        let lf = lines(b"title\nGood row\nBad row\nAnother good\n");
        let crlf = lines(b"title\r\nGood row\r\nBad row\r\nAnother good\r\n");
        assert_eq!(lf, vec![2, 3, 4]);
        assert_eq!(crlf, lf, "line endings must not change the row numbers");
    }

    /// The calibration has to reach the PARSE-ERROR branch too: a malformed row
    /// is reported with the line it is on, and `docs/guide/generators.md`
    /// promises the scaffolded importer surfaces it "against that row's line".
    /// Pre-fix the CRLF fixture blamed line 2 for the row on line 3.
    #[test]
    fn import_csv_parse_error_line_numbers_match_across_dialects() {
        let lines = |csv: &[u8]| -> u64 {
            let report = import_csv(csv, &ImportOptions::default(), |_line, _row, _mode| {
                ImportRowResult::Inserted
            });
            assert_eq!(report.errors.len(), 1, "one malformed row per fixture");
            assert!(
                report.errors[0].message.starts_with("CSV parse error"),
                "expected a parse error, got {:?}",
                report.errors[0].message
            );
            report.errors[0].line
        };
        // Row 2 carries a third field the two-column header cannot take.
        assert_eq!(lines(b"title,note\na,b\nc,d,e\nf,g\n"), 3);
        assert_eq!(lines(b"title,note\r\na,b\r\nc,d,e\r\nf,g\r\n"), 3);
    }

    /// The line number in the `line` FIELD and any line number inside the
    /// `message` must never contradict each other. The parser's own Display
    /// embeds its uncalibrated position, which on a CRLF file is one behind, so
    /// the report would show a reader two different lines for one row.
    #[test]
    fn import_csv_parse_error_message_carries_no_second_line_number() {
        for csv in [
            &b"title,note\na,x\nBAD\nc,z\n"[..],
            &b"title,note\r\na,x\r\nBAD\r\nc,z\r\n"[..],
        ] {
            let report = import_csv(csv, &ImportOptions::default(), |_line, _row, _mode| {
                ImportRowResult::Inserted
            });
            assert_eq!(report.errors.len(), 1);
            assert_eq!(report.errors[0].line, 3);
            assert!(
                !report.errors[0].message.contains("line:"),
                "the parser's own line number must not travel in the message: {:?}",
                report.errors[0].message
            );
            // The failure itself is still described.
            assert!(
                report.errors[0].message.contains("fields"),
                "{:?}",
                report.errors[0].message
            );
        }
    }

    /// A header that cannot be read at all aborts the import before any row
    /// handler runs, and is reported against line 1 — the line the header is on.
    #[test]
    fn import_csv_header_error_is_reported_against_line_one_and_runs_no_rows() {
        let report = import_csv(
            &[0xff, 0xfe, b'a', b'\n'][..],
            &ImportOptions::default(),
            |_line, _row, _mode| unreachable!("no row handler may run for a bad header"),
        );
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].line, 1);
        assert!(
            report.errors[0].message.starts_with("CSV header error"),
            "{:?}",
            report.errors[0].message
        );
        assert_eq!(report.total_rows(), 1);
    }

    /// The header a caller checks a file's shape against, before importing any
    /// of it.
    #[test]
    fn read_header_returns_the_column_names_in_file_order() {
        assert_eq!(
            read_header(b"title,note\na,x\n".as_ref()),
            vec!["title", "note"]
        );
        // CRLF, and a quoted header field carrying a comma and a newline.
        assert_eq!(
            read_header(b"title,note\r\na,x\r\n".as_ref()),
            vec!["title", "note"]
        );
        assert_eq!(
            read_header(b"\"a,b\",\"c\nd\"\nx,y\n".as_ref()),
            vec!["a,b", "c\nd"]
        );
        // A header-only file still HAS a header; an empty one has none.
        assert_eq!(read_header(b"title,note\n".as_ref()), vec!["title", "note"]);
        assert!(read_header(b"".as_ref()).is_empty());
    }

    /// The counting pass a caller uses to bound work before importing has to
    /// count the rows the import itself would never hand to a row handler — a
    /// malformed row is exactly the one that costs the most to accumulate.
    #[test]
    fn count_data_rows_counts_every_data_row_including_malformed_ones() {
        // Two well-formed rows, header excluded.
        assert_eq!(count_data_rows(b"title,note\na,x\nb,y\n".as_ref()), 2);
        // A row with the wrong field count is still a row.
        assert_eq!(count_data_rows(b"title,note\na,x\nBAD\nb,y\n".as_ref()), 3);
        // A quoted field spanning lines is ONE row, not two.
        assert_eq!(
            count_data_rows(b"title,note\n\"a\nz\",x\nb,y\n".as_ref()),
            2
        );
        // CRLF counts the same as LF.
        assert_eq!(count_data_rows(b"title,note\r\na,x\r\nb,y\r\n".as_ref()), 2);
        // A header-only file, and an empty one.
        assert_eq!(count_data_rows(b"title,note\n".as_ref()), 0);
        assert_eq!(count_data_rows(b"".as_ref()), 0);
    }

    /// The count and what `import_csv` reports must agree, or a caller bounding
    /// work by the first would be bounding the wrong number.
    #[test]
    fn count_data_rows_agrees_with_import_csv_total_rows() {
        let csv = b"title,note\na,x\nBAD\nb,y\n";
        let report = import_csv(csv.as_ref(), &ImportOptions::default(), |_l, _r, _m| {
            ImportRowResult::Inserted
        });
        assert_eq!(count_data_rows(csv.as_ref()), report.total_rows());
    }

    /// A header that itself spans two lines (a quoted field with an embedded
    /// newline in it) pushes every data row down by one — and the reported
    /// numbers must move with it, in both dialects. This is what stops the
    /// dialect calibration from assuming every header is one line: doing so
    /// would take numbers that were RIGHT on an LF file and make them wrong.
    #[test]
    fn import_csv_line_numbers_survive_a_multi_line_header() {
        let lines = |csv: &[u8]| -> Vec<u64> {
            let mut seen = Vec::new();
            import_csv(csv, &ImportOptions::default(), |line, _row, _mode| {
                seen.push(line);
                ImportRowResult::Inserted
            });
            seen
        };
        // The header occupies lines 1-2, so the data rows are on 3 and 4.
        assert_eq!(lines(b"\"a\nz\",b\nr1,x\nr2,y\n"), vec![3, 4]);
        assert_eq!(lines(b"\"a\r\nz\",b\r\nr1,x\r\nr2,y\r\n"), vec![3, 4]);
    }

    /// A quoted field carrying an embedded newline moves the rows after it down
    /// the file, and the reported numbers have to move with them — the operator
    /// counts lines in a text editor, not records.
    #[test]
    fn import_csv_line_numbers_follow_multi_line_quoted_fields() {
        let lines = |csv: &[u8]| -> Vec<u64> {
            let mut seen = Vec::new();
            import_csv(csv, &ImportOptions::default(), |line, _row, _mode| {
                seen.push(line);
                ImportRowResult::Inserted
            });
            seen
        };
        // Row 1 spans lines 2-3; row 2 therefore starts on line 4.
        assert_eq!(lines(b"title,note\n\"a\nb\",fine\nsecond,x\n"), vec![2, 4]);
        assert_eq!(
            lines(b"title,note\r\n\"a\r\nb\",fine\r\nsecond,x\r\n"),
            vec![2, 4]
        );
    }

    /// Blank lines are skipped by the parser, but they still take up lines.
    /// A row after them is reported on its own line (#3328).
    #[test]
    fn import_csv_line_numbers_skip_blank_lines() {
        let lines = |csv: &[u8]| -> Vec<u64> {
            let mut seen = Vec::new();
            import_csv(csv, &ImportOptions::default(), |line, _row, _mode| {
                seen.push(line);
                ImportRowResult::Inserted
            });
            seen
        };
        for eol in ["\n", "\r\n"] {
            let f = |s: &str| lines(s.replace('\n', eol).as_bytes());
            assert_eq!(f("a,b\n\n1,2\n"), vec![3], "{eol:?}");
            assert_eq!(f("a,b\n1,2\n\n3,4\n5,6\n"), vec![2, 4, 5], "{eol:?}");
            assert_eq!(f("a,b\n\n\n\n1,2\n3,4\n"), vec![5, 6], "{eol:?}");
            assert_eq!(f("a,b\n\"x\ny\",1\n\n3,4\n"), vec![2, 5], "{eol:?}");
            // Last row has no terminator.
            assert_eq!(f("a,b\n1,2\n\n\n3,4"), vec![2, 5], "{eol:?}");
            assert_eq!(f("a,b\n\n1,2"), vec![3], "{eol:?}");
            // Trailing blank lines add no rows.
            assert_eq!(f("a,b\n1,2\n\n\n"), vec![2], "{eol:?}");
        }
    }

    /// Parse errors and handler errors use the same corrected line.
    #[test]
    fn import_csv_error_lines_skip_blank_lines() {
        let report = import_csv(
            &b"a,b\n1,2\n\n\nx,y,z\n\n3,4\n"[..],
            &ImportOptions::default(),
            |line, row, _mode| {
                if row["a"] == "3" {
                    ImportRowResult::RowError(format!("bad at {line}"))
                } else {
                    ImportRowResult::Inserted
                }
            },
        );
        let lines: Vec<u64> = report.errors.iter().map(|e| e.line).collect();
        assert_eq!(lines, vec![5, 7]);
    }

    /// Reads at most `n` bytes per call.
    struct Chunked<'a>(&'a [u8], usize);

    impl io::Read for Chunked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.1.min(self.0.len()).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    fn row_lines(csv: &[u8], chunk: usize) -> Vec<u64> {
        let mut seen = Vec::new();
        import_csv(
            Chunked(csv, chunk),
            &ImportOptions::default(),
            |line, _row, _mode| {
                seen.push(line);
                ImportRowResult::Inserted
            },
        );
        seen
    }

    /// Same lines for any read size, so the end probe holds across chunks.
    #[test]
    fn import_csv_blank_line_numbers_do_not_depend_on_read_size() {
        for chunk in [1, 2, 3, 8192] {
            assert_eq!(
                row_lines(b"a,b\n1,2\n\n\n3,4", chunk),
                vec![2, 5],
                "{chunk}"
            );
            assert_eq!(
                row_lines(b"a,b\r\n1,2\r\n\r\n3,4", chunk),
                vec![2, 4],
                "{chunk}"
            );
            assert_eq!(row_lines(b"\n\na,b\n1,2\n", chunk), vec![4], "{chunk}");
        }
    }

    /// Blank lines before the header push every row down.
    #[test]
    fn import_csv_line_numbers_follow_leading_blank_lines() {
        assert_eq!(row_lines(b"\n\na,b\n1,2\n3,4\n", 8192), vec![4, 5]);
        assert_eq!(row_lines(b"\r\n\r\na,b\r\n1,2\r\n", 8192), vec![4]);
        assert_eq!(row_lines(b"\n\"a\nb\",c\n\n1,2\n", 8192), vec![5]);
        assert_eq!(row_lines(b"\"a\nb\",c\n\n1,2\n", 8192), vec![4]);
    }

    /// Blank lines inside a quoted field, and lines with only spaces, are data.
    #[test]
    fn import_csv_blank_lines_inside_data_still_count() {
        for eol in ["\n", "\r\n"] {
            let f = |s: &str| row_lines(s.replace('\n', eol).as_bytes(), 8192);
            assert_eq!(f("a,b\n\"x\n\ny\",1\n\n3,4\n"), vec![2, 6], "{eol:?}");
            assert_eq!(f("a\n  \nx\n"), vec![2, 3], "{eol:?}");
        }
    }

    /// A row that is not valid UTF-8 gets the corrected line too.
    #[test]
    fn import_csv_utf8_error_line_skips_blank_lines() {
        let mut seen = Vec::new();
        let report = import_csv(
            &b"a,b\n1,2\n\n\xff,x\n\n3,4\n"[..],
            &ImportOptions::default(),
            |line, _row, _mode| {
                seen.push(line);
                ImportRowResult::Inserted
            },
        );
        assert_eq!(seen, vec![2, 6]);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].line, 4);
    }

    /// Blank lines keep the numbers right at scale.
    #[test]
    fn import_csv_blank_line_numbers_hold_for_large_files() {
        let mut csv = String::from("a,b\n");
        let mut want = Vec::new();
        let mut line = 1u64;
        for i in 0..3000 {
            if i % 7 == 0 {
                csv.push('\n');
                line += 1;
            }
            csv.push_str("1,2\n");
            line += 1;
            want.push(line);
        }
        assert_eq!(row_lines(csv.as_bytes(), 8192), want);
    }

    /// A file with CR-only line endings reports 2 for each row.
    #[test]
    fn import_csv_bare_cr_rows_report_line_two() {
        assert_eq!(row_lines(b"a,b\r1,2\r3,4\r", 8192), vec![2, 2]);
    }

    #[test]
    fn import_csv_field_error_records_column_name() {
        let csv = b"email\nbad-email\n";
        let report = import_csv(
            csv.as_ref(),
            &ImportOptions::default(),
            |_line, row, _mode| {
                if row.get("email").map_or("", String::as_str).contains('@') {
                    ImportRowResult::Inserted
                } else {
                    ImportRowResult::FieldError {
                        column: "email".into(),
                        message: "must be a valid email".into(),
                    }
                }
            },
        );
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].column.as_deref(), Some("email"));
        assert_eq!(report.errors[0].message, "must be a valid email");
    }

    #[test]
    fn import_csv_dry_run_counts_but_does_not_write() {
        let csv = b"id,title\n1,Hello\n2,World\n";
        let mut write_called = false;
        let opts = ImportOptions {
            mode: ImportMode::DryRun,
            batch_size: 100,
        };
        let report = import_csv(csv.as_ref(), &opts, |_line, _row, mode| {
            // A correctly-implemented handler gates writes on mode.
            if !matches!(mode, ImportMode::DryRun) {
                write_called = true;
            }
            ImportRowResult::Inserted
        });
        assert!(!write_called, "handler must not write in dry-run mode");
        assert_eq!(report.inserted, 2, "dry-run should still count rows");
    }

    #[test]
    fn import_csv_upsert_mode_counts_updated() {
        let csv = b"id,title\n1,Hello\n2,World\n";
        let opts = ImportOptions {
            mode: ImportMode::Upsert {
                by: vec!["id".into()],
            },
            batch_size: 100,
        };
        let report = import_csv(csv.as_ref(), &opts, |_line, _row, _mode| {
            ImportRowResult::Updated
        });
        assert_eq!(report.updated, 2);
        assert_eq!(report.inserted, 0);
    }

    #[test]
    fn import_csv_reports_error_at_exact_row_for_large_file() {
        // Simulates a large file where row 27143 has a validation error
        let target_line: usize = 27143;
        let mut csv = String::from("value\n");
        for i in 1..=target_line {
            if i == target_line - 1 {
                // this is the "bad" row (line 27143 in CSV = data row 27142)
                csv.push_str("BAD\n");
            } else {
                csv.push_str("good\n");
            }
        }

        let report = import_csv(
            csv.as_bytes(),
            &ImportOptions::default(),
            |_line, row, _mode| {
                if row.get("value").map(String::as_str) == Some("BAD") {
                    ImportRowResult::RowError("value is BAD".into())
                } else {
                    ImportRowResult::Inserted
                }
            },
        );
        assert_eq!(report.errors.len(), 1, "exactly one error expected");
        assert!(!report.errors.is_empty());
        assert_eq!(report.errors[0].message, "value is BAD");
        // The test's name promises an EXACT row; assert it, so this 27k-row
        // fixture buys something the small ones above do not. The bad row is
        // written at `i == target_line - 1`, which — with the header on line 1 —
        // is file line 27143.
        assert_eq!(report.errors[0].line, 27_143);
    }

    #[test]
    fn import_csv_skipped_rows_counted() {
        let csv = b"status\nactive\narchived\nactive\n";
        let report = import_csv(
            csv.as_ref(),
            &ImportOptions::default(),
            |_line, row, _mode| {
                if row.get("status").map(String::as_str) == Some("archived") {
                    ImportRowResult::Skipped
                } else {
                    ImportRowResult::Inserted
                }
            },
        );
        assert_eq!(report.inserted, 2);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.total_rows(), 3);
    }

    #[test]
    fn import_options_default_is_insert_batch_500() {
        let opts = ImportOptions::default();
        assert!(matches!(opts.mode, ImportMode::Insert));
        assert_eq!(opts.batch_size, 500);
    }

    // ── Round-trip test ───────────────────────────────────────────────────────

    #[test]
    fn export_then_import_round_trips_data() {
        let posts = sample_posts();
        let mut exported = Vec::new();
        export_csv(posts, &mut exported).unwrap();

        let mut titles_imported = Vec::new();
        import_csv(
            exported.as_slice(),
            &ImportOptions::default(),
            |_line, row, _mode| {
                titles_imported.push(row.get("title").cloned().unwrap_or_default());
                ImportRowResult::Inserted
            },
        );

        assert_eq!(
            titles_imported,
            vec!["Hello, World", "Goodbye cruel \"world\""]
        );
    }
}
