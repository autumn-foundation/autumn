//! Same-seed trace check (issue #3067).
//!
//! [`capture`] records every framework `tracing` event that a sim run logs,
//! at all levels down to `TRACE`, with the sim time of each event. Run one seed
//! twice and [`Trace::diff`] the two traces: a difference is nondeterminism in
//! the app, the framework or the harness itself. The seed sweep does this for
//! its framework scenarios.
//!
//! ```rust,ignore
//! let (_, first) = autumn_web::sim::trace::capture(scenario(Sim::from_seed(7))).await;
//! let (_, second) = autumn_web::sim::trace::capture(scenario(Sim::from_seed(7))).await;
//! assert_eq!(first.diff(&second), None);
//! ```
//!
//! The capture is per thread. It records the tasks of a current-thread sim
//! runtime, which is what `#[sim_test]` builds. It does not record events from
//! other threads, such as blocking work.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt as _};

/// The target prefix [`capture`] records: the framework's own events.
pub const FRAMEWORK_TARGET: &str = "autumn_web";

/// The events one sim run logged, one line each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    lines: Vec<String>,
}

impl Trace {
    /// The trace lines, in log order.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The number of lines.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the trace has no lines.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The first line where `self` and `other` differ, or `None` when they are
    /// equal.
    #[must_use]
    pub fn diff(&self, other: &Self) -> Option<TraceDiff> {
        let len = self.lines.len().max(other.lines.len());
        (0..len).find_map(|line| {
            let left = self.lines.get(line);
            let right = other.lines.get(line);
            (left != right).then(|| TraceDiff {
                line,
                left: left.cloned(),
                right: right.cloned(),
            })
        })
    }
}

/// The first difference between two traces.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceDiff {
    /// The line number, from 0.
    pub line: usize,
    /// The line in the first trace, or `None` when it ended first.
    pub left: Option<String>,
    /// The line in the second trace, or `None` when it ended first.
    pub right: Option<String>,
}

impl std::fmt::Display for TraceDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let show = |line: &Option<String>| line.clone().unwrap_or_else(|| "<end>".to_owned());
        write!(
            f,
            "traces differ at line {}:\n  first:  {}\n  second: {}",
            self.line,
            show(&self.left),
            show(&self.right)
        )
    }
}

/// Run `future` and record the framework events it logs (target
/// [`FRAMEWORK_TARGET`]).
///
/// Records on this thread until `future` ends. Call it inside the sim runtime.
pub async fn capture<F: std::future::Future>(future: F) -> (F::Output, Trace) {
    capture_targets(&[FRAMEWORK_TARGET], future).await
}

/// Like [`capture`], but record the events whose target starts with one of
/// `prefixes`, for example your own crate name too.
pub async fn capture_targets<F: std::future::Future>(
    prefixes: &[&str],
    future: F,
) -> (F::Output, Trace) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder {
        lines: Arc::clone(&lines),
        prefixes: prefixes.iter().map(|prefix| (*prefix).to_owned()).collect(),
        start: tokio::time::Instant::now(),
    };
    let subscriber = tracing_subscriber::registry().with(recorder);
    let previous = ALIASES.with(|aliases| aliases.replace(Some(Vec::new())));
    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);
        future.await
    };
    let mut aliases = ALIASES
        .with(|aliases| aliases.replace(previous))
        .unwrap_or_default();
    // Longest first, so a token that starts another token does not cut it.
    aliases.sort_by_key(|(token, _)| std::cmp::Reverse(token.len()));
    let mut lines = std::mem::take(&mut *lines.lock().unwrap_or_else(PoisonError::into_inner));
    for line in &mut lines {
        for (token, alias) in &aliases {
            if line.contains(token.as_str()) {
                *line = line.replace(token.as_str(), alias);
            }
        }
    }
    (output, Trace { lines })
}

thread_local! {
    /// Run-unique tokens and their stable names, while a capture runs here.
    static ALIASES: std::cell::RefCell<Option<Vec<(String, String)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Show `token` as `<kind N>` in the trace a capture on this thread records.
/// `N` counts the tokens of this kind in the capture. Use it for a value that
/// is unique per run by design, such as a database name. Outside a capture it
/// does nothing.
#[cfg(feature = "sqlite")]
pub(crate) fn alias(kind: &str, token: &str) {
    ALIASES.with(|aliases| {
        if let Some(aliases) = aliases.borrow_mut().as_mut() {
            let prefix = format!("<{kind} ");
            let n = aliases
                .iter()
                .filter(|(_, alias)| alias.starts_with(&prefix))
                .count();
            aliases.push((token.to_owned(), format!("{prefix}{n}>")));
        }
    });
}

/// The layer that turns each event into one line.
struct Recorder {
    lines: Arc<Mutex<Vec<String>>>,
    prefixes: Vec<String>,
    start: tokio::time::Instant,
}

impl Recorder {
    fn wants(&self, target: &str) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| target.starts_with(prefix))
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        self.wants(metadata.target())
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if !self.wants(metadata.target()) {
            return;
        }
        let at = tokio::time::Instant::now().saturating_duration_since(self.start);
        let mut line = format!(
            "{}.{:06}s {} {}:",
            at.as_secs(),
            at.subsec_micros(),
            metadata.level(),
            metadata.target()
        );
        event.record(&mut LineVisitor(&mut line));
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

/// Appends ` name=value` for each field.
struct LineVisitor<'a>(&'a mut String);

impl Visit for LineVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={value}", field.name());
    }
}

#[cfg(test)]
mod tests {
    use super::{Trace, capture, capture_targets};

    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_records_framework_events_with_sim_time() {
        let ((), trace) = capture(async {
            tracing::info!(target: "autumn_web::x", job = 7, "started");
            tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
            tracing::trace!(target: "autumn_web::x", "fine");
            tracing::info!(target: "elsewhere", "not ours");
        })
        .await;
        assert_eq!(
            trace.lines(),
            [
                "0.000000s INFO autumn_web::x: message=started job=7",
                "1.500000s TRACE autumn_web::x: message=fine",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_targets_adds_prefixes() {
        let ((), trace) = capture_targets(&["mine"], async {
            tracing::warn!(target: "mine::sub", "here");
            tracing::warn!(target: "autumn_web", "not asked for");
        })
        .await;
        assert_eq!(trace.len(), 1);
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test(start_paused = true)]
    async fn sim_trace_aliases_hide_run_unique_tokens() {
        let ((), trace) = capture(async {
            super::alias("db", "file:x-123");
            tracing::info!(target: "autumn_web::db", url = "file:x-123", "open");
        })
        .await;
        assert_eq!(
            trace.lines(),
            ["0.000000s INFO autumn_web::db: message=open url=<db 0>"]
        );
        super::alias("db", "outside a capture: no effect");
    }

    #[test]
    fn diff_finds_the_first_difference_and_a_short_trace() {
        let a = Trace {
            lines: vec!["x".into(), "y".into()],
        };
        let b = Trace {
            lines: vec!["x".into(), "z".into()],
        };
        let c = Trace {
            lines: vec!["x".into()],
        };
        assert_eq!(a.diff(&a), None);
        let diff = a.diff(&b).expect("differs");
        assert_eq!(diff.line, 1);
        assert!(diff.to_string().contains("first:  y"), "{diff}");
        let diff = a.diff(&c).expect("shorter");
        assert_eq!(
            (diff.left.as_deref(), diff.right.as_deref()),
            (Some("y"), None)
        );
        assert!(diff.to_string().contains("<end>"), "{diff}");
        assert!(Trace::default().is_empty());
    }
}
