# Plan: role-aware background tasks (issue #3545)

Part of #3540. Uses the capability split (#3542) and named roles (#3543).

## Goal

A plugin declares its background work. The framework starts it only on the
roles that run its capability, stops it with the server, and shows it in
`/actuator/tasks` and `autumn graph`.

## Brainstorming

1. `AppBuilder::background_task(name, capability, |state, shutdown| ...)`.
   The closure returns `AutumnResult<()>`, as `on_startup` does.
2. `AppBuilder::background(BackgroundTask)` is the full form. It adds
   `.queues([...])` and `.explicit(Option<bool>)`.
3. `capability` is `impl Into<BackgroundCapability>`: a built-in
   `Capability`, or a `PluginCapability`.
4. `PluginCapability::new("harvest.worker", Capability::JobWorkers)` is a
   `const`. Its default is the role value of the built-in capability.
5. `AppBuilder::plugin_capability(cap)` declares a capability that no task
   uses. A task declares its own capability.
6. `[roles.<name>] capabilities = { "harvest.worker" = false }` sets a
   plugin capability on a named role.
7. A role cannot turn on a plugin capability that its base does not run, as
   for built-in keys. `true` is valid only when
   the base runs the parent capability.
8. `state.role().runs_plugin(cap)` lets a plugin gate code that is not a
   task.
9. A new built-in capability `maintenance` for plugin housekeeping loops
   (reapers, retention sweeps of a plugin). Same defaults as `retention`.
10. A task with queues runs only when the role runs job workers and drains
    one of them. The preset `pin` (or `jobs.pin`) applies, as for `#[job]`
    queues.
11. Task queues go into the `autumn jobs manifest` output, so the doctor
    coverage check includes them.
12. A pin that names only task queues does not log "pin matches no queue".
13. Each task gets a child of the server shutdown token. The drain waits for
    the tasks for at most half of the remaining `shutdown_timeout_secs`,
    before the fleet close and the `on_shutdown` hooks.
14. `/actuator/tasks` adds `background_tasks`: capability, queues, roles
    that run it, status on this process.
15. `autumn graph` adds a `background_task` node with the same facts.
    `#[track_caller]` gives the `file:line` location.
16. `explicit(Some(true))` runs the task on a role without the capability
    and logs a warning. `explicit(Some(false))` never runs it.
17. `TestApp` starts the tasks with the same placement rule, and stops them
    when the client drops.
18. The reference plugin registers one task and one plugin capability.

## Reverse brainstorming

Question: "How can this API run work on the wrong process, or lose work?"

| Way to fail | Guard |
|---|---|
| A task runs on `web`. | Placement reads the role capability. Table test over every built-in role. |
| A preset turns a plugin capability on above its base. | Boot fails, as for built-in keys. Test. |
| A typo in `capabilities = { "harvset.worker" = false }` does nothing. | Boot fails on an unknown plugin capability. Test. |
| `capabilities = { scheduler = false }` duplicates a top-level key. | Load fails. Test. |
| Two plugins declare one capability name with two parents. | Panic at registration. Test. |
| Two tasks have one name. | Panic at registration. Test. |
| A task ignores the token and blocks the drain. | The drain waits only for the budget, then aborts it and logs a warning. Test. |
| A task returns early or panics, and nobody sees it. | Status `exited`, `failed` or `panicked` in `/actuator/tasks`, and a log line. Test. |
| A pin names only a task queue and logs a false error. | The job runtime counts task queues. Test. |
| Doctor misses a task queue. | Queues go into the jobs manifest. Test. |
| The graph and the runtime disagree on the roles. | Both use one function, `roles_running`. Test. |
| An explicit switch is refused. | `explicit(Some(true))` runs with a warning. Test. |
| A one-off task (`autumn task`) starts plugin loops. | Only `run()` and `TestApp` start tasks. |

## Six thinking hats

- **White (facts).** Plugins self-gate today, or not at all. Media, billing
  and harvest start loops on every replica. `Capability` and
  `RoleCapabilities` are `#[non_exhaustive]`. `RolePreset` denies unknown
  keys. `NodeKind` is not `#[non_exhaustive]`.
- **Red (feeling).** Operators expect `AUTUMN_ROLE=web` to mean "no
  background work". A plugin loop on web feels like a bug.
- **Black (risk).** A new `NodeKind` variant breaks an exhaustive `match`
  outside the crate. Record it as breaking in the migration guide. A new
  config key `capabilities` in a preset changes `RolePreset`'s fields; it is
  `#[non_exhaustive]`, so this is additive.
- **Yellow (value).** One rule for framework and plugin work. The graph and
  the actuator can answer "which process runs what".
- **Green (ideas).** Later: a restart policy, a `harvest` port to this API,
  a doctor row that lists tasks per role.
- **Blue (process).** Spec (Verus model of placement) → red tests → green
  code → refactor. Docs, reference plugin, changelog fragment, ADR,
  migration guide in the same PR.

## Spec

`verification/background_tasks.rs` models placement. Proofs:

- A task never runs on a role whose base does not run its parent
  capability, unless `explicit = Some(true)`.
- `explicit = Some(false)` never runs.
- With no override and no queues, a plugin capability follows its parent.
- A task with queues runs only if the role drains one of them.

`verification/role_presets.rs` gets the `maintenance` field.

## Tests (red first)

- `background::tests`: placement table over `combined`, `web`, `worker` and
  presets; plugin capability overrides; ceiling error; unknown key error;
  queues and pin; explicit switch; `roles_running`.
- `autumn/tests/integration/background_tasks.rs`: a task runs on `combined`
  and `worker`, does not run on `web`, stops when the client drops; a preset
  turns a plugin capability off; `/actuator/tasks` lists the task and its
  roles.
- Graph: a `background_task` node with roles.
- Jobs manifest: task queues are present.

## Out of scope

Porting the media, billing and harvest plugins: #3546.
