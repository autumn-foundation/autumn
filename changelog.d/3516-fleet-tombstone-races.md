### Fixed

- **db (fleet):** two processes that share a fleet root and a control
  database no longer lose a replica tombstone (issue #3516). A create on
  demand now answers `410` while the delete is still pending, not only
  during the retention. Only `provision` ends the undo window early. Two
  deletes of one file at the same time now keep its tombstone.
