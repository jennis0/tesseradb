//! Handing croaring **finished containers** instead of inserting values into a bitmap.
//!
//! # Why this is a crate of its own
//!
//! Two callers in different layers need it. The filter index builds a scan's result in entity space
//! (`tessera-filter::values`); `Permutation::project` builds a session's authorised set in row space
//! (`tessera-store::permutation`). `check-layers.sh` denies filter → store, and store must not
//! acquire a dependency on the query-side filter index to reach a byte encoder, so the shared part
//! sits below both rather than inside either.
//!
//! # What it does
//!
//! A Roaring bitmap divides the `u32` universe into blocks of 2¹⁶ by the high bits, and stores a
//! block holding more than 4,096 members as a *bitset container*: a flat 8 KB of words. Both callers
//! already hold exactly those words — the filter because it evaluates its predicate into a bit per
//! entity, the projection because it stamps looked-up rows into a bit array — so the container is
//! not something to be built from their output. It **is** their output.
//!
//! # Why the format is written by hand
//!
//! croaring exposes no container-level API: the safe wrapper has none, and `croaring-sys` binds only
//! the public `roaring.h` surface. The one route that accepts a finished container is the **portable
//! serialization format**, which is stable and cross-language — a cookie, a `(key, cardinality − 1)`
//! descriptor per container, an offset table, then the payloads. Deserialising is a validated pass
//! and a memcpy per container, against an insertion per value.
//!
//! This is a real cost in reviewability and it is worth naming: a bug here does not crash, it
//! produces a **wrong mask** — which is a disclosure if it is too wide and a silently blanked map if
//! it is too narrow. Three things hold that down. The writer is small and total: every container it
//! emits is derived from one block's words and its popcount, with no path that can emit a key twice
//! or a cardinality that disagrees with the payload. A deserialization that fails **falls back to
//! inserting the block's entities** rather than yielding what it managed to parse (see
//! [`Sink::flush`]), so a format error can cost latency but cannot cost correctness. And the tests
//! assert the packed result equals the unpacked one across selectivities, both container encodings,
//! and the boundaries between them.
//!
//! # What it measured
//!
//! For the filter, at 10⁹ (`probes/2026-08-08-filter-layout/` arms 7 and 8): inserting matches one
//! at a time cost 3.4 s for a predicate matching a quarter of a whole-corpus candidate and 10.2 s at
//! three quarters, against a ruled budget of 0.5–1 s — where the scan itself is ~280 ms. For the
//! projection, at 10⁹ over a 25% grant (`probes/2026-08-14-project-decomposition/`): the whole
//! primitive went from 8,267 ms to 1,277 ms single-threaded, of which this encoder is the larger
//! half — the same buckets emitted through `add_many` instead measure 2,306 ms.
//!
//! Both callers reach it the same way and neither owns it, which is the point: the argument above is
//! made once.

use croaring::{Bitmap, Portable};


/// Values per Roaring block — the universe's high 16 bits. Entities for the filter index, rows
/// for the projection; the encoder does not care which.
pub const BLOCK: usize = 1 << 16;
/// 64-bit words in one block's bitset container.
pub const WORDS: usize = BLOCK / 64;
/// croaring's array/bitset threshold: a container holding more than this is stored as a bitset.
const ARRAY_MAX: u32 = 4096;
/// Containers staged before a stream is handed to croaring. Bounds the transient buffer at roughly
/// a megabyte rather than the whole serialized result, which at half of 10⁹ would be 125 MB.
const STAGE: usize = 128;
/// The portable format's cookie for a stream with no run containers.
const COOKIE_NO_RUN: u32 = 12346;

/// Accumulates finished blocks and folds them into a bitmap a stream at a time.
pub struct Sink {
    keys: Vec<u16>,
    cards: Vec<u32>,
    starts: Vec<u32>,
    payload: Vec<u8>,
    stream: Vec<u8>,
    out: Bitmap,
}

/// `new()` takes no arguments and carries no state a caller chooses, so `Default` is exactly it.
impl Default for Sink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink {
    pub fn new() -> Self {
        Sink {
            keys: Vec::with_capacity(STAGE),
            cards: Vec::with_capacity(STAGE),
            starts: Vec::with_capacity(STAGE),
            payload: Vec::new(),
            stream: Vec::new(),
            out: Bitmap::new(),
        }
    }

