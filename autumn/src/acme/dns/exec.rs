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
//! Autumn runs `command` with exactly three appended arguments:
//!
//! ```text
//! /usr/local/bin/acme-dns-hook present _acme-challenge.myapp.com <txt-value>
//! /usr/local/bin/acme-dns-hook cleanup _acme-challenge.myapp.com <txt-value>
//! ```
//!
//! No end-of-options `--` marker is inserted, because the action is always the
//! first appended argument and is always the literal `present` or `cleanup`:
//! `getopts` and every conventional argument parser stop at it, so the record
//! value that follows can never be read as an option however it starts. Adding
//! a marker would only shift every argument by one for the shell script that
//! most hooks actually are — `[ "$1" = present ]` would silently be false on
//! every publish. A hook that forwards `"$@"` on to another CLI should insert
//! its own `--` there.
//!
//! A zero exit status means the record was written (or removed); any other
//! status fails the order, with the hook's `stderr` carried into the message so
//! the operator sees what their own tool said. That `stderr` is read through a
//! bounded buffer and scrubbed of credentials before it is published.
//!
//! # No shell
//!
//! `command` is an **argv array**, executed directly. The record name and value
//! are separate arguments, never interpolated into a command line, so a
//! challenge value can never be read as shell syntax.

use std::process::Stdio;

use futures::future::BoxFuture;

use super::{DnsProvider, TxtRecord, sanitize_upstream};

