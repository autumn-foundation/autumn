use vstd::prelude::*;

verus! {

/// The shadow of a `SubscriptionView` for one rule.
/// `accepted` is `true` when the rule accepts the view's plan.
pub struct View {
    pub entitled: bool,
    pub has_plan: bool,
    pub accepted: bool,
    pub event: i64,
    pub id: u64,
}

/// A view grants the rule only when it is entitled and its plan accepts it.
pub open spec fn satisfies(v: View) -> bool {
    v.entitled && v.has_plan && v.accepted
}

/// `a` comes first in the gate order: newer event first, then lower id.
/// This is the order `DbBillingStore::subscriptions_for_customer` uses.
pub open spec fn first(a: View, b: View) -> bool {
    a.event > b.event || (a.event == b.event && a.id <= b.id)
}

/// The selection contract over the first `bound` views.
/// `None` means that no view satisfies the rule: default deny.
/// `Some(k)` means that view `k` satisfies the rule and comes first.
pub open spec fn selected(views: Seq<View>, bound: int, r: Option<usize>) -> bool {
    match r {
        None => forall|i: int| 0 <= i < bound ==> !satisfies(views[i]),
        Some(k) => {
            &&& 0 <= k < bound
            &&& satisfies(views[k as int])
            &&& forall|i: int| 0 <= i < bound && satisfies(views[i])
                ==> first(views[k as int], views[i])
        },
    }
}

/// An unentitled view never grants a rule, whatever its plan.
pub proof fn unentitled_never_satisfies(v: View)
    requires !v.entitled
    ensures !satisfies(v)
{}

/// The gate order is transitive, so one pass finds the first view.
pub proof fn first_is_transitive(a: View, b: View, c: View)
    requires first(a, b), first(b, c)
    ensures first(a, c)
{}

fn comes_first(a: &View, b: &View) -> (r: bool)
    ensures r == first(*a, *b)
{
    a.event > b.event || (a.event == b.event && a.id <= b.id)
}

/// The runtime shape of `best_satisfying` in `autumn-billing/src/gate.rs`.
pub fn best_satisfying(views: &Vec<View>) -> (r: Option<usize>)
    ensures selected(views@, views@.len() as int, r)
{
    let mut best: Option<usize> = None;
    let mut i: usize = 0;
    while i < views.len()
        invariant
            i <= views@.len(),
            selected(views@, i as int, best),
        decreases views@.len() - i,
    {
        let v = &views[i];
        if v.entitled && v.has_plan && v.accepted {
            match best {
                None => {
                    best = Some(i);
                },
                Some(k) => {
                    if !comes_first(&views[k], v) {
                        best = Some(i);
                    }
                },
            }
        }
        i = i + 1;
    }
    best
}

fn main() {}

} // verus!