    /// Stage the container for the block whose entities begin at `key << 16`.
    ///
    /// `card` must be `words`' popcount. A block with no matches is not a container — the format
    /// stores `cardinality − 1` in sixteen bits, so an empty one is unrepresentable — and is
    /// dropped here rather than at the call site.
    /// **`#[inline]` because this used to be in the caller's own crate.** Both callers had this
    /// function beside them before it was lifted here, so both were getting it inlined for free,
    /// and the workspace builds release with neither LTO nor a single codegen unit — so a plain
    /// `pub fn` would not cross the boundary. The attribute restores what they had rather than
    /// buying something new, which matters most to the filter scan: its measured figures were taken
    /// with this code in-crate, and a silent regression there would not have shown up in the
    /// projection's numbers at all. With it, the projection through this crate measured within a
    /// few percent of the same code compiled beside its caller.
    #[inline]
    pub fn push_block(&mut self, key: u16, card: u32, words: &[u64; WORDS]) {
        if card == 0 {
            return;
        }
        self.keys.push(key);
        self.cards.push(card);
        self.starts.push(self.payload.len() as u32);
        if card > ARRAY_MAX {
            // Reserved once rather than grown per word: the length is known exactly, and a capacity
            // check per 8 bytes is what stops this becoming the single copy it is.
            self.payload.reserve(WORDS * 8);
            for w in words {
                self.payload.extend_from_slice(&w.to_le_bytes());
            }
        } else {
            // Below the threshold croaring expects a sorted `u16` array, and emitting a bitset
            // instead would be a *format* error rather than a size one: the deserializer chooses how
            // to read a payload from the cardinality, not from what we wrote.
            for (wi, &w0) in words.iter().enumerate() {
                let mut w = w0;
                while w != 0 {
                    let low = (wi as u32) * 64 + w.trailing_zeros();
                    self.payload.extend_from_slice(&(low as u16).to_le_bytes());
                    w &= w - 1;
                }
            }
        }
        if self.keys.len() == STAGE {
            self.flush();
        }
    }

    /// Hand the staged containers to croaring and union them into the result.
    ///
    /// **A stream croaring refuses is rebuilt entity by entity rather than dropped.** That path
    /// should be unreachable — it would mean this module wrote a malformed stream — but the failure
    /// it guards against is serving a mask that is missing entities, which is indistinguishable from
    /// a correct answer and which the whole design exists to prevent. Costing latency to stay
    /// correct is the right trade, and `debug_assert` makes it loud where a test can see it.
    fn flush(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        self.assemble();
        match Bitmap::try_deserialize::<Portable>(&self.stream) {
            Some(bitmap) => self.out.or_inplace(&bitmap),
            None => {
                debug_assert!(false, "the packed stream must be a valid portable bitmap");
                self.rebuild_staged();
            }
        }
        self.keys.clear();
        self.cards.clear();
        self.starts.clear();
        self.payload.clear();
    }

    /// The portable stream: cookie, container count, `(key, cardinality − 1)` descriptors, the
    /// offset table, then the payloads in the same order.
    ///
    /// The offset table is written unconditionally, which is what this cookie requires: croaring
    /// reads it whenever the stream declares no run containers, whatever the container count.
    fn assemble(&mut self) {
        self.stream.clear();
        let size = self.keys.len() as u32;
        self.stream.extend_from_slice(&COOKIE_NO_RUN.to_le_bytes());
        self.stream.extend_from_slice(&size.to_le_bytes());
        for (key, card) in self.keys.iter().zip(&self.cards) {
            self.stream.extend_from_slice(&key.to_le_bytes());
            self.stream
                .extend_from_slice(&((card - 1) as u16).to_le_bytes());
        }
        // 4 cookie + 4 count + 4 per descriptor + 4 per offset.
        let base = 8 + 8 * size;
        for start in &self.starts {
            self.stream.extend_from_slice(&(base + start).to_le_bytes());
        }
        self.stream.extend_from_slice(&self.payload);
    }

    /// The correctness fallback: read the staged payloads back and add their entities directly.
    fn rebuild_staged(&mut self) {
        for (i, &key) in self.keys.iter().enumerate() {
            let start = self.starts[i] as usize;
            let end = self
                .starts
                .get(i + 1)
                .map_or(self.payload.len(), |s| *s as usize);
            let base = u32::from(key) << 16;
            let card = self.cards[i];
            let mut entities: Vec<u32> = Vec::with_capacity(card as usize);
            if card > ARRAY_MAX {
                for (wi, w) in self.payload[start..end].chunks_exact(8).enumerate() {
                    let mut w = u64::from_le_bytes(w.try_into().expect("eight bytes"));
                    while w != 0 {
                        entities.push(base + (wi as u32) * 64 + w.trailing_zeros());
                        w &= w - 1;
                    }
                }
            } else {
                for pair in self.payload[start..end].chunks_exact(2) {
                    let low = u16::from_le_bytes(pair.try_into().expect("two bytes"));
                    entities.push(base + u32::from(low));
                }
            }
            self.out.add_many(&entities);
        }
    }

