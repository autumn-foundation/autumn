### Fixed

- **plugin sandbox:** a call that changes state runs only when the fuel left
  can also read its largest reply, at 128 units per reply byte. Before, a guest
  could send an outbound `POST`, then use all its fuel to read the reply. The
  request failed, and a retry sent the `POST` again. Now the call comes back
  `quota-exceeded` and nothing changes. An outbound write with the default
  quotas needs about 74 million units. Reads do not change (issue #2523).
- **plugin sandbox:** the activity log puts a dropped call in the window when
  the call occurred, not when its request ended. New items: `DroppedCalls`,
  `CapabilityRuntime::dropped_calls`,
  `PluginActivityLog::ingest_dropped_calls`, and `dropped_calls` on
  `SandboxOutcome` and `SandboxRenderOutcome` (issue #2523).
