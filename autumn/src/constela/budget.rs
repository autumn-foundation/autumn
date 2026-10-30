//! The one place a render or a dispatch spends retained bytes.
//!
//! Each copy of a value, and each value that the pipeline keeps, charges a
//! [`Budget`] **before** it allocates. A new path that copies or keeps bytes
//! must call this module. The memory property test
//! (`autumn/tests/constela_memory.rs`) and the `constela` fuzz target can find
//! a path that does not. They do not prove that each path charges. See
//! `docs/adr/0025-constela-retention-accounting.md`.
//!
//! Two byte meters, because they bound two different things:
//!
//! - [`Budget`]: values that the evaluator builds or copies, and values a
//!   render keeps outside its markup (metadata, payloads, portal targets,
//!   ids).
//! - [`Output`]: markup bytes, for the body and every portal together.
//!
//! A third meter, fuel, bounds work and not bytes. See [`Budget::burn`].

// autumn-panic-gate: request-path module — it parses, validates and renders a
// document written by a language model, so every panic in it is reachable by
// hostile input and would be a 500 rather than a diagnostic. Production code
// path must be panic-free. See CONTRIBUTING.md "Request-path panic gate".
// Justify exceptions with #[allow(clippy::<lint>, reason = "…")] at the
// narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::borrow::Cow;
use std::cell::Cell;

use serde_json::Value;

use super::error::{ConstelaError, Diagnostic, codes};
use super::path::Path;

/// The nominal bytes one value slot costs, before its contents.
///
/// Nominal, not `size_of::<Value>()`: with this weight a charge stays near the
/// JSON size of the value, which is what a document author can see. The
/// bound stays linear in the real size, which is what it is for.
pub const SLOT_BYTES: usize = 8;

/// Bytes of scanning that cost one unit of fuel.
///
/// A comparison or a search runs at memory speed. One unit buys about as much
/// time as one `eval_ref` call when it covers this many bytes.
pub const SCAN_BLOCK_BYTES: usize = 64;

/// Bytes that a render or a dispatch may build, copy or keep.
///
/// Shared by every expression in one render, or in one dispatch. A budget per
/// expression bounds each value and not the sum.
#[derive(Debug)]
pub struct Budget {
    remaining: Cell<usize>,
    limit: usize,
    fuel: Cell<u64>,
    fuel_limit: u64,
}

impl Budget {
    /// A budget of `limit` bytes.
    pub const fn new(limit: usize) -> Self {
        Self {
            remaining: Cell::new(limit),
            limit,
            fuel: Cell::new(u64::MAX),
            fuel_limit: u64::MAX,
        }
    }

