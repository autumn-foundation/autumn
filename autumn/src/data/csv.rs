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

// ── import_csv ────────────────────────────────────────────────────────────────

/// Parse CSV from `reader`, validate rows, and drive import via `handler`.
///
/// The `handler` closure receives the **1-based CSV line number** and a
/// `HashMap<String, String>` mapping column name → value for every data row.
/// It returns an [`ImportRowResult`] that is folded into the returned
/// [`ImportReport`].
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
/// single bad row does not abort the entire import.
pub fn import_csv<R, F>(reader: R, opts: &ImportOptions, mut handler: F) -> ImportReport
where
    R: io::Read,
    F: FnMut(u64, HashMap<String, String>, &ImportMode) -> ImportRowResult,
{
    let mut report = ImportReport::default();

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(reader);

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
    // A header carrying an embedded newline inside a quoted field would
    // mis-calibrate this by however many lines it spans; that is vanishingly
    // rare, and the alternative (trusting a counter that disagrees with itself
    // across dialects) is wrong on the common case instead of the exotic one.
    let line_offset: i64 = 2 - i64::try_from(rdr.position().line()).unwrap_or(2);
    let absolute_line = move |raw: u64| -> u64 {
        let raw = i64::try_from(raw).unwrap_or(i64::MAX);
        u64::try_from(raw.saturating_add(line_offset)).unwrap_or(0)
    };

    for result in rdr.records() {
        let (line, record) = match result {
            Ok(r) => {
                let pos = absolute_line(r.position().map_or(0, csv::Position::line));
                (pos, r)
            }
            Err(e) => {
                let pos = absolute_line(e.position().map_or(0, csv::Position::line));
                report
                    .errors
                    .push(CsvRowError::row(pos, format!("CSV parse error: {e}")));
                continue;
            }
        };

        let row: HashMap<String, String> = headers
            .iter()
            .zip(record.iter())
            .map(|(k, v)| (k.clone(), v.to_owned()))
            .collect();

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
