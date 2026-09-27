### Performance

- **money: `ledger::post` writes a transaction's postings in one statement.**
  Posting a transaction with N legs (a payroll run, a marketplace revenue
  split) previously ran N separate `INSERT` round trips in a loop — one
  parse/prepare/exec cycle per leg. It now builds one multi-row `INSERT`
  and binds every posting to it, so a transaction's postings commit in a
  single statement regardless of how many legs it carries.
