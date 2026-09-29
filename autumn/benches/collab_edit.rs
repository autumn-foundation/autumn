//! Drives sequential live typing through the collaborative text CRDT's
//! public insert path, so a profiler can attribute per-keystroke cost in
//! [`CollabText`] (the `collab` feature, issue #1806).
//!
//! Mirrors `CollabHub`'s live-edit message loop
//! (`autumn/src/collab/hub.rs`, the `insert_after` call in its `Insert`
//! branch): a real editor sends one op per keystroke, each anchored on the
//! id of the character the author just typed. This drives the exact same
//! call, `CollabText::insert_after`, one character at a time, building up a
//! multi-paragraph note the size `examples/collab-notes` is meant for —
//! not a synthetic loop over the function already suspected of being slow.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check on the output shape: it is a workload to
//! point a profiler at. `required-features` since `collab` is an opt-in
//! feature, unlike most of the other benches in this crate.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench collab_edit --features collab
//! BIN=$(find target/release/deps -maxdepth 1 -name "collab_edit-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --chars 6000
//! callgrind_annotate --threshold=80 callgrind.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --chars 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --chars 6000
//! ```
//!
//! `--chars N` types N characters (cycling a fixed prose paragraph) into one
//! document, one `insert_after` call per character.

use std::hint::black_box;

use autumn_web::collab::{CollabOp, CollabText, OpId};

/// A realistic shared-note paragraph — the shape `examples/collab-notes`
/// actually stores, not lorem ipsum. Cycled to reach the requested length,
/// the same way a real note grows past one paragraph as the session runs.
const PARAGRAPH: &str = "Standup notes: the ingest worker is caught up again \
after last night's backlog, and the on-call rotation starts with Priya on \
Monday. We still need to decide whether the retry budget applies per job or \
per batch before the next release goes out. Dana is drafting the migration \
guide for the schema change and will share a link once the first draft is \
ready for review. Let's revisit the open questions at Thursday's sync rather \
than blocking the release on them today.\n\n";

fn main() {
    let chars: usize = std::env::args()
        .position(|a| a == "--chars")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(6_000);

    let body: String = PARAGRAPH.chars().cycle().take(chars).collect();

    let mut doc = CollabText::new();
    let mut after: Option<OpId> = None;
    let mut char_buf = [0u8; 4];

    for ch in body.chars() {
        let text = ch.encode_utf8(&mut char_buf);
        let ops = doc
            .insert_after("author", after.as_ref(), text)
            .expect("insert stays under the counter ceiling");
        after = ops.last().and_then(|op| match op {
            CollabOp::Insert { id, .. } => Some(id.clone()),
            CollabOp::Delete { .. } => None,
        });
    }

    assert_eq!(doc.len(), chars);
    black_box(doc.text());

    println!(
        "typed {chars} characters, document holds {}",
        doc.element_count()
    );
}
