//! Source-file reading utilities for the dev error overlay.
//!
//! Reads lines of Rust source files from disk at overlay-render time and
//! returns a window of context lines (±N lines around the failing line).
//! Only called in dev mode; never compiled into release binaries.

use super::dev_badge::SourceLine;

/// How many context lines to include above and below the failing line.
const CONTEXT_RADIUS: u32 = 5;

/// Read source context around `failing_line` from `file_path`.
///
/// Returns up to `2 * CONTEXT_RADIUS + 1` lines centred on `failing_line`.
/// The line at `failing_line` has `is_highlighted = true`.
/// Returns an empty vec if the file cannot be read or the line is out of range.
pub fn read_source_context(file_path: &str, failing_line: u32) -> Vec<SourceLine> {
    if failing_line == 0 || file_path.is_empty() {
        return Vec::new();
    }

    let resolved = resolve_path(file_path);
    let Ok(contents) = std::fs::read_to_string(&resolved) else {
        return Vec::new();
    };

    let all_lines: Vec<&str> = contents.lines().collect();
    let Ok(total) = u32::try_from(all_lines.len()) else {
        return Vec::new();
    };

    if failing_line > total {
        return Vec::new();
    }

    let start = failing_line.saturating_sub(CONTEXT_RADIUS).max(1);
    let end = (failing_line + CONTEXT_RADIUS).min(total);

    (start..=end)
        .map(|n| SourceLine {
            line_no: n,
            content: all_lines[(n - 1) as usize].to_owned(),
            is_highlighted: n == failing_line,
        })
        .collect()
}

/// Resolve a file path that may be relative (to cwd) or absolute.
fn resolve_path(file_path: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(p)
    } else {
        p.to_path_buf()
    }
}

/// Classify a backtrace frame file path as belonging to the project workspace.
///
/// Workspace frames are relative paths or absolute paths inside the cwd.
/// Stdlib (`/rustc/`) and cargo registry (`/.cargo/registry/`) frames are
/// excluded.
pub fn is_workspace_file(file_path: &str) -> bool {
    if file_path.is_empty() {
        return false;
    }
    // Normalize separators so Windows backslash paths match the same checks.
    let normalized = file_path.replace('\\', "/");
    if normalized.contains("/rustc/")
        || normalized.contains("/.cargo/registry/")
        || normalized.contains("/.cargo/git/")
    {
        return false;
    }
    let p = std::path::Path::new(file_path);
    if !p.is_absolute() {
        return true;
    }
    std::env::current_dir().is_ok_and(|cwd| p.starts_with(&cwd))
}

/// Parse a `std::backtrace::Backtrace` display string into structured frames.
///
/// The expected format (from Rust's stdlib Display impl):
/// ```text
/// stack backtrace:
///    0: rust_begin_unwind
///              at /rustc/.../panicking.rs:661:5
///    1: my_crate::handler
///              at src/routes/handler.rs:42:5
/// ```
pub fn parse_backtrace_string(
    backtrace: &str,
    max_frames: usize,
) -> Vec<super::dev_badge::StackFrame> {
    use super::dev_badge::StackFrame;

    let mut frames: Vec<StackFrame> = Vec::new();
    let mut lines = backtrace.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        // Frame index lines look like: "   0: symbol_name"
        if let Some(colon_pos) = trimmed.find(": ") {
            let index_part = &trimmed[..colon_pos];
            // `all()` is true on an empty iterator, so reject an empty index explicitly.
            if index_part.is_empty() || !index_part.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let function = trimmed[colon_pos + 2..].trim().to_owned();
            if function.is_empty() {
                continue;
            }

            // The next line may be the location: "   at FILE:LINE:COL". Copied out so the peek
            // borrow ends before `lines.next()`.
            let at_rest_owned = lines
                .peek()
                .and_then(|s| s.trim().strip_prefix("at "))
                .map(str::to_owned);
            let (file, line_no) = at_rest_owned.map_or_else(
                || (String::new(), 0),
                |at_rest| {
                    lines.next();
                    parse_location(&at_rest)
                },
            );

            let in_workspace = is_workspace_file(&file);
            let source_context = if in_workspace && line_no > 0 {
                read_source_context(&file, line_no)
            } else {
                Vec::new()
            };

            frames.push(StackFrame {
                file,
                line: line_no,
                function,
                source_context,
                is_in_workspace: in_workspace,
            });

            if frames.len() >= max_frames {
                break;
            }
        }
    }

    frames
}