    /// The result of every block staged. Empty when nothing was packed, which is the ordinary
    /// answer for a candidate with no whole blocks in it.
    pub fn finish(mut self) -> Bitmap {
        self.flush();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the same set both ways — packed, and by direct insertion — and require agreement.
    /// `density` picks which entities of each block match.
    fn agrees(blocks: &[u16], density: impl Fn(u16, usize) -> bool) {
        let mut sink = Sink::new();
        let mut want = Bitmap::new();
        for &key in blocks {
            let mut words = [0u64; WORDS];
            let mut card = 0u32;
            for slot in 0..BLOCK {
                if density(key, slot) {
                    words[slot >> 6] |= 1u64 << (slot & 63);
                    card += 1;
                    want.add((u32::from(key) << 16) + slot as u32);
                }
            }
            sink.push_block(key, card, &words);
        }
        assert_eq!(sink.finish(), want);
    }

    /// A bitset container — above croaring's 4,096 threshold, where the payload is the words
    /// themselves.
    #[test]
    fn a_dense_block_round_trips_as_a_bitset_container() {
        agrees(&[0], |_, slot| slot % 2 == 0);
    }

    /// An array container — at or below the threshold, where the payload is sorted `u16`s. Writing
    /// the wrong encoding for the cardinality is the format error most likely to go unnoticed,
    /// because croaring picks how to read a payload from the descriptor rather than from the bytes.
    #[test]
    fn a_sparse_block_round_trips_as_an_array_container() {
        agrees(&[3], |_, slot| slot % 1_000 == 0);
    }

    /// The array/bitset boundary from both sides, and the single-member block.
    #[test]
    fn the_container_encoding_boundary_is_exact() {
        for card in [1usize, 4_095, 4_096, 4_097] {
            agrees(&[7], move |_, slot| slot < card);
        }
    }

    /// A block with no matches is not a container: the format cannot express one, and emitting it
    /// would shift every following descriptor.
    #[test]
    fn an_empty_block_contributes_no_container() {
        let mut sink = Sink::new();
        sink.push_block(4, 0, &[0u64; WORDS]);
        assert!(sink.finish().is_empty());

        // ...and does not disturb the blocks either side of it.
        agrees(&[1, 2, 3], |key, slot| key != 2 && slot % 3 == 0);
    }

    /// More blocks than one stream stages, so the sink flushes and unions several times — and the
    /// keys are non-contiguous, which a stream that assumed density would get wrong.
    #[test]
    fn a_result_spanning_several_streams_is_complete() {
        let keys: Vec<u16> = (0..(STAGE as u16 * 2 + 5)).map(|k| k * 3).collect();
        agrees(&keys, |key, slot| slot % (usize::from(key % 7) + 2) == 0);
    }

    /// Both encodings inside one stream, which is the ordinary case for a real filter: density
    /// varies block to block and croaring must read each payload the way its descriptor says.
    #[test]
    fn one_stream_carries_both_encodings() {
        agrees(&[0, 1, 2, 3], |key, slot| {
            if key % 2 == 0 {
                slot % 2 == 0 // dense: bitset
            } else {
                slot % 5_000 == 0 // sparse: array
            }
        });
    }

    /// The fallback must produce the same answer as the format path — it is the reason a malformed
    /// stream cannot become a wrong mask, so it is exercised directly rather than trusted.
    #[test]
    fn the_rebuild_fallback_agrees_with_the_format_path() {
        for sparse in [true, false] {
            let mut packed = Sink::new();
            let mut rebuilt = Sink::new();
            let mut want = Bitmap::new();
            for key in 0u16..3 {
                let mut words = [0u64; WORDS];
                let mut card = 0;
                for slot in 0..BLOCK {
                    let hit = if sparse {
                        slot % 900 == 0
                    } else {
                        slot % 2 == 0
                    };
                    if hit {
                        words[slot >> 6] |= 1u64 << (slot & 63);
                        card += 1;
                        want.add((u32::from(key) << 16) + slot as u32);
                    }
                }
                packed.push_block(key, card, &words);
                rebuilt.push_block(key, card, &words);
            }
            rebuilt.rebuild_staged();
            rebuilt.keys.clear();
            rebuilt.cards.clear();
            rebuilt.starts.clear();
            rebuilt.payload.clear();
            assert_eq!(packed.finish(), want);
            assert_eq!(rebuilt.finish(), want, "sparse={sparse}");
        }
    }

    /// **The block path is taken at all**, which every test above is blind to: they compare the
    /// finished bitmap with one built by insertion, and an implementation that quietly *became*
    /// that insertion would satisfy all of them while giving up the whole reason this crate
    /// exists — `CLAUDE.md`'s measured cost model, bitmap operations costing O(containers
    /// touched) rather than O(cardinality). The projection measured 8,267 ms against 1,277 ms at
    /// 10⁹ on exactly that difference.
    ///
    /// Asserted structurally rather than by timing: `correctness-suite.md` §17 keeps timing
    /// assertions out of tests that run on developer machines. The observables are the staged
    /// container itself — one descriptor per non-empty block, a payload of exactly the width its
    /// cardinality selects, and `out` still **empty**, because a sink that inserted values would
    /// have populated it before any stream was assembled — and then the stream croaring is
    /// actually handed, which must deserialise, since `flush`'s fallback to `rebuild_staged` is
    /// the per-value path this test exists to keep off the hot route.
    ///
    /// Mutations this kills: `push_block` rewritten to stamp entities into `out` directly;
    /// emitting a bitset payload for an array container or the reverse; a descriptor or offset
    /// that disagrees with the payload, which croaring refuses and `flush` then answers correctly
    /// but slowly.
    #[test]
    fn a_block_is_staged_as_a_container_rather_than_inserted_value_by_value() {
        let mut sink = Sink::new();
        let mut want = Bitmap::new();
        // Two blocks, one either side of the array/bitset threshold, and fewer than `STAGE` so
        // nothing flushes underneath the assertions.
        for (key, step) in [(0u16, 2usize), (5u16, 1_000usize)] {
            let mut words = [0u64; WORDS];
            let mut card = 0u32;
            for slot in (0..BLOCK).step_by(step) {
                words[slot >> 6] |= 1u64 << (slot & 63);
                card += 1;
                want.add((u32::from(key) << 16) + slot as u32);
            }
            sink.push_block(key, card, &words);
        }

        assert_eq!(
            sink.keys,
            vec![0u16, 5],
            "one descriptor per non-empty block"
        );
        assert!(
            sink.cards[0] > ARRAY_MAX && sink.cards[1] <= ARRAY_MAX,
            "the fixture must straddle the threshold, or this test proves nothing: {:?}",
            sink.cards
        );
        assert_eq!(
            sink.payload.len(),
            WORDS * 8 + 2 * sink.cards[1] as usize,
            "the payload must be one bitset container's words followed by one array container's \
             u16s — a sink that inserted its entities would have staged no payload at all"
        );
        assert_eq!(
            sink.starts,
            vec![0u32, (WORDS * 8) as u32],
            "the second container must begin exactly where the first one ends"
        );
        assert!(
            sink.out.is_empty(),
            "no entity may reach the result before a stream is handed to croaring: value-by-value \
             insertion is what this crate exists to avoid"
        );

        // The stream `flush` hands croaring. It must deserialise: `flush`'s fallback answers
        // correctly by rebuilding entity by entity, so a stream croaring refuses is exactly the
        // regression this test is for, and `debug_assert` would only catch it in a debug build.
        sink.assemble();
        assert_eq!(sink.stream[0..4], COOKIE_NO_RUN.to_le_bytes());
        assert_eq!(sink.stream[4..8], 2u32.to_le_bytes());
        assert_eq!(sink.stream[8..10], 0u16.to_le_bytes(), "first key");
        assert_eq!(
            sink.stream[10..12],
            ((sink.cards[0] - 1) as u16).to_le_bytes(),
            "cardinality is stored one less than it is"
        );
        let deserialized = Bitmap::try_deserialize::<Portable>(&sink.stream)
            .expect("the assembled stream must be a portable bitmap, or flush rebuilds per value");
        assert_eq!(deserialized, want, "and it must carry the set it staged");

        assert_eq!(sink.finish(), want);
    }
}
