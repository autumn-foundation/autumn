### Breaking Changes

- **Breaking:** `#[query_budget]` reports an associated function handed a
  database or repository handle, such as `Post::published(&mut db)`, instead
  of counting it as one query. Nothing at the call site tells a one-query
  finder from a helper that loops. Put `#[query_cost(N)]` on the statement
  (#2316, [migration guide](docs/migrations/next.md)).

### Fixed

- **query budgets:** an early `return`, `break` or `continue` no longer adds
  its cost to the code it skips. `if cached { return repo.find_cached().await; }
  repo.find_fresh().await` now costs 1 query, not 2. In a loop, a path that
  leaves the loop is paid once, not once per pass (#2316).
- **query budgets:** the analysis tracks a handle through every binding form:
  `let`, `let`-`else`, assignment, `if let`, `while let`, `match` arms, `for`
  patterns, closure and transaction parameters, and annotated statements.
  After a branch, a name holds a handle if it holds one on any path. An
  assignment inside a block stays after the block. A value that a block, a
  `match` arm or a `break` gives keeps its handle. Before, some of these
  forms lost the handle, and a query through it was not counted (#2316).
- **query budgets:** a handle kept in a container is tracked. An index, a
  field, a pattern, `?` or an element method (`remove`, `unwrap`) on
  `[repo]`, `vec![repo]`, `Some(repo)` or a `Vec`, `Option`, map or set
  parameter of a handle type gives a handle. `Arc<Vec<…>>` is a container,
  and a container of containers keeps its shape. An unknown method on such
  a container, or any method on a user struct that holds a handle, is
  reported. A container method is known only for the container type that
  has it: `sort` on a `VecDeque` is reported. After `list.push(repo)` or `fill(&mut list, &repo)`, `list` holds
  a handle. A type annotation made only of standard and primitive types
  (`Vec<i64>`) marks a binding as plain (#2316).
- **query budgets:** a parameter of type `Arc<PgPostRepository>`, `Box<…>`,
  `Rc<…>`, `dyn PostRepository` or `impl PostRepository` is a handle. A
  handle assigned into a field (`deps.0 = repo`) is tracked. Before, a query
  through these was not counted (#2316).
- **query budgets:** `db.tx_immediate(…)`, `scoped_immediate_transaction`
  and `maybe_immediate_transaction` count their callback like `db.tx(…)`.
  A query future built inside `vec![…]` is counted. A function passed by
  name as a transaction or iterator callback is reported (#2316).
- **query budgets:** `#[query_cost]` and `#[query_exempt]` on an assignment
  or compound-assignment statement (`=`, `+=`) now apply, as in
  `#[query_cost(2)] links = load_links(&mut db).await?;`. Before, they were
  ignored (#2316).