/// How long a hook may run before it is killed and the order fails. A hook that
/// hangs must not park the renewal loop.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How much of a hook's `stderr` is kept.
///
/// Only [`UPSTREAM_EXCERPT_CHARS`](super::UPSTREAM_EXCERPT_CHARS) of it is ever
/// published, so this is generous — but it is a hard bound, because a hook stuck
/// in a `yes`-style loop would otherwise grow this process's memory without
/// limit: the 120s timeout caps how *long* a hook runs, not how much it writes.
/// Bytes past the cap are read and discarded rather than left in the pipe, so a
/// runaway hook still runs to its timeout instead of blocking on a full buffer.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// Substrings that mark an environment variable's *value* as a credential.
///
/// An `exec` hook authenticates itself from the inherited environment — that is
/// the documented way to reach a provider autumn does not ship, and it means
/// autumn cannot know the hook's credentials the way it knows its own. What it
/// can do is refuse to republish anything the environment holds under a
/// credential-shaped name. Matched case-insensitively as a substring, so
/// `HETZNER_API_TOKEN`, `AWS_SECRET_ACCESS_KEY` and `tsig_password` all match.
const SECRET_ENV_MARKERS: [&str; 7] = [
    "TOKEN",
    "SECRET",
    "KEY",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

/// Environment values shorter than this are not scrubbed: they collide with
/// ordinary words often enough to turn a diagnostic into noise, and a real API
/// credential is never this short.
const MIN_REDACTABLE_ENV_VALUE: usize = 8;

/// The `exec` [`DnsProvider`].
pub struct ExecProvider {
    command: Vec<String>,
    /// Credential values that must be scrubbed out of the hook's `stderr` before
    /// it is published. The hook inherits the process environment, so it can
    /// echo them even though autumn passes it none.
    secrets: Vec<super::SecretString>,
    /// Where [`Self::secrets`] looks for *inherited* credentials. `None` reads
    /// this process's real environment; tests inject pairs instead, because
    /// mutating the process environment is `unsafe` (this crate forbids it) and
    /// would race every other test in the binary.
    env_override: Option<Vec<(String, String)>>,
}

impl ExecProvider {
    /// Build a provider running `command` (an argv array).
    #[must_use]
    pub const fn new(command: Vec<String>) -> Self {
        Self {
            command,
            secrets: Vec::new(),
            env_override: None,
        }
    }

    /// Scrub `secrets` out of anything the hook writes to `stderr`.
    #[must_use]
    pub fn with_secrets(mut self, secrets: Vec<super::SecretString>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Read inherited credentials from `vars` instead of the process
    /// environment. Test seam only.
    #[cfg(test)]
    fn with_env(mut self, vars: Vec<(String, String)>) -> Self {
        self.env_override = Some(vars);
        self
    }

    /// Every value that must not survive into a published error message: the
    /// credentials autumn itself handed the provider, plus whatever the
    /// inherited environment holds under a credential-shaped name.
    fn secrets(&self) -> Vec<String> {
        let inherited = self.env_override.as_ref().map_or_else(
            || {
                // `vars_os` rather than `vars`: the latter PANICS if any
                // inherited key or value is not UTF-8, and this runs while
                // building the diagnostic for a failed hook — a panic there
                // takes the renewal task down and leaves the served certificate
                // to expire. Lossy conversion is also what makes the match work:
                // the hook's stderr is decoded the same way, so a non-UTF-8
                // credential still lines up with its lossy spelling there.
                let vars: Vec<(String, String)> = std::env::vars_os()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.to_string_lossy().into_owned(),
                        )
                    })
                    .collect();
                secret_env_values(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            },
            |vars| secret_env_values(vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
        );
        self.secrets
            .iter()
            .map(|s| super::SecretString::expose(s).to_owned())
            .chain(inherited)
            .collect()
    }

    async fn run(&self, action: &str, record: &TxtRecord) -> Result<(), String> {
        let (program, leading) = self
            .command
            .split_first()
            .ok_or_else(|| "[server.tls.acme.dns] exec command is empty".to_owned())?;

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(leading)
            // Exactly the three documented arguments, with no end-of-options
            // marker: the action is always first and always a bare literal, so
            // an argument parser stops there before it can mistake the value
            // that follows for an option. A `--` here would instead shift every
            // argument by one for the shell scripts most hooks are, silently
            // sending a publish down the cleanup branch.
            .arg(action)
            .arg(&record.fqdn)
            .arg(&record.value)
            .stdin(Stdio::null())
            // Nothing reads the hook's stdout, so it goes straight to /dev/null
            // rather than into a buffer this process would have to hold.
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // The hook inherits the process environment, which may hold the
            // provider's own credentials — that is how an `nsupdate`/CLI hook is
            // expected to authenticate. Autumn passes no secret of its own, and
            // scrubs credential-shaped environment values back out of `stderr`.
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "could not run the DNS-01 exec hook `{program}`: {e}. Check that \
                 [server.tls.acme.dns] command names an executable file this process can run"
            )
        })?;
        // Taken before `wait`, so draining the pipe and reaping the child can
        // borrow the child disjointly and run concurrently. Without the
        // concurrent drain a chatty hook deadlocks on a full pipe buffer.
        let mut pipe = child
            .stderr
            .take()
            .ok_or_else(|| format!("the DNS-01 exec hook `{program}` had no stderr pipe"))?;

        let finished = tokio::time::timeout(HOOK_TIMEOUT, async {
            let mut stderr = Vec::new();
            let drain = read_bounded(&mut pipe, &mut stderr, MAX_STDERR_BYTES);
            let ((), status) = tokio::join!(drain, child.wait());
            (stderr, status)
        })
        .await;

        // On timeout the async block is dropped, releasing its borrow of
        // `child`; `child` itself is dropped when `run` returns just below, and
        // `kill_on_drop` kills the hook then.
        let (stderr, status) = finished.map_err(|_| {
            format!(
                "the DNS-01 exec hook `{program}` did not finish within {}s while running \
                 `{action}` for TXT {}; it was killed and the order failed",
                HOOK_TIMEOUT.as_secs(),
                record.fqdn
            )
        })?;
        let status = status
            .map_err(|e| format!("could not wait for the DNS-01 exec hook `{program}`: {e}"))?;

        if status.success() {
            return Ok(());
        }
        let secrets = self.secrets();
        let borrowed: Vec<&str> = secrets.iter().map(String::as_str).collect();
        Err(format!(
            "the DNS-01 exec hook `{program}` failed `{action}` for TXT {} ({}){}",
            record.fqdn,
            describe_status(status),
            stderr_excerpt(&stderr, &borrowed)
        ))
    }
}

