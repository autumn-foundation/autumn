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
//! other threads, such as blocking work. Captures that run together on one
//! thread, for example with `tokio::join!`, each record the events of their own
//! future. A task that a capture spawns is polled outside it. Its events go to
//! the newest capture that is live.

use std::cell::RefCell;
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
    let state = Arc::new(Capture {
        lines: Mutex::new(Vec::new()),
        aliases: Mutex::new(Vec::new()),
        prefixes: prefixes.iter().map(|prefix| (*prefix).to_owned()).collect(),
        start: tokio::time::Instant::now(),
    });
    let _live = Live::new(&state);
    let mut future = Some(Box::pin(future));
    let output = std::future::poll_fn(|cx| {
        let _polling = Polling::new(&state);
        let Some(running) = future.as_mut() else {
            unreachable!("capture polled after it ended");
        };
        let output = std::task::ready!(running.as_mut().poll(cx));
        // Drop it here: events from its `Drop` belong to this capture.
        future = None;
        std::task::Poll::Ready(output)
    })
    .await;
    let mut aliases = std::mem::take(&mut *lock(&state.aliases));
    // Longest first, so a token that starts another token does not cut it.
    aliases.sort_by_key(|(token, _)| std::cmp::Reverse(token.len()));
    let mut lines = std::mem::take(&mut *lock(&state.lines));
    for line in &mut lines {
        for (token, alias) in &aliases {
            if line.contains(token.as_str()) {
                *line = line.replace(token.as_str(), alias);
            }
        }
    }
    (output, Trace { lines })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The state of one capture.
struct Capture {
    lines: Mutex<Vec<String>>,
    /// Run-unique tokens and their stable names.
    aliases: Mutex<Vec<(String, String)>>,
    prefixes: Vec<String>,
    start: tokio::time::Instant,
}

impl Capture {
    fn wants(&self, target: &str) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| target.starts_with(prefix))
    }
}

thread_local! {
    /// The captures that run on this thread, oldest first.
    static LIVE: RefCell<Vec<Arc<Capture>>> = const { RefCell::new(Vec::new()) };
    /// The capture whose future this thread polls now.
    static POLLING: RefCell<Option<Arc<Capture>>> = const { RefCell::new(None) };
    /// The one dispatcher of this thread, while a capture runs.
    static ROUTER: RefCell<Option<tracing::subscriber::DefaultGuard>> =
        const { RefCell::new(None) };
}

/// The capture that gets an event now: the one that polls, or else the newest
/// one. So the tasks a capture spawns are recorded too.
fn target_capture() -> Option<Arc<Capture>> {
    POLLING
        .with(|polling| polling.borrow().clone())
        .or_else(|| LIVE.with(|live| live.borrow().last().cloned()))
}

/// A capture that runs on this thread. The first one installs the dispatcher.
/// The drop, also on a panic or a cancel, removes it, and the last one removes
/// the dispatcher.
struct Live(Arc<Capture>);

impl Live {
    fn new(state: &Arc<Capture>) -> Self {
        let first = LIVE.with(|live| {
            let mut live = live.borrow_mut();
            live.push(Arc::clone(state));
            live.len() == 1
        });
        if first {
            let subscriber = tracing_subscriber::registry().with(Router);
            let guard = tracing::subscriber::set_default(subscriber);
            ROUTER.with(|router| *router.borrow_mut() = Some(guard));
        }
        Self(Arc::clone(state))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let empty = LIVE.with(|live| {
            let mut live = live.borrow_mut();
            live.retain(|state| !Arc::ptr_eq(state, &self.0));
            live.is_empty()
        });
        if empty {
            // Take the guard out first: its drop may run tracing code.
            let guard = ROUTER.with(|router| router.borrow_mut().take());
            drop(guard);
        }
    }
}

/// Marks a capture as the one that polls, for one poll. The drop puts the
/// previous one back.
struct Polling(Option<Arc<Capture>>);

impl Polling {
    fn new(state: &Arc<Capture>) -> Self {
        Self(POLLING.with(|polling| polling.borrow_mut().replace(Arc::clone(state))))
    }
}

impl Drop for Polling {
    fn drop(&mut self) {
        POLLING.with(|polling| *polling.borrow_mut() = self.0.take());
    }
}

/// Show `token` as `<kind N>` in the trace of the capture that runs here.
/// `N` counts the tokens of this kind in the capture. Use it for a value that
/// is unique per run by design, such as a database name. Outside a capture it
/// does nothing.
#[cfg(feature = "sqlite")]
pub(crate) fn alias(kind: &str, token: &str) {
    if let Some(capture) = target_capture() {
        let prefix = format!("<{kind} ");
        let mut aliases = lock(&capture.aliases);
        let n = aliases
            .iter()
            .filter(|(_, alias)| alias.starts_with(&prefix))
            .count();
        aliases.push((token.to_owned(), format!("{prefix}{n}>")));
        drop(aliases);
    }
}

