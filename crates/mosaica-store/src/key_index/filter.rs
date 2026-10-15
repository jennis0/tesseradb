//! The key filter a run may carry: a blocked Bloom filter over its keys.
//!
//! The filter is an array of 64-byte blocks. A key is hashed to one block and sets
//! [`HASHES`] bits inside it, so a probe reads one cache line. Its keys at [`BITS_PER_KEY`] bits
//! each, a filter answers "maybe" for about one key in a hundred it does not hold, and "maybe"
//! for every key it does.
//!
//! A key is hashed from its value zero-extended to 128 bits, `lo` and `hi` its low and high 64
//! bits, with `mix` the SplitMix64 finaliser (`z ^= z >> 30; z *= 0xbf58476d1ce4e5b9;
//! z ^= z >> 27; z *= 0x94d049bb133111eb; z ^= z >> 31`, wrapping at 64 bits) and
//! `C = 0x9e3779b97f4a7c15`:
//!
//! - `x = mix(lo ^ mix(hi ^ C))`, and the key's block is `(x × blocks) >> 64`;
//! - `y = mix(x ^ C)`, and the key's `i`th bit, for `i` from 0 to the hash count less one, is bit
//!   `(y >> 9i) & 511` of the block: bit `b & 7` of the block's byte `b >> 3`.

/// Bytes in a block.
pub(super) const BLOCK: usize = 64;

/// Bits the writer gives each distinct key.
pub(super) const BITS_PER_KEY: u64 = 10;

/// Bits the writer sets for each key.
pub(super) const HASHES: u32 = 7;

/// The most bits a key can set: nine bits of `y` choose each.
pub(super) const MAX_HASHES: u32 = 7;

const C: u64 = 0x9e37_79b9_7f4a_7c15;

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The blocks a filter over `keys` distinct keys is given: at least one.
pub(super) fn blocks_for(keys: u64) -> u64 {
    (keys.saturating_mul(BITS_PER_KEY))
        .div_ceil(8 * BLOCK as u64)
        .max(1)
}

/// The offset of `key`'s block in a filter of `blocks` blocks, and the bits it sets there.
#[inline]
fn place(key: u128, blocks: u64) -> (usize, u64) {
    let x = mix(key as u64 ^ mix((key >> 64) as u64 ^ C));
    let block = ((x as u128 * blocks as u128) >> 64) as usize;
    (block * BLOCK, mix(x ^ C))
}

/// A filter being filled.
pub(super) struct FilterBuilder {
    bytes: Vec<u8>,
    blocks: u64,
    hashes: u32,
}

impl FilterBuilder {
    pub(super) fn new(blocks: u64) -> Self {
        FilterBuilder {
            bytes: vec![0; blocks as usize * BLOCK],
            blocks,
            hashes: HASHES,
        }
    }

    pub(super) fn insert(&mut self, key: u128) {
        let (at, mut y) = place(key, self.blocks);
        let block = &mut self.bytes[at..at + BLOCK];
        for _ in 0..self.hashes {
            let bit = (y & 511) as usize;
            block[bit >> 3] |= 1 << (bit & 7);
            y >>= 9;
        }
    }

    pub(super) fn hashes(&self) -> u32 {
        self.hashes
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Whether the filter `bytes`, of `blocks` blocks setting `hashes` bits a key, may hold `key`.
#[inline]
pub(super) fn may_hold(bytes: &[u8], blocks: u64, hashes: u32, key: u128) -> bool {
    let (at, mut y) = place(key, blocks);
    let block = &bytes[at..at + BLOCK];
    for _ in 0..hashes {
        let bit = (y & 511) as usize;
        if block[bit >> 3] & (1 << (bit & 7)) == 0 {
            return false;
        }
        y >>= 9;
    }
    true
}
