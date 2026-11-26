### Performance

- **outbox:** a message written through the outbox (`write`, `publish`,
  `enqueue_job`, `deliver_mail`, `dispatch_webhook`) gets a time-ordered
  UUID v7 id instead of a random v4. `autumn_outbox`'s unique `id` index and
  the `autumn_inbox` key take their inserts on the same few pages, which
  cuts the WAL of the write → relay → `Inbox::seen` cycle by 14–71% on a
  history of 100k–3M messages (`docs/perf/outbox-message-id/`). Ids stay
  opaque UUID strings; ids already stored and ids a remote producer sends
  are unchanged. A v7 id carries its write time in its first 48 bits.
