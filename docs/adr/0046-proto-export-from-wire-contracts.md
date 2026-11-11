# ADR 0046: Protobuf Schemas Exported from the Wire Contract

- Status: Accepted
- Date: 2026-11-11
- Deciders: Autumn maintainers
- Tags: wire-contracts, contracts, cli, protobuf

## Context

Issue [#3665](https://github.com/autumn-foundation/autumn/issues/3665), part
of epic [#3548](https://github.com/autumn-foundation/autumn/issues/3548).

Wire contracts protect callers in one Cargo workspace. A consumer in another
repository or language has only the Rust source. Protobuf is the usual schema
at that boundary. The epic rules out an IDL as the source of truth, so the
schema must come from the Rust types.

## Options

1. `.proto` as the source, with codegen into Autumn. Rejected: a non-goal of
   the epic.
2. Export `.proto` text from the bundle. The Rust types stay the source.
3. Serve protobuf or gRPC from Autumn endpoints. Rejected for now: JSON stays
   the wire format.

## Decision

Option 2.

- The export reads the bundle and its `wire_type`s. It writes text only. The
  CLI has no protobuf dependency and does not run `protoc`.
- One file per service (`autumn.<service>`), and one per crate for events
  (`autumn.<crate>.events`). The directory matches the package, as buf asks.
  Each file holds every message it needs. The copies of one type in two
  files are separate messages, so each file stands alone.
- `json_name` is the serde wire name. The proto field name is the wire name
  in lower snake case.
- A message is named after its Rust type. Two types with one name in one
  file get names from their type ids.
- The path parameters and the body join into one `<Rpc>Request`. The rpc
  returns the message that Autumn sends as the body, not a `<Rpc>Response`
  wrapper. A wrapper would change the JSON.
- A shape that protobuf cannot write in place gets a structural wrapper
  message (`DoubleListWrapper`, `NullableStringWrapper`). A comment says that
  the JSON is the bare value.
- `proto.lock` keys numbers by the full message name and the wire name. A
  type id would move all numbers when the type moves to another module. A
  field that goes away keeps its number in `reserved`. A field that comes
  back with the same wire name gets its number again. `#[wire(tag = N)]`
  wins over the lock. The number it replaces is reserved.
- `buf.yaml` uses `STANDARD` lint minus `RPC_RESPONSE_STANDARD_NAME`,
  `RPC_REQUEST_RESPONSE_UNIQUE` and `PACKAGE_VERSION_SUFFIX`, and `WIRE_JSON`
  for breaking changes.

## Consequences

- An outside consumer builds against a standard schema, and a protobuf JSON
  parser reads Autumn's JSON. A wrapper is the exception: the consumer
  unwraps it.
- `autumn contracts check` stays the gate. proto3 has no required fields and
  buf has no read-sets, so buf approves some changes that the check refuses.
  `autumn-cli/tests/integration/contracts_proto.rs` names each case.
- A message rename, also one caused by a new type with the same name, gives
  new numbers under the new name.
- `verification/proto_field_numbers.rs` proves that new numbers are valid,
  distinct and above every used number. A proptest checks the lock over
  random field sets.
