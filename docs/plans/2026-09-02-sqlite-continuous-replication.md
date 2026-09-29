# Continuous SQLite replication with point-in-time restore (#1628)

Planning record for the #1628 slice: brainstorming → reverse brainstorming →
six-hats review → the design that fell out of them, and the TDD order the
implementation follows.

---

## 1. Brainstorming — candidate mechanisms

| # | Idea | Verdict |
| --- | --- | --- |
| B1 | Bundle/supervise a Litestream binary from `autumn serve` | ✗ Contradicts the single-binary pitch; the issue names this as the gap. |
| B2 | Periodic `VACUUM INTO` snapshots every N seconds | ✗ O(db size) per tick; a 2 GB database cannot hit a 10 s RPO. |
| B3 | SQLite online-backup API, incremental pages | ✗ diesel exposes no backup handle; still copies whole pages per tick. |
| B4 | `sqlite3_wal_hook` / `preupdate_hook` → logical change stream | ✗ Needs `libsqlite3-sys` hook registration diesel doesn't surface, and a logical stream cannot be replayed by SQLite itself (we would own the apply path). |
| B5 | **Ship raw WAL frame ranges + a byte-faithful base snapshot; restore by letting SQLite recover the reassembled `-wal`** | ✓ Litestream's model. O(bytes written), not O(db size). Restore is SQLite's own recovery path, so we never hand-apply pages. |
| B6 | Replicate through the app's repository layer (row-level CDC to S3) | ✗ Misses raw SQL / migrations / anything not going through repositories. Not a durability guarantee. |
| B7 | Filesystem-level snapshots (LVM/btrfs/ZFS) | ✗ Not portable, not offsite, needs host privileges. |
| B8 | rsync/rclone the `.db` + `-wal` on a timer | ✗ Torn reads: the `-wal` mutates mid-copy; no consistency guarantee, no PITR. |

**Chosen: B5.** Supporting ideas kept:

- B9 — a **destination trait** with a filesystem implementation, so the whole
  loop (including the end-to-end CI proof) runs with no Docker and no network,
  and S3 is one implementation rather than the only one.
- B10 — **generations**: a generation is the lifetime of one WAL salt sequence
  (base snapshot + a contiguous sequence of segments). A checkpoint ends a
  generation. This is what makes "restore to time T" a bounded search.
