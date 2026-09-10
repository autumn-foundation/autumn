# Confidential fields: slice-one threat model

This document defines the security boundary for the first confidential-fields
slice. It is deliberately narrower than a claim that an application server can
never read a field.

## Assets

The protected assets are confidential-field plaintext and the client root keys
that can recover it. Slice one protects both at rest in PostgreSQL, logs,
backups, replay capsules, and server-managed key stores.

The machine-readable counterpart to this document is
[`confidential-fields-threat-model.json`](confidential-fields-threat-model.json).
Its protected-sink identifiers are also the coverage contract for the
end-to-end leak-sentinel test.

## Trusted components

Slice one trusts the client that performs encryption and holds the client root
key, the cryptographic implementation delivered to that client, and the owner's
endpoint while it handles plaintext. Key generation, encryption, and decryption
must occur inside that trusted boundary. PostgreSQL, server processes, log and
backup infrastructure, replay tooling, and server-managed key stores are not
trusted with plaintext or client root keys.

## Operator capabilities

The operator controls the application servers and storage infrastructure. The
operator can read and modify stored records, logs, backups, capsules, and
server-managed keys; observe and alter traffic; deploy server and client code;
and correlate activity across requests. Accordingly, slice one is protection
against accidental or passive disclosure at the listed sinks, not against every
action available to a malicious operator.

## Visible data

The operator can see ciphertext size, access timing, owner and tenant
identifiers, equality patterns, traffic metadata, schema, and
application-controlled plaintext fields. Applications must treat each of these
as permitted leakage and must not place secrets in identifiers or other
plaintext fields.

## Hidden data

When the trusted client and endpoint remain uncompromised, confidential-field
plaintext and client root keys are hidden **at rest** in PostgreSQL, logs,
backups, replay capsules, and server-managed key stores. This is a sink-specific
claim; it is not a claim that the server can never cause plaintext to be
revealed.

## Metadata leakage

Encryption does not conceal record existence, ciphertext length, when or how
often a record is accessed, tenant or owner relationships, repeated-value
equality patterns exposed by the chosen representation, network traffic shape,
the database schema, or fields the application elects to keep in plaintext.
Padding, batching, private information retrieval, and traffic-flow
confidentiality are outside slice one.

## Excluded attacks

Slice one does not protect against a malicious server changing client code,
active endpoint compromise, client compromise, traffic analysis, denial of
service, or an authenticated owner disclosing its data. In particular, avoid
claims such as “the operator physically cannot read this” unless independently
trusted client delivery or remote attestation is implemented. Those mechanisms
are not part of slice one.
