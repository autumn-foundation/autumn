//! A thin router for a cell deployment (issue #3072).
//!
//! A cell is one full, independent copy of the app. A thin router in front
//! of the cells sends each tenant to its cell. [`CellRouter`] maps a tenant
//! to a cell with the same 16,384-slot hash as [`crate::sharding`], so a
//! cell can own whole shards.

use crate::config::{SLOT_COUNT, SlotSpec};
use crate::sharding::{ShardKey, SlotId, slot_for_key};

/// One cell: a name, the base URL of its ingress, and the slots it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CellSpec {
    /// A stable name, for example `"cell-eu-1"`.
    pub name: String,
    /// The base URL of the cell's ingress, for example `"http://cell-1:3000"`.
    pub base_url: String,
    /// The slots this cell owns. Leave every cell empty to split the slots
    /// evenly in order.
    pub slots: Vec<SlotSpec>,
}

impl CellSpec {
    /// Make a cell. Give empty `slots` to every cell to split the slots
    /// evenly in order.
    #[must_use]
    pub fn new(name: impl Into<String>, base_url: impl Into<String>, slots: Vec<SlotSpec>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            slots,
        }
    }
}

/// An error in the cell list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cell router: {0}")]
pub struct CellRouterError(String);

/// Maps a tenant or a slot to a cell.
///
/// It holds no state other than the cell list, so a router process can run
/// many copies of it.
#[derive(Debug, Clone)]
pub struct CellRouter {
    cells: Vec<CellSpec>,
    /// Slot -> index into `cells`. Has `SLOT_COUNT` entries.
    slot_map: Vec<u16>,
}

impl CellRouter {
    /// Make a router.
    ///
    /// Give every cell explicit `slots`, or leave every cell empty to split
    /// the slots evenly in order (the same rule as `[[database.shards]]`).
    ///
    /// # Errors
    ///
    /// Returns an error when there are no cells, two cells have one name, or
    /// the slots are not covered exactly once.
    pub fn new(cells: Vec<CellSpec>) -> Result<Self, CellRouterError> {
        let slot_count = usize::from(SLOT_COUNT);
        if cells.is_empty() {
            return Err(CellRouterError("give at least one cell".to_owned()));
        }
        if cells.len() > slot_count {
            return Err(CellRouterError(format!(
                "at most {slot_count} cells, got {}",
                cells.len()
            )));
        }
        let mut names = std::collections::HashSet::new();
        for cell in &cells {
            if !names.insert(cell.name.as_str()) {
                return Err(CellRouterError(format!(
                    "duplicate cell name {:?}",
                    cell.name
                )));
            }
        }

        let declared = cells.iter().filter(|c| !c.slots.is_empty()).count();
        let slot_map = if declared == 0 {
            // The same even split as `DatabaseConfig::resolved_slot_map`.
            let n = cells.len();
            (0..slot_count)
                .map(|slot| u16::try_from(slot * n / slot_count).unwrap_or(u16::MAX))
                .collect()
        } else if declared == cells.len() {
            explicit_slot_map(&cells)?
        } else {
            return Err(CellRouterError(
                "give slots to every cell or to none".to_owned(),
            ));
        };
        Ok(Self { cells, slot_map })
    }

    /// The cell that owns `slot`. An out-of-range slot maps to the last cell.
    #[must_use]
    pub fn for_slot(&self, slot: SlotId) -> &CellSpec {
        let index = self
            .slot_map
            .get(usize::from(slot.0))
            .map_or(self.cells.len() - 1, |&i| usize::from(i));
        &self.cells[index]
    }

    /// The cell that owns `key`.
    #[must_use]
    pub fn for_key(&self, key: ShardKey<'_>) -> &CellSpec {
        self.for_slot(slot_for_key(key))
    }

    /// The cell that owns `tenant`.
    #[must_use]
    pub fn for_tenant(&self, tenant: &str) -> &CellSpec {
        self.for_key(ShardKey::Str(tenant))
    }

    /// The URL in `tenant`'s cell for `path_and_query`. The result always
    /// has a `/` after the base URL, so the input cannot change the host.
    #[must_use]
    pub fn url_for(&self, tenant: &str, path_and_query: &str) -> String {
        let base = self.for_tenant(tenant).base_url.trim_end_matches('/');
        let path = path_and_query.trim_start_matches('/');
        format!("{base}/{path}")
    }

    /// The cells, in the order given.
    #[must_use]
    pub fn cells(&self) -> &[CellSpec] {
        &self.cells
    }
}

/// The slot map from explicit `slots`: each slot in exactly one cell.
fn explicit_slot_map(cells: &[CellSpec]) -> Result<Vec<u16>, CellRouterError> {
    let slot_count = usize::from(SLOT_COUNT);
    let mut map: Vec<Option<u16>> = vec![None; slot_count];
    for (index, cell) in cells.iter().enumerate() {
        let owner = u16::try_from(index).unwrap_or(u16::MAX);
        for spec in &cell.slots {
            let slots = spec
                .expand()
                .map_err(|e| CellRouterError(format!("cell {:?}: {e}", cell.name)))?;
            for slot in slots {
                let Some(entry) = map.get_mut(usize::from(slot)) else {
                    return Err(CellRouterError(format!(
                        "cell {:?}: slot {slot} is out of range (slots are 0..{slot_count})",
                        cell.name
                    )));
                };
                if let Some(other) = entry {
                    return Err(CellRouterError(format!(
                        "cell {:?}: slot {slot} is already in cell {:?}",
                        cell.name,
                        cells[usize::from(*other)].name
                    )));
                }
                *entry = Some(owner);
            }
        }
    }
    let missing = map.iter().filter(|owner| owner.is_none()).count();
    if missing != 0 {
        return Err(CellRouterError(format!(
            "{missing} slots have no cell; the cells must cover 0..{slot_count}"
        )));
    }
    Ok(map.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(name: &str, slots: &[&str]) -> CellSpec {
        CellSpec::new(
            name,
            format!("http://{name}:3000/"),
            slots
                .iter()
                .map(|s| SlotSpec::Range((*s).to_owned()))
                .collect(),
        )
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
        assert_eq!(
            router.url_for("acme", "/orders?x=1"),
            "http://a:3000/orders?x=1"
        );
        // A path without a leading `/` cannot change the host.
        assert_eq!(
            router.url_for("acme", "@evil.example/x"),
            "http://a:3000/@evil.example/x"
        );
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