- B11 — reuse the existing **health-indicator → alert** pipeline (#1610) rather
  than inventing a new alert condition, so lag/verification failures inherit
  grace periods, dedup, recovery, and every configured channel for free.

---

## 2. Reverse brainstorming — "how would we make this lose data?"

Each failure mode below became a test.

| # | How to break it | Mitigation shipped |
| --- | --- | --- |
| R1 | Let SQLite auto-checkpoint: the WAL resets and frames we never shipped are overwritten. | `PRAGMA wal_autocheckpoint = 0` on every pooled connection when replication is enabled; the replicator is the **only** checkpointer. |
| R2 | Ship a half-written frame the writer was still appending. | Validate the WAL frame checksum chain and ship only through the **last commit frame** (`db_size_after != 0`). |
| R3 | Ship frames from a *new* WAL generation as if they continued the old one. | Salt-1/salt-2 in the WAL header is the generation identity; a salt change forces a new generation + snapshot. |
| R4 | Snapshot with `VACUUM INTO` — page numbers get renumbered and WAL frames no longer line up. | Snapshot is a **byte copy** of the main database file, taken immediately after our own `wal_checkpoint(TRUNCATE)`. In WAL mode the main file is mutated *only* by a checkpointer, and we are the only one. |
| R5 | Checkpoint (and truncate the WAL) while an upload is still in flight. | Checkpoint is only attempted when `shipped_offset == wal_len` — i.e. everything durable is already offsite. A stalled destination stalls checkpoints, never data. |
| R6 | Destination silently drops a segment; restore quietly stops early and looks fine. | Segments carry `seq` + `start_offset`/`end_offset`; restore refuses a gap, a seq jump, or a sha256 mismatch. SQLite's own recovery would have stopped silently — we refuse loudly *before* handing it the file. |
| R7 | Restore a corrupted replica over a live database. | `PRAGMA integrity_check` on the restored file **before** it is published, plus the #1595 `--force`/production guard. Restore writes to a temp path and renames only after verification. |
| R8 | "Uploaded" is mistaken for "restorable". | The in-process verifier performs a **real restore** into a temp dir on an interval and checks integrity; failure flips the health indicator → alert. |
| R9 | Replication silently stops (task panics, credentials rotated). | The status handle records `last_error` + `last_success_at`; the health indicator goes `Down` when lag exceeds the threshold *or* the loop reported an error. Never silent. |
| R10 | Point-in-time restore silently rounds to whatever is available. | Restore refuses a timestamp older than the oldest generation ("outside the retention window") and reports the exact commit time it landed on. |
| R11 | Replication points at the same bucket as user blob storage and a bucket-lifecycle rule eats the replicas. | `allow_shared_bucket` defaults to `false`, mirroring #1619. |
| R12 | Credentials leak into logs/argv. | `*_env` indirection only (#1619 convention); no `Debug` on the credential struct; errors carry status codes and keys, never secrets. |
| R13 | Replication turned on against Postgres. | Config validation refuses replication unless the resolved database URL is a SQLite target. |

---

## 3. Six hats

**White (facts).** No WAL/checkpoint handling exists anywhere in the tree today.
`autumn-cli/src/db/s3.rs` is a working synchronous SigV4 S3 client (#1619).
`autumn/src/alerts.rs` already escalates a non-healthy `HealthIndicator` past a
grace period onto every configured channel. diesel's SQLite backend is in the
graph under the plain `db` feature (the `sqlite` feature only flips
`RuntimeConnection`). The default `cargo test --workspace` therefore *can*
exercise a real SQLite file — no feature flip, no container.

**Red (instinct).** The scary part is not S3; it is the checkpoint interlock.
Everything about this design should be arranged so that "we could not ship" and
"we checkpointed" are mutually exclusive states.

**Black (risks).**
- Disabling auto-checkpoint means a wedged destination grows the WAL without
  bound. Accepted deliberately (durability over disk) and made loud: lag alert +
  a documented disk-watch note in the guide.
- A second process that checkpoints (a stray `sqlite3` shell) breaks the
  invariant. #1614's single-host/single-writer contract already forbids it; the
  guide restates it.
- Two SigV4 implementations (CLI sync, framework async) would rot apart.
- A timing-based "does not degrade the writer" assertion is inherently flaky.

**Yellow (upside).** The destination trait means the *entire* end-to-end proof
(seed → replicate → destroy → PITR → row equality) runs in the ordinary
`cargo test --workspace` lane in about a second, with the S3 path proved
separately against MinIO. The restore planner is pure and unit-testable. The
verifier is literally the restore path, so "verified restorable" is not a proxy
metric.

**Green (alternatives kept).** Ship a `file://` destination as a first-class
feature, not just a test double — it is exactly what an operator wants for a
second disk or an NFS/SSHFS mount. Keep segment payloads self-describing so a
future tool can read them without an index.

**Blue (process).** Land it in the three phases the issue names, red→green→
refactor per phase, with the SigV4 de-duplication as an explicit refactor step
once both call sites exist and are green.

---

## 4. Resulting design

### Remote layout

```text
{prefix}/{profile}/generations/{gen_id}/snapshot.db.gz
{prefix}/{profile}/generations/{gen_id}/snapshot.json          # commit marker
{prefix}/{profile}/generations/{gen_id}/segments/{index:05}-{seq:010}-{ms:013}.seg
```

`gen_id` = `{created_ms:013}-{nonce:016x}` — lexicographically chronological. The
salt is deliberately *not* in the id: a generation opens by snapshotting the
database file, which happens before the first write reveals the new WAL's salt.
Salt agreement is checked at restore time against each index's own segment 0.

A segment payload is a JSON header line followed by the gzip'd raw WAL byte
range; the header carries `index`, `seq`, `start_offset`, `end_offset`, `sha256`,
`frame_count`, `commit_count`, `page_size`, `db_size_pages`, `created_at`.

Segment `0` of each **index** starts at offset `0`, so it contains the 32-byte
WAL header — restore reassembles `snapshot.db` + a `db-wal` per index from the
objects alone.

**Why two levels.** A generation is one base snapshot; an index is one WAL salt
sequence within it. The `-wal` cannot grow forever, so the replicator must
checkpoint — but treating every checkpoint as a new generation would re-upload
the whole database each time `max_wal_bytes` is reached (gigabytes per hour on a
busy database). A checkpoint therefore costs one index bump; a full base snapshot
happens on `snapshot_interval`, which is also what bounds restore replay.

### Runtime

`Replicator::run(shutdown)` — one tokio task, file/SQLite work inside
`spawn_blocking`, uploads on the async runtime.

```text
tick (rpo/2, default 5 s):
  read WAL header → no generation, or an UNEXPECTED salt change → base snapshot
  scan frames from shipped_offset, validating the checksum chain
  ship [shipped_offset, last_commit_end) as one segment
  shipped_offset = last_commit_end; record lag
  if wal_len >= max_wal_bytes and shipped_offset == wal_len:
      wal_checkpoint(TRUNCATE)
      generation older than snapshot_interval ? new generation : next index
```

### Restore (CLI, `autumn db replica restore`)

Pure planner (`replication::restore`) shared by the CLI and the in-process
verifier: choose generation → select segments ≤ T → download → verify chain →
then, **index by index**, write `db-wal` beside the snapshot and
`wal_checkpoint(TRUNCATE)` it in → `integrity_check` → publish.

### Observability & alerting

`ReplicationStatus` (atomics + a small mutex) → `sqlite-replication`
`HealthIndicator` (`HealthOnly` group) → `/actuator/health` details, and
`Down` past the grace period is escalated by the existing #1610 alerter.

---

## 5. TDD order

| Phase | Red | Green |
| --- | --- | --- |
| 0 | WAL format unit tests (header, frame checksums, commit boundary, salt change) | `replication::wal` |
| 0 | Segment framing + key naming round-trip | `replication::segment` |
| 0 | Destination trait contract tests (file impl) | `replication::destination` |
| 1 | Config parse/env-override/validation tests | `config::ReplicationConfig` |
| 1 | Replicator ships a generation + segments for real writes | `replication::Replicator` |
| 1 | Lag/status observable; auto-checkpoint disabled | status + pool pragma |
| 2 | Restore latest; restore to timestamp; refuse gap/corruption/out-of-window | `replication::restore` |
| 2 | CLI guard/`--force`/integrity refusal | `db::replica` |
| 3 | Verifier flips the indicator on a corrupted replica | `replication::verify` |
| 3 | End-to-end: seed → replicate → destroy → PITR → row equality | integration test |
| 3 | Docs: durability section in the SQLite guide | guide + CHANGELOG |
