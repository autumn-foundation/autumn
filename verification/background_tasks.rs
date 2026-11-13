use vstd::prelude::*;

verus! {

// Background task placement (issue #3545, ADR 0039).
//
// The runtime is `background::place` in `autumn/src/background.rs`. A task
// names a capability: a built-in one, or a plugin capability with a built-in
// parent. The role gives two values for that built-in capability:
//
// - `ceiling`: the base runs it.
// - `resolved`: the role runs it, after the preset overrides.
//
// `role_presets.rs` proves `resolved ==> ceiling`. The proofs here show:
//
// - With no explicit switch, a task runs only where the base runs its
//   capability.
// - An explicit switch wins: `Some(false)` never runs, `Some(true)` always
//   runs.
// - With no override and no queues, a task follows its capability.
// - A valid plugin override gets its value.
// - A task with queues runs only where the role runs job workers and its pin
//   admits one of the queues.

/// The shadow of `BackgroundTask` and the role facts that placement reads.
pub struct Task {
    /// The capability is a `PluginCapability`.
    pub plugin: bool,
    /// The `[roles.<name>] capabilities` value for a plugin capability.
    pub override_value: Option<bool>,
    /// The `BackgroundTask::explicit` switch.
    pub explicit: Option<bool>,
    /// The task declares queues.
    pub has_queues: bool,
    /// The role runs job workers.
    pub job_workers: bool,
    /// The role pin admits one of the task queues. An empty pin admits all.
    pub pin_admits: bool,
}

/// Whether the role drains one of the task queues.
pub open spec fn drains(t: Task) -> bool {
    !t.has_queues || (t.job_workers && t.pin_admits)
}

/// Whether the role runs the task capability.
pub open spec fn capability_on(t: Task, ceiling: bool, resolved: bool) -> bool {
    if t.plugin {
        match t.override_value {
            Some(v) => v && ceiling,
            None => resolved,
        }
    } else {
        resolved
    }
}

/// The boot check: a plugin override can only turn a capability on where the
/// base runs its parent.
pub open spec fn valid(t: Task, ceiling: bool) -> bool {
    !(t.plugin && t.override_value == Some(true) && !ceiling)
}

/// `background::place`: whether this process starts the task.
pub open spec fn places(t: Task, ceiling: bool, resolved: bool) -> bool {
    match t.explicit {
        Some(v) => v,
        None => capability_on(t, ceiling, resolved) && drains(t),
    }
}

/// The executable shadow of `background::place`.
pub fn place_exec(t: &Task, ceiling: bool, resolved: bool) -> (r: bool)
    ensures
        r == places(*t, ceiling, resolved),
{
    match t.explicit {
        Some(v) => v,
        None => {
            let on = if t.plugin {
                match t.override_value {
                    Some(v) => v && ceiling,
                    None => resolved,
                }
            } else {
                resolved
            };
            on && (!t.has_queues || (t.job_workers && t.pin_admits))
        },
    }
}

/// With no explicit switch, a task runs only where the base runs its
/// capability. This holds for every override, valid or not.
pub proof fn base_is_the_ceiling(t: Task, ceiling: bool, resolved: bool)
    requires
        resolved ==> ceiling,
        t.explicit is None,
    ensures
        places(t, ceiling, resolved) ==> ceiling,
{
}

/// A task whose capability the base does not run, for example a background
/// capability on `web`, never runs there without an explicit switch.
pub proof fn web_runs_no_task(t: Task, resolved: bool)
    requires
        !resolved,
        t.explicit is None,
    ensures
        !places(t, false, resolved),
{
}

/// `explicit = Some(false)` never runs.
pub proof fn explicit_off_never_runs(t: Task, ceiling: bool, resolved: bool)
    requires
        t.explicit == Some(false),
    ensures
        !places(t, ceiling, resolved),
{
}

/// `explicit = Some(true)` always runs.
pub proof fn explicit_on_always_runs(t: Task, ceiling: bool, resolved: bool)
    requires
        t.explicit == Some(true),
    ensures
        places(t, ceiling, resolved),
{
}

/// With no override, no queues and no switch, a task follows its capability.
pub proof fn default_follows_the_capability(t: Task, ceiling: bool, resolved: bool)
    requires
        t.override_value is None,
        t.explicit is None,
        !t.has_queues,
    ensures
        places(t, ceiling, resolved) == resolved,
{
}

/// A valid plugin override gets its value.
pub proof fn valid_override_is_respected(t: Task, ceiling: bool, resolved: bool)
    requires
        valid(t, ceiling),
        t.plugin,
        t.explicit is None,
        !t.has_queues,
    ensures
        t.override_value == Some(true) ==> places(t, ceiling, resolved),
        t.override_value == Some(false) ==> !places(t, ceiling, resolved),
{
}

/// A task with queues runs only where the role runs job workers and its pin
/// admits one of the queues.
pub proof fn queues_follow_the_pin(t: Task, ceiling: bool, resolved: bool)
    requires
        t.has_queues,
        t.explicit is None,
    ensures
        places(t, ceiling, resolved) ==> t.job_workers && t.pin_admits,
{
}

fn main() {
}

} // verus!
