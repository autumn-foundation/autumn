### Fixed

- **plugin sandbox:** a call that changes state runs only when the fuel left
  can also read its largest reply and the replies not yet read, at 128 units
  per byte. After a write has run, every later call keeps this fuel too.
  Before, a guest could send an outbound `POST`, then use all its fuel to read
  the reply. The request failed, and a retry sent the `POST` again. Now the
  call comes back `quota-exceeded` and nothing changes. With the default
  `fuel` and `outbound_response_bytes`, an outbound write needs about 74
  million of the 200 million units, so a plugin with less `fuel` cannot make
  one. Lower `outbound_response_bytes` to need less. New item:
  `CapabilityCall::commits` (issue #2523).
- **plugin sandbox:** the activity log puts a dropped call in the window when
  the call occurred, not when its request ended. New items: `DroppedCalls`,
  `CapabilityRuntime::dropped_calls`,
  `PluginActivityLog::ingest_dropped_calls`, and `dropped_calls` on
  `SandboxOutcome` and `SandboxRenderOutcome` (issue #2523).
