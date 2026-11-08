### Changed

- **media:** the room reaper and the recording retention sweep are background
  tasks (issue #3546). The role decides where they run. `media.room_reaper`
  on a `db` store needs `media.maintenance` (`combined`, `worker`). On a
  `memory` store it needs `serves_http` (`combined`, `web`), because the rooms
  are in the memory of that process. `media.recording_retention` needs
  `media.retention` and runs where the media queue is drained. A fleet of
  only `web` replicas no longer reaps a `db` store or sweeps recordings: add a
  `worker`, or call `MediaPlugin::room_reaper_on_every_role()` and
  `MediaPlugin::retention_on_every_role()`.
- **billing:** the dunning re-arm at boot is the background task
  `billing.dunning_rearm` (capability `billing.dunning`, queue `billing`). A
  `web` replica no longer re-arms every open row. Each `combined` or `worker`
  process that drains `billing` re-arms once at boot. Two re-arms of one row
  queue one job.

### Fixed

- **media:** the room reaper reads its store from the `RoomService`. Before, a
  `db` store with no pool fell back to two memory stores, and the reaper swept
  the empty one.

### Added

- **media:** `run_room_reaper` and `run_retention_sweep`: the loops as async
  functions. Each returns when its shutdown token is cancelled. Also
  `RoomService::store`, `MediaPlugin::room_reaper_on_every_role`,
  `MediaPlugin::retention_on_every_role`, and the constants
  `MEDIA_MAINTENANCE`, `MEDIA_RETENTION`, `ROOM_REAPER_TASK` and
  `RETENTION_TASK`.
- **billing:** `dunning::QUEUE`, `dunning::DUNNING` and `dunning::REARM_TASK`.

### Deprecated

- **media:** `spawn_room_reaper_loop` and `spawn_retention_sweep_loop`. Each
  starts its loop on every role. Use the background task that the plugin
  registers, or call `run_room_reaper` / `run_retention_sweep` from your own
  `background_task`.

### Testing

- A test in `xtask` (`plugin_spawns`) fails on a spawn that a plugin crate's
  `on_startup` or `state_initializer` argument reaches, directly or through a
  function of the same crate. Request-scoped spawns go on its allow-list.
