### Added

- **sla:** calendar-aware SLA obligations (issue #1826). A
  `BusinessCalendar` sets working hours, weekends and holidays. An
  `Obligation` such as `"2 business days"` runs only in working time. The
  `Sla` extractor tracks, meets and reads obligations, and gives the
  remaining budget and the breach state. `#[obligation]` declares one on a
  model. On breach, Autumn puts one typed `SlaBreach` escalation job on the
  queue. Enable the `sla` Cargo feature. See `docs/guide/sla.md`.
