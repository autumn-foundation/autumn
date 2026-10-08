//! A thin router for a cell deployment (issue #3072).
//!
//! A cell is one full, independent copy of the app. A thin router in front
//! of the cells sends each tenant to its cell. [`CellRouter`] maps a tenant
//! to a cell with the same 16,384-slot hash as [`crate::sharding`], so a
//! cell can own whole shards.

use crate::config::SlotSpec;
use crate::sharding::{ShardKey, SlotId};

/// One cell: a name, the base URL of its ingress, and the slots it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSpec {
    /// A stable name, for example `"cell-eu-1"`.
    pub name: String,
    /// The base URL of the cell's ingress, for example `"http://cell-1:3000"`.
    pub base_url: String,
    /// The slots this cell owns. Leave every cell empty to split the slots
    /// evenly in order.
    pub slots: Vec<SlotSpec>,
}

/// An error in the cell list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cell router: {0}")]
pub struct CellRouterError(String);

/// Maps a tenant or a slot to a cell.
#[derive(Debug, Clone)]
pub struct CellRouter {
    _cells: Vec<CellSpec>,
}

impl CellRouter {
    /// Make a router.
    ///
    /// # Errors
    ///
    /// Returns an error when there are no cells, two cells have one name, or
    /// the slots are not covered exactly once.
    pub fn new(_cells: Vec<CellSpec>) -> Result<Self, CellRouterError> {
        todo!()
    }

    /// The cell that owns `slot`.
    #[must_use]
    pub fn for_slot(&self, _slot: SlotId) -> &CellSpec {
        todo!()
    }

    /// The cell that owns `key`.
    #[must_use]
    pub fn for_key(&self, _key: ShardKey<'_>) -> &CellSpec {
        todo!()
    }

    /// The cell that owns `tenant`.
    #[must_use]
    pub fn for_tenant(&self, _tenant: &str) -> &CellSpec {
        todo!()
    }

    /// The URL in `tenant`'s cell for `path_and_query` (which starts with `/`).
    #[must_use]
    pub fn url_for(&self, _tenant: &str, _path_and_query: &str) -> String {
        todo!()
    }

    /// The cells, in the order given.
    #[must_use]
    pub fn cells(&self) -> &[CellSpec] {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharding::slot_for_key;

    fn cell(name: &str, slots: &[&str]) -> CellSpec {
        CellSpec {
            name: name.to_owned(),
            base_url: format!("http://{name}:3000/"),
            slots: slots.iter().map(|s| SlotSpec::Range((*s).to_owned())).collect(),
        }
    }

    #[test]
    fn explicit_slots_route_by_the_shard_hash() {
        let router = CellRouter::new(vec![cell("a", &["0-8191"]), cell("b", &["8192-16383"])])
            .expect("valid");
        assert_eq!(router.for_slot(SlotId(0)).name, "a");
        assert_eq!(router.for_slot(SlotId(16383)).name, "b");
        let slot = slot_for_key(ShardKey::Str("acme"));
        let want = if slot.0 < 8192 { "a" } else { "b" };
        assert_eq!(router.for_tenant("acme").name, want);
        assert_eq!(router.for_key(ShardKey::Str("acme")).name, want);
    }

    #[test]
    fn empty_slots_split_evenly_in_order() {
        let router = CellRouter::new(vec![cell("a", &[]), cell("b", &[])]).expect("valid");
        assert_eq!(router.for_slot(SlotId(8191)).name, "a");
        assert_eq!(router.for_slot(SlotId(8192)).name, "b");
        assert_eq!(router.cells().len(), 2);
    }

    #[test]
    fn url_for_joins_the_base_url_and_the_path() {
        let router = CellRouter::new(vec![cell("a", &[])]).expect("valid");
        assert_eq!(router.url_for("acme", "/orders?x=1"), "http://a:3000/orders?x=1");
    }

    #[test]
    fn bad_cell_lists_are_rejected() {
        assert!(CellRouter::new(vec![]).is_err(), "no cells");
        assert!(
            CellRouter::new(vec![cell("a", &[]), cell("a", &[])]).is_err(),
            "duplicate name"
        );
        assert!(
            CellRouter::new(vec![cell("a", &["0-100"]), cell("b", &["50-16383"])]).is_err(),
            "overlap"
        );
        assert!(
            CellRouter::new(vec![cell("a", &["0-100"]), cell("b", &["200-16383"])]).is_err(),
            "gap"
        );
        assert!(
            CellRouter::new(vec![cell("a", &["0-16383"]), cell("b", &[])]).is_err(),
            "mixed explicit and empty slots"
        );
        assert!(
            CellRouter::new(vec![cell("a", &["0-16384"])]).is_err(),
            "out of range"
        );
    }
}
