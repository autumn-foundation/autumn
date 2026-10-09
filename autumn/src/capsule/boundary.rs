//! Work that leaves the recorded seams (#2351 items 1 and 3).
//!
//! Two kinds of work can escape a capsule:
//!
//! * **Egress outside the outbound-HTTP seam.** A subsystem with its own
//!   network client (CAPTCHA, `OAuth2`, an S3 store) does not go through
//!   [`http_client`](crate::http_client). [`guard_egress`] refuses that call
//!   during a replay, and marks the capsule incomplete during capture.
//! * **Detached tasks.** The capture scope and the replay tape are task-locals,
//!   and `tokio::spawn` does not copy them. [`spawn`] marks the capsule
//!   incomplete during capture, and copies the tape during a replay, so the
//!   task's seams are served or refused rather than reaching live services.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate".
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set while this process replays a capsule. Then [`guard_egress`] refuses
/// every call, also on a task that has no tape. It is never unset.
static EGRESS_BLOCKED: AtomicBool = AtomicBool::new(false);

/// Refuse all egress from now on. `autumn replay` calls it before it
/// builds the app.
pub(crate) fn block_egress_for_replay() {
    EGRESS_BLOCKED.store(true, Ordering::SeqCst);
}

/// Egress that a replay refused because the capsule cannot answer it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{subsystem} {method} {target} was blocked during replay: the call is not recorded in the capsule"
)]
pub struct UnrecordedEgress {
    subsystem: &'static str,
    method: String,
    /// The URL without its query string.
    target: String,
}

impl UnrecordedEgress {
    /// The subsystem that made the call.
    #[must_use]
    pub const fn subsystem(&self) -> &'static str {
        self.subsystem
    }
}

/// Check egress that does not go through the outbound-HTTP seam.
///
/// Call it immediately before a subsystem with its own network client sends a
/// request.
///
/// * During a replay it returns an error, and logs an unrecorded-effect
///   divergence when a replay tape serves this task.
/// * During capture it notes the call and marks the capsule incomplete, then
///   returns `Ok`. Replay refuses an incomplete capsule.
/// * Otherwise it returns `Ok`.
///
/// # Errors
///
/// [`UnrecordedEgress`] during a replay.
pub fn guard_egress(
    subsystem: &'static str,
    method: &str,
    url: &str,
) -> Result<(), UnrecordedEgress> {
    check_egress(
        subsystem,
        method,
        url,
        EGRESS_BLOCKED.load(Ordering::SeqCst),
    )
}

/// [`guard_egress`], with the process-wide replay block given.
fn check_egress(
    subsystem: &'static str,
    method: &str,
    url: &str,
    blocked: bool,
) -> Result<(), UnrecordedEgress> {
    // The error and the divergence are printed, so the target keeps no query
    // string and no user info: either can hold a credential.
    let target = printable_target(url);
    let refuse = || UnrecordedEgress {
        subsystem,
        method: method.to_owned(),
        target: target.clone(),
    };
    if let Some(tape) = crate::capsule::effects::current_tape() {
        tape.refuse_unrecorded_egress(subsystem, method, &target);
        return Err(refuse());
    }
    // `autumn replay` blocks the whole process, including a task that carries
    // no tape.
    if blocked {
        return Err(refuse());
    }
    if let Some(scope) = crate::capsule::current_scope() {
        // No URL in the note: notes are persisted, and a URL can hold a token.
        scope.note(format!(
            "{subsystem} made an outbound call outside the recorded HTTP seam; the capsule \
             cannot replay it"
        ));
        scope.mark_truncated();
    }
    Ok(())
}

/// `url` with no query string, fragment or user info.
fn printable_target(url: &str) -> String {
    let head = url.split(['?', '#']).next().unwrap_or_default();
    match head.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at_checked(authority_end).unwrap_or((rest, ""));
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            format!("{scheme}://{host}{path}")
        }
        None => head.to_owned(),
    }
}

/// Spawn `future` on the Tokio runtime, and keep the capsule honest about it.
///
/// Use this in place of `tokio::spawn` for work a request starts but does not
/// await.
///
/// * During capture, the capsule is noted and marked incomplete: the task's
///   effects are not on the tape.
/// * During a replay, the task gets the replay tape, so its effects are
///   served from the capsule or refused, and never reach live services.
///
/// # Panics
///
/// Panics when called outside a Tokio runtime, as `tokio::spawn` does.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    note_detached_work();
    tokio::spawn(carry_tape(future))
}

/// Why a capsule with detached work is not replayable.
pub(crate) const DETACHED_WORK_NOTE: &str = "the run started work on a detached task; the \
     task's effects are not on the tape";

/// Mark the in-flight capsule incomplete: work it cannot see was started.
pub(crate) fn note_detached_work() {
    if let Some(scope) = crate::capsule::current_scope() {
        scope.note(DETACHED_WORK_NOTE);
        scope.mark_truncated();
    }
}

/// Marks the in-flight capsule incomplete when it drops armed.
///
/// Hold one while the caller waits for a spawned task. If the caller is
/// dropped first, the task continues without it.
pub(crate) struct DetachGuard(Option<std::sync::Arc<crate::capsule::CaptureScope>>);

impl DetachGuard {
    /// Arm the guard for this task's capture scope.
    pub(crate) fn arm() -> Self {
        Self(crate::capsule::current_scope())
    }

