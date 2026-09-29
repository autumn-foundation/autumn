### Fixed

- **shadow: `GET`/`HEAD` requests carrying a request body are no longer
  mirrored.** The mirror replays method, target, and headers but no body, so
  mirroring a body-carrying `GET` asked the candidate a different request
  than the live build answered and recorded the manufactured difference as a
  divergence. Declared bodies (non-zero `Content-Length`, any
  `Transfer-Encoding`) are caught from the headers; undeclared ones — body
  frames with no declaring headers — from the body's own end-of-stream
  signal. Such requests now sit out under the `has_request_body` skip reason
  until the mutating-traffic follow-up brings real request-body replay.
