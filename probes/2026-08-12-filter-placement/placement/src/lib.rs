//! Fixtures shared by the campaign's arms: a permutation, the mask shapes, and a viewport.
//!
//! **The permutation is a genuine Fisher–Yates shuffle**, for the reason
//! `probes/2026-08-11-viewport-crossing/` gives: every route here is dominated by how the two
//! orders relate, and a structured map (a multiplicative shuffle, say) would flatter them all
//! equally and wrongly. Entity ids are assigned in permission-signature order and rows in Morton
//! order, so the two are uncorrelated by construction.

use std::ops::Range;

use croaring::Bitmap;

#[inline]
pub fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `(entity_to_row, row_to_entity)` — a genuine shuffle, so neither direction is contiguous.
pub fn permutation(n: usize) -> (Vec<u32>, Vec<u32>) {
    let mut entity_to_row: Vec<u32> = (0..n as u32).collect();
    let mut state = 0x5DEE_CE66_D1CE_u64;
    for i in (1..n).rev() {
        state = splitmix(state);
        let j = (state % (i as u64 + 1)) as usize;
        entity_to_row.swap(i, j);
    }
    let mut row_to_entity = vec![0u32; n];
    for (entity, &row) in entity_to_row.iter().enumerate() {
        row_to_entity[row as usize] = entity as u32;
    }
    (entity_to_row, row_to_entity)
}

/// The shape of an authorised set in **entity space**.
///
/// Three, because the design's cost model turns on which one a principal has: ids are assigned in
/// permission-signature order, so a principal holding one broad grant is a run and one holding many
/// narrow grants is scattered. The middle shape — blocks of ~2¹⁶ — is what the category-membership
/// campaign found realistic for a principal composed of a handful of terms.
#[derive(Clone, Copy, Debug)]
pub enum MaskShape {
    /// One run from the start: the cheapest shape anything entity-space can be handed.
    Contiguous,
    /// Runs of `BLOCK` entities, spread to hit `share` of the corpus.
    Blocked,
    /// Every *k*-th entity: Roaring's worst case, and what many narrow grants produce.
    Scattered,
}

impl MaskShape {
    pub fn name(self) -> &'static str {
        match self {
            MaskShape::Contiguous => "contiguous",
            MaskShape::Blocked => "blocked",
            MaskShape::Scattered => "scattered",
        }
    }
}

/// A block is one Roaring container wide, so a blocked mask is containers-full-or-empty — the shape
/// that makes `and_cardinality` cost O(containers touched) rather than O(cardinality).
const BLOCK: u32 = 1 << 16;

pub fn mask(n: usize, shape: MaskShape, share: f64) -> Bitmap {
    let mut out = Bitmap::new();
    match shape {
        MaskShape::Contiguous => {
            out.add_range(0..(n as f64 * share) as u32);
        }
        MaskShape::Blocked => {
            let stride = (BLOCK as f64 / share) as u32;
            let mut lo = 0u32;
            while (lo as usize) < n {
                let hi = (lo + BLOCK).min(n as u32);
                out.add_range(lo..hi);
                lo = lo.saturating_add(stride);
                if stride == 0 {
                    break;
                }
            }
        }
        MaskShape::Scattered => {
            let step = (1.0 / share).max(1.0) as u64;
            let mut v: Vec<u32> = Vec::with_capacity((n as f64 * share) as usize + 1024);
            let mut e = 0u64;
            let mut state = 0x1234_5678u64;
            while e < n as u64 {
                v.push(e as u32);
                state = splitmix(state);
                e += 1 + state % (2 * step - 1);
            }
            out.add_many(&v);
        }
    }
    out.run_optimize();
    out
}

/// The row ranges a request's tiles span, merged as `crossing_domain` merges them: a viewport is
/// adjacent Morton ranges, so what a route walks is a handful of long runs rather than one walk per
/// tile.
pub fn viewport(n: usize, tiles: usize, width: u32) -> Vec<Range<u32>> {
    let stride = (n / tiles) as u32;
    (0..tiles as u32)
        .map(|t| {
            let lo = t * stride;
            let hi = (lo + width).min(n as u32);
            lo..hi
        })
        .collect()
}

pub fn rows_in(ranges: &[Range<u32>]) -> u64 {
    ranges.iter().map(|r| (r.end - r.start) as u64).sum()
}

pub fn range_bitmap(ranges: &[Range<u32>]) -> Bitmap {
    let mut b = Bitmap::new();
    for r in ranges {
        b.add_range(r.clone());
    }
    b.run_optimize();
    b
}

/// `Permutation::project`'s algorithm — gather, sort, bulk-add — run single-threaded.
///
/// The shipped one is parallel over the result; every route in this campaign is timed
/// single-threaded so the ratios are comparable, and both routes parallelise over their own axis.
pub fn project(mask: &Bitmap, entity_to_row: &[u32]) -> Bitmap {
    let mut rows: Vec<u32> = mask.iter().map(|e| entity_to_row[e as usize]).collect();
    rows.sort_unstable();
    Bitmap::of(&rows)
}

pub fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}
