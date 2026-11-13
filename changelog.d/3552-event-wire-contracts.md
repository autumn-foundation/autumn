### Added

- **events:** wire contracts for events and outbox messages. `#[event]`
  derives `WireShape`, and gets a topic and a schema version
  (`#[event(name = "contact.updated", version = 2)]` sends on
  `contact.updated.v2`). A `#[listener]` that reads a field the event does
  not send fails the build, and the message names the event and the field.
  `#[event_consumer(E)]` gives an outbox topic handler the same check. Events
  and consumers write descriptors, and `autumn contracts check` covers them:
  a removed field that a deployed consumer reads is a break, and the report
  says when an old event version can be dropped. See `docs/guide/events.md`
  and `docs/guide/wire-contracts.md`.

### Breaking Changes

- **Breaking:** `#[event]` refuses a payload whose wire shape it cannot read:
  generics, enums, tuple structs, `#[serde(flatten)]` and the other shapes
  `#[derive(WireShape)]` refuses. Write `#[event(opaque)]` to keep such a
  payload without a contract. An `#[event]` type that also derives
  `WireShape` must drop that derive. A `#[listener]` that reads a
  `skip_serializing` or `skip_deserializing` field now fails to build
  ([migration guide](docs/migrations/next.md)).
