//! Small `syn` helpers that more than one macro crate needs.

/// Splits a `match` arm's pattern from its `if` guard.
///
/// Since syn 3 a guard is no longer a field of [`syn::Arm`]; it is carried
/// as a top-level [`syn::Pat::Guard`] wrapping the arm's real pattern. Every
/// walker that binds names from the pattern and then evaluates the guard
/// needs the two apart, so this is the one place that knows the shape.
#[must_use]
pub fn arm_pat_and_guard(arm: &syn::Arm) -> (&syn::Pat, Option<&syn::Expr>) {
    match &arm.pat {
        syn::Pat::Guard(g) => (&g.pat, Some(&g.guard)),
        pat => (pat, None),
    }
}
