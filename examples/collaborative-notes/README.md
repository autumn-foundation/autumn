# Collaborative Notes

A minimal runnable demonstration of Autumn's transport-independent text CRDT.
Two replicas insert at the same logical position and deterministic character
IDs produce the same `AB` result regardless of delivery order.

## Prerequisites

Rust 1.88.0 or newer.

## Quick start

```sh
cargo run -p collaborative-notes
```
