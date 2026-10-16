### Changed

- **posture gate:** an acknowledgment now means "no wider than this" (issue #2497).
  The report prints `/ack-posture <digest> ceiling=<posture>`. A later
  narrowing keeps it valid. A removed constraint blocks again. A bare digest
  still means "exactly this".
