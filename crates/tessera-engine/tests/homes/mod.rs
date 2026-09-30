//! **Every place one item's data lives**, for the two tests that must reach all of them: a
//! deletion must remove the item from each at the fold that executes it
//! (`deletion_reaches_every_home.rs`), and an edit must carry each to the item's new entity
//! (`edit_carries_every_home.rs`).
//!
//! Both tests match on [`Home`] exhaustively, so a home added here does not compile until each
//! says what it does about it.

pub mod fixture;

/// Declares [`Home`] and [`Home::ALL`] from one list, so no home can be left out of `ALL`.
macro_rules! homes {
    ($($(#[$doc:meta])* $home:ident,)*) => {
        /// One home of an item's data.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Home {
            $($(#[$doc])* $home,)*
        }

        impl Home {
            pub const ALL: &'static [Home] = &[$(Home::$home,)*];
        }
    };
}

homes! {
    /// The item's row in each view's row space, and its position there.
    Row,
    /// A `render` column's value, in the hot row.
    RenderColumn,
    /// Which rows of a render column hold a value.
    RenderPresence,
    /// An `index` column's entity-space value column, with its presence bitmap.
    ValueColumn,
    /// A public category's membership postings.
    CategoryPostings,
    /// A keyword column's dictionary.
    KeywordDictionary,
    /// A text column's token dictionary and the postings over it.
    TextIndex,
    /// The per-entity record blob: every field with no other home, and a text column's prose.
    RecordBlob,
    /// The access-control postings that decide who may see the item.
    TermPostings,
    /// A unique column's index from value to item.
    UniqueIndex,
    /// The edited-items map from the item's number to its entity, which its `tessera_id` is
    /// resolved through.
    EditedItems,
    /// The item's memberships of an enumerated layer's artifacts.
    Membership,
    /// The items a supplied content was generated from.
    GeneratingSet,
    /// A suppression standing against the item.
    Suppression,
    /// A group-scoped family's value, per key.
    ScopedValue,
    /// A group-scoped text family's prose and its token index, per key.
    ScopedProse,
}
