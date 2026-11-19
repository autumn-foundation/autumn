# ADR 0047: Avro Schemas Exported from the Wire Contract

- Status: Accepted
- Date: 2026-11-12
- Deciders: Autumn maintainers
- Tags: wire-contracts, contracts, cli, avro, events

## Context

Issue [#3666](https://github.com/autumn-foundation/autumn/issues/3666), part
of epic [#3548](https://github.com/autumn-foundation/autumn/issues/3548).

Event payloads are a wire contract. A consumer outside the workspace (a data
pipeline, a warehouse loader, a Kafka Connect sink) has no schema for them.
Avro is the usual schema for event data, and a schema registry checks
compatibility for each subject.

## Options

1. Avro binary as the outbox encoding, with a registry client. Rejected for
   now: a non-goal of the issue.
2. Export `.avsc` text from the bundle. The Rust types stay the source.
3. One Avro IDL (`.avdl`) file per crate. Rejected: a registry takes one
   schema per subject, and a topic is a subject.

## Decision

Option 2.

- One `.avsc` per topic. A new version is a new topic, so it is a new
  subject.
- The event record name comes from the topic, and the namespace from the
  crate. Avro resolution compares record names, so a Rust rename of the
  event type does not break it. A nested record gets its name from its type
  id. The export refuses two types with one Avro name. A file defines each
  record once, then names it.
- Field names are serde wire names. An alias is an Avro alias. A name that is
  not an Avro name stops the export. A changed name would make the schema
  describe other data.
- Forms that Avro cannot hold stop the export: `u64`, `usize` and 128-bit
  integers, and `any` outside an opaque event. Each message names the fix. A
  lossy silent mapping would let a consumer decode wrong values.
- The export writes text with a fixed key order, so two exports give the same
  bytes. The CLI has no Avro dependency. The tests parse each schema with
  `apache-avro`.
- `autumn.topic` (or `autumn.endpoint`) marks each file. Only a marked file
  is removed as stale.

## Consequences

- A consumer outside the workspace gets a registry-ready schema per topic.
- The schema describes the data, not Avro's JSON encoding. A bridge to Avro
  binary converts each payload.
- `autumn contracts check` stays the gate. Avro resolution agrees with it in
  each deploy window, except for the cases that
  `autumn-cli/src/contracts/avro/agreement.rs` names: read-sets,
  `deny_unknown_fields`, topics, unit events, opaque events and integer
  widths.
- `FULL`, `FORWARD` and `BACKWARD` registry modes match the check with no
  order, callee-first and caller-first.
- `verification/avro_integer_width.rs` proves that the integer mapping never
  narrows. A proptest checks that each export parses or refuses.