/// Read `pipe` to EOF, keeping at most `cap` bytes in `sink`.
///
/// Everything past `cap` is read and dropped rather than left unread: a hook
/// that never stops writing must not be able to grow this process's memory, but
/// it also must not block on a full pipe buffer, which would turn a runaway hook
/// into a silent 120-second stall instead of the error it deserves.
async fn read_bounded<R>(pipe: &mut R, sink: &mut Vec<u8>, cap: usize)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut chunk = [0_u8; 4096];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let room = cap.saturating_sub(sink.len());
                if room > 0 {
                    sink.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
}

/// The values in `vars` held under a credential-shaped name.
///
/// An `exec` hook authenticates itself from the inherited environment, so
/// autumn cannot know its credentials by name the way it knows its own. This is
/// the next best thing: anything held under a name matching
/// [`SECRET_ENV_MARKERS`] is treated as a secret and scrubbed out of the hook's
/// `stderr` before that `stderr` reaches a log, an alert, or
/// `/actuator/health`. Read per invocation rather than cached, so a credential
/// rotated into the environment is covered from the next order onward.
///
/// This is a backstop, not a guarantee: a credential in a variable named
/// something like `MY_THING` is not recognisable as one. A hook that must not
/// risk it should not trace its own credentials to `stderr` — which the guide
/// says.
fn secret_env_values<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    vars.into_iter()
        .filter(|(name, value)| {
            value.len() >= MIN_REDACTABLE_ENV_VALUE && {
                let name = name.to_ascii_uppercase();
                SECRET_ENV_MARKERS.iter().any(|m| name.contains(m))
            }
        })
        .map(|(_, value)| value.to_owned())
        .collect()
}

/// Describe a process exit status, including a signal death (which has no code).
fn describe_status(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by a signal".to_owned(),
        |c| format!("exit status {c}"),
    )
}

