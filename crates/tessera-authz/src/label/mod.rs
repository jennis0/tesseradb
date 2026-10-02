//! The index over access labels, and their evaluation from a credential's terms.
//!
//! The grammar, normalisation and the label DAG are in [`tessera_access`], below every crate that
//! reads a label. This module decides what an item is indexed under, in the one dictionary every posting
//! is addressed by:
//!
//! - `public` under the term `public`, which every session holds;
//! - a term or a disjunction of terms under each of its terms, so that the union of the postings
//!   of the terms a credential holds is exactly the set these labels admit;
//! - any other label, one holding a conjunction, under one key of its own: its canonical text after
//!   a byte no term can hold. A credential's terms never name such a key. The key's ordinal is the
//!   label's id, and [`LabelIndex`] evaluates the label in a shared expression DAG.
//!
//! The DAG is derived from the dictionary, so it is rebuilt when a bundle opens and extended when
//! a flush promotes new keys, and nothing beside the dictionary stores it.

mod index;

pub use index::{index_keys, label_of_key, LabelIndex, COMPOUND_KEY};
