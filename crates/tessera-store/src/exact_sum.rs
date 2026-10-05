//! A sum of numbers kept exactly, so that subtracting a part of it gives the sum of the rest.
//!
//! Every finite `f64` is an integer multiple of 2^-1074, and so is every integer, so a sum of them
//! is an integer count of 2^-1074. It is held as two unsigned fixed-point magnitudes, the positive
//! values' and the negative values', in [`LIMBS`] 64-bit words: 2,098 bits reach the largest
//! `f64`, and the rest leave room for 2^78 of them. Adding never borrows, so a carry rarely runs
//! past the word it starts in. Subtracting a sum adds its positive part to the negative magnitude
//! and its negative part to the positive one.
//!
//! The mean is rounded once from the exact quotient: to the nearest `f64`, or for a timestamp to
//! the nearest whole microsecond, a tie to even either way.

/// Words in one magnitude.
const LIMBS: usize = 34;

/// The bit of the fixed point at which 2^0 sits.
const ONE: usize = 1074;

/// An exact sum of finite `f64`s and integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactSum {
    positive: [u64; LIMBS],
    negative: [u64; LIMBS],
}

impl Default for ExactSum {
    fn default() -> Self {
        ExactSum {
            positive: [0; LIMBS],
            negative: [0; LIMBS],
        }
    }
}

/// Add `m << at` to `words`.
fn add_at(words: &mut [u64; LIMBS], m: u128, at: usize) {
    let (word, shift) = (at / 64, at % 64);
    let parts = [
        (m as u64) << shift,
        if shift == 0 {
            (m >> 64) as u64
        } else {
            ((m >> (64 - shift)) & u128::from(u64::MAX)) as u64
        },
        if shift == 0 {
            0
        } else {
            (m >> (128 - shift)) as u64
        },
    ];
    let mut carry = false;
    let mut i = word;
    for part in parts {
        let (sum, over) = words[i].overflowing_add(part);
        let (sum, over_carry) = sum.overflowing_add(u64::from(carry));
        words[i] = sum;
        carry = over || over_carry;
        i += 1;
    }
    while carry {
        let (sum, over) = words[i].overflowing_add(1);
        words[i] = sum;
        carry = over;
        i += 1;
    }
}

/// `a += b`.
fn add_words(a: &mut [u64; LIMBS], b: &[u64; LIMBS]) {
    let mut carry = false;
    for (x, &y) in a.iter_mut().zip(b) {
        let (sum, over) = x.overflowing_add(y);
        let (sum, over_carry) = sum.overflowing_add(u64::from(carry));
        *x = sum;
        carry = over || over_carry;
    }
}

impl ExactSum {
    /// Add a finite `f64`.
    #[inline]
    pub fn add_float(&mut self, x: f64) {
        debug_assert!(x.is_finite());
        let bits = x.to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as usize;
        let fraction = bits & ((1 << 52) - 1);
        let (m, at) = match exponent {
            0 => (fraction, 0),
            e => (fraction | 1 << 52, e - 1),
        };
        if m == 0 {
            return;
        }
        let side = match bits >> 63 {
            0 => &mut self.positive,
            _ => &mut self.negative,
        };
        add_at(side, u128::from(m), at);
    }

    /// Add an integer.
    pub fn add_int(&mut self, x: i128) {
        if x == 0 {
            return;
        }
        let side = match x < 0 {
            false => &mut self.positive,
            true => &mut self.negative,
        };
        add_at(side, x.unsigned_abs(), ONE);
    }

    /// `self + other`.
    pub fn plus(mut self, other: &ExactSum) -> ExactSum {
        add_words(&mut self.positive, &other.positive);
        add_words(&mut self.negative, &other.negative);
        self
    }

    /// `self - other`.
    pub fn minus(mut self, other: &ExactSum) -> ExactSum {
        add_words(&mut self.positive, &other.negative);
        add_words(&mut self.negative, &other.positive);
        self
    }

    /// The sum over `count`, exactly: the quotient's magnitude with 64 bits below the fixed
    /// point's, whether a remainder was left, and whether it is negative.
    fn quotient(&self, count: u64) -> ([u64; LIMBS + 1], bool, bool) {
        let (larger, smaller, negative) =
            match self.positive.iter().rev().cmp(self.negative.iter().rev()) {
                std::cmp::Ordering::Less => (&self.negative, &self.positive, true),
                _ => (&self.positive, &self.negative, false),
            };
        let mut magnitude = [0u64; LIMBS + 1];
        let mut borrow = false;
        for i in 0..LIMBS {
            let (d, under) = larger[i].overflowing_sub(smaller[i]);
            let (d, under_borrow) = d.overflowing_sub(u64::from(borrow));
            magnitude[i + 1] = d;
            borrow = under || under_borrow;
        }
        let mut remainder: u128 = 0;
        for word in magnitude.iter_mut().rev() {
            let wide = (remainder << 64) | u128::from(*word);
            *word = (wide / u128::from(count)) as u64;
            remainder = wide % u128::from(count);
        }
        (magnitude, remainder != 0, negative)
    }

    /// The sum over `count`, rounded once to the nearest `f64`, a tie to even. `None` where
    /// `count` is 0.
    pub fn mean(&self, count: u64) -> Option<f64> {
        if count == 0 {
            return None;
        }
        let (magnitude, sticky, negative) = self.quotient(count);
        let rounded = rounded(&magnitude, sticky, -(ONE as i32) - 64);
        Some(if negative { -rounded } else { rounded })
    }

