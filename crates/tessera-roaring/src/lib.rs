//! Builds a croaring bitmap from finished containers instead of inserting values.
//!
//! A Roaring bitmap splits the `u32` universe into blocks of 2¹⁶ by the high bits. The callers
//! already hold a block's members in the form croaring stores them, as 1,024 words or as a sorted
//! list, and croaring has no API that accepts a container. The portable serialization format does:
//! a cookie, a `(key, cardinality − 1)` descriptor per container, an offset table, then the
//! payloads. [`Sink`] writes that stream and deserialises it, which costs a validated copy per
//! container against an insertion per value.
//!
//! A mistake in the stream would be a wrong mask, not a crash. croaring validates what it
//! deserialises, and a stream it refuses is rebuilt by inserting the staged members, so a format
//! error costs time and never members.
//!
//! The crate sits below `tessera-filter` and `tessera-store` because both use it and neither may
//! depend on the other.

use croaring::{Bitmap, Portable};

/// Values in one Roaring block.
pub const BLOCK: usize = 1 << 16;
/// 64-bit words in one block's bitset container.
pub const WORDS: usize = BLOCK / 64;
/// A container holding more than this many members is a bitset; otherwise a sorted `u16` array.
/// A caller holding a sorted list uses [`Sink::push_members`] up to here and stamps words for
/// [`Sink::push_block`] above it.
pub const ARRAY_MAX: u32 = 4096;
/// Containers staged before a stream is handed to croaring, which bounds the staged bytes at
/// about a megabyte.
const STAGE: usize = 128;
/// The portable format's cookie for a stream with no run containers.
const COOKIE_NO_RUN: u32 = 12346;

