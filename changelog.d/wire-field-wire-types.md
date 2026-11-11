### Added

- **wire contracts:** each field in a wire descriptor records its wire type:
  the JSON form serde writes (`string`, `integer`, `number`, `boolean`,
  `array`, `map`, `object`, `nullable` or `any`), with a format such as
  `date-time` or `uuid`. `#[derive(WireShape)]` and `#[event]` write it. An
  IDL export can map it.
- **wire contracts:** `#[wire(type = "…", format = "…", nullable)]` names the
  wire type of a field that the macro cannot read, such as one with
  `#[serde(with)]`. An unknown type is a compile error. `nullable` lets a
  request leave out a field whose type is an `Option` under another name.
- **wire contracts:** `autumn contracts check` compares wire types. A new
  JSON form with the same Rust type, for example `DateTime<Utc>` that gets
  `#[serde(with = "ts_seconds")]` and `#[wire(type = "integer")]`, is
  reported in both deploy windows. `any` is not compared.

### Breaking Changes

- **Breaking:** `autumn contracts export` writes bundle format 4, which an
  older CLI refuses. This CLI reads formats 1 to 4
  ([migration guide](docs/migrations/next.md)).
