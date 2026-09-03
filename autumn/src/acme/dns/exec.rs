//! The `exec` DNS-01 escape hatch (issue #1620).
//!
//! The curated provider set is deliberately small. Everything else reaches
//! DNS-01 through an operator-supplied hook program — RFC 2136 dynamic updates
//! via `nsupdate`, a registrar's own CLI, a shell script that calls a webhook.
//! This is the same shape cert-manager's webhook extension point takes, minus
//! the cluster.
//!
//! ```toml
//! [server.tls.acme.dns]
//! provider = "exec"
//! command = ["/usr/local/bin/acme-dns-hook"]
//! ```
//!
//! Autumn runs `command` with three appended arguments:
//!
//! ```text
//! /usr/local/bin/acme-dns-hook present _acme-challenge.myapp.com <txt-value>
//! /usr/local/bin/acme-dns-hook cleanup _acme-challenge.myapp.com <txt-value>
//! ```
//!
//! A zero exit status means the record was written (or removed); any other
//! status fails the order, with the hook's `stderr` carried into the message so
//! the operator sees what their own tool said.
//!
//! # No shell
//!
//! `command` is an **argv array**, executed directly. The record name and value
//! are separate arguments, never interpolated into a command line, so a
//! challenge value can never be read as shell syntax.

use std::process::Stdio;

use futures::future::BoxFuture;

use super::{DnsProvider, TxtRecord};

/// How long a hook may run before it is killed and the order fails. A hook that
/// hangs must not park the renewal loop.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How much of the hook's `stderr` is quoted back in an error message.
const STDERR_EXCERPT_BYTES: usize = 2000;

/// The `exec` [`DnsProvider`].
pub struct ExecProvider {
    command: Vec<String>,
}

impl ExecProvider {
    /// Build a provider running `command` (an argv array).
    #[must_use]
    pub const fn new(command: Vec<String>) -> Self {
        Self { command }
    }

    async fn run(&self, action: &str, record: &TxtRecord) -> Result<(), String> {
        let (program, leading) = self
            .command
            .split_first()
            .ok_or_else(|| "[server.tls.acme.dns] exec command is empty".to_owned())?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(leading)
            .arg(action)
            .arg(&record.fqdn)
            .arg(&record.value)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The hook inherits the process environment, which may hold the
            // provider's own credentials — that is how an `nsupdate`/CLI hook is
            // expected to authenticate. Autumn passes no secret of its own.
            .kill_on_drop(true);

        let output = tokio::time::timeout(HOOK_TIMEOUT, cmd.output())
            .await
            .map_err(|_| {
                format!(
                    "the DNS-01 exec hook `{program}` did not finish within {}s while running \
                     `{action}` for TXT {}; it was killed and the order failed",
                    HOOK_TIMEOUT.as_secs(),
                    record.fqdn
                )
            })?
            .map_err(|e| {
                format!(
                    "could not run the DNS-01 exec hook `{program}`: {e}. Check that \
                     [server.tls.acme.dns] command names an executable file this process can run"
                )
            })?;

        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "the DNS-01 exec hook `{program}` failed `{action}` for TXT {} ({}){}",
            record.fqdn,
            describe_status(output.status),
            stderr_excerpt(&output.stderr)
        ))
    }
}

/// Describe a process exit status, including a signal death (which has no code).
fn describe_status(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by a signal".to_owned(),
        |c| format!("exit status {c}"),
    )
}

/// The hook's `stderr`, trimmed and truncated for an error message.
fn stderr_excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut excerpt: String = trimmed.chars().take(STDERR_EXCERPT_BYTES).collect();
    if excerpt.len() < trimmed.len() {
        excerpt.push('…');
    }
    format!(": {excerpt}")
}

impl DnsProvider for ExecProvider {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn upsert_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.run("present", record).await })
    }

    fn delete_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.run("cleanup", record).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> TxtRecord {
        TxtRecord::new("myapp.com", "challenge-value")
    }

    #[tokio::test]
    async fn a_zero_exit_publishes_the_record() {
        // `true` ignores its arguments and exits 0 — the "hook worked" contract.
        let provider = ExecProvider::new(vec!["/usr/bin/env".to_owned(), "true".to_owned()]);
        assert!(provider.upsert_txt(&record()).await.is_ok());
        assert!(provider.delete_txt(&record()).await.is_ok());
    }

    #[tokio::test]
    async fn the_hook_receives_action_fqdn_and_value_as_separate_arguments() {
        // `sh -c '...' --` shifts the appended args into $1/$2/$3, so the hook
        // can assert on exactly what autumn passes.
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            r#"test "$1" = present && test "$2" = _acme-challenge.myapp.com && test "$3" = challenge-value"#
                .to_owned(),
            "hook".to_owned(),
        ]);
        provider
            .upsert_txt(&record())
            .await
            .expect("the hook must see present, the fqdn, and the value");

        // …and `cleanup` for the removal.
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            r#"test "$1" = cleanup"#.to_owned(),
            "hook".to_owned(),
        ]);
        provider
            .delete_txt(&record())
            .await
            .expect("cleanup action");
    }

    #[tokio::test]
    async fn a_non_zero_exit_fails_the_order_and_quotes_stderr() {
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo 'zone is not delegated' >&2; exit 3".to_owned(),
            "hook".to_owned(),
        ]);
        let err = provider
            .upsert_txt(&record())
            .await
            .expect_err("a failing hook must fail the order");
        assert!(err.contains("exit status 3"), "got: {err}");
        assert!(err.contains("zone is not delegated"), "got: {err}");
        assert!(err.contains("_acme-challenge.myapp.com"), "got: {err}");
    }

    #[tokio::test]
    async fn a_missing_hook_program_is_an_actionable_error() {
        let provider = ExecProvider::new(vec!["/nonexistent/acme-dns-hook".to_owned()]);
        let err = provider
            .upsert_txt(&record())
            .await
            .expect_err("a missing program must fail");
        assert!(err.contains("/nonexistent/acme-dns-hook"), "got: {err}");
        assert!(err.contains("command"), "got: {err}");
    }

    // The value is an argv entry, never shell syntax: a hook invoked with a
    // value containing `;` or backticks must still see it verbatim.
    #[tokio::test]
    async fn the_value_is_never_interpreted_as_shell_syntax() {
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            r#"test "$3" = '; touch /tmp/pwned`whoami`'"#.to_owned(),
            "hook".to_owned(),
        ]);
        let hostile = TxtRecord::new("myapp.com", "; touch /tmp/pwned`whoami`");
        provider
            .upsert_txt(&hostile)
            .await
            .expect("the value reaches the hook verbatim, unexpanded");
    }

    #[test]
    fn stderr_excerpt_is_bounded_and_omitted_when_empty() {
        assert_eq!(stderr_excerpt(b"   "), "");
        assert_eq!(stderr_excerpt(b" boom "), ": boom");
        let long = vec![b'x'; STDERR_EXCERPT_BYTES * 2];
        let excerpt = stderr_excerpt(&long);
        assert!(excerpt.ends_with('…'));
        assert!(excerpt.chars().count() <= STDERR_EXCERPT_BYTES + 3);
    }
}
