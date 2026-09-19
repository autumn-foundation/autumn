### Fixed

- **`#[repository]` / `#[model]`:** generated reads now select the model's own
  column list (`Model::as_select()`) and generated `INSERT`/`UPDATE` writes use
  an explicit `RETURNING` column list, instead of decoding the table's physical
  column order positionally into the model (issue #2854). A model whose field
  order differed from its `table!` column order — e.g. the SaaS example's
  `Project`, where `name` precedes `tenant_id` in the struct but follows it in
  the table — previously came back with fields swapped, so the dashboard
  rendered the tenant ID as the project name.
