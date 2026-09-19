// A hooks-enabled repository over a model with encrypted columns must compile.
//
// `hooks = ...` (and `broadcasts = true`, which synthesizes an internal hooks
// type) switches the repository onto the hooks-aware `update_many` path. That
// path builds a `Vec<Model>` of proposed rows and updates them chunk-wise.
// Iterating a chunk yields `&Model`, and binding *that* to `.set(..)` requires
// `&Model: AsChangeset` — which diesel does NOT implement once any field
// carries `#[diesel(serialize_as = ...)]`, as every `#[encrypted]` field does
// (the wrapper consumes the value on the way to SQL). The path must therefore
// pass an owned record.
//
// This is what `autumn generate scaffold --live 'col:String{encrypted}'`
// expands to (issue #1340); before the fix it failed with
// "the trait bound `&VaultEntry: AsChangeset` is not satisfied".

mod schema {
    autumn_web::reexports::diesel::table! {
        vault_entries (id) {
            id -> Int8,
            label -> Text,
            secret -> Text,
            lookup_key -> Text,
        }
    }
}

use autumn_web::prelude::*;
use schema::vault_entries;

#[autumn_web::model]
pub struct VaultEntry {
    #[id]
    pub id: i64,
    pub label: String,
    #[encrypted]
    pub secret: String,
    #[encrypted(deterministic)]
    pub lookup_key: String,
}

#[derive(Clone, Default)]
pub struct VaultEntryHooks;

impl MutationHooks for VaultEntryHooks {
    type Model = VaultEntry;
    type NewModel = NewVaultEntry;
    type UpdateModel = UpdateVaultEntry;
}

#[autumn_web::repository(VaultEntry, hooks = VaultEntryHooks)]
pub trait VaultEntryRepository {
    // The deterministic column still supports a derived equality lookup: the
    // macro routes the bound `String` through the encrypted-column registry.
    fn find_by_lookup_key(lookup_key: String) -> Vec<VaultEntry>;
}

fn main() {}
