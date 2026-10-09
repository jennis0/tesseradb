//! One entry page: how the writer packs entries into it and how a reader decodes them.

use super::{Key, PAGE_BODY, PAGE_SIZE};

/// Bytes of a page before its packed gaps: the entry count (u16), the gap width in bits (u8) and
/// the first key.
const fn head_len(key_width: usize) -> usize {
    3 + key_width
}

/// Bytes of `n` gaps of `bits` bits each, packed.
const fn gap_bytes(n: usize, bits: u32) -> usize {
    (n * bits as usize).div_ceil(8)
}

/// Bytes a page of `n` entries whose gaps are `bits` wide uses before its checksum.
pub(super) const fn used_len(key_width: usize, n: usize, bits: u32) -> usize {
    head_len(key_width) + gap_bytes(n.saturating_sub(1), bits) + 4 * n
}

/// The most entries a page with `key_width`-byte keys holds: every key equal, so no gap bits.
pub(super) const fn max_entries(key_width: usize) -> usize {
    (PAGE_BODY - head_len(key_width)) / 4
}

/// Bits needed to write `gap`.
fn bits_for(gap: u128) -> u32 {
    128 - gap.leading_zeros()
}

/// OR the low `bits` bits of `value` into `out` at bit `at`, least significant bit first. `out`
/// must have eight bytes from the byte holding bit `at` on; a page's gaps always do, since a page
/// with a gap has at least two entities, eight bytes, after its gaps.
fn put_bits(out: &mut [u8], at: usize, value: u128, bits: u32) {
    if bits > 56 {
        put_bits(out, at, value & ((1 << 56) - 1), 56);
        put_bits(out, at + 56, value >> 56, bits - 56);
    } else if bits > 0 {
        let byte = at / 8;
        let mut word = u64::from_le_bytes(out[byte..byte + 8].try_into().expect("eight bytes"));
        word |= (value as u64) << (at % 8);
        out[byte..byte + 8].copy_from_slice(&word.to_le_bytes());
    }
}

/// The `bits` bits of `bytes` from bit `at`, as [`put_bits`] wrote them.
#[inline]
fn get_bits(bytes: &[u8], at: usize, bits: u32) -> u128 {
    if bits > 56 {
        get_bits(bytes, at, 56) | (get_bits(bytes, at + 56, bits - 56) << 56)
    } else if bits > 0 {
        let byte = at / 8;
        let word = u64::from_le_bytes(bytes[byte..byte + 8].try_into().expect("eight bytes"));
        ((word >> (at % 8)) & ((1u64 << bits) - 1)) as u128
    } else {
        0
    }
}

/// The entries of the page a writer is filling.
pub(super) struct PageBuilder<K: Key> {
    keys: Vec<K>,
    entities: Vec<u32>,
    bits: u32,
}

impl<K: Key> PageBuilder<K> {
    pub(super) fn new() -> Self {
        PageBuilder {
            keys: Vec::with_capacity(max_entries(K::WIDTH)),
            entities: Vec::with_capacity(max_entries(K::WIDTH)),
            bits: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub(super) fn first_key(&self) -> Option<K> {
        self.keys.first().copied()
    }

    /// Add an entry at or past the last one's key, if the page has room for it at the gap width
    /// it needs; false, adding nothing, when it has not.
    pub(super) fn try_push(&mut self, key: K, entity: u32) -> bool {
        let bits = match self.keys.last() {
            Some(last) => self.bits.max(bits_for(key.widen() - last.widen())),
            None => 0,
        };
        if used_len(K::WIDTH, self.keys.len() + 1, bits) > PAGE_BODY {
            return false;
        }
        self.keys.push(key);
        self.entities.push(entity);
        self.bits = bits;
        true
    }

    /// The page, checksum included, into `page`; the builder is left empty.
    pub(super) fn seal(&mut self, page: &mut [u8]) {
        page.fill(0);
        let n = self.keys.len();
        page[0..2].copy_from_slice(&(n as u16).to_le_bytes());
        page[2] = self.bits as u8;
        self.keys[0].write(&mut page[3..]);
        let gaps = &mut page[head_len(K::WIDTH)..];
        for (i, pair) in self.keys.windows(2).enumerate() {
            let gap = pair[1].widen() - pair[0].widen();
            put_bits(gaps, i * self.bits as usize, gap, self.bits);
        }
        let mut at = used_len(K::WIDTH, n, self.bits) - 4 * n;
        for entity in &self.entities {
            page[at..at + 4].copy_from_slice(&entity.to_le_bytes());
            at += 4;
        }
        let crc = crc32fast::hash(&page[..PAGE_BODY]);
        page[PAGE_BODY..PAGE_SIZE].copy_from_slice(&crc.to_le_bytes());
        self.keys.clear();
        self.entities.clear();
        self.bits = 0;
    }
}

/// An entry page whose count and gap width fit a page. Its checksum and order are the reader's
/// to check.
#[derive(Clone, Copy)]
pub(super) struct Page<'a, K: Key> {
    bytes: &'a [u8],
    len: usize,
    bits: u32,
    first: K,
    entities_at: usize,
}

impl<'a, K: Key> Page<'a, K> {
    /// The page in `bytes`, one whole page; refused when its count and gap width do not fit it.
    pub(super) fn parse(bytes: &'a [u8]) -> std::result::Result<Self, &'static str> {
        let len = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        let bits = bytes[2] as u32;
        if len == 0 {
            return Err("the page's entry count is zero");
        }
        if bits > 8 * K::WIDTH as u32 {
            return Err("the page's gap width is wider than its keys");
        }
        if used_len(K::WIDTH, len, bits) > PAGE_BODY {
            return Err("the page's entry count and gap width need more bytes than a page has");
        }
        Ok(Page {
            bytes,
            len,
            bits,
            first: K::read(&bytes[3..]),
            entities_at: used_len(K::WIDTH, len, bits) - 4 * len,
        })
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn first_key(&self) -> K {
        self.first
    }

    /// Bytes the page uses before its checksum; the rest are zero in a well-formed page.
    pub(super) fn used(&self) -> usize {
        self.entities_at + 4 * self.len
    }

    /// The bits of the packed gaps' last byte past the last gap, which a well-formed page leaves
    /// zero.
    pub(super) fn gap_padding(&self) -> u8 {
        let bits = (self.len - 1) * self.bits as usize;
        match bits % 8 {
            0 => 0,
            used => self.bytes[head_len(K::WIDTH) + bits / 8] >> used,
        }
    }

    /// The key of entry `i + 1` less the key of entry `i`.
    #[inline]
    pub(super) fn gap(&self, i: usize) -> u128 {
        get_bits(
            &self.bytes[head_len(K::WIDTH)..],
            i * self.bits as usize,
            self.bits,
        )
    }

    #[inline]
    pub(super) fn entity(&self, i: usize) -> u32 {
        let at = self.entities_at + 4 * i;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("four bytes"))
    }

    /// The page's keys in order, each as a widened value, `None` from the first that is wider
    /// than the key.
    pub(super) fn checked_keys(&self) -> impl Iterator<Item = Option<u128>> + '_ {
        let limit = K::narrow(u128::MAX).widen();
        (0..self.len).scan(Some(self.first.widen()), move |key, i| {
            let this = *key;
            if i + 1 < self.len {
                *key = key
                    .and_then(|k| k.checked_add(self.gap(i)))
                    .filter(|&k| k <= limit);
            }
            Some(this)
        })
    }