/// The layer that turns each event into one line of the target capture.
struct Router;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Router {
    // The answer of `enabled` depends on the live captures, so it must run for
    // each event and not once for each callsite.
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        target_capture().is_some_and(|capture| capture.wants(metadata.target()))
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let Some(capture) = target_capture() else {
            return;
        };
        if !capture.wants(metadata.target()) {
            return;
        }
        let at = tokio::time::Instant::now().saturating_duration_since(capture.start);
        let mut line = format!(
            "{}.{:06}s {} {}:",
            at.as_secs(),
            at.subsec_micros(),
            metadata.level(),
            metadata.target()
        );
        event.record(&mut LineVisitor(&mut line));
        lock(&capture.lines).push(line);
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

    /// Two captures polled together on one thread each keep only their own
    /// events.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_overlapping_captures_keep_their_own_events() {
        async fn scenario(name: &'static str) {
            for step in 0..3 {
                tracing::info!(target: "autumn_web::x", name, step, "step");
                tokio::task::yield_now().await;
            }
        }
        let (((), first), ((), second)) =
            tokio::join!(capture(scenario("a")), capture(scenario("b")));
        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        assert!(
            first.lines().iter().all(|l| l.contains("name=a")),
            "{first:?}"
        );
        assert!(
            second.lines().iter().all(|l| l.contains("name=b")),
            "{second:?}"
        );
    }

    /// An event whose target no capture wants is off: its fields are not
    /// evaluated.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_skips_the_fields_of_unwanted_targets() {
        let evaluated = std::cell::Cell::new(false);
        let ((), trace) = capture_targets(&["mine"], async {
            tracing::warn!(target: "elsewhere", value = { evaluated.set(true); 1 }, "no");
            tracing::warn!(target: "mine", "yes");
        })
        .await;
        assert_eq!(trace.len(), 1);
        assert!(!evaluated.get(), "the fields of an unwanted event ran");
    }

    /// A future that logs when it drops is still recorded.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_records_the_drop_of_the_future() {
        struct Noisy(std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>);
        impl std::future::Future for Noisy {
            type Output = ();
            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                self.0.as_mut().poll(cx)
            }
        }
        impl Drop for Noisy {
            fn drop(&mut self) {
                tracing::info!(target: "autumn_web::x", "dropped");
            }
        }
        let ((), trace) = capture(Noisy(Box::pin(async {}))).await;
        assert_eq!(trace.len(), 1, "{trace:?}");
    }

    /// One callsite that a capture rejects first is still recorded by a later
    /// capture that wants it.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_filters_each_event_not_each_callsite() {
        fn emit() {
            tracing::info!(target: "autumn_web::x", "from one callsite");
        }
        let ((), outer) = capture_targets(&["other"], async {
            emit();
            let ((), inner) = capture(async { emit() }).await;
            assert_eq!(inner.len(), 1, "{inner:?}");
        })
        .await;
        assert!(outer.is_empty(), "{outer:?}");
    }

    /// Tasks the captured future spawns are recorded too.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_records_spawned_tasks() {
        let ((), trace) = capture(async {
            tokio::spawn(async { tracing::info!(target: "autumn_web::x", "in task") })
                .await
                .unwrap();
        })
        .await;
        assert_eq!(trace.len(), 1, "{trace:?}");
    }

    /// A panic in a captured future leaves no capture state on the thread.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_capture_panic_leaves_no_state_behind() {
        let result =
            futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(capture(async {
                panic!("boom")
            })))
            .await;
        assert!(result.is_err());
        let ((), trace) = capture(async {
            tracing::info!(target: "autumn_web::x", "after");
        })
        .await;
        assert_eq!(trace.len(), 1);
        tracing::info!(target: "autumn_web::x", "outside any capture");
        assert!(super::target_capture().is_none());
    }

    /// An inner capture keeps its own events, and the outer capture keeps the
    /// rest.
    #[tokio::test(start_paused = true)]
    async fn sim_trace_nested_captures_split_their_events() {
        let (inner_trace, outer) = capture(async {
            tracing::info!(target: "autumn_web::x", "outer before");
            let ((), inner) = capture(async {
                tracing::info!(target: "autumn_web::x", "inner");
            })
            .await;
            tracing::info!(target: "autumn_web::x", "outer after");
            inner
        })
        .await;
        assert_eq!(inner_trace.len(), 1);
        assert_eq!(outer.len(), 2);
    }

    /// Aliases belong to the capture that set them.
    #[cfg(feature = "sqlite")]
    #[tokio::test(start_paused = true)]
    async fn sim_trace_overlapping_captures_keep_their_own_aliases() {
        async fn scenario(token: &'static str) {
            tokio::task::yield_now().await;
            super::alias("db", token);
            tokio::task::yield_now().await;
            tracing::info!(target: "autumn_web::db", url = token, "open");
        }
        let (((), first), ((), second)) =
            tokio::join!(capture(scenario("tok-a")), capture(scenario("tok-b")));
        let want = ["0.000000s INFO autumn_web::db: message=open url=<db 0>"];
        assert_eq!(first.lines(), want);
        assert_eq!(second.lines(), want);
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
