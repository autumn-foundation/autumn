//! Compile-time per-route database query budgets (#1667).
//!
//! The work of this feature happens in the [`query_budget`] attribute macro,
//! which walks a handler's AST at build time and fails the build when a
//! statically reachable path can exceed the declared query count. What lives
//! here is the small, zero-cost artefact that expansion leaves behind: a
//! [`StaticQueryBudget`] constant recording what the handler declared and what
//! the analysis proved.
//!
//! The constant carries no runtime behaviour — nothing checks it during a
//! request; that job belongs to timeouts, load-shedding, and the runtime
//! query-count assertions in [`crate::test`]. It exists so tooling and tests
//! can read the proof back:
//!
//! ```ignore
//! use autumn_web::{get, query_budget};
//!
//! #[get("/posts")]
//! #[query_budget(2)]
//! async fn index(repo: PgPostRepository) -> AutumnResult<Markup> { /* … */ }
//!
//! // The expansion emits `__AUTUMN_QUERY_BUDGET_index`:
//! assert_eq!(__AUTUMN_QUERY_BUDGET_index.declared, Some(2));
//! assert_eq!(__AUTUMN_QUERY_BUDGET_index.proven_max, Some(2));
//! ```
//!
//! See `docs/guide/query-budgets.md` for the user-facing guide.
//!
//! [`query_budget`]: macro@crate::query_budget

/// The compile-time query budget proved for one handler.
///
/// Emitted by [`query_budget`](macro@crate::query_budget) as a `const` named
/// `__AUTUMN_QUERY_BUDGET_{handler}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticQueryBudget {
    /// The handler function's name.
    pub handler: &'static str,
    /// The declared ceiling — `None` for `#[query_budget(unbounded, …)]`.
    pub declared: Option<u32>,
    /// The upper bound the analysis proved, or `None` when a reachable path
    /// could not be bounded (only possible under an `unbounded` declaration,
    /// since a bounded one fails the build instead).
    pub proven_max: Option<u32>,
}

impl StaticQueryBudget {
    /// Construct a budget record. Called by the macro expansion.
    #[must_use]
    pub const fn new(
        handler: &'static str,
        declared: Option<u32>,
        proven_max: Option<u32>,
    ) -> Self {
        Self {
            handler,
            declared,
            proven_max,
        }
    }

    /// Whether the handler opted out of a finite budget.
    #[must_use]
    pub const fn is_unbounded(&self) -> bool {
        self.declared.is_none()
    }

    /// How much of the declared budget the handler leaves unused, when both a
    /// ceiling and a proof exist.
    #[must_use]
    pub const fn headroom(&self) -> Option<u32> {
        match (self.declared, self.proven_max) {
            (Some(declared), Some(proven)) => Some(declared.saturating_sub(proven)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StaticQueryBudget;

    #[test]
    fn headroom_is_the_unused_slack() {
        let budget = StaticQueryBudget::new("index", Some(5), Some(2));
        assert_eq!(budget.headroom(), Some(3));
        assert!(!budget.is_unbounded());
    }

    #[test]
    fn a_handler_at_its_ceiling_has_no_headroom() {
        let budget = StaticQueryBudget::new("index", Some(2), Some(2));
        assert_eq!(budget.headroom(), Some(0));
    }

    #[test]
    fn an_unbounded_handler_reports_no_ceiling_and_no_headroom() {
        let budget = StaticQueryBudget::new("backfill", None, None);
        assert!(budget.is_unbounded());
        assert_eq!(budget.headroom(), None);
    }
}
