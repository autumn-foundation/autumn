// Stand-ins for the in-place upgrade hooks. Built when `upgrade` is off.

/// No upgrade can hand over a socket, so no process is a successor.
pub(crate) const fn handoff_requested() -> bool {
    false
}
