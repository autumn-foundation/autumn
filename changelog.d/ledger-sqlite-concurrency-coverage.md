### Testing

- **🪝 Snag: raced real `SQLite` connections against the money ledger's
  negative-balance invariant (no bug found):** every existing `SQLite` test
  for the double-entry ledger (`tests/sqlite_money_ledger.rs`) either ran
  fully sequentially or shared a connection pool of size 1, which makes
  `pool.get()` itself the serializer — no test had ever actually raced two
  `SQLite` connections against `ledger::post` at the same instant. A new
  `concurrent_withdrawals_cannot_double_spend_a_disallow_negative_wallet`
  test does that: 100 `tokio::spawn` tasks, each with its own connection from
  a pool of 8, race a genuinely distinct $80 withdrawal against a $100
  disallow-negative wallet. The invariant held — at most one withdrawal ever
  posts, and the stored balance always matches what actually posted — closing
  the gap rather than reporting a defect.
