//! Compaction's pass 2 (compaction §3): the term sweep.
//!
//! One ascending sweep over term ordinals `0..dict_len`. Per term: the base posting, unioned
//! with every live delta tier's posting for that term, **minus** the tombstone bitmap —
//! re-encoded through [`crate::encode_posting_bitmap`] and appended to a [`crate::PostingsSpool`]
//! at record ordinal = term id.
//!
//! ## Subtraction only
//!
//! With the evaluate arm deleted (decision 0048), a fold never *adds* a posting: every entity
//! this sweep can produce for a term was already present in the base or a live tier's posting for
//! it. There is no per-term scatter, no descriptor resolution and no dictionary write —
//! [`sweep_term_postings`] does not take a `Dict` at all, only the ordinal count it must produce
//! records for.
//!
//! ## `terms/pairs.parquet` is the other half of the same sweep, over a crate boundary
//!
//! `pairs.parquet` (contracts §2.4) is the flat `(entity_id, term_id)` relation the I1 mask
//! differential runs against, sorted term-then-entity — exactly the order this sweep visits terms,
//! and each term's final entities, in. Writing it here would be the natural shape, and is not
//! possible: the writer lives in `tessera-store`, which this crate must not depend on
//! (`scripts/check-layers.sh` denies `authz → store`) — and it was in `tessera-build` when this
//! sweep was written, which this crate cannot depend on either, since `tessera-build` already
//! depends on `tessera-authz` and the reverse edge is a cycle. Either way the dependency runs the
//! other way. So [`sweep_term_postings`] takes an `on_term` callback instead
//! of writing the file itself — it hands the caller each term's final (post-union,
//! post-subtraction) bitmap, in the order the file requires, and a caller on the legal side of the
//! boundary drives the Parquet write from it (`tessera_store::PairsParquetWriter`, via its
//! `push_iter`). `pairs.parquet` cannot be carried forward from the old prefix instead: it would
//! then disagree with the new base postings about every folded deletion, which is the one
//! disagreement the I1 differential exists to catch.
//!
//! ## Both halves of a deletion, or neither
//!
//! This pass discharges the *postings* half of Rule F's retirement obligation (write-path §5.4):
//! an entity dropped from `tombstones` here must also be dropped from row space by pass 1
//! (compaction §3, row space), over the *same* tombstone set, in the *same* fold. A post-fold
//! fragment that still named a deleted entity would make Rule F's retirement re-expose it
//! (architecture §11.3, r33). The two passes are not independently shippable, and nothing in this
//! module can enforce that on its own — it is a property of the caller that runs them together.
//!
//! ## `tombstones` is `D₀`, not `executed`
//!
//! The caller passes the fold plan's tombstone clone (`D₀`, compaction §5) as `tombstones`, taken
//! whole and unmodified. This module does not derive it and must not: `executed ⊆ D₀` is computed
//! at *publication*, hours after this sweep has already run, from what the fold's publication
//! demonstrably carried forward. Using `executed` here would mean predicting the carry-forward set
//! at plan time — the fail-open compaction §5's r3 review replaced with the current rule. Passes
//! 1–3 execute over `D₀`; only retirement uses `executed`.
//!
//! ## Memory
//!
//! Per compaction §3's budget table, this sweep's own state is one accumulator `Bitmap` and two
//! small scratch buffers (Roaring views and small-array entities) per term, none of which persist
//! across terms — the union is computed and discarded every iteration. What is *not* bounded by
//! this module, and belongs to its caller and to [`crate::PostingsSpool`]: the spool's offsets
//! buffer (scales with the dictionary) and the widest term's own encode (scales with the corpus ×
//! that term's coverage, measured 125.12 MB as portable Roaring at 10⁹ — `probes/results.md`
//! §4.2). **Not measured here**: this module has no benchmark of its own peak RSS at scale: the
//! claim above is architectural (no corpus-sized buffer is allocated by this code), not a
//! measurement, and should be read as such.

use std::io;
use std::sync::Arc;

use croaring::Bitmap;

use tessera_types::TermId;

use crate::postings::{encode_posting_bitmap, union_postings, PostingsReader, PostingsSpool};
use crate::tier::DeltaTier;

/// Run the sweep: ascending term ordinals `0..dict_len`.
///
/// For each term: union the base posting (if any) with every tier's posting (if any) for that
/// term — Roaring sources through [`Bitmap::fast_or`], small tag-0 arrays folded in with
/// `add_many`, the same split [`crate::build_fragment_with_deltas`] uses and for the same reason
/// (never a `Vec<u32>` for a Roaring source; see the module doc) — subtract `tombstones` as one
/// operand (`Bitmap`'s `SubAssign`, `O(containers touched)`, never a per-term walk of the
/// tombstone set), encode the result through [`crate::encode_posting_bitmap`] and append it to
/// `spool` at the next ordinal.
///
/// `spool` must be freshly created (no records appended yet): [`PostingsSpool::append`] requires
/// its records in term order starting at ordinal 0, and this is the only ordinal order this
/// function produces.
///
/// `on_term(term, entities)` is called once per term, in ascending order, with the exact bitmap
/// the record for that term was just encoded from — the `pairs.parquet` hook (module doc). A
/// caller with no side output wired up may pass `|_, _| Ok(())`.
///
/// `dict_len` is written through unconditionally, never inferred from `base` or `tiers`: a term
/// with no data anywhere still gets a record — an empty one, tag 0, cardinality 0 — never a
/// dropped ordinal. `dict.len()` must not decrease and every ordinal must stay stable across a
/// fold (the staleness hint's counter and every session's granted terms depend on it), and this is
/// how that holds even for a term the fold empties entirely.
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

        // `None` from either side is an ordinary "this file carries nothing for this term", not
        // an error (§5.2) — a sparse delta tier, and a base file shorter than `dict_len` because
        // a descriptor was promoted after the last build, both rely on this. `base.posting` and
        // `DeltaTier::posting` already answer it that way; this loop just has to not treat `None`
        // as anything but "contributes nothing".
        let mut sources = Vec::new();
        sources.extend(base.posting(term)?);
        for tier in tiers {
            sources.extend(tier.posting(term)?);
        }

        let mut union = union_postings(sources);

        // The one tombstone operand, applied whole — never a per-term walk of its members.
        union -= tombstones;

        let record = encode_posting_bitmap(&union, small_term_threshold)?;
        spool.append(&record)?;

        on_term(term, &union)?;
    }
    Ok(())
}
