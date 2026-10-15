//! **Which ordinals of a level a per-shard file lists, and where each listed one is.**
//!
//! A shard holds something of a few of its level's artifacts, and a file with an entry per ordinal
//! pays for every one it holds nothing of. This index is one bit per ordinal and a running count
//! per 64, so a file lists only what its shard holds and a reader finds a listed ordinal's entry in
//! one population count.
//!
//! ```text
//! words := ceil(ordinals / 64) × u64 LE   -- bit `o % 64` of word `o / 64` is set where `o` is listed
//! ranks := ceil(ordinals / 64) × u32 LE   -- how many ordinals the words before this one list
//! ```

use crate::{le_u32, le_u64};

fn words(ordinals: u32) -> usize {
    ordinals.div_ceil(64) as usize
}

/// The index's length for a level of `ordinals`.
pub(crate) fn len(ordinals: u32) -> usize {
    words(ordinals) * 12
}

/// The index of a level of `ordinals`, built from the ordinals it lists in ascending order.
#[derive(Debug)]
pub(crate) struct PresenceWriter {
    words: Vec<u64>,
    listed: u32,
}

impl PresenceWriter {
    pub(crate) fn new(ordinals: u32) -> Self {
        PresenceWriter {
            words: vec![0; words(ordinals)],
            listed: 0,
        }
    }

    /// List `ordinal`. Ordinals arrive ascending, each at most once.
    pub(crate) fn list(&mut self, ordinal: u32) {
        self.words[ordinal as usize / 64] |= 1 << (ordinal % 64);
        self.listed += 1;
    }

    /// How many ordinals are listed: the next one's entry.
    pub(crate) fn listed(&self) -> u32 {
        self.listed
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.words.len() * 12);
        for word in &self.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
        let mut before = 0u32;
        for word in &self.words {
            out.extend_from_slice(&before.to_le_bytes());
            before += word.count_ones();
        }
        out
    }
}

/// An index of a level of `ordinals`, at `at` in `bytes`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Presence {
    at: usize,
    ordinals: u32,
}

impl Presence {
    /// The index at `at`, checked: no bit past the level, each running count the sum of the words
    /// before it, and `listed` ordinals in all. `None` where it is not.
    pub(crate) fn check(bytes: &[u8], at: usize, ordinals: u32, listed: u32) -> Option<Self> {
        if at.checked_add(len(ordinals))? > bytes.len() {
            return None;
        }
        let presence = Presence { at, ordinals };
        let n = words(ordinals);
        let mut before = 0u32;
        for k in 0..n {
            let word = presence.word(bytes, k);
            if presence.rank_before(bytes, k) != before {
                return None;
            }
            before = before.checked_add(word.count_ones())?;
        }
        let spare = (n * 64) as u32 - ordinals;
        let last_ok = n == 0 || spare == 0 || presence.word(bytes, n - 1) >> (64 - spare) == 0;
        (last_ok && before == listed).then_some(presence)
    }

    fn word(&self, bytes: &[u8], k: usize) -> u64 {
        le_u64(bytes, self.at + k * 8)
    }

    fn rank_before(&self, bytes: &[u8], k: usize) -> u32 {
        le_u32(bytes, self.at + words(self.ordinals) * 8 + k * 4)
    }

    /// `ordinal`'s entry among the listed ones, or `None` where it is not listed or past the level.
    pub(crate) fn entry(&self, bytes: &[u8], ordinal: u32) -> Option<usize> {
        if ordinal >= self.ordinals {
            return None;
        }
        let k = ordinal as usize / 64;
        let word = self.word(bytes, k);
        let bit = 1u64 << (ordinal % 64);
        if word & bit == 0 {
            return None;
        }
        Some((self.rank_before(bytes, k) + (word & (bit - 1)).count_ones()) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(ordinals: u32, listed: &[u32]) -> Vec<u8> {
        let mut writer = PresenceWriter::new(ordinals);
        for &o in listed {
            writer.list(o);
        }
        writer.into_bytes()
    }

    #[test]
    fn a_listed_ordinal_is_found_at_its_rank_and_no_other_is() {
        let listed = [0, 3, 63, 64, 200, 299];
        let bytes = index(300, &listed);
        assert_eq!(bytes.len(), len(300));
        let presence = Presence::check(&bytes, 0, 300, listed.len() as u32).unwrap();
        for o in 0..310 {
            assert_eq!(
                presence.entry(&bytes, o),
                listed.iter().position(|&l| l == o)
            );
        }
    }

    #[test]
    fn a_count_or_a_bit_that_disagrees_is_refused() {
        let bytes = index(100, &[1, 70]);
        assert!(Presence::check(&bytes, 0, 100, 2).is_some());
        assert!(Presence::check(&bytes, 0, 100, 3).is_none());
        assert!(Presence::check(&bytes[..bytes.len() - 1], 0, 100, 2).is_none());
        let mut past = bytes.clone();
        past[15] |= 0x80;
        assert!(
            Presence::check(&past, 0, 100, 3).is_none(),
            "a bit past the level"
        );
        let mut count = bytes.clone();
        count[20] = 9;
        assert!(
            Presence::check(&count, 0, 100, 2).is_none(),
            "a running count"
        );
    }
}
