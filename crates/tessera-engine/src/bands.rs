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

use tessera_store::bands::{band_below, FIRST_BAND};

use crate::compose::EffectiveMask;
use crate::error::{EngineError, Result};
use crate::select::{served_count, SelectParams, SelectionPart, Threshold};

/// The band route answers requests at zooms below this; the scan answers the rest. Where the
/// threshold is wider than the widest band the route declines each tile, at any zoom.
pub const BANDS_BELOW_ZOOM: u8 = 10;

/// What the bands answer for one tile.
#[derive(Debug, PartialEq, Eq)]
pub enum BandAnswer {
    /// The tile's served rows, in view row space and ascending by `tessera_id`, and each one's
    /// entry in its segment's bands. `widened` where the band first read held fewer than `m`
    /// visible entries and a wider one answered.
    Served {
        rows: Vec<u32>,
        entries: Vec<u32>,
        entries_read: u64,
        widened: bool,
    },
    /// The widest band holds fewer than `m` of the tile's visible rows: its few visible rows are
    /// read from the identity column. `read` where the bands were read before that was known,
    /// rather than expected from the tile's visible count.
    Sparse { read: bool },
    /// No band holds every row below the threshold (a saturated or wide threshold), or the
    /// request serves no point: the shipped scan answers.
    Declined,
}

/// The tile `parts` under `mask`, answered from the bands where they can answer it. `matched` is
/// the tile's count in the set selection draws from, `Σ part.visible`, and each part's `visible`
/// its own count, as [`crate::select::Selection::of`] takes them.
pub fn select(
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
    let Some(holding) = band_below(cut) else {
        return Ok(BandAnswer::Declined);
    };
    // Identities are uniform, so band `j` holds about `matched / 2^j` of the tile's visible rows.
    // Start at the narrowest band expected to hold `m` of them; a wider one is always as exact.
    let expected_below = ((u128::from(matched) * u128::from(cut)) >> 64) as u64;
    let expected_m = served_count(expected_below, params, matched).max(1) as u64;
    let ratio = matched / expected_m;
    if ratio == 0 {
        return Ok(BandAnswer::Sparse { read: false });
    }
    let start = holding.min(63 - ratio.leading_zeros());
    if start < FIRST_BAND {
        return Ok(BandAnswer::Sparse { read: false });
    }

    let mut candidates: Vec<(u64, u32, u32)> = Vec::new();
    let mut entries_read = 0u64;
    for band in (FIRST_BAND..=start).rev() {
        candidates.clear();
        let mut below = 0u64;
        for part in parts {
            let whole = part.visible == u64::from(part.range.end - part.range.start);
            entries_read += admitted_entries(
                part.segment,
                part.row_base,
                band,
                part.range.clone(),
                |view_row| whole || mask.contains_row(view_row),
                |e, view_row, id| {
                    below += u64::from(id < cut);
                    candidates.push((id, view_row, e as u32));
                },
            )?;
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
                widened: band < start,
            });
        }
    }
    Ok(BandAnswer::Sparse { read: true })
}

/// Each entry of `segment`'s band `band` whose row lies in `range`, segment-local, and whose view
/// row (`row_base` plus the row) `admits`, in entry order: `each(entry, view_row, tessera_id)`.
/// Answers how many entries were read. The mask, or a set composed under it, is what `admits`
/// asks, so the band decides only which rows are read, never which are visible.
pub(crate) fn admitted_entries(
    segment: &tessera_store::read::SegmentData,
    row_base: u32,
    band: u32,
    range: std::ops::Range<u32>,
    admits: impl Fn(u32) -> bool,
    mut each: impl FnMut(usize, u32, u64),
) -> Result<u64> {
    let bands = &segment.bands;
    bands.advise_random();
    let span = bands.band(band);
    let (rows, ids) = (bands.rows(), bands.ids());
    let lo = span.start + rows[span.clone()].partition_point(|&r| r < range.start);
    let hi = span.start + rows[span.clone()].partition_point(|&r| r < range.end);
    bands.will_need(rows, lo..hi);
    bands.will_need(ids, lo..hi);
    for e in lo..hi {
        let row = rows[e];
        // The entries are trusted only as far as `tessera verify --deep` checked them, so a row is
        // checked against the range, and the segment, before it names anything.
        if !range.contains(&row) {
            return Err(EngineError::Malformed(format!(
                "segment '{}' has a band entry naming row {row}, outside the rows {range:?} it was \
                 found among; run `tessera verify --deep` on the bundle and rebuild it",
                segment.seg_id
            )));
        }
        let view_row = row_base + row;
        if admits(view_row) {
            each(e, view_row, ids[e]);
        }
    }
    Ok((hi - lo) as u64)
}
