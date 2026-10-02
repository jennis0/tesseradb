//! Selection from a segment's identity bands ([`tessera_store::bands`]): the same served set
//! [`crate::select`] defines, read from the few entries of a tile that can be in it.
//!
//! Band `j` holds every row whose `tessera_id` is below `2^(64 - j)`, so it holds every row below
//! the cut `P_d` wherever `j <= leading_zeros(P_d)`. For a tile, with its visible rows taken from
//! the composed mask:
//!
//! - `C_θ` is the count of the band's visible entries below the cut, exactly, since every visible
//!   row below the cut is in the band. That fixes `m`.
//! - If the band holds at least `m` visible entries, the `m` smallest of them are the tile's `m`
//!   smallest visible identities: every visible row outside the band has a larger identity than
//!   every entry in it.
//! - Otherwise a wider band is tried, and below the widest the tile is not answered here; the
//!   caller reads it from the identity column as the shipped scan does.
//!
//! The mask is applied to every entry before it is counted or kept, so the bands decide only how
//! far the read goes, never which rows are visible: a tile with one visible row is answered by the
//! column when the bands do not hold it, and serves it. A tile is the union of its segments'
//! parts, and every part is read at the same band, so the count and the `m` smallest are taken over
//! the union, as [`crate::select::SelectionParts`] requires.

use tessera_store::bands::FIRST_BAND;

use crate::compose::EffectiveMask;
use crate::error::{EngineError, Result};
use crate::select::{served_count, SelectParams, SelectionPart, Threshold};

/// The band route answers requests at zooms below this; the scan answers the rest.
///
/// Measured 2026-10-02 on GeoNames (13.5 million rows) and the 64-partition GBIF slice (25.8
/// million), with whole-map requests at zooms 0 to 9 and 289-tile windows at zooms 8 to 14, for
/// principals seeing 1% to 100%, warm and cold. Wherever the bands answered tiles the request took
/// the same time or less and served the same bytes: the 100% principal on the GBIF slice took
/// 144 ms by the scan and 3 ms by the bands at zoom 5, and the 85% one 116 ms and 61 ms at zoom 9.
/// Where the threshold is wider than the widest band the route declines each tile and costs what
/// the scan costs. On those corpora no tile deeper than zoom 9 was answered from the bands, so the
/// deeper zooms keep the scan until a larger corpus measures them.
pub const BANDS_BELOW_ZOOM: u8 = 10;

/// What the bands answer for one tile.
pub(crate) enum BandAnswer {
    /// The tile's served rows, in view row space and ascending by `tessera_id`, and each one's
    /// entry in its segment's bands.
    Served {
        rows: Vec<u32>,
        entries: Vec<u32>,
        entries_read: u64,
    },
    /// The widest band holds fewer than `m` of the tile's visible rows: its few visible rows are
    /// read from the identity column.
    Sparse,
    /// No band holds every row below the threshold (a saturated or wide threshold), or the
    /// request serves no point: the shipped scan answers.
    Declined,
}

/// The tile `parts` under `mask`, answered from the bands where they can answer it. `matched` is
/// the tile's count in the set selection draws from, `Σ part.visible`.
pub(crate) fn select(
    mask: &EffectiveMask,
    parts: &[SelectionPart<'_>],
    params: &SelectParams,
    matched: u64,
) -> Result<BandAnswer> {
    let Threshold::Cut(cut) = params.threshold else {
        return Ok(BandAnswer::Declined);
    };
    if params.cap == 0 || matched == 0 {
        return Ok(BandAnswer::Declined);
    }
    // The narrowest band holding every identity below the cut.
    let holding = cut.leading_zeros();
    if holding < FIRST_BAND {
        return Ok(BandAnswer::Declined);
    }
    // Identities are uniform, so band `j` holds about `matched / 2^j` of the tile's visible rows.
    // Start at the narrowest band expected to hold `m` of them; a wider one is always as exact.
    let expected_below = ((u128::from(matched) * u128::from(cut)) >> 64) as u64;
    let expected_m = served_count(expected_below, params, matched).max(1) as u64;
    let ratio = matched / expected_m;
    if ratio == 0 {
        return Ok(BandAnswer::Sparse);
    }
    let start = holding.min(63 - ratio.leading_zeros());
    if start < FIRST_BAND {
        return Ok(BandAnswer::Sparse);
    }

    let mut candidates: Vec<(u64, u32, u32)> = Vec::new();
    let mut entries_read = 0u64;
    for band in (FIRST_BAND..=start).rev() {
        candidates.clear();
        let mut below = 0u64;
        for part in parts {
            let bands = &part.segment.bands;
            bands.advise_random();
            let span = bands.band(band);
            let (rows, ids) = (bands.rows(), bands.ids());
            let lo = span.start + rows[span.clone()].partition_point(|&r| r < part.range.start);
            let hi = span.start + rows[span.clone()].partition_point(|&r| r < part.range.end);
            bands.will_need(rows, lo..hi);
            bands.will_need(ids, lo..hi);
            let whole = part.visible == u64::from(part.range.end - part.range.start);
            for e in lo..hi {
                let row = rows[e];
                // The entries are trusted only as far as `tessera verify --deep` checked them, so a
                // row is checked against the part, and the segment, before it names anything.
                if !part.range.contains(&row) {
                    return Err(EngineError::Malformed(format!(
                        "segment '{}' has a band entry naming row {row}, outside the rows {:?} it \
                         was found among; run `tessera verify --deep` on the bundle and rebuild it",
                        part.segment.seg_id, part.range
                    )));
                }
                let view_row = part.row_base + row;
                if whole || mask.contains_row(view_row) {
                    let id = ids[e];
                    below += u64::from(id < cut);
                    candidates.push((id, view_row, e as u32));
                }
            }
            entries_read += (hi - lo) as u64;
        }
        let m = served_count(below, params, matched);
        if candidates.len() >= m {
            if m < candidates.len() {
                candidates.select_nth_unstable(m);
                candidates.truncate(m);
            }
            candidates.sort_unstable();
            return Ok(BandAnswer::Served {
                rows: candidates.iter().map(|&(_, row, _)| row).collect(),
                entries: candidates.iter().map(|&(_, _, e)| e).collect(),
                entries_read,
            });
        }
    }
    Ok(BandAnswer::Sparse)
}