    /// The sum over `count`, rounded once to the nearest integer, a tie to even. `None` where
    /// `count` is 0. Exact for any mean of integers that fit an `i128`.
    pub fn mean_whole(&self, count: u64) -> Option<i128> {
        if count == 0 {
            return None;
        }
        let (magnitude, sticky, negative) = self.quotient(count);
        let point = ONE + 64;
        let bit = |b: usize| magnitude[b / 64] >> (b % 64) & 1 == 1;
        let mut whole = (point..magnitude.len() * 64)
            .filter(|&b| bit(b))
            .fold(0u128, |m, b| {
                m | 1u128.checked_shl((b - point) as u32).unwrap_or(0)
            });
        let half = bit(point - 1);
        let below = sticky || (0..point - 1).any(bit);
        if half && (below || whole & 1 == 1) {
            whole += 1;
        }
        let whole = whole as i128;
        Some(if negative { -whole } else { whole })
    }

    /// The words as they are written to disk: the positive magnitude, then the negative.
    pub fn words(&self) -> impl Iterator<Item = u64> + '_ {
        self.positive.iter().chain(&self.negative).copied()
    }

    /// The sum written as [`Self::words`] gives it, where there are as many words.
    pub fn of_words(words: &[u64]) -> Option<ExactSum> {
        let (positive, negative) = words.split_at_checked(LIMBS)?;
        Some(ExactSum {
            positive: positive.try_into().ok()?,
            negative: negative.try_into().ok()?,
        })
    }

    /// How many words [`Self::words`] gives.
    pub const WORDS: usize = 2 * LIMBS;
}

/// `words · 2^scale`, with `sticky` saying whether something smaller than its lowest bit was left
/// out, to the nearest `f64` with a tie to even.
fn rounded(words: &[u64], sticky: bool, scale: i32) -> f64 {
    let Some(top) = (0..words.len() * 64)
        .rev()
        .find(|&b| words[b / 64] >> (b % 64) & 1 == 1)
    else {
        return 0.0;
    };
    let bit = |b: usize| words[b / 64] >> (b % 64) & 1 == 1;
    // The lowest bit kept: 53 bits below the top, and none below 2^-1074.
    let lowest = (top as i64 - 52).max(-1074 - i64::from(scale));
    if lowest <= 0 {
        let kept = (0..=top).fold(0u64, |m, b| m | u64::from(bit(b)) << b);
        return scaled(kept as f64, scale);
    }
    let lowest = lowest as usize;
    let mut kept = (lowest..=top).fold(0u64, |m, b| m | u64::from(bit(b)) << (b - lowest));
    let half = bit(lowest - 1);
    let below = sticky || (0..lowest - 1).any(bit);
    if half && (below || kept & 1 == 1) {
        kept += 1;
    }
    scaled(kept as f64, lowest as i32 + scale)
}

/// `x · 2^e`, for an `x` and `e` whose product an `f64` holds exactly: each step is exact, since
/// every value between `x` and the product does.
fn scaled(mut x: f64, mut e: i32) -> f64 {
    while e > 1000 {
        x *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= 2f64.powi(-1000);
        e += 1000;
    }
    x * 2f64.powi(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mean(values: &[f64]) -> Option<f64> {
        let mut sum = ExactSum::default();
        for &x in values {
            sum.add_float(x);
        }
        sum.mean(values.len() as u64)
    }

    /// A value the sum swallowed is given back exactly by subtracting it, whatever its size.
    #[test]
    fn subtracting_a_part_leaves_the_rest_exactly() {
        let mut all = ExactSum::default();
        let mut huge = ExactSum::default();
        for x in [1.0, 2.0, 1e17, f64::MAX, f64::MAX, -5e-324] {
            all.add_float(x);
        }
        for x in [1e17, f64::MAX, f64::MAX] {
            huge.add_float(x);
        }
        let mut rest = ExactSum::default();
        for x in [1.0, 2.0, -5e-324] {
            rest.add_float(x);
        }
        assert_eq!(all.minus(&huge).mean(3), rest.mean(3));
        assert_eq!(rest.mean(2), Some(1.5));
    }

    /// The mean is the exact quotient rounded once.
    #[test]
    fn the_mean_is_rounded_once() {
        assert_eq!(mean(&[0.1; 10]), Some(0.1));
        assert_eq!(mean(&[f64::MAX, f64::MAX]), Some(f64::MAX));
        assert_eq!(mean(&[-f64::MAX, f64::MAX, 3.0]), Some(1.0));
        assert_eq!(mean(&[5e-324, 0.0]), Some(0.0), "a tie to even");
        assert_eq!(mean(&[5e-324, 5e-324, 5e-324, 0.0]), Some(5e-324));
        assert_eq!(mean(&[1.0, 2.0, 2.0]), Some(5.0 / 3.0));
        assert_eq!(mean(&[]), None);
        let mut ints = ExactSum::default();
        for x in [i128::from(i64::MAX), i128::from(i64::MAX), -1] {
            ints.add_int(x);
        }
        assert_eq!(
            ints.mean(3),
            Some(((2 * i128::from(i64::MAX) - 1) / 3) as f64)
        );
    }
}
