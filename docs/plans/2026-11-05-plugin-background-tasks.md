# Plan: in-repo plugins on the role-aware background API (issue #3546)

Part of #3540. Uses the API of #3545 (`AppBuilder::background_task`,
`BackgroundTask`, `PluginCapability`).

## Goal

No in-repo plugin starts a loop from `on_startup`. Each loop is a declared
background task. The role decides where it runs. A guard test keeps it so.

## Audit

| Plugin | Work | Decision |
|---|---|---|
| media | Room reaper, `db` store | Task `media.room_reaper` on `media.maintenance` (parent `maintenance`). Off on `web`. `MediaPlugin::room_reaper_on_every_role()` is the opt-in for a fleet of only `web` replicas. |
| media | Room reaper, `memory` store | Same task on `serves_http`. Reason: the rooms live in the memory of the process that serves the room routes. A reaper on another process sees an empty store. |
| media | Recording retention sweep | Task `media.recording_retention` on `media.retention` (parent `retention`), queue = the media queue. It runs where the encode jobs run: those processes read the same recordings root. This holds for the `local` and the `s3` storage backends, because the recordings root is a local path for both. `MediaPlugin::retention_on_every_role()` is the opt-in for replicas that each have their own recordings disk. |
| media | Encode jobs | `.jobs(...)` puts the queue in each `JobInfo`. The jobs manifest and the pin read it. Test only. |
| billing | Dunning re-arm on boot | Task `billing.dunning_rearm` on `billing.dunning` (parent `job_workers`), queue `billing`. It re-arms once, then waits for shutdown: the framework warns on a task that returns early. |
| billing | Dunning jobs | As media jobs. Test only. |
| search | Indexing jobs | As media jobs. Test only. `on_startup` installs the client and stays. |
| cache-redis | Config check | No change. |
| edge | Upgrade proxy | Request scoped. No change. Not a `Plugin`. |
| reference | `on_startup` example | `on_startup` shows one-time setup only. The heartbeat is a real loop on `background_task`. |

Duplicate re-arms: the retry job is `unique`, `unique_by = "invoice_id"`,
`unique_window = "pending"`. Two re-arms of one row queue one job. A test
shows this.

## Brainstorming

1. Move each loop body into an `async fn run_*(…, shutdown)` that returns
   when the token is cancelled. The task closure calls it.
2. Keep `spawn_room_reaper_loop` and `spawn_retention_sweep_loop` as
   deprecated wrappers. No break for callers.
3. The reaper reads its store from the `RoomService` extension. Then it
   always reaps the store that the service writes. Today a `db` store with
   no pool falls back to a second, empty memory store in the reaper.
4. Plugin capabilities let an operator turn a loop off on one named role.
5. The billing re-arm is a one-shot task. Status `exited` in
   `/actuator/tasks` shows that it ran.
6. A guard test in `xtask` scans every plugin crate. It finds each
   `on_startup` and `state_initializer` argument, and each function that
   the argument calls in the same crate. It fails on a spawn there. An
   allow-list names request-scoped spawns.
7. The plugin declares `media.maintenance` and `media.retention` on every
   path, so a `[roles.<name>] capabilities` key for them never fails the
   boot.

## Reverse brainstorming

Question: "How can this port lose work or run it on the wrong process?"

| Way to fail | Guard |
|---|---|
| A `web` replica with the `memory` store never reaps its rooms. | The memory reaper uses `serves_http`. Test on `web`. |
| The reaper reaps another store than the service uses. | It reads the store from `RoomService`. Test. |
| No process sweeps recordings, so files stay past retention. | Sweep follows the media queue. `retention_on_every_role()` for host-local disks. Docs. |
| A `web` replica re-arms every open row at boot. | The re-arm needs `billing.dunning` and the `billing` queue. Test on `web` and `worker`. |
| Two workers re-arm the same row twice. | Unique pending window. Test. |
| The re-arm blocks the drain. | It selects on the shutdown token. |
| A helper hides a spawn from the guard. | The guard follows calls one level into the same crate. Self-test. |
| The guard flags a request-scoped spawn. | Allow-list with a reason per entry. Self-test. |
| A loop task panics or exits and nobody sees it. | The framework shows the status. No change. |
| `retention_days = 0` registers a task that does nothing. | No task then. Test. |
| A fleet of only `web` replicas on a `db` store reaps nothing. | `room_reaper_on_every_role()`. Docs. |
| A preset key for a capability that no task declares fails the boot. | The plugin declares both capabilities always. Test. |
| A closure in a variable, or a `Type::new` name clash, fools the guard. | The guard resolves `let` closures and `impl` types. Self-tests. |

## Six thinking hats

- **White.** Media starts two loops in `on_startup`; billing spawns the
  re-arm from `on_startup`. Job queues already reach the manifest through
  `JobInfo::queue`.
- **Red.** An operator expects `AUTUMN_ROLE=web` to do no housekeeping. A
  reaper that must run on `web` needs a clear written reason.
- **Black.** The `memory` room store, and the recordings disk. Both are
  process or host local. Moving their loop off `web` can lose work.
- **Yellow.** `/actuator/tasks` and `autumn graph` show every plugin loop
  and its roles.
- **Green.** Later: port the `reddit-clone` example live-feed relay; a
  doctor row per task.
- **Blue.** Red tests per plugin, then the port, then the guard, then docs
  and a changelog fragment.

## Spec

The placement rule is in `verification/background_tasks.rs` (#3545). This
port adds no new rule. It chooses a capability and queues per task.

## Tests (red first)

- media: on `web` the `db` reaper and the sweep do not run; on `worker` they
  do. On `web` the `memory` reaper runs. The sweep deletes an expired file on
  `worker` and keeps it on `web`. `run_room_reaper` and
  `run_retention_sweep` return on shutdown.
- billing: on `web` the re-arm does not run; on `worker` it does. Two
  re-arms queue one job.
- jobs: plugin job queues reach the jobs manifest and admit a pin.
- guard: fails on a spawn in `on_startup`, direct or one call deep; passes
  the allow-list; passes the tree.
