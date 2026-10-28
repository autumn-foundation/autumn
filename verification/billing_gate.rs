use vstd::prelude::*;

verus! {

/// The shadow of a `SubscriptionView` for one rule.
/// `accepted` is `true` when the rule accepts the view's plan.
/// `event` stands for `last_event_at`. `id` stands for the rank of the
/// runtime `String` id in byte order.
/// The name is not `View`, because vstd exports a `View` trait that `@` uses.
pub struct GateView {
    pub entitled: bool,
    pub has_plan: bool,
    pub accepted: bool,
    pub event: i64,
    pub id: u64,
}

/// A view grants the rule only when it is entitled and its plan accepts it.
pub open spec fn satisfies(v: GateView) -> bool {
    v.entitled && v.has_plan && v.accepted
}

/// `a` comes first in the gate order: later event first, then lower id.
/// The gate makes this order itself. It does not use the store row order.
pub open spec fn first(a: GateView, b: GateView) -> bool {
    a.event > b.event || (a.event == b.event && a.id <= b.id)
}

/// The selection contract over the first `bound` views.
/// `None` means that no view satisfies the rule: default deny.
/// `Some(k)` means that view `k` satisfies the rule and comes first.
pub open spec fn selected(views: Seq<GateView>, bound: int, r: Option<usize>) -> bool {
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
pub proof fn unentitled_never_satisfies(v: GateView)
    requires !v.entitled
    ensures !satisfies(v)
{}

/// The gate order is transitive, so one pass finds the first view.
pub proof fn first_is_transitive(a: GateView, b: GateView, c: GateView)
    requires first(a, b), first(b, c)
    ensures first(a, c)
{}

fn comes_first(a: &GateView, b: &GateView) -> (r: bool)
    ensures r == first(*a, *b)
{
    a.event > b.event || (a.event == b.event && a.id <= b.id)
}

/// Models the selection in `best_satisfying` in `autumn-billing/src/gate.rs`.
pub fn best_satisfying(views: &Vec<GateView>) -> (r: Option<usize>)
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
                        assert forall|j: int| 0 <= j <= i && satisfies(views@[j])
                            implies first(views@[i as int], views@[j]) by {
                            if j < i {
                                assert(first(views@[k as int], views@[j]));
                            }
                        }
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
