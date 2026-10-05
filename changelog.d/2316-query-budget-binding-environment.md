### Breaking Changes

- **Breaking:** `#[query_budget]` reports an associated function handed the
  `Db` handle, such as `Post::published(&mut db)`, instead of counting it as
  one query. Nothing at the call site tells a one-query finder from a helper
  that loops. Put `#[query_cost(N)]` on the statement
  ([migration guide](docs/migrations/next.md)).

### Fixed

- **query budgets:** an early `return`, `break` or `continue` no longer adds
  its cost to the code it skips. `if cached { return repo.find_cached().await; }
  repo.find_fresh().await` now proves 1 query, not 2 (#2316).
- **query budgets:** the analysis tracks a handle through every binding form:
  `let`, `let`-`else`, assignment, `if let`, `while let`, `match` arms, `for`
  patterns, closure and transaction parameters, and annotated statements. A
  branch joins its bindings, and a scope never undoes an assignment made
  through it (#2316).
- **query budgets:** a handle kept in a container is tracked. Elements,
  fields and unwrapped values of `[repo]`, `vec![repo]`, `Some(repo)` or a
  `Vec<PgPostRepository>` parameter are handles (#2316).
- **query budgets:** `#[query_cost]` and `#[query_exempt]` on an assignment
  statement (`#[query_cost(2)] links = load_links(&mut db).await?;`) now
  apply. Before, they were ignored (#2316).
