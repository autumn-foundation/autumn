### Performance

- **money:** `ledger::post` takes the stored transaction and its postings from
  `RETURNING` on its two inserts instead of reading them back. A posted call
  issues 4 statements instead of 6. The returned `PostedTransaction` is the
  same.
