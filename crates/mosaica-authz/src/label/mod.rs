//! The index over access labels, and their evaluation from a credential's terms.
//!
//! The grammar, normalisation and the label DAG are in [`tessera_access`], below every crate that
//! reads a label. This module decides what an item is indexed under, in the one dictionary every
//! posting is addressed by:
//!
//! - `public` under the term `public`, which every session holds;
//! - every other label as one disjunction with the item's other labels, normalised, and each of its
//!   operands on its own: a term under itself, and a conjunction under one key of its own, its
//!   canonical text after a byte no term can hold.
//!
//! The union of the postings of the terms a credential holds and of the conjunctions it satisfies
//! is then exactly the set the labels admit. A credential's terms never name a conjunction's key.
//! [`LabelIndex`] evaluates the conjunctions in a shared expression DAG, in which a subexpression
//! inside a conjunction is one node. A disjunction inside a conjunction is not expanded.
//!
//! The DAG is derived from the dictionary, so it is rebuilt when a bundle opens and extended when
//! a flush promotes new keys, and nothing beside the dictionary stores it.

mod index;

pub use index::{index_keys, label_of_key, LabelIndex, COMPOUND_KEY};