    /// The same budget, with `fuel` units of work. Without this call, work is
    /// not metered.
    #[must_use]
    pub const fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = Cell::new(fuel);
        self.fuel_limit = fuel;
        self
    }

    /// Spend `units` of work, or fail with [`codes::RENDER_LIMIT`] at `path`.
    ///
    /// Byte charges bound what a render keeps. Fuel bounds what it does.
    pub fn burn(&self, units: u64, path: Path<'_>) -> Result<(), ConstelaError> {
        let left = self.fuel.get();
        if units > left {
            return Err(ConstelaError::Render(Diagnostic::new(
                path.to_string(),
                codes::RENDER_LIMIT,
                format!(
                    "the document asks for more than the {}-unit work fuel limit",
                    self.fuel_limit
                ),
            )));
        }
        self.fuel.set(left.saturating_sub(units));
        Ok(())
    }

    /// Spend fuel for `bytes` that a comparison or a scan reads.
    ///
    /// One unit for each started [`SCAN_BLOCK_BYTES`]. Call it before the scan.
    pub fn burn_scan(&self, bytes: usize, path: Path<'_>) -> Result<(), ConstelaError> {
        let units = u64::try_from(bytes.div_ceil(SCAN_BLOCK_BYTES)).unwrap_or(u64::MAX);
        self.burn(units, path)
    }

    /// The most bytes the fuel left can pay to scan.
    fn scan_cap(&self) -> usize {
        usize::try_from(self.fuel.get())
            .unwrap_or(usize::MAX)
            .saturating_mul(SCAN_BLOCK_BYTES)
    }

    /// Spend fuel for walking all of `value`, as a search or a shift does.
    ///
    /// Measured up to what the fuel left can pay for. Free when work is not
    /// metered.
    pub fn burn_value(&self, value: &Value, path: Path<'_>) -> Result<(), ConstelaError> {
        if self.fuel_limit == u64::MAX {
            return Ok(());
        }
        self.burn_scan(approx_bytes(value, self.scan_cap()), path)
    }

    /// Spend fuel for comparing two values: the size of the smaller one.
    ///
    /// Each value is measured up to what the fuel left can pay for, so the
    /// measuring is bounded too. Free when work is not metered.
    pub fn burn_compare(
        &self,
        left: &Value,
        right: &Value,
        path: Path<'_>,
    ) -> Result<(), ConstelaError> {
        if self.fuel_limit == u64::MAX || !same_shape(left, right) {
            return Ok(());
        }
        let cap = self.scan_cap();
        // Measure one side, then stop the other at that size: the walk of
        // the larger value is bounded by the smaller.
        let first = approx_bytes(left, cap);
        let smaller = first.min(approx_bytes(right, first));
        self.burn_scan(smaller, path)
    }

    /// Spend `bytes`, or fail with [`codes::RENDER_LIMIT`] at `path`.
    pub fn charge(&self, bytes: usize, path: Path<'_>) -> Result<(), ConstelaError> {
        let remaining = self.remaining.get();
        if bytes > remaining {
            return Err(ConstelaError::Render(Diagnostic::new(
                path.to_string(),
                codes::RENDER_LIMIT,
                format!(
                    "values built or kept here go over the {}-byte allocation budget",
                    self.limit
                ),
            )));
        }
        self.remaining.set(remaining.saturating_sub(bytes));
        Ok(())
    }

    /// Spend the approximate size of `value`.
    pub fn charge_value(&self, value: &Value, path: Path<'_>) -> Result<(), ConstelaError> {
        self.charge(approx_bytes(value, self.remaining.get()), path)
    }

    /// Own `value`. A borrowed value is charged, then copied. An owned value
    /// was charged when it was built.
    pub fn own(&self, value: Cow<'_, Value>, path: Path<'_>) -> Result<Value, ConstelaError> {
        match value {
            Cow::Owned(value) => Ok(value),
            Cow::Borrowed(value) => {
                self.charge_value(value, path)?;
                Ok(value.clone())
            }
        }
    }

    /// Own `text`. Borrowed text is charged, then copied. Owned text was
    /// charged when it was built.
    pub fn own_str(&self, text: Cow<'_, str>, path: Path<'_>) -> Result<String, ConstelaError> {
        match text {
            Cow::Owned(text) => Ok(text),
            Cow::Borrowed(text) => {
                self.charge(text.len(), path)?;
                Ok(text.to_string())
            }
        }
    }
}

/// A JSON writer that charges a [`Budget`] for each chunk before it keeps it.
///
/// For text whose size is known only while it is written, such as a
/// serialized payload. The charge is exact.
pub struct ChargingWriter<'b> {
    budget: &'b Budget,
    path: Path<'b>,
    out: Vec<u8>,
    refused: Option<ConstelaError>,
}

impl<'b> ChargingWriter<'b> {
    /// A writer that charges `budget`, and reports at `path`.
    pub const fn new(budget: &'b Budget, path: Path<'b>) -> Self {
        Self {
            budget,
            path,
            out: Vec::new(),
            refused: None,
        }
    }

    /// Serialize `value`, charging each chunk as it is written.
    ///
    /// # Errors
    ///
    /// [`codes::RENDER_LIMIT`] when the text goes over the budget, or
    /// [`codes::EVAL`] when the value cannot be serialized.
    pub fn serialize(mut self, value: &impl serde::Serialize) -> Result<String, ConstelaError> {
        let written = serde_json::to_writer(&mut self, value);
        if let Some(refused) = self.refused {
            return Err(refused);
        }
        let path = self.path;
        let failed = |message: String| {
            ConstelaError::Render(Diagnostic::new(path.to_string(), codes::EVAL, message))
        };
        written.map_err(|err| failed(format!("value could not be serialized: {err}")))?;
        String::from_utf8(self.out)
            .map_err(|err| failed(format!("serialized value is not UTF-8: {err}")))
    }
}

impl std::io::Write for ChargingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Err(refused) = self.budget.charge(buf.len(), self.path) {
            self.refused = Some(refused);
            return Err(std::io::Error::other("allocation budget spent"));
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Bytes of markup that one render may write, across all its buffers.
///
/// One meter for the body and every portal. Each buffer writes through
/// [`Self::write`], so the total is exact and needs no per-buffer arithmetic.
#[derive(Debug)]
pub struct Output {
    written: Cell<usize>,
    limit: usize,
}

impl Output {
    /// A meter of `limit` bytes.
    pub const fn new(limit: usize) -> Self {
        Self {
            written: Cell::new(0),
            limit,
        }
    }

