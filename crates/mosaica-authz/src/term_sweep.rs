//! Compaction's term sweep.
//!
//! One ascending sweep over term ordinals `0..dict_len`. Per term: the base posting, unioned
//! with every live delta tier's posting for that term, minus the tombstone bitmap, re-encoded
//! through [`crate::encode_posting_bitmap`] and appended to a [`crate::PostingsSpool`] at record
//! ordinal = term id. A fold never adds a posting, so [`sweep_term_postings`] does not take a
//! `Dict` at all, only the ordinal count it must produce records for.
//!
//! `terms/pairs.parquet` is the flat `(entity_id, term_id)` relation the mask differential runs
//! against, sorted term-then-entity, exactly the order this sweep visits terms in. Its writer
//! lives in `tessera-store`, which this crate must not depend on, so [`sweep_term_postings`]
//! takes an `on_term` callback instead of writing the file itself: it hands the caller each
//! term's final bitmap, and a caller on the legal side of the boundary drives the write from it.
//!
//! This pass discharges the postings half of a deletion's retirement obligation: an entity
//! dropped from `tombstones` here must also be dropped from row space by the fold's other pass,
//! over the same tombstone set, in the same fold, or a post-fold fragment would re-expose it.
//! Nothing in this module can enforce that; it is a property of the caller that runs them
//! together.
//!
//! The caller passes the fold plan's tombstone clone as `tombstones`, unmodified. This module
//! does not derive it: the set actually carried forward is computed at publication, after this
//! sweep has already run.

use std::io;
use std::sync::Arc;

use croaring::Bitmap;

use tessera_types::TermId;

use crate::postings::{encode_posting_bitmap, union_postings, PostingsReader, PostingsSpool};
use crate::tier::DeltaTier;

/// Run the sweep: ascending term ordinals `0..dict_len`.
///
/// For each term: union the base posting, if any, with every tier's posting, if any, for that
/// term, subtract `tombstones` as one operand, encode the result and append it to `spool` at the
/// next ordinal.
///
/// `spool` must be freshly created, no records appended yet.
///
/// `on_term(term, entities)` is called once per term, in ascending order, with the exact bitmap
/// the record for that term was just encoded from.
///
/// `dict_len` is written through unconditionally, never inferred from `base` or `tiers`: a term
/// with no data anywhere still gets a record, an empty one, never a dropped ordinal. Every
/// ordinal must stay stable across a fold, even for a term the fold empties entirely.
pub fn sweep_term_postings<F>(
    dict_len: u32,
    base: &PostingsReader,
    tiers: &[Arc<DeltaTier>],
    tombstones: &Bitmap,
    small_term_threshold: u32,
    spool: &mut PostingsSpool,
    mut on_term: F,
) -> io::Result<()>
where
    F: FnMut(TermId, &Bitmap) -> io::Result<()>,
{
    for raw in 0..dict_len {
        let term = TermId::new(raw);

        // `None` from either side means this file carries nothing for this term, not an error.
        let mut sources = Vec::new();
        sources.extend(base.posting(term)?);
        for tier in tiers {
            sources.extend(tier.posting(term)?);
        }

        let mut union = union_postings(sources);

        // The one tombstone operand, applied whole, never a per-term walk of its members.
        union -= tombstones;

        let record = encode_posting_bitmap(&union, small_term_threshold)?;
        spool.append(&record)?;

        on_term(term, &union)?;
    }
    Ok(())
}