    /// The last entry, decoded from the first.
    pub(super) fn last(&self) -> (K, u32) {
        let mut key = self.first.widen();
        for i in 0..self.len - 1 {
            key = key.wrapping_add(self.gap(i));
        }
        (K::narrow(key), self.entity(self.len - 1))
    }
}

/// A position in a page, with the key of the entry there.
#[derive(Clone, Copy)]
pub(super) struct PageCursor<'a, K: Key> {
    pub(super) page: Page<'a, K>,
    pub(super) at: usize,
    pub(super) key: K,
}

impl<'a, K: Key> PageCursor<'a, K> {
    pub(super) fn new(page: Page<'a, K>) -> Self {
        PageCursor {
            page,
            at: 0,
            key: page.first,
        }
    }

    #[inline]
    pub(super) fn entity(&self) -> u32 {
        self.page.entity(self.at)
    }

    /// Move forward to the first entry whose key is at or past `key`, or to the page's last entry
    /// when none is.
    #[inline]
    pub(super) fn seek(&mut self, key: K) {
        if self.key >= key {
            return;
        }
        let (bits, last) = (self.page.bits, self.page.len - 1);
        if bits == 0 {
            // Every key is the first, which is below `key`.
            self.at = last;
            return;
        }
        if bits > 56 {
            let gaps = &self.page.bytes[head_len(K::WIDTH)..];
            let (target, mut k) = (key.widen(), self.key.widen());
            while self.at < last && k < target {
                k = k.wrapping_add(get_bits(gaps, self.at * bits as usize, bits));
                self.at += 1;
            }
            self.key = K::narrow(k);
            return;
        }
        // Every read below starts at or before byte PAGE_BODY - 8 of a page that parsed, so the
        // clamp never moves it; it lets the compiler drop the bounds check.
        let page: &[u8; PAGE_SIZE] = self.page.bytes.try_into().expect("a whole page");
        let (base, mask) = (8 * head_len(K::WIDTH), (1u64 << bits) - 1);
        let gap = |at: usize| {
            let bit = base + at * bits as usize;
            let byte = (bit / 8).min(PAGE_SIZE - 8);
            let word = u64::from_le_bytes(page[byte..byte + 8].try_into().expect("eight bytes"));
            (word >> (bit % 8)) & mask
        };
        let mut at = self.at;
        if K::WIDTH <= 8 {
            let (target, mut k) = (key.widen() as u64, self.key.widen() as u64);
            while at < last && k < target {
                k = k.wrapping_add(gap(at));
                at += 1;
            }
            self.key = K::narrow(k as u128);
        } else {
            let (target, mut k) = (key.widen(), self.key.widen());
            while at < last && k < target {
                k = k.wrapping_add(gap(at) as u128);
                at += 1;
            }
            self.key = K::narrow(k);
        }
        self.at = at;
    }

    /// Move to the next entry; false, not moving, at the page's last.
    #[inline]
    pub(super) fn step(&mut self) -> bool {
        if self.at + 1 >= self.page.len {
            return false;
        }
        self.key = K::narrow(self.key.widen().wrapping_add(self.page.gap(self.at)));
        self.at += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_round_trip_at_every_width_and_offset() {
        let mut out = vec![0u8; 64];
        let cases = [(0usize, 1u32, 1u128), (3, 7, 0x55), (9, 56, (1 << 56) - 1)];
        for (at, bits, value) in cases {
            put_bits(&mut out, at, value, bits);
        }
        for (at, bits, value) in cases {
            assert_eq!(get_bits(&out, at, bits), value);
        }
        let mut out = vec![0u8; 64];
        put_bits(&mut out, 5, u128::MAX, 128);
        assert_eq!(get_bits(&out, 5, 128), u128::MAX);
        assert_eq!(get_bits(&out, 0, 5), 0);
        assert_eq!(get_bits(&out, 133, 8), 0);
    }
}
