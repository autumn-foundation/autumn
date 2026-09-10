# Client-sealed confidential text: first slice

## Scope

The first slice supports UTF-8 text only. The server stores it as
`ConfidentialText`. The server never handles the text or a root key.

## Wire format

The client sends one JSON object. It has `version` (`1`), `algorithm`
(`A256GCM`), `nonce` (12 bytes), `ciphertext` (the encrypted text plus the
16-byte tag), `blind_index` (32 bytes), and `key_generation` (a positive
integer). Binary values use unpadded base64url. The complete JSON value is at
most 24 KiB. Ciphertext is at most 16 KiB.

The client derives an encryption key and a separate blind-index key from its
root key. Autumn does not define root-key storage. A client must keep the root
key. The server must not load it from credentials or process state.

The blind index is a keyed, 32-byte token over normalized text and the same
field scope. It supports equality lookup. The server treats it as opaque.

## Ownership and identity

An owner is exactly one of these values:

* `user:<stable-user-id>` for a user-owned row.
* `tenant:<stable-tenant-id>` for a tenant-owned row.

The authenticated session supplies exactly one matching owner. No owner,
both owner kinds, or a different owner is an error. Insert and read use this
check. A read returns the sealed envelope only to that session.

Associated data is the canonical, length-prefixed encoding of `application`,
`model`, `field`, `owner kind`, `owner id`, and `record id`. These values are
UTF-8 strings. Each value has a four-byte big-endian length. The record ID is
required. A client must reserve or receive an ID before it seals an insert.

## Changes and attacks

The server may store the same valid envelope again for the same row. The first
slice has no replay counter. Associated data makes a copy to another row,
field, model, application, or owner fail client decryption.

Ownership transfer needs a new envelope made by a client that can decrypt the
old envelope and use the new owner's key. The server must not change only the
owner column.

`key_generation` selects a client key generation. Rotation is lazy: a client
reads with an old key, then writes a new envelope with a higher generation.
The server can store both generations during a migration but returns only the
selected row value.

Before persistence, the server rejects an unauthenticated owner context,
owner mismatch, empty identity fields, bad base64url, wrong lengths, oversized
values, unknown versions, unknown algorithms, zero key generations, and
ciphertext too short to contain an AEAD tag. Only the client can verify the
AEAD tag because only the client has the key.
