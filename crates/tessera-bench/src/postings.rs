//! Reading one term's postings as an owned `Bitmap`.
//!
//! `tessera_authz::build_fragment` unions many terms at once and is the thing the authorise arm
//! measures; this is the per-term accessor the *setup* needs — computing `TermStats`, choosing
//! grants by shape, and building the synthetic masks the gather probe drives. Deliberately
//! separate from `build_fragment` so no arm accidentally times a helper that does extra work.

use croaring::Bitmap;

use tessera_authz::{PostingRef, PostingsReader};
use tessera_types::TermId;

/// One term's postings as an owned bitmap.
///
/// The dual encoding (`probes/optimisations.md`: Roaring above `SMALL_TERM_THRESHOLD_DEFAULT`,
/// sorted `u32` arrays below) is flattened here — a caller choosing grants by shape cares about
/// cardinality and container span, not about which representation the writer picked. Note that
/// the tag-0 path *copies*, so this is setup-only and must never appear inside a timed loop.
pub fn to_bitmap(postings: &PostingsReader, term: TermId) -> std::io::Result<Bitmap> {
    let Some(posting) = postings.posting(term)? else {
        // Absent means the file carries no record for this term — an empty posting, not an error.
        return Ok(Bitmap::new());
    };
    Ok(match posting {
        PostingRef::Roaring(view) => (*view).clone(),
        PostingRef::Array(bytes) => {
            let mut values: Vec<u32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect();
            values.sort_unstable();
            let mut bitmap = Bitmap::new();
            bitmap.add_many(&values);
            bitmap
        }
    })
}

/// The union of several terms' postings — the same set `build_fragment` produces, used where a
/// mask is *needed* rather than *timed*.
pub fn union(postings: &PostingsReader, terms: &[TermId]) -> std::io::Result<Bitmap> {
    let mut out = Bitmap::new();
    for &term in terms {
        out.or_inplace(&to_bitmap(postings, term)?);
    }
    out.run_optimize();
    Ok(out)
}
