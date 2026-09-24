### Performance

- **⚡ Bolt: batch `money::ledger::post`'s per-account lock loop into one
  query (instructions -5.5%, DHAT alloc blocks -21.7%):** `post`
  (`autumn/src/money/ledger.rs`, issue #1837) locked every account a
  transaction's postings touch one at a time — `lock_account` ran its own
  `SELECT ... WHERE id = ?{FOR UPDATE}` round trip per distinct account, so a
  payout split across many destinations (a payroll run, a marketplace
  revenue share) paid one parse-prepare-execute cycle per leg just to take
  the locks, on top of the already-batched postings insert and balance
  check (#1837 follow-up). `lock_account` is now `lock_accounts`: one
  `WHERE id IN (...) ORDER BY id{FOR UPDATE}` query locks every account in
  the transaction. `ORDER BY` still runs before the lock is taken (Postgres
  plans `LockRows` above the `Sort`), so accounts are still locked in
  ascending id order — the deadlock-avoidance property the one-at-a-time
  loop existed for. Measured on `autumn/benches/ledger_post.rs` (300
  payouts, 40 postings each, `valgrind` against the in-memory SQLite
  backend): `--tool=callgrind` instructions 6,272,092,627 → 5,928,183,951
  (**-5.5%** overall, **-5.4%** on the per-payout marginal after subtracting
  the shared 0-iteration fixed cost) — the drop traces to SQL
  parse/compile machinery (`sqlite3RunParser`, `yy_reduce`,
  `sqlite3WhereBegin`, `sqlite3GenerateColumnNames`) paid once per payout
  instead of 40 times, not to the row lookups themselves (`sqlite3VdbeExec`
  and `sqlite3BtreeTableMoveto` are unchanged, since the same 40 rows are
  still read either way). `--tool=dhat` allocation block count
  3,581.7 → 2,806.4 per payout (**-21.7%**), one fewer heap allocation set
  per eliminated round trip. Behavior is unchanged: the existing
  `sqlite_money_ledger` suite (27 tests, including the
  `UnknownAccount`/`AccountCurrency` refusal cases) and the `money::ledger`
  lib tests (23 tests) pass unmodified, and the Postgres
  `a_negative_balance_check_holds_under_concurrency` conformance test still
  exercises the same row-lock semantics under `IN (...) ORDER BY ...
  FOR UPDATE`.
