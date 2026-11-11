### Fixed

- **storage:** a running direct upload now also cancels an htmx request
  that carries its form (issue #3601). Before, an `hx-post` button sent the
  form on `click` with no blob key. The guard uses the htmx 2 rule: the
  requester form, the closest form for a request that is not `GET`, and each
  `hx-include` target. A click or a submit shows the error line and moves
  focus to the file input. Other requests, for example live validation, stop
  with no message. A request on `autumn:upload` is not cancelled. For
  autosave, use `hx-trigger="change, autumn:upload"`.
