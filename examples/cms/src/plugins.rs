//! Actions and filters — the plugin API.
//!
//! This is WordPress's single most consequential design decision: `do_action`
//! and `apply_filters` are why a twenty-year-old plugin ecosystem exists.
//! Everything else in WordPress can be replaced; the hook system is what makes
//! replacing it possible.
//!
//! The shape here is the same — named hook points, ordered listeners, filters
//! that transform a value as it passes through — with the untyped parts made
//! typed:
//!
//! | WordPress | Here |
//! |---|---|
//! | `add_action('init', 'cb')` | [`add_action`] with a [`Action`] variant |
//! | `do_action('init')` | [`do_action`] |
//! | `add_filter('the_content', 'cb')` | [`add_filter`] with a [`Filter`] variant |
//! | `apply_filters('the_content', $v)` | [`apply_filters`] |
//! | `$priority` (default 10) | the `priority` argument, same default |
//!
//! A hook name is an enum rather than a string, so a typo in a hook name is a
//! compile error instead of a callback that silently never runs — the single
//! most common WordPress plugin bug.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// WordPress's default hook priority.
pub const DEFAULT_PRIORITY: i32 = 10;

/// A side-effecting hook point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    /// A post was saved (created or updated). Carries the post id.
    PostSaved,
    /// A post's status changed. Carries the post id.
    PostTransitioned,
    /// A comment was submitted, before moderation. Carries the comment id.
    CommentPosted,
    /// A comment was approved. Carries the comment id.
    CommentApproved,
    /// A file was added to the media library. Carries the attachment id.
    AttachmentUploaded,
}

/// A value-transforming hook point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Filter {
    /// The rendered HTML of a post body, after Markdown and sanitization.
    /// WordPress's `the_content`.
    TheContent,
    /// A post's title, before it is escaped into a heading.
    TheTitle,
    /// The `<title>` element's full text.
    DocumentTitle,
    /// A post's excerpt.
    TheExcerpt,
}

/// `Arc` rather than `Box` so a dispatch can *clone the handles it needs and
/// drop the lock* before running any plugin code. See `do_action`.
type ActionFn = std::sync::Arc<dyn Fn(i64) + Send + Sync>;
type FilterFn = std::sync::Arc<dyn Fn(String) -> String + Send + Sync>;

/// Registered listeners, ordered by `(priority, registration order)` — the
/// `BTreeMap` key — so two listeners at the same priority run in the order they
/// were added, exactly as WordPress's hook array does.
struct Registry {
    actions: BTreeMap<(Action, i32, usize), ActionFn>,
    filters: BTreeMap<(Filter, i32, usize), FilterFn>,
    next_seq: usize,
}

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        RwLock::new(Registry {
            actions: BTreeMap::new(),
            filters: BTreeMap::new(),
            next_seq: 0,
        })
    })
}

/// Register a listener for `action`. Lower priorities run first.
pub fn add_action(action: Action, priority: i32, listener: impl Fn(i64) + Send + Sync + 'static) {
    let mut registry = registry().write().expect("hook registry poisoned");
    let seq = registry.next_seq;
    registry.next_seq += 1;
    registry
        .actions
        .insert((action, priority, seq), std::sync::Arc::new(listener));
}

/// Register a transformer for `filter`. Lower priorities run first, and each
/// receives the previous one's output.
pub fn add_filter(
    filter: Filter,
    priority: i32,
    transform: impl Fn(String) -> String + Send + Sync + 'static,
) {
    let mut registry = registry().write().expect("hook registry poisoned");
    let seq = registry.next_seq;
    registry.next_seq += 1;
    registry
        .filters
        .insert((filter, priority, seq), std::sync::Arc::new(transform));
}

/// Fire every listener registered for `action`.
///
/// Listeners are synchronous and infallible by design: a hook that can fail or
/// block is a hook that can take down the request that fired it, and WordPress's
/// habit of letting a plugin's fatal error white-screen the site is the thing
/// worth *not* reproducing. Work that can fail belongs in a `#[job]`, which a
/// listener can enqueue.
pub fn do_action(action: Action, subject_id: i64) {
    // The matching handles are cloned and the guard dropped *before* any
    // listener runs. Holding the read lock across the call deadlocks the
    // request the moment a listener registers another hook — `add_action` wants
    // the write lock, `RwLock` is not reentrant, and the thread waits on itself
    // forever. Registering from inside a hook is a normal thing for a plugin to
    // do, so the dispatch has to survive it.
    //
    // The snapshot is also the right semantics: a listener added *during* this
    // dispatch belongs to the next one, not to a set already being iterated.
    let listeners: Vec<ActionFn> = {
        let registry = registry().read().expect("hook registry poisoned");
        registry
            .actions
            .iter()
            .filter(|((registered, _, _), _)| *registered == action)
            .map(|(_, listener)| std::sync::Arc::clone(listener))
            .collect()
    };
    for listener in listeners {
        listener(subject_id);
    }
}

/// Pass `value` through every transformer registered for `filter`, in priority
/// order, and return the result.
#[must_use]
pub fn apply_filters(filter: Filter, value: String) -> String {
    // Snapshot-then-release, for the same reason as `do_action`.
    let transforms: Vec<FilterFn> = {
        let registry = registry().read().expect("hook registry poisoned");
        registry
            .filters
            .iter()
            .filter(|((registered, _, _), _)| *registered == filter)
            .map(|(_, transform)| std::sync::Arc::clone(transform))
            .collect()
    };
    let mut value = value;
    for transform in transforms {
        value = transform(value);
    }
    value
}

/// How many listeners are registered for `action` — the equivalent of
/// WordPress's `has_action`, and what the admin's plugin screen counts.
#[must_use]
pub fn action_listener_count(action: Action) -> usize {
    registry()
        .read()
        .expect("hook registry poisoned")
        .actions
        .keys()
        .filter(|(registered, _, _)| *registered == action)
        .count()
}

/// How many transformers are registered for `filter`.
#[must_use]
pub fn filter_listener_count(filter: Filter) -> usize {
    registry()
        .read()
        .expect("hook registry poisoned")
        .filters
        .keys()
        .filter(|(registered, _, _)| *registered == filter)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[test]
    fn filters_run_in_priority_order_and_compose() {
        // `TheExcerpt` is used only here, so this test owns the process-wide
        // registry entry for it and cannot race the other tests.
        add_filter(Filter::TheExcerpt, 20, |v| format!("{v} second"));
        add_filter(Filter::TheExcerpt, 5, |v| format!("{v} first"));
        assert_eq!(
            apply_filters(Filter::TheExcerpt, "start".to_owned()),
            "start first second",
            "lower priority must run first, and each must see the previous output"
        );
    }

    #[test]
    fn actions_fire_every_listener() {
        static SEEN: AtomicI64 = AtomicI64::new(0);
        add_action(Action::AttachmentUploaded, DEFAULT_PRIORITY, |id| {
            SEEN.fetch_add(id, Ordering::SeqCst);
        });
        add_action(Action::AttachmentUploaded, DEFAULT_PRIORITY, |id| {
            SEEN.fetch_add(id * 10, Ordering::SeqCst);
        });
        do_action(Action::AttachmentUploaded, 3);
        assert_eq!(SEEN.load(Ordering::SeqCst), 33);
        assert_eq!(action_listener_count(Action::AttachmentUploaded), 2);
    }

    #[test]
    fn an_unhooked_filter_returns_its_input_unchanged() {
        assert_eq!(
            apply_filters(Filter::DocumentTitle, "unchanged".to_owned()),
            "unchanged"
        );
    }
}