    /// The wait ended: the task is not detached.
    pub(crate) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for DetachGuard {
    fn drop(&mut self) {
        if let Some(scope) = self.0.take() {
            scope.note(DETACHED_WORK_NOTE);
            scope.mark_truncated();
        }
    }
}

/// Give `future` this task's capture scope and replay tape.
///
/// Only for work the calling task awaits at once: its effects then land in
/// the capsule in order, as inline work would.
pub(crate) fn carry_scopes<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let scope = crate::capsule::current_scope();
    let future = carry_tape(future);
    async move {
        match scope {
            Some(scope) => {
                crate::capsule::capture::CAPSULE_SCOPE
                    .scope(scope, future)
                    .await
            }
            None => future.await,
        }
    }
}

/// Give `future` this task's replay tape, when one serves it.
///
/// Read on the calling task, before a spawn: a task-local does not cross
/// `tokio::spawn`.
pub(crate) fn carry_tape<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let tape = crate::capsule::effects::current_tape();
    async move {
        match tape {
            Some(tape) => crate::capsule::effects::with_effect_tape(tape, future).await,
            None => future.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::capsule::capture::{CaptureScope, CaptureSettings, with_capture_scope};
    use crate::capsule::effects::{
        EffectDivergenceKind, EffectSeam, ReplayEffects, current_tape, with_effect_tape,
    };
    use crate::capsule::schema::CapsuleEffects;

    fn scope() -> Arc<CaptureScope> {
        Arc::new(CaptureScope::new(
            "boundary".to_owned(),
            Arc::new(CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ))
    }

    /// Codex review on #3222: the process-wide replay block refuses a call
    /// with no tape, also in a build without the HTTP client.
    #[test]
    fn the_replay_block_refuses_egress_with_no_tape() {
        let blocked = check_egress("s3", "PUT", "https://bucket.example/key", true);
        assert_eq!(blocked.map_err(|error| error.subsystem()), Err("s3"));
        assert!(check_egress("s3", "PUT", "https://bucket.example/key", false).is_ok());
    }

    #[tokio::test]
    async fn egress_under_a_replay_tape_is_refused_and_logged() {
        let tape = Arc::new(ReplayEffects::new(CapsuleEffects::default()));
        let result = with_effect_tape(Arc::clone(&tape), async {
            guard_egress(
                "captcha",
                "POST",
                "https://hcaptcha.example/verify?secret=s",
            )
        })
        .await;
        let error = result.expect_err("a replay refuses unrecorded egress");
        assert_eq!(error.subsystem(), "captcha");
        assert!(!error.to_string().contains("secret=s"), "{error}");
        let divergences = tape.divergences();
        assert_eq!(divergences.len(), 1, "{divergences:?}");
        assert_eq!(divergences[0].seam, EffectSeam::Http);
        assert_eq!(divergences[0].kind, EffectDivergenceKind::Unrecorded);
    }

    #[test]
    fn a_printed_target_keeps_no_credential() {
        assert_eq!(
            printable_target("https://user:pw@api.example/v1/x?token=t#f"),
            "https://api.example/v1/x"
        );
        assert_eq!(printable_target("mediamtx api"), "mediamtx api");
    }

    #[tokio::test]
    async fn egress_under_capture_marks_the_capsule_incomplete() {
        let scope = scope();
        let result = with_capture_scope(Arc::clone(&scope), async {
            guard_egress("oauth2", "POST", "https://idp.example/token")
        })
        .await;
        assert!(result.is_ok());
        assert!(scope.is_truncated());
        assert!(
            scope.notes().iter().any(|note| note.contains("oauth2")),
            "{:?}",
            scope.notes()
        );
    }

    #[tokio::test]
    async fn egress_outside_any_scope_is_open() {
        assert!(guard_egress("captcha", "POST", "https://hcaptcha.example").is_ok());
    }

    /// Every framework subsystem with its own network client calls the
    /// guard before it sends.
    #[test]
    fn subsystems_with_their_own_client_call_the_guard() {
        for (name, source) in [
            (
                "security/captcha.rs",
                include_str!("../security/captcha.rs"),
            ),
            ("auth.rs", include_str!("../auth.rs")),
            ("inbound_mail.rs", include_str!("../inbound_mail.rs")),
        ] {
            assert!(
                source.contains("crate::capsule::guard_egress("),
                "{name} sends outside the outbound-HTTP seam and must call guard_egress"
            );
        }
        let shadow = include_str!("../shadow/layer.rs");
        assert!(
            shadow.contains("crate::capsule::effects::tape_active()"),
            "a shadow mirror must not start during a replay"
        );
    }

    #[tokio::test]
    async fn a_spawn_under_capture_marks_the_capsule_incomplete() {
        let scope = scope();
        with_capture_scope(Arc::clone(&scope), async {
            spawn(async {}).await.expect("task runs");
        })
        .await;
        assert!(scope.is_truncated());
    }

    #[tokio::test]
    async fn a_spawn_under_a_replay_tape_carries_the_tape() {
        let tape = Arc::new(ReplayEffects::new(CapsuleEffects::default()));
        let carried = with_effect_tape(Arc::clone(&tape), async {
            spawn(async { current_tape().is_some() })
                .await
                .expect("task runs")
        })
        .await;
        assert!(carried, "the spawned task must see the replay tape");
    }
}
