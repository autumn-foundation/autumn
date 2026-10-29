// No-op alert hooks. Built when the `alerts` feature is off.

use crate::AppState;

pub(crate) const fn notify_dead_lettered_job(_: &AppState, _: &str, _: &str, _: &str) {}
pub(crate) const fn notify_scheduled_task_failure(_: &AppState, _: &str, _: &str) {}
pub(crate) const fn notify_scheduled_task_recovered(_: &AppState, _: &str) {}
