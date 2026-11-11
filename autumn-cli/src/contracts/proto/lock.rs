//! `proto.lock`: the protobuf field number of each field, kept across
//! exports.
//!
//! JSON has no field numbers, so the export gives them. A field gets the next
//! free number the first time it appears and keeps it. A field that goes away
//! moves to `reserved` with its number, so no other field gets that number.
//! `#[wire(tag = N)]` sets a number and wins over the lock.
//!
//! A message is keyed by its full protobuf name, and a field by its wire
//! name. The file is sorted TOML, so two exports give the same bytes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The largest protobuf field number.
pub const MAX_NUMBER: u32 = 536_870_911;

/// The field numbers protobuf keeps for its own use.
pub const KEPT_NUMBERS: std::ops::RangeInclusive<u32> = 19_000..=19_999;

/// The lock format this CLI writes and reads.
pub const LOCK_FORMAT: u32 = 1;

/// The header of the lock file.
const HEADER: &str = "\
# Field numbers of `autumn contracts export --proto`. Commit this file.
# A number never moves. A removed field keeps its number in `reserved`.
";

/// Every message's field numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    /// The lock format. A reader refuses a format it does not know.
    pub format: u32,
    /// Each message, by its full protobuf name.
    #[serde(default)]
    pub messages: BTreeMap<String, LockedMessage>,
}

impl Default for Lock {
    fn default() -> Self {
        Self {
            format: LOCK_FORMAT,
            messages: BTreeMap::new(),
        }
    }
}

/// The numbers of one message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedMessage {
    /// Each live field's number, by wire name.
    #[serde(default)]
    pub fields: BTreeMap<String, u32>,
    /// Numbers that no field may use again, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved: Vec<Reserved>,
}

/// One number that no other field may use.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Reserved {
    /// The field number.
    pub number: u32,
    /// The wire name of the removed field. `None` when the field is still
    /// live under another number (a `tag` moved it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One field to number: its wire name and its `#[wire(tag = N)]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// The field's wire name.
    pub name: String,
    /// The number `#[wire(tag = N)]` sets.
    pub tag: Option<u32>,
}

impl Lock {
    /// Parse a lock file.
    ///
    /// # Errors
    /// The text is not a lock, or its format is not one this CLI knows.
    pub fn parse(text: &str) -> Result<Self, String> {
        let lock: Self = toml::from_str(text).map_err(|e| format!("not a proto lock: {e}"))?;
        if lock.format != LOCK_FORMAT {
            return Err(format!(
                "proto lock format {} is not supported; this CLI reads format {LOCK_FORMAT}",
                lock.format
            ));
        }
        Ok(lock)
    }

    /// The canonical text: a header, then sorted TOML.
    #[must_use]
    pub fn render(&self) -> String {
        // A map of strings and numbers always serializes.
        let body = toml::to_string(self).unwrap_or_default();
        format!("{HEADER}\n{body}")
    }

    /// Number the fields of `message`, and update the lock.
    ///
    /// The result has one number for each field in `fields`, in that order.
    ///
    /// # Errors
    /// Two fields have one name or one tag, a tag is not a valid protobuf
    /// number, a tag is the number of another field or a reserved number, or
    /// the message has no free number left.
    pub fn assign(&mut self, message: &str, fields: &[Wanted]) -> Result<Vec<u32>, String> {
        let at = |text: String| format!("message `{message}`: {text}");
        let mut entry = self.messages.get(message).cloned().unwrap_or_default();
        check_wanted(fields).map_err(at)?;

        let live: BTreeSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        // A field that went away keeps its number in `reserved`.
        let gone: Vec<(String, u32)> = entry
            .fields
            .iter()
            .filter(|(name, _)| !live.contains(name.as_str()))
            .map(|(name, number)| (name.clone(), *number))
            .collect();
        for (name, number) in gone {
            entry.fields.remove(&name);
            entry.reserved.push(Reserved {
                number,
                name: Some(name),
            });
        }

        // A tag wins. The number it replaces is reserved.
        for field in fields {
            let Some(tag) = field.tag else { continue };
            if let Some((other, _)) = entry
                .fields
                .iter()
                .find(|(name, number)| **number == tag && **name != field.name)
            {
                return Err(at(format!(
                    "`#[wire(tag = {tag})]` on field `{}` is the number of field `{other}`",
                    field.name
                )));
            }
            if let Some(kept) = entry
                .reserved
                .iter()
                .find(|r| r.number == tag && r.name.as_deref() != Some(field.name.as_str()))
            {
                let was = kept
                    .name
                    .as_deref()
                    .map_or_else(String::new, |n| format!(" of removed field `{n}`"));
                return Err(at(format!(
                    "`#[wire(tag = {tag})]` on field `{}` is the reserved number{was}; a \
                     number is never used again",
                    field.name
                )));
            }
            if let Some(old) = entry.fields.insert(field.name.clone(), tag)
                && old != tag
            {
                entry.reserved.push(Reserved {
                    number: old,
                    name: None,
                });
            }
            entry.reserved.retain(|r| r.number != tag);
        }

        // A field that comes back gets its old number again.
        for field in fields.iter().filter(|f| f.tag.is_none()) {
            if entry.fields.contains_key(&field.name) {
                continue;
            }
            if let Some(index) = entry
                .reserved
                .iter()
                .position(|r| r.name.as_deref() == Some(field.name.as_str()))
            {
                let back = entry.reserved.remove(index);
                entry.fields.insert(field.name.clone(), back.number);
            }
        }

        // A new field gets the next free number.
        let mut top = entry
            .fields
            .values()
            .chain(entry.reserved.iter().map(|r| &r.number))
            .copied()
            .max()
            .unwrap_or(0);
        for field in fields {
            if entry.fields.contains_key(&field.name) {
                continue;
            }
            top = next_number(top).ok_or_else(|| at("no free field number is left".to_owned()))?;
            entry.fields.insert(field.name.clone(), top);
        }

        // A live name is not reserved. Its old number stays reserved.
        for kept in &mut entry.reserved {
            if kept
                .name
                .as_ref()
                .is_some_and(|n| live.contains(n.as_str()))
            {
                kept.name = None;
            }
        }
        entry.reserved.sort();
        entry.reserved.dedup();
        let numbers = fields.iter().map(|f| entry.fields[&f.name]).collect();
        self.messages.insert(message.to_owned(), entry);
        Ok(numbers)
    }
}

