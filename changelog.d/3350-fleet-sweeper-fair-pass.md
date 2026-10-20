### Fixed

- **db:** the SQLite fleet sweeper no longer lets busy open databases take
  every pass (issue #3350). A pass takes the rows in the order they became
  due, then the oldest. An open database goes first only on a tie. A new
  mark is due at its own time, behind the rows that were due before it.
