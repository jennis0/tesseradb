//! The filter scan's packing kernel: a predicate evaluated into a bit per entity.
//!
//! The container assembly this feeds — and the argument for writing the portable format by hand —
//! lives in [`tessera_roaring`], which `tessera-store` shares. What stays here is the kernel, which
//! is the filter's alone and whose inlining is measured against *this* crate's codegen units.

pub(crate) use tessera_roaring::{Sink, BLOCK, WORDS};

/// Evaluate `pred` over one block's values, one result bit per value, and return the popcount.
///
/// **Branchless by construction**: the bit is shifted in whether or not it is set, so the running
/// time does not depend on how many values match. That is what removes the misprediction term — a
/// predicate matching half its input mispredicts on roughly half of it — and it also makes the work
/// a function of the block alone.
///
/// **`inline(always)`, not `inline`, and the difference is 70%.** The kernel is one call per 65,536
/// values, so inlining it looks like a detail — but the predicate reaches it as `&mut impl FnMut`,
/// and only inlining turns that into the direct comparison the branchless loop is written around.
/// As a hint it is a codegen-unit lottery: adding an unrelated *module* to this crate (the value
/// column's writers) re-partitioned the crate's CGUs, `pack_run` stopped getting this body, and the
/// broad-candidate arm went from 13.3 to 22.1 ms at 2×10⁸ with not a line of the scan changed.
/// That is the same failure mode as `values.rs`'s two `inline(never)` markers, from the other
/// direction, and the fix is the same: state the inlining rather than hint at it.
#[inline(always)]
pub(crate) fn pack_block<T>(
    values: &[T],
    pred: &mut impl FnMut(&T) -> bool,
    words: &mut [u64; WORDS],
) -> u32 {
    debug_assert_eq!(values.len(), BLOCK, "a packed block is whole");
    let mut card = 0u32;
    for (wi, chunk) in values.as_chunks::<64>().0.iter().enumerate() {
        let mut w = 0u64;
        for (bi, v) in chunk.iter().enumerate() {
            w |= u64::from(pred(v)) << bi;
        }
        words[wi] = w;
        card += w.count_ones();
    }
    card
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's popcount is what the sink trusts for the encoding choice, so it must agree with
    /// the words it produced.
    #[test]
    fn the_kernel_counts_what_it_packed() {
        let values: Vec<u32> = (0..BLOCK as u32).collect();
        let mut words = [0u64; WORDS];
        let card = pack_block(&values, &mut |v| v % 3 == 0, &mut words);
        let counted: u32 = words.iter().map(|w| w.count_ones()).sum();
        assert_eq!(card, counted);
        assert_eq!(card, (BLOCK as u32).div_ceil(3));
        assert!(words[0] & 1 != 0, "entity 0 matches");
        assert!(words[0] & 2 == 0, "entity 1 does not");
    }

}