/// The first valid field number above `after`, if there is one.
#[must_use]
pub fn next_number(after: u32) -> Option<u32> {
    let next = after.checked_add(1)?;
    let next = if KEPT_NUMBERS.contains(&next) {
        KEPT_NUMBERS.end() + 1
    } else {
        next
    };
    (next <= MAX_NUMBER).then_some(next)
}

/// Whether `number` is a field number protobuf allows.
#[must_use]
pub fn valid_number(number: u32) -> bool {
    (1..=MAX_NUMBER).contains(&number) && !KEPT_NUMBERS.contains(&number)
}

/// Refuse two fields with one name or one tag, and a bad tag.
fn check_wanted(fields: &[Wanted]) -> Result<(), String> {
    let mut names = BTreeSet::new();
    let mut tags = BTreeMap::new();
    for field in fields {
        if !names.insert(field.name.as_str()) {
            return Err(format!("two fields have the wire name `{}`", field.name));
        }
        let Some(tag) = field.tag else { continue };
        if !valid_number(tag) {
            return Err(format!(
                "`#[wire(tag = {tag})]` on field `{}` is not a protobuf field number: use 1 to \
                 {MAX_NUMBER}, but not 19000 to 19999",
                field.name
            ));
        }
        if let Some(other) = tags.insert(tag, field.name.as_str()) {
            return Err(format!(
                "fields `{other}` and `{}` both have `#[wire(tag = {tag})]`",
                field.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn wanted(names: &[&str]) -> Vec<Wanted> {
        names
            .iter()
            .map(|n| Wanted {
                name: (*n).to_owned(),
                tag: None,
            })
            .collect()
    }

    fn tagged(name: &str, tag: u32) -> Wanted {
        Wanted {
            name: name.to_owned(),
            tag: Some(tag),
        }
    }

    #[test]
    fn new_fields_get_numbers_in_order() {
        let mut lock = Lock::default();
        let numbers = lock.assign("a.Item", &wanted(&["id", "name"])).expect("ok");
        assert_eq!(numbers, [1, 2]);
    }

    #[test]
    fn a_number_never_moves_when_fields_are_added_before_it() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "name"])).expect("ok");
        let numbers = lock
            .assign("a.Item", &wanted(&["added", "id", "name"]))
            .expect("ok");
        assert_eq!(numbers, [3, 1, 2]);
    }

    #[test]
    fn a_removed_field_is_reserved_and_its_number_is_not_used_again() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "old", "name"]))
            .expect("ok");
        let numbers = lock
            .assign("a.Item", &wanted(&["id", "name", "new"]))
            .expect("ok");
        assert_eq!(numbers, [1, 3, 4]);
        assert_eq!(
            lock.messages["a.Item"].reserved,
            [Reserved {
                number: 2,
                name: Some("old".to_owned())
            }]
        );
    }

    #[test]
    fn a_reserved_top_number_is_not_used_again() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "last"])).expect("ok");
        lock.assign("a.Item", &wanted(&["id"])).expect("ok");
        let numbers = lock.assign("a.Item", &wanted(&["id", "new"])).expect("ok");
        assert_eq!(numbers, [1, 3]);
    }

    #[test]
    fn a_field_that_comes_back_gets_its_old_number() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "sku"])).expect("ok");
        lock.assign("a.Item", &wanted(&["id"])).expect("ok");
        let numbers = lock.assign("a.Item", &wanted(&["id", "sku"])).expect("ok");
        assert_eq!(numbers, [1, 2]);
        assert!(lock.messages["a.Item"].reserved.is_empty());
    }

    #[test]
    fn a_tag_wins_over_the_lock_and_reserves_the_old_number() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "name"])).expect("ok");
        let numbers = lock
            .assign("a.Item", &[tagged("id", 10), wanted(&["name"]).remove(0)])
            .expect("ok");
        assert_eq!(numbers, [10, 2]);
        assert_eq!(
            lock.messages["a.Item"].reserved,
            [Reserved {
                number: 1,
                name: None
            }]
        );
    }

    #[test]
    fn a_new_field_goes_above_a_tag() {
        let mut lock = Lock::default();
        let numbers = lock
            .assign("a.Item", &[tagged("id", 7), wanted(&["name"]).remove(0)])
            .expect("ok");
        assert_eq!(numbers, [7, 8]);
    }

    #[test]
    fn a_tag_on_the_number_of_another_field_is_refused() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "name"])).expect("ok");
        let err = lock
            .assign("a.Item", &[wanted(&["id"]).remove(0), tagged("name", 1)])
            .expect_err("clash");
        assert!(err.contains("is the number of field `id`"), "{err}");
    }

    #[test]
    fn a_tag_on_a_reserved_number_is_refused() {
        let mut lock = Lock::default();
        lock.assign("a.Item", &wanted(&["id", "old"])).expect("ok");
        lock.assign("a.Item", &wanted(&["id"])).expect("ok");
        let err = lock
            .assign("a.Item", &[wanted(&["id"]).remove(0), tagged("name", 2)])
            .expect_err("reserved");
        assert!(err.contains("removed field `old`"), "{err}");
    }

    #[test]
    fn two_equal_tags_and_bad_tags_are_refused() {
        let mut lock = Lock::default();
        let err = lock
            .assign("a.Item", &[tagged("a", 3), tagged("b", 3)])
            .expect_err("clash");
        assert!(err.contains("both have"), "{err}");
        for bad in [0, 19_000, 19_999, MAX_NUMBER + 1] {
            let err = lock.assign("a.Item", &[tagged("a", bad)]).expect_err("bad");
            assert!(err.contains("not a protobuf field number"), "{err}");
        }
    }

    #[test]
    fn numbering_skips_the_numbers_protobuf_keeps() {
        assert_eq!(next_number(18_998), Some(18_999));
        assert_eq!(next_number(18_999), Some(20_000));
        assert_eq!(next_number(MAX_NUMBER), None);
    }

    #[test]
    fn the_text_round_trips_and_is_sorted() {
        let mut lock = Lock::default();
        lock.assign("b.Z", &wanted(&["z", "a"])).expect("ok");
        lock.assign("a.Y", &wanted(&["id", "gone"])).expect("ok");
        lock.assign("a.Y", &wanted(&["id"])).expect("ok");
        let text = lock.render();
        assert!(text.starts_with("# Field numbers"), "{text}");
        assert!(text.find("a.Y").expect("a") < text.find("b.Z").expect("b"));
        assert_eq!(Lock::parse(&text).expect("parses"), lock);
        assert_eq!(Lock::parse(&text).expect("parses").render(), text);
    }

    #[test]
    fn an_unknown_lock_format_is_refused() {
        let err = Lock::parse("format = 9\n").expect_err("refused");
        assert!(err.contains("format 9"), "{err}");
    }

    proptest! {
        /// Over any sequence of field sets, a live field keeps its number,
        /// two live fields never share one, and a number once given is never
        /// given to another name.
        #[test]
        fn numbers_never_move_and_are_never_reused(
            steps in prop::collection::vec(prop::collection::btree_set(0u8..12, 0..8), 1..12)
        ) {
            let mut lock = Lock::default();
            let mut owner: BTreeMap<u32, String> = BTreeMap::new();
            let mut last: BTreeMap<String, u32> = BTreeMap::new();
            for step in steps {
                let names: Vec<String> = step.iter().map(|n| format!("f{n}")).collect();
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                let numbers = lock.assign("m.M", &wanted(&refs)).expect("ok");
                let distinct: BTreeSet<u32> = numbers.iter().copied().collect();
                prop_assert_eq!(distinct.len(), numbers.len());
                for (name, number) in names.iter().zip(&numbers) {
                    prop_assert!(valid_number(*number));
                    if let Some(before) = last.get(name) {
                        prop_assert_eq!(before, number);
                    }
                    if let Some(first) = owner.insert(*number, name.clone()) {
                        prop_assert_eq!(&first, name);
                    }
                    last.insert(name.clone(), *number);
                }
            }
        }
    }
}