    /// The bytes that can still be written.
    pub const fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.written.get())
    }

    /// Fail if `extra` more bytes would go over the limit. Writes nothing.
    ///
    /// Call this before an allocation whose result is written later, such as
    /// an escaped copy.
    pub fn check(&self, extra: usize, path: Path<'_>) -> Result<(), ConstelaError> {
        if self.written.get().saturating_add(extra) > self.limit {
            return Err(ConstelaError::Render(Diagnostic::new(
                path.to_string(),
                codes::RENDER_LIMIT,
                format!(
                    "render would produce more than {} bytes of markup",
                    self.limit
                ),
            )));
        }
        Ok(())
    }

    /// Append `text` to `out`. The only way markup reaches a buffer.
    pub fn write(&self, out: &mut String, text: &str, path: Path<'_>) -> Result<(), ConstelaError> {
        self.check(text.len(), path)?;
        self.written
            .set(self.written.get().saturating_add(text.len()));
        out.push_str(text);
        Ok(())
    }
}

/// Whether `==` on these values must read their contents.
///
/// Values of two kinds, or containers and strings of two lengths, are
/// unequal at once. That compare reads nothing.
fn same_shape(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(a), Value::String(b)) => a.len() == b.len(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len(),
        (Value::Object(a), Value::Object(b)) => a.len() == b.len(),
        _ => std::mem::discriminant(left) == std::mem::discriminant(right),
    }
}

