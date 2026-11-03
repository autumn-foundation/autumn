//! Split a command line written as one string into its arguments.
//!
//! Lane commands are written the way a shell reads them, so a reviewer sees
//! the exact command CI runs:
//! `cargo clippy -p x --features "a,b" -- -D warnings`. Only what those
//! commands need is supported: whitespace separates words, and a double-quoted
//! run is one word with the quotes removed. Nothing is expanded.

use anyhow::{Result, bail};

/// Split `line` into words, honouring double quotes.
///
/// # Errors
///
/// Returns an error for an unterminated quote or an empty command.
pub fn split(line: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if quoted {
        bail!("unterminated double quote in command: {line}");
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        bail!("empty command");
    }
    Ok(words)
}

/// Render `argv` back as one line, quoting words that hold whitespace or a
/// comma, so `--dry-run` output can be pasted into a shell.
#[must_use]
pub fn join(argv: &[String]) -> String {
    argv.iter()
        .map(|w| {
            if w.is_empty() || w.contains(|c: char| c.is_whitespace() || c == ',') {
                format!("\"{w}\"")
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(
            split("cargo fmt --all -- --check").unwrap(),
            ["cargo", "fmt", "--all", "--", "--check"]
        );
    }

    #[test]
    fn a_quoted_run_is_one_word_without_its_quotes() {
        assert_eq!(
            split(r#"cargo test --features "a,b c" --no-run"#).unwrap(),
            ["cargo", "test", "--features", "a,b c", "--no-run"]
        );
    }

    #[test]
    fn collapses_repeated_whitespace() {
        assert_eq!(split("  a \t b  ").unwrap(), ["a", "b"]);
    }

    #[test]
    fn an_empty_quoted_word_is_kept() {
        assert_eq!(split(r#"a "" b"#).unwrap(), ["a", "", "b"]);
    }

    #[test]
    fn rejects_an_unterminated_quote() {
        let err = split(r#"cargo "oops"#).unwrap_err().to_string();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn rejects_an_empty_command() {
        assert!(split("   ").is_err());
    }

    #[test]
    fn join_round_trips_through_split() {
        let line = r#"cargo test --features "a,b" --test integration_tests --no-run"#;
        let argv = split(line).unwrap();
        assert_eq!(join(&argv), line);
        assert_eq!(split(&join(&argv)).unwrap(), argv);
    }
}
