### Performance

- **money:** `ledger::post` no longer reads the posting history (issue #3339).
  Each account row now stores its balance (`balance_minor`), and a trigger on
  the postings table keeps it. The cost of a post no longer increases with the
  history. On the `ledger_post` bench, 900 payouts went from 47.5G to 4.1G
  instructions. `ledger::balance` reads the stored balance too. Run
  `autumn migrate` before you deploy: the new framework migration
  `20261029183512_add_money_account_balance` adds the column. See
  [the guide](docs/guide/money.md#balances).

### Added

- **money:** `ledger::balance_drift(conn)` returns each account whose stored
  balance is not the sum of its postings (issue #3339). The list must be
  empty. Set `balance_minor` to `NULL` to repair an account: the next `post`
  sums the postings and stores the result.
