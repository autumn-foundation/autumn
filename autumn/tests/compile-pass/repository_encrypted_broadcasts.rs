// A `broadcasts = true` (i.e. hooks-enabled) repository over a model with
// encrypted columns must compile.
//
// `broadcasts = true` synthesizes an internal hooks type, which switches the
// repository onto the hooks-aware `update_many` path. That path builds a
// `Vec<Model>` of proposed rows and updates them chunk-wise. Iterating a chunk
// yields `&Model`, and binding *that* to `.set(..)` requires `&Model:
// AsChangeset` — which diesel does NOT implement once any field carries
// `#[diesel(serialize_as = ...)]`, as every `#[encrypted]` field does (the
// wrapper consumes the value on the way to SQL). The path must therefore pass
// an owned record.
//
// This is what `autumn generate scaffold --live 'col:String{encrypted}'`
// expands to (issue #1340); before the fix it failed with
// "the trait bound `&VaultEntry: AsChangeset` is not satisfied".

use autumn_web::model;
use autumn_web::repository;

diesel::table! {
    vault_entries (id) {
        id -> Int8,
        label -> Text,
        secret -> Text,
        lookup_key -> Text,
    }
}

#[model(table = "vault_entries")]
pub struct VaultEntry {
    #[id]
    pub id: i64,
    pub label: String,
    #[encrypted]
    pub secret: String,
    #[encrypted(deterministic)]
    pub lookup_key: String,
}

#[repository(VaultEntry, table = "vault_entries", broadcasts = true)]
pub trait VaultEntryRepository {}

fn main() {}
