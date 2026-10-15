### Changed

- **posture gate:** an acknowledgment now says "no wider than this" (issue #2497).
  The report prints `/ack-posture <digest> ceiling=<posture>`. A later
  narrowing keeps it valid. Removing a constraint blocks again. A bare digest
  still means "exactly this".