/// Parse a `FILE:LINE:COL` or `FILE:LINE` location string.
///
/// Uses a segment-based approach working from the right of the string so that
/// Windows drive-letter colons (e.g. `C:\path\file.rs:42`) are handled
/// correctly — the drive letter colon is not mistaken for a line/col separator.
fn parse_location(s: &str) -> (String, u32) {
    let parts: Vec<&str> = s.split(':').collect();
    let n = parts.len();
    if n >= 2
        && let Some(&last) = parts.last()
        && last.parse::<u32>().is_ok()
    {
        {
            // Last part is numeric — could be COL. Check if second-last is LINE.
            if n >= 3
                && let Ok(line_no) = parts[n - 2].parse::<u32>()
            {
                let file = parts[..n - 2].join(":");
                return (file, line_no);
            }
            // No COL: last part is LINE.
            let line_no = last.parse::<u32>().unwrap_or(0);
            if line_no > 0 {
                let file = parts[..n - 1].join(":");
                return (file, line_no);
            }
        }
    }
    (s.to_owned(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_location_file_line_col() {
        let (file, line) = parse_location("src/routes/posts.rs:42:5");
        assert_eq!(file, "src/routes/posts.rs");
        assert_eq!(line, 42);
    }

    #[test]
    fn parse_location_file_line_only() {
        let (file, line) = parse_location("src/lib.rs:10");
        assert_eq!(file, "src/lib.rs");
        assert_eq!(line, 10);
    }

    #[test]
    fn parse_location_absolute_path() {
        let (file, line) = parse_location("/home/user/project/src/main.rs:5:3");
        assert_eq!(file, "/home/user/project/src/main.rs");
        assert_eq!(line, 5);
    }

    #[test]
    fn is_workspace_file_relative() {
        assert!(is_workspace_file("src/lib.rs"));
        assert!(is_workspace_file("autumn/src/error.rs"));
    }

    #[test]
    fn is_workspace_file_rejects_stdlib() {
        assert!(!is_workspace_file(
            "/rustc/abc123/library/std/src/panicking.rs"
        ));
    }

    #[test]
    fn is_workspace_file_rejects_registry() {
        assert!(!is_workspace_file(
            "/home/user/.cargo/registry/src/github.com-1/axum-0.8.0/src/lib.rs"
        ));
    }

    #[test]
    fn is_workspace_file_empty_is_false() {
        assert!(!is_workspace_file(""));
    }

    #[test]
    fn parse_backtrace_string_extracts_workspace_frames() {
        let trace = r"stack backtrace:
   0: rust_begin_unwind
             at /rustc/abc/library/std/src/panicking.rs:661:5
   1: core::panicking::panic_fmt
             at /rustc/abc/library/core/src/panicking.rs:74:14
   2: reddit_clone::routes::posts::create_post
             at examples/reddit-clone/src/routes/posts.rs:55:5
   3: axum::handler::future
             at /home/user/.cargo/registry/src/axum-0.8.0/src/lib.rs:1:1";

        let frames = parse_backtrace_string(trace, 20);
        assert!(!frames.is_empty(), "should parse at least one frame");

        let workspace_frames: Vec<_> = frames.iter().filter(|f| f.is_in_workspace).collect();
        assert!(
            !workspace_frames.is_empty(),
            "should identify workspace frame"
        );
        assert!(
            workspace_frames
                .iter()
                .any(|f| f.function.contains("reddit_clone")),
            "should include reddit_clone frame"
        );
    }

    #[test]
    fn read_source_context_returns_empty_for_missing_file() {
        let lines = read_source_context("/nonexistent/file.rs", 5);
        assert!(lines.is_empty());
    }

    #[test]
    fn read_source_context_returns_empty_for_zero_line() {
        let lines = read_source_context("src/lib.rs", 0);
        assert!(lines.is_empty());
    }

    #[test]
    fn read_source_context_highlights_correct_line() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line one").unwrap();
        writeln!(tmp, "line two").unwrap();
        writeln!(tmp, "line three").unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        let lines = read_source_context(&path, 2);
        assert!(!lines.is_empty());
        let highlighted: Vec<_> = lines.iter().filter(|l| l.is_highlighted).collect();
        assert_eq!(highlighted.len(), 1, "exactly one highlighted line");
        assert_eq!(highlighted[0].line_no, 2);
        assert_eq!(highlighted[0].content, "line two");
    }

    #[test]
    fn read_source_context_returns_window_of_lines() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=20u32 {
            writeln!(tmp, "line {i}").unwrap();
        }
        let path = tmp.path().to_str().unwrap().to_owned();

        let lines = read_source_context(&path, 10);
        assert!(!lines.is_empty());
        let line_nos: Vec<u32> = lines.iter().map(|l| l.line_no).collect();
        assert!(line_nos.contains(&10), "should include failing line");
        assert!(line_nos.contains(&5), "should include 5 lines before");
        assert!(line_nos.contains(&15), "should include 5 lines after");
    }
}