/// The hook's `stderr`, made safe to publish in an issuance error.
///
/// The hook inherits this process's environment, so a script written with
/// `set -x` traces its own `curl -H "Authorization: Bearer $TOKEN"` — and this
/// message is published on the unauthenticated `/actuator/health` and pushed to
/// the operator's alert destination. `secrets` therefore carries every DNS
/// credential value still live in this process, and [`sanitize_upstream`] also
/// bounds the excerpt and strips the control characters a hook could otherwise
/// use to forge log lines or repaint terminal output.
fn stderr_excerpt(stderr: &[u8], secrets: &[&str]) -> String {
    let safe = sanitize_upstream(&String::from_utf8_lossy(stderr), secrets);
    if safe.is_empty() {
        return String::new();
    }
    format!(": {safe}")
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

    /// Regression (#1620): the hook's argv must be exactly the three documented
    /// arguments, with `$1` the action.
    ///
    /// An earlier revision inserted a `--` end-of-options marker first. That
    /// broke the contract the guide advertises and the five-line `nsupdate`
    /// script it ships: `[ "$1" = present ]` is false when `$1` is `--`, so
    /// every publish silently took the *cleanup* branch and deleted a record
    /// named `present`. `nsupdate` exits 0 on that, so nothing failed — issuance
    /// just waited for a record that had never been written, until propagation
    /// timed out.
    #[tokio::test]
    async fn the_hook_receives_action_fqdn_and_value_as_separate_arguments() {
        // `sh -c '...' hook` shifts the appended args into $1/$2/$3, so the hook
        // can assert on exactly what autumn passes.
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            r#"test "$1" = present && test "$2" = _acme-challenge.myapp.com && test "$3" = challenge-value && test "$#" = 3"#
                .to_owned(),
            "hook".to_owned(),
        ]);
        provider
            .upsert_txt(&record())
            .await
            .expect("the hook must see exactly present, the fqdn, and the value");

        // …and `cleanup` for the removal.
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            r#"test "$1" = cleanup && test "$#" = 3"#.to_owned(),
            "hook".to_owned(),
        ]);
        provider
            .delete_txt(&record())
            .await
            .expect("cleanup action");
    }

    /// The exact script the TLS guide ships must work when copied verbatim.
    ///
    /// It is the one piece of this feature an operator runs unmodified, so it is
    /// pinned here rather than left to review: the assertion is that the
    /// documented `$1 = present|cleanup, $2 = fqdn, $3 = value` contract holds.
    /// An earlier revision inserted a `--` first, which made `[ "$1" = present ]`
    /// false on every publish — the script then built a *delete* for a name
    /// called `present`, `nsupdate` exited 0, and issuance waited for a record
    /// that had never been written.
    #[tokio::test]
    async fn the_documented_hook_script_branches_correctly() {
        // The guide's script, with `nsupdate` replaced by a branch assertion.
        fn documented_hook(expect: &str) -> ExecProvider {
            let script = format!(
                r#"
[ "$1" = present ] && ACTION="add $2. 60 TXT \"$3\"" || ACTION="delete $2. TXT \"$3\""
case "$ACTION" in
  '{expect}'*) exit 0 ;;
  *) echo "took the wrong branch: $ACTION" >&2; exit 1 ;;
esac
"#
            );
            ExecProvider::new(vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script,
                "hook".to_owned(),
            ])
        }

        documented_hook("add _acme-challenge.myapp.com. 60 TXT")
            .upsert_txt(&record())
            .await
            .expect("the documented script must take the `add` branch on present");

        documented_hook("delete _acme-challenge.myapp.com. TXT")
            .delete_txt(&record())
            .await
            .expect("the documented script must take the `delete` branch on cleanup");
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
        assert_eq!(stderr_excerpt(b"   ", &[]), "");
        assert_eq!(stderr_excerpt(b" boom ", &[]), ": boom");
        let long = vec![b'x'; super::super::UPSTREAM_EXCERPT_CHARS * 2];
        let excerpt = stderr_excerpt(&long, &[]);
        assert!(excerpt.ends_with('…'));
        assert!(excerpt.chars().count() <= super::super::UPSTREAM_EXCERPT_CHARS + 3);
    }

    // A hook written with `set -x` traces its own credentials to stderr, and
    // that stderr is published on `/actuator/health` and to the operator's alert
    // destination. It must not carry the token through.
    #[tokio::test]
    async fn a_hook_cannot_leak_a_traced_credential_through_its_stderr() {
        const TOKEN: &str = "cf-live-token-DO-NOT-LEAK-9f3a";
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("echo '+ curl -H \"Authorization: Bearer {TOKEN}\"' >&2; exit 1"),
            "hook".to_owned(),
        ])
        .with_secrets(vec![super::super::SecretString::new(TOKEN)]);

        let err = provider
            .upsert_txt(&record())
            .await
            .expect_err("a failing hook fails the order");
        assert!(!err.contains(TOKEN), "the hook's credential leaked: {err}");
        assert!(err.contains("<redacted>"), "got: {err}");
        // The operator still sees that the hook failed and roughly why.
        assert!(err.contains("exit status 1"), "got: {err}");
    }

    // The TXT value is base64url, whose alphabet includes `-`, so ~1 in 64
    // challenge values starts with one. It still arrives as `$3`: the action
    // precedes it and is not an option, so any conventional parser has already
    // stopped by then — which is why no `--` marker is needed (or wanted, since
    // one would shift the whole documented contract by an argument).
    #[tokio::test]
    async fn a_value_starting_with_a_dash_still_reaches_the_hook_as_a_value() {
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            r#"test "$1" = present && test "$3" = -A-leading-dash-value"#.to_owned(),
            "hook".to_owned(),
        ]);
        let dashed = TxtRecord::new("myapp.com", "-A-leading-dash-value");
        provider
            .upsert_txt(&dashed)
            .await
            .expect("a leading-dash value reaches the hook as the third argument");
    }

    /// Regression (#1620): a hook that authenticates from an *inherited*
    /// environment variable — the documented way to reach a provider autumn does
    /// not ship — must not be able to republish that value through its stderr.
    ///
    /// Autumn never sees this credential: it is not in the credentials store and
    /// was never passed to `with_secrets`. It is recognised by the shape of its
    /// variable name alone.
    #[tokio::test]
    async fn an_inherited_provider_credential_is_scrubbed_from_stderr() {
        const TOKEN: &str = "hetzner-live-token-DO-NOT-LEAK-4c1d";

        // A `set -x`-style hook tracing its own authenticated request. The token
        // is written by the hook itself; autumn only ever learns it from the
        // environment listing injected below.
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(r#"echo '+ curl -H "Authorization: Bearer {TOKEN}"' >&2; exit 7"#),
            "hook".to_owned(),
        ])
        .with_env(vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HETZNER_API_TOKEN".to_owned(), TOKEN.to_owned()),
        ]);

        let err = provider
            .upsert_txt(&record())
            .await
            .expect_err("a failing hook fails the order");

        assert!(
            !err.contains(TOKEN),
            "a credential autumn never handled leaked through the hook's stderr: {err}"
        );
        assert!(err.contains("<redacted>"), "got: {err}");
        assert!(err.contains("exit status 7"), "got: {err}");
    }

    /// An ordinary environment variable is not mistaken for a credential — the
    /// scrubber must not turn every diagnostic into `<redacted>` soup.
    #[test]
    fn only_credential_shaped_environment_names_are_scrubbed() {
        let secrets = secret_env_values([
            ("PLAIN_VALUE", "an-ordinary-configuration-value"),
            ("HETZNER_API_TOKEN", "a-credential-shaped-value"),
            ("AWS_SECRET_ACCESS_KEY", "another-credential-value"),
            ("tsig_password", "lowercase-names-match-too"),
            // Below the length floor: too short to scrub without eating words.
            ("SHORT_KEY", "short"),
            // The classic false positive: a path, not a credential.
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ]);

        for expected in [
            "a-credential-shaped-value",
            "another-credential-value",
            "lowercase-names-match-too",
        ] {
            assert!(
                secrets.iter().any(|s| s == expected),
                "a credential-shaped name must be scrubbed: {expected}"
            );
        }
        assert!(
            !secrets
                .iter()
                .any(|s| s == "an-ordinary-configuration-value"),
            "an ordinary variable must not be scrubbed"
        );
        assert!(
            !secrets.iter().any(|s| s == "short"),
            "a value below the length floor must not be scrubbed"
        );
    }

    /// Regression (#1620): a hook that never stops writing must not be able to
    /// grow this process's memory. The 120s timeout bounds how *long* a hook
    /// runs, not how much it writes, so `stderr` is read through a fixed buffer
    /// and the overflow discarded as it arrives.
    #[tokio::test]
    async fn a_runaway_hook_cannot_grow_this_process() {
        // Writes far more than the cap, then fails so the excerpt path runs.
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "i=0; while [ $i -lt {} ]; do echo 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' >&2; \
                 i=$((i+1)); done; exit 4",
                MAX_STDERR_BYTES / 8
            ),
            "hook".to_owned(),
        ]);
        let err = provider
            .upsert_txt(&record())
            .await
            .expect_err("a failing hook fails the order");

        // The hook ran to completion (it was not blocked on a full pipe)…
        assert!(err.contains("exit status 4"), "got: {err}");
        // …and the published message is still bounded by the excerpt limit,
        // nowhere near what the hook actually wrote.
        assert!(
            err.chars().count() <= super::super::UPSTREAM_EXCERPT_CHARS + 200,
            "the published error grew with the hook's output: {} chars",
            err.chars().count()
        );
    }

    /// A hook writing a lot of stdout is never buffered at all: stdout goes to
    /// `/dev/null`, so it cannot pressure this process either.
    #[tokio::test]
    async fn hook_stdout_is_discarded_rather_than_buffered() {
        let provider = ExecProvider::new(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "i=0; while [ $i -lt 20000 ]; do echo 'noise on stdout'; i=$((i+1)); done".to_owned(),
            "hook".to_owned(),
        ]);
        provider
            .upsert_txt(&record())
            .await
            .expect("a chatty-but-successful hook still publishes the record");
    }
}