/// Stages containers and folds them into a bitmap a stream at a time.
///
/// Keys may arrive in any order and may repeat; the result is the union. Ascending keys are the
/// fast case, because a key that does not ascend ends the stream being staged.
pub struct Sink {
    keys: Vec<u16>,
    cards: Vec<u32>,
    starts: Vec<u32>,
    payload: Vec<u8>,
    stream: Vec<u8>,
    out: Bitmap,
    stage: usize,
}

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
            stage: STAGE,
        }
    }

    /// A sink that stages every container until [`Self::finish`], for a caller building many
    /// small bitmaps at once.
    ///
    /// Every flush after the first is a union, which copies each container a second time; with a
    /// few hundred members a container that copy is most of the cost. The price is memory: the
    /// sink holds the whole bitmap's serialized bytes, up to 512 MB.
    pub fn unstaged() -> Self {
        Sink {
            stage: usize::MAX,
            ..Sink::new()
        }
    }

    /// Stage the container for block `key` from its bitset words. `card` is the words' popcount;
    /// an empty block stages nothing.
    // The workspace builds without LTO, so without this the call does not inline into the
    // filter scan or the projection.
    #[inline]
    pub fn push_block(&mut self, key: u16, card: u32, words: &[u64; WORDS]) {
        if card == 0 {
            return;
        }
        let start = self.begin(key);
        if card > ARRAY_MAX {
            self.payload.reserve(WORDS * 8);
            for w in words {
                self.payload.extend_from_slice(&w.to_le_bytes());
            }
            self.stage(key, card, start);
            return;
        }
        // croaring reads a payload as the descriptor's cardinality says, so at or below the
        // threshold the words are written out as the sorted array it expects.
        for (wi, &w0) in words.iter().enumerate() {
            let mut w = w0;
            while w != 0 {
                let low = (wi as u32) * 64 + w.trailing_zeros();
                self.payload.extend_from_slice(&(low as u16).to_le_bytes());
                w &= w - 1;
            }
        }
        // The descriptor takes the count written, so a wrong `card` cannot make croaring read a
        // prefix of the array as the whole container.
        let written = (self.payload.len() - start) as u32 / 2;
        debug_assert_eq!(card, written, "card is the words' popcount");
        if written == 0 {
            return;
        }
        self.stage(key, written, start);
    }

    /// Stage the container for block `key` from its members, ascending. Only the low 16 bits of
    /// a member are read, so absolute values and offsets within the block both serve. A repeated
    /// member is staged once.
    ///
    /// This is the cheaper form for a caller that holds a sorted list, since [`Self::push_block`]
    /// reads all 1,024 words however few members there are. Above [`ARRAY_MAX`] members the
    /// format wants a bitset, so the words are stamped here and handed to `push_block`.
    pub fn push_members(&mut self, key: u16, members: &[u32]) {
        debug_assert!(
            members.windows(2).all(|pair| pair[0] <= pair[1]),
            "push_members takes an ascending run"
        );
        if members.is_empty() {
            return;
        }
        if members.len() > ARRAY_MAX as usize {
            let mut words = [0u64; WORDS];
            let mut card = 0u32;
            for value in members {
                let low = (*value & 0xFFFF) as usize;
                let bit = 1u64 << (low & 63);
                if words[low >> 6] & bit == 0 {
                    words[low >> 6] |= bit;
                    card += 1;
                }
            }
            self.push_block(key, card, &words);
            return;
        }
        let start = self.begin(key);
        self.payload.reserve(members.len() * 2);
        let mut card = 0u32;
        // Outside the `u32` universe, so the first member is never a repeat.
        let mut last = u64::MAX;
        for value in members {
            if last == u64::from(*value) {
                continue;
            }
            last = u64::from(*value);
            card += 1;
            self.payload
                .extend_from_slice(&((*value & 0xFFFF) as u16).to_le_bytes());
        }
        self.stage(key, card, start);
    }

    /// Where the next container's payload starts. A key that does not ascend flushes first: the
    /// format requires strictly ascending keys within a stream, which also caps a stream at 2¹⁶
    /// containers and keeps its offsets inside the `u32` the format writes them in.
    #[inline]
    fn begin(&mut self, key: u16) -> usize {
        if self.keys.last().is_some_and(|last| *last >= key) {
            self.flush();
        }
        self.payload.len()
    }

    #[inline]
    fn stage(&mut self, key: u16, card: u32, start: usize) {
        self.keys.push(key);
        self.cards.push(card);
        self.starts.push(start as u32);
        if self.keys.len() >= self.stage {
            self.flush();
        }
    }

    /// Hand the staged containers to croaring and union them into the result.
    fn flush(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        self.assemble();
        let part = Bitmap::try_deserialize::<Portable>(&self.stream).unwrap_or_else(|| {
            debug_assert!(false, "the packed stream must be a valid portable bitmap");
            self.staged_by_value()
        });
        // The first stream becomes the result: a union would copy every container again.
        if self.out.is_empty() {
            self.out = part;
        } else {
            self.out.or_inplace(&part);
        }
        self.keys.clear();
        self.cards.clear();
        self.starts.clear();
        self.payload.clear();
    }

    /// Write the portable stream for the staged containers. This cookie requires the offset
    /// table whatever the container count.
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

    /// The staged containers as a bitmap built by insertion: what [`Self::flush`] uses if
    /// croaring refuses the stream.
    fn staged_by_value(&self) -> Bitmap {
        let mut out = Bitmap::new();
        let mut members: Vec<u32> = Vec::new();
        for (i, &key) in self.keys.iter().enumerate() {
            let start = self.starts[i] as usize;
            let end = self
                .starts
                .get(i + 1)
                .map_or(self.payload.len(), |s| *s as usize);
            let base = u32::from(key) << 16;
            members.clear();
            if self.cards[i] > ARRAY_MAX {
                for (wi, w) in self.payload[start..end].as_chunks::<8>().0.iter().enumerate() {
                    let mut w = u64::from_le_bytes(*w);
                    while w != 0 {
                        members.push(base + (wi as u32) * 64 + w.trailing_zeros());
                        w &= w - 1;
                    }
                }
            } else {
                for pair in self.payload[start..end].as_chunks::<2>().0 {
                    let low = u16::from_le_bytes(*pair);
                    members.push(base + u32::from(low));
                }
            }
            out.add_many(&members);
        }
        out
    }

    /// The union of every container staged.
    pub fn finish(mut self) -> Bitmap {
        self.flush();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One block's members as words and their popcount.
    fn stamp(members: &[u32]) -> ([u64; WORDS], u32) {
        let mut words = [0u64; WORDS];
        for value in members {
            let low = (*value & 0xFFFF) as usize;
            words[low >> 6] |= 1u64 << (low & 63);
        }
        let card = words.iter().map(|w| w.count_ones()).sum();
        (words, card)
    }

    /// `card` members of block `key`, spread so the array payload is not the identity.
    fn spread(key: u16, card: usize) -> Vec<u32> {
        let base = u32::from(key) << 16;
        (0..card).map(|i| base + (i as u32) * 7).collect()
    }

    /// Either side of the array/bitset threshold, the single member, and the first and last block.
    #[test]
    fn both_forms_stage_the_set_inserting_would_build() {
        for card in [1usize, 2, 4095, 4096, 4097, 9000] {
            for key in [0u16, u16::MAX] {
                let members = spread(key, card);
                let (words, popcount) = stamp(&members);
                let want: Bitmap = members.iter().copied().collect();
                let mut listed = Sink::new();
                listed.push_members(key, &members);
                assert_eq!(listed.finish(), want, "the list form differs at {card}");
                let mut stamped = Sink::new();
                stamped.push_block(key, popcount, &words);
                assert_eq!(stamped.finish(), want, "the words form differs at {card}");
            }
        }
    }

    #[test]
    fn a_repeated_member_is_one_member() {
        let at = 3u32 << 16;
        let want: Bitmap = [at + 4, at + 9].into_iter().collect();
        let mut sink = Sink::new();
        sink.push_members(3, &[at + 4, at + 4, at + 9]);
        assert_eq!(sink.finish(), want);

        // Enough repeats to cross into the stamped path while the set stays an array.
        let mut run = vec![at + 4; ARRAY_MAX as usize + 1];
        run.push(at + 9);
        let mut sink = Sink::new();
        sink.push_members(3, &run);
        assert_eq!(sink.finish(), want);
    }

    #[test]
    fn an_empty_block_contributes_no_container() {
        let mut sink = Sink::new();
        sink.push_members(1, &spread(1, 10));
        sink.push_block(2, 0, &[0u64; WORDS]);
        sink.push_members(2, &[]);
        sink.push_members(3, &spread(3, 5000));
        let want: Bitmap = spread(1, 10).into_iter().chain(spread(3, 5000)).collect();
        assert_eq!(sink.finish(), want);
        assert!(Sink::new().finish().is_empty());
    }

    /// More containers than a stage holds, gaps between keys, and both encodings in each stream.
    /// The unstaged sink takes them as one stream and must agree.
    #[test]
    fn a_result_spanning_several_streams_is_complete() {
        let mut staged = Sink::new();
        let mut unstaged = Sink::unstaged();
        let mut want = Bitmap::new();
        for key in (0..STAGE as u16 * 2 + 5).map(|k| k * 3) {
            let members = spread(key, if key % 2 == 0 { 300 } else { 5000 });
            let (words, card) = stamp(&members);
            staged.push_block(key, card, &words);
            unstaged.push_members(key, &members);
            want.add_many(&members);
        }
        assert_eq!(staged.finish(), want);
        assert_eq!(unstaged.finish(), want);
    }

    #[test]
    fn keys_out_of_order_or_repeated_give_the_union() {
        let at = 5u32 << 16;
        let mut sink = Sink::unstaged();
        let mut want = Bitmap::new();
        for (key, members) in [
            (9u16, spread(9, 5000)),
            (5, vec![at + 1, at + 2]),
            (5, vec![at + 2, at + 3]),
            (2, spread(2, 40)),
        ] {
            sink.push_members(key, &members);
            want.add_many(&members);
        }
        assert_eq!(sink.finish(), want);
    }

    #[test]
    fn the_by_value_fallback_builds_what_the_stream_does() {
        let mut sink = Sink::new();
        let mut want = Bitmap::new();
        for (key, card) in [(0u16, 70), (1, 5000), (4, 4096)] {
            let members = spread(key, card);
            sink.push_members(key, &members);
            want.add_many(&members);
        }
        assert_eq!(sink.staged_by_value(), want);
        assert_eq!(sink.finish(), want);
    }
}
