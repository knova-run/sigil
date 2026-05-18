//! Fixture for the `--no-doc` flag behavior of `sigil semantic`.
//!
//! The doc contains a deliberately distinctive token ("zebrafish") that
//! appears nowhere in the name or signature. A query for "zebrafish"
//! should:
//!   - match this entity when doc is indexed (default behavior)
//!   - match nothing when doc is masked (`--no-doc`)

/// Look up zebrafish records by genome accession id.
pub fn lookup_record(_id: &str) -> Option<String> {
    todo!()
}

/// A second function whose docstring deliberately contains no
/// distinctive identifiers — used as a control to confirm the masked
/// retriever finds nothing for the topic-only query.
pub fn unrelated_helper(_x: u32) -> u32 {
    todo!()
}
