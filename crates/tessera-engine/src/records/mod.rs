//! `POST /v1/items`: a viewer reads, page by page, every item they may see in one view that
//! matches a filter, with the fields they name. [`Engine::items_stream`] serves one response: a
//! head, then pages, each an Arrow batch with a page end carrying the cursor to resume from, and a
//! trailer it returns.
//!
//! Every page is built from the latest generation with the visible set composed again, exactly as
//! a viewport composes it, so a deletion or suppression accepted during a read applies from the
//! next page. A row is taken only from inside that mask, and a field is read only for a row
//! already taken. Nothing is sampled and nothing is counted outside the visible set. Rows are
//! addressed by `tessera_id`; an entity id or any other internal position reaches the caller only
//! inside a sealed cursor.
//!
//! Map order is `(cell, tessera_id)` merged across the view's segments, and stored order is
//! ascending item number. Either resumes from a position that is a value, found again in whatever
//! segments the next page's generation holds, so a flush, merge or fold between pages loses no
//! row and repeats none.

mod cursor;

/// The order a read returns its rows in. Both return the same rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordsOrder {
    /// By map cell in the view, then `tessera_id`.
    Map,
    /// By the internal item numbering the record store holds rows in.
    Stored,
}

impl RecordsOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordsOrder::Map => "map",
            RecordsOrder::Stored => "stored",
        }
    }
}