/// The approximate bytes of `value`, counted without recursion and stopping
/// once `cap` is passed.
///
/// No recursion, because the value can come from a document. The early stop
/// is enough, because the only question is "is this too big".
pub fn approx_bytes(value: &Value, cap: usize) -> usize {
    let mut total = 0usize;
    let mut stack = vec![value];
    while let Some(current) = stack.pop() {
        total = total.saturating_add(match current {
            Value::Null | Value::Bool(_) | Value::Number(_) => SLOT_BYTES,
            Value::String(text) => text.len(),
            Value::Array(items) => SLOT_BYTES.saturating_mul(items.len()),
            Value::Object(entries) => entries
                .keys()
                .map(|key| key.len().saturating_add(SLOT_BYTES))
                .fold(0usize, usize::saturating_add),
        });
        // Checked before the children are stacked, so a huge container is
        // refused without a stack entry for each child.
        if total > cap {
            return total;
        }
        match current {
            Value::Array(items) => stack.extend(items.iter()),
            Value::Object(entries) => stack.extend(entries.iter().map(|(_, value)| value)),
            _ => {}
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_charge_spends_and_an_overdraft_fails() {
        let budget = Budget::new(10);
        budget.charge(6, Path::root("a")).expect("fits");
        budget.charge(4, Path::root("b")).expect("fits exactly");
        let err = budget.charge(1, Path::root("c")).expect_err("over");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
        assert_eq!(err.diagnostics()[0].path, "c");
    }

    #[test]
    fn a_failed_charge_spends_nothing() {
        let budget = Budget::new(10);
        budget.charge(11, Path::root("a")).expect_err("over");
        budget
            .charge(10, Path::root("b"))
            .expect("the full budget is still there");
    }

    #[test]
    fn owning_a_borrowed_value_charges_its_size() {
        let budget = Budget::new(100);
        let value = json!("x".repeat(60));
        let owned = budget
            .own(Cow::Borrowed(&value), Path::root("p"))
            .expect("fits");
        assert_eq!(owned, value);
        budget
            .own(Cow::Borrowed(&value), Path::root("p"))
            .expect_err("second copy is over");
    }

    #[test]
    fn owning_an_owned_value_charges_nothing() {
        let budget = Budget::new(0);
        let owned = budget
            .own(Cow::Owned(json!("x".repeat(60))), Path::root("p"))
            .expect("built values were charged when built");
        assert_eq!(owned.as_str().map(str::len), Some(60));
    }

    #[test]
    fn a_charging_writer_charges_the_exact_text() {
        let budget = Budget::new(100);
        let text = ChargingWriter::new(&budget, Path::root("p"))
            .serialize(&json!({"a": "x\"y"}))
            .expect("fits");
        assert_eq!(text, r#"{"a":"x\"y"}"#);
        budget
            .charge(100 - text.len(), Path::root("p"))
            .expect("charged exactly");
        budget.charge(1, Path::root("p")).expect_err("nothing left");
    }

    #[test]
    fn a_charging_writer_stops_at_the_budget() {
        let budget = Budget::new(10);
        let err = ChargingWriter::new(&budget, Path::root("p"))
            .serialize(&json!("x".repeat(100)))
            .expect_err("over");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    }

    #[test]
    fn output_counts_every_buffer_against_one_limit() {
        let output = Output::new(8);
        let mut body = String::new();
        let mut portal = String::new();
        output
            .write(&mut body, "abcd", Path::root("a"))
            .expect("fits");
        output
            .write(&mut portal, "efgh", Path::root("b"))
            .expect("fits exactly");
        output
            .write(&mut body, "i", Path::root("c"))
            .expect_err("over");
        assert_eq!(body, "abcd");
        assert_eq!(portal, "efgh");
    }

    #[test]
    fn approx_bytes_counts_strings_keys_and_slots() {
        let value = json!({"ab": ["xyz", 1]});
        let expected = (2 + SLOT_BYTES) + 2 * SLOT_BYTES + 3 + SLOT_BYTES;
        assert_eq!(approx_bytes(&value, usize::MAX), expected);
    }

    #[test]
    fn approx_bytes_stops_once_past_the_cap() {
        let value = json!(["x".repeat(100), "y".repeat(100)]);
        assert!(approx_bytes(&value, 50) > 50);
        assert!(approx_bytes(&value, 50) < 2 * SLOT_BYTES + 200);
    }

    #[test]
    fn fuel_spends_and_an_overdraft_fails() {
        let budget = Budget::new(0).with_fuel(10);
        budget.burn(6, Path::root("a")).expect("fits");
        budget.burn(4, Path::root("b")).expect("fits exactly");
        let err = budget.burn(1, Path::root("c")).expect_err("over");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
        assert_eq!(err.diagnostics()[0].path, "c");
    }

    #[test]
    fn a_scan_burns_one_unit_for_each_started_block() {
        let budget = Budget::new(0).with_fuel(3);
        budget.burn_scan(0, Path::root("p")).expect("free");
        budget.burn_scan(1, Path::root("p")).expect("one block");
        budget
            .burn_scan(SCAN_BLOCK_BYTES + 1, Path::root("p"))
            .expect("two blocks");
        budget
            .burn_scan(1, Path::root("p"))
            .expect_err("fuel is spent");
    }

    #[test]
    fn a_comparison_burns_the_smaller_value() {
        let budget = Budget::new(0).with_fuel(10);
        let big = json!("x".repeat(100_000));
        budget
            .burn_compare(&big, &json!("y"), Path::root("p"))
            .expect("different lengths: nothing is read");
        let err = budget
            .burn_compare(&big, &big, Path::root("p"))
            .expect_err("two big values");
        assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    }

    #[test]
    fn a_comparison_of_two_kinds_reads_nothing() {
        let budget = Budget::new(0).with_fuel(1);
        let items = json!(vec![0; 100_000]);
        budget
            .burn_compare(&items, &json!(1), Path::root("p"))
            .expect("kinds differ");
        budget
            .burn_compare(&items, &json!([1]), Path::root("p"))
            .expect("lengths differ");
    }

    #[test]
    fn unmetered_work_is_free() {
        let budget = Budget::new(0);
        let big = json!("x".repeat(100_000));
        budget.burn_value(&big, Path::root("p")).expect("free");
        budget
            .burn_compare(&big, &big, Path::root("p"))
            .expect("free");
    }

    #[test]
    fn a_scan_of_a_full_block_costs_one_unit() {
        let budget = Budget::new(0).with_fuel(1);
        budget
            .burn_scan(SCAN_BLOCK_BYTES, Path::root("p"))
            .expect("one block is one unit");
        budget.burn(1, Path::root("p")).expect_err("spent");

        let budget = Budget::new(0).with_fuel(1);
        budget
            .burn_scan(SCAN_BLOCK_BYTES + 1, Path::root("p"))
            .expect_err("one byte more is a second unit");
    }

    #[test]
    fn a_failed_burn_spends_nothing() {
        let budget = Budget::new(0).with_fuel(5);
        budget.burn(6, Path::root("p")).expect_err("over");
        budget
            .burn(5, Path::root("p"))
            .expect("all the fuel is left");
    }

    #[test]
    fn measuring_a_huge_value_stops_at_the_fuel() {
        let budget = Budget::new(0).with_fuel(2);
        let huge = json!(vec!["x".repeat(1000); 10_000]);
        budget
            .burn_value(&huge, Path::root("p"))
            .expect_err("far over two units");
    }

    #[test]
    fn a_failed_burn_names_the_fuel_limit() {
        let budget = Budget::new(0).with_fuel(0);
        let err = budget.burn(1, Path::root("p")).expect_err("over");
        assert!(err.diagnostics()[0].message.contains("fuel"));
    }
}
