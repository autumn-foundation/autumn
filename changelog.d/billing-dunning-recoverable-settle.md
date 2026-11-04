### Fixed

- **billing:** a store error no longer leaves a dunning row stuck (issue #3090).
  The retry job now writes the mirror first and settles the row last. After
  an error, the next job repeats the write. If a payment event settles the
  row first, the job undoes the `Unpaid` status.
