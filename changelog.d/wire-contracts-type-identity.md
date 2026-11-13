### Fixed

- **wire contracts:** `autumn contracts check` finds a type by its path, not
  its last name. `Json<shared::Item>` beside a local `Item` now resolves to
  `shared::Item`. A name that can mean more than one type stops the export.
- **wire contracts:** nested `WireShape` types are in the bundle and are
  compared field by field. Removing a required field from a nested type is a
  break.
- **wire contracts:** a unit struct (`struct Empty;`, JSON `null`) and an
  empty struct (`struct Empty {}`, JSON `{}`) are different shapes. A change
  from one to the other is a break in both deploy windows.
- **wire contracts:** an `#[endpoint]` that no `routes![…]` registers is not
  served. Removing it from the router is a removed route.
- **wire contracts:** a user type named `NoBody` is compared like any other
  type.

### Added

- **wire contracts:** `#[wire(opaque)]` on a `WireShape` field compares it by
  type name only.
- **wire contracts:** `autumn contracts export --from <dir>` joins a directory
  of descriptors. No build runs.

### Breaking Changes

- **Breaking:** `autumn contracts export` stops on a field whose type has no
  `WireShape` descriptor and is not a known leaf, such as an enum. Mark the
  field `#[wire(opaque)]`. The export writes bundle format 3, which an older
  CLI refuses ([migration guide](docs/migrations/next.md)).
