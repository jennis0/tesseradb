//! Key widths, and the keys a field's values become.

use std::fmt::Debug;
use std::hash::Hash;

/// An unsigned integer a run is keyed by: `u32`, `u64` or `u128`. Keys order as the integers do,
/// and are stored little-endian in [`Key::WIDTH`] bytes.
pub trait Key: Copy + Ord + Hash + Debug + Send + Sync + 'static + sealed::Sealed {
    /// Bytes a key takes in a run: 4, 8 or 16.
    const WIDTH: usize;

    /// The key from its [`Key::WIDTH`] little-endian bytes.
    fn read(bytes: &[u8]) -> Self;

    /// The key's [`Key::WIDTH`] little-endian bytes, into `out`.
    fn write(self, out: &mut [u8]);

    /// The key zero-extended, as the header records its smallest and largest.
    fn widen(self) -> u128;

    /// The key from a widened value that fits the width.
    fn narrow(value: u128) -> Self;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
    impl Sealed for u128 {}
}

macro_rules! key {
    ($t:ty) => {
        impl Key for $t {
            const WIDTH: usize = std::mem::size_of::<$t>();

            #[inline]
            fn read(bytes: &[u8]) -> Self {
                <$t>::from_le_bytes(bytes[..Self::WIDTH].try_into().expect("a key's width"))
            }

            #[inline]
            fn write(self, out: &mut [u8]) {
                out[..Self::WIDTH].copy_from_slice(&self.to_le_bytes());
            }

            #[inline]
            fn widen(self) -> u128 {
                self as u128
            }

            #[inline]
            fn narrow(value: u128) -> Self {
                value as $t
            }
        }
    };
}
key!(u32);
key!(u64);
key!(u128);

/// The key of an unsigned integer value (`u8` to `u64`, widened): the value itself.
pub fn unsigned_key(value: u64) -> u64 {
    value
}

/// The key of a signed integer value (`i8` to `i64`, widened, and timestamps, which are `i64`
/// microseconds): the value with its sign bit flipped, so that every negative value orders before
/// every non-negative one and keys order as the values do.
pub fn signed_key(value: i64) -> u64 {
    (value as u64) ^ (1 << 63)
}

/// The value [`signed_key`] made `key` from.
pub fn signed_value(key: u64) -> i64 {
    (key ^ (1 << 63)) as i64
}

/// The key of a keyword value: the XXH3-128 hash of its UTF-8 bytes with seed 0. Two distinct
/// strings share a key with probability about 2⁻¹²⁸ a pair, and a lookup never reads the string
/// back to tell them apart.
pub fn keyword_key(value: &str) -> u128 {
    twox_hash::XxHash3_128::oneshot_with_seed(0, value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_keys_order_as_their_values_with_negatives_first() {
        let values = [
            i64::MIN,
            i64::MIN + 1,
            -1_000_000,
            -1,
            0,
            1,
            1_000_000,
            i64::MAX - 1,
            i64::MAX,
        ];
        let keys: Vec<u64> = values.iter().map(|&v| signed_key(v)).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:x?}");
        assert!(signed_key(-1) < signed_key(0));
        for v in values {
            assert_eq!(signed_value(signed_key(v)), v);
        }
    }

    #[test]
    fn unsigned_keys_are_the_values() {
        for v in [0, 1, 255, u32::MAX as u64, u64::MAX] {
            assert_eq!(unsigned_key(v), v);
        }
    }

    /// Vectors from the reference C implementation (libxxhash 0.8.3, `XXH3_128bits`, seed 0),
    /// one input for each of XXH3's length classes: 0, 1-3, 4-8, 9-16, 17-128, 129-240, over 240.
    #[test]
    fn keyword_keys_are_the_reference_xxh3_128() {
        let cases: [(&str, u128); 10] = [
            ("", 0x99aa06d3014798d86001c324468d497f),
            ("a", 0xa96faf705af16834e6c632b61e964e1f),
            ("abc", 0x06b05ab6733a618578af5f94892f3950),
            ("abcdef", 0x389197a55db2b2e4da35a6714d34f8a2),
            ("hello world", 0xdf8d09e93f874900a99b8775cc15b6c7),
            ("0123456789abcdef", 0xccba8085a0434e9e0befb4873dbe58f8),
            (
                "The quick brown fox jumps over the lazy dog",
                0xddd650205ca3e7fa24a1cc2e3a8a7651,
            ),
            (&"x".repeat(100), 0xadd9998d55ed3962e18dc405a95cc094),
            (&"y".repeat(200), 0x833cf59a501ae2a8661514be62296c9c),
            (&"z".repeat(1000), 0x66a9b2d587876ce7cd3a574700eddf41),
        ];
        for (input, expected) in cases {
            assert_eq!(
                keyword_key(input),
                expected,
                "input of {} bytes",
                input.len()
            );
        }
    }

    #[test]
    fn keys_round_trip_through_their_bytes() {
        fn round_trip<K: Key>(k: K) {
            let mut bytes = vec![0u8; K::WIDTH];
            k.write(&mut bytes);
            assert_eq!(K::read(&bytes), k);
            assert_eq!(K::narrow(k.widen()), k);
        }
        round_trip(0xdead_beefu32);
        round_trip(u64::MAX - 7);
        round_trip(keyword_key("round trip"));
    }
}
