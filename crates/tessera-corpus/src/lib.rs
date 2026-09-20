//! The test-data generator for the correctness suite, which is also its answer key.
//!
//! At a billion points the expected answers are too large to store, so they are recomputed.
//! Every property of item `e` (its position, its access terms and each field) is a function of
//! `(seed, e)` alone, computed in constant time with no table and no file. [`Corpus::item`] and
//! [`Corpus::terms`] are those functions. [`Corpus::census`] is the one method that visits all
//! `n` items.
//!
//! This crate may depend on `tessera-types` and `tessera-spatial` and on nothing else in the
//! workspace. `scripts/check-layers.sh` enforces this. If the generator could read a bundle or
//! use an engine type, the suite would compare the system with itself. `tessera-spatial` is
//! allowed because it defines quantisation and tile addressing, and the census must put a
//! position in the same cell as the build does.
//!
//! # Properties of the generator
//!
//! Every value is [`keyed`]`(seed, salt, e)`, where each property has its own salt.
//!
//! - It is prefix-stable: `n` is used in no value, so the same seed with a smaller `n` gives the
//!   same corpus cut short. A failure at 10^9 items can then be bisected on `n`.
//! - Positions are spread at any size. Each axis uses 24 bits of its own draw, which is about
//!   1.7 x 10^7 distinct positions per axis.
//! - Position, terms and each field are uncorrelated, because each uses its own salt.
//! - Each row identifies its item. The `fx_key` column holds a keyed bijection of `e`, which
//!   [`Corpus::item_of_fx_key`] inverts. It is not `e` itself, because the build and ingest
//!   assign entity ids in their own order, and a served entity id must not pass for the key.
//!
//! # Terms
//!
//! Each item has one or two terms from a space of 16 levels with 64 terms each. Level `L` is
//! chosen with probability `2^-(L+1)`, so a term at level `L` covers about `n / 2^(L+1) / 64`
//! items, and term sizes range from dense to nearly single. A term's descriptor is its decimal
//! string, which is the form `builtin:passthrough` expects, so a [`Grant`] here and an access
//! label on the server name the same items. [`Corpus::with_terms_per_level`] changes the number
//! of terms per level. The number of levels stays 16.

pub mod artifacts;
pub mod boundary;
pub mod hierarchy;
pub mod materialise;
pub mod partition;

use std::collections::HashMap;

use tessera_spatial::{cell, interleave_bits, Bounds};
use tessera_types::TermId;

/// SplitMix64's stream increment: the `e`-th state of a stream from key `K` is `K + e·GOLDEN`.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
/// The multiplicative inverse of `GOLDEN` mod 2^64.
const GOLDEN_INV: u64 = 0xF1DE_83E1_9937_733D;

/// SplitMix64's finaliser: a bijection over `u64`.
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The inverse of [`mix64`].
const fn unmix64(mut z: u64) -> u64 {
    z ^= (z >> 31) ^ (z >> 62);
    z = z.wrapping_mul(0x3196_42B2_D24D_8EC3);
    z ^= (z >> 27) ^ (z >> 54);
    z = z.wrapping_mul(0x96DE_1B17_3F11_9089);
    z ^= (z >> 30) ^ (z >> 60);
    z
}

/// The `e`-th output of the SplitMix64 stream keyed by `mix64(seed ^ salt)`. Bijective in `e` for
/// a fixed `(seed, salt)`, which `fx_key` relies on.
const fn keyed(seed: u64, salt: u64, e: u64) -> u64 {
    mix64(mix64(seed ^ salt).wrapping_add(e.wrapping_mul(GOLDEN)))
}

/// Dimension salts: each constant is the little-endian reading of its own name.
const fn salt(name: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*name)
}
const SALT_X: u64 = salt(b"axis-x  ");
const SALT_Y: u64 = salt(b"axis-y  ");
const SALT_FX: u64 = salt(b"fx-key  ");
const SALT_TERM_A: u64 = salt(b"term-a  ");
const SALT_TERM_B: u64 = salt(b"term-b  ");
const SALT_WEIGHT: u64 = salt(b"weight  ");
const SALT_SEEN: u64 = salt(b"seen-at ");
const SALT_BAY: u64 = salt(b"bay     ");
const SALT_TAG: u64 = salt(b"tag     ");
const SALT_BLURB: u64 = salt(b"blurb   ");

/// The generated corpus: a seed, a size, and the extent positions quantise against. `n` bounds
/// [`Corpus::census`] and the materialisers and nothing else: [`Corpus::item`] and
/// [`Corpus::terms`] are defined for every `e`.
#[derive(Debug, Clone, Copy)]
pub struct Corpus {
    seed: u64,
    n: u64,
    extent: Bounds,
    /// Slots per term level, a power of two.
    terms_per_level: u32,
}

/// One item, every declared field evaluated. `None` means the item has no value for that column.
/// Each optional field is absent for about one item in 64, decided by the top six bits of its own
/// draw.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The value of the points file's `entity_id` column.
    pub e: u64,
    /// A keyed bijection of `e` (see the module doc), inverted by [`Corpus::item_of_fx_key`].
    pub fx_key: u64,
    pub x: f64,
    pub y: f64,
    pub weight: Option<u32>,
    /// Microseconds since the epoch, within 2020-2026.
    pub seen_at: Option<i64>,
    /// The value key. Codes are assigned by the build.
    pub bay: Option<&'static str>,
    pub tag: Option<String>,
    /// Three words from a fixed lexicon.
    pub blurb: Option<String>,
}

/// The category's value keys, in code order. Code 0 is the absent sentinel.
pub const BAY_VALUES: [&str; 7] = ["amber", "basalt", "cedar", "dune", "ember", "flint", "gale"];

/// The text lexicon: 64 plain lowercase words.
const LEXICON: [&str; 64] = [
    "alder", "basin", "cairn", "delta", "eddy", "fjord", "glade", "heath", "islet", "jetty",
    "knoll", "lagoon", "marsh", "nook", "outcrop", "pool", "quarry", "ridge", "shoal", "tarn",
    "upland", "vale", "weir", "yard", "arch", "bluff", "cove", "dune", "esker", "flat", "gorge",
    "hollow", "inlet", "junction", "kettle", "ledge", "mesa", "notch", "oxbow", "pass", "quay",
    "reef", "spur", "trench", "under", "vent", "wash", "expanse", "yield", "zone", "bank", "crest",
    "drift", "edge", "ford", "gap", "hill", "island", "jut", "kame", "loch", "moor", "neck",
    "orbit",
];

/// The number of term levels.
const TERM_LEVELS: u32 = 16;
/// The default slot width: 1024 terms total.
const DEFAULT_TERMS_PER_LEVEL: u32 = 64;
/// One past the largest term id the default-width generator can emit. [`Grant::parse`] refuses a
/// descriptor outside this space.
pub const TERM_SPACE: u32 = TERM_LEVELS * DEFAULT_TERMS_PER_LEVEL;

/// A depth-`zoom` Morton prefix over the cell grid: the census's tile key.
pub type TileId = u64;

/// A principal's granted term set: comma-separated decimal term descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    terms: Vec<TermId>,
}

impl Grant {
    /// Parse the catalogue's principal encoding against the default term space: comma-separated
    /// decimal descriptors, empties dropped. Refuses a descriptor that is not a decimal term id,
    /// or that names a term outside [`TERM_SPACE`].
    pub fn parse(encoded: &str) -> Result<Grant, String> {
        Self::parse_bounded(encoded, TERM_SPACE)
    }

    /// [`Grant::parse`], against `term_space` instead of the default.
    pub fn parse_bounded(encoded: &str, term_space: u32) -> Result<Grant, String> {
        let mut terms = Vec::new();
        for part in encoded.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let raw: u32 = part
                .parse()
                .map_err(|_| format!("grant descriptor '{part}' is not a decimal term id"))?;
            if raw >= term_space {
                return Err(format!(
                    "grant descriptor '{part}' names no term this corpus can emit (term space is \
                     0..{term_space})"
                ));
            }
            terms.push(TermId::new(raw));
        }
        terms.sort_unstable();
        terms.dedup();
        Ok(Grant { terms })
    }

    /// The granted terms, sorted and deduplicated.
    pub fn terms(&self) -> &[TermId] {
        &self.terms
    }

    fn contains(&self, term: TermId) -> bool {
        self.terms.binary_search(&term).is_ok()
    }
}

impl Corpus {
    /// A corpus of `n` items under `seed`, positioned within `extent`, at the default term space.
    /// Refuses a degenerate extent.
    pub fn new(seed: u64, n: u64, extent: Bounds) -> Result<Corpus, String> {
        Self::with_terms_per_level(seed, n, extent, DEFAULT_TERMS_PER_LEVEL)
    }

    /// [`Corpus::new`], with the slot count per level changed from the default 64. Refuses a
    /// `terms_per_level` that is zero or not a power of two.
    pub fn with_terms_per_level(
        seed: u64,
        n: u64,
        extent: Bounds,
        terms_per_level: u32,
    ) -> Result<Corpus, String> {
        extent.validate()?;
        if terms_per_level == 0 || !terms_per_level.is_power_of_two() {
            return Err(format!(
                "terms_per_level must be a nonzero power of two, got {terms_per_level}"
            ));
        }
        Ok(Corpus {
            seed,
            n,
            extent,
            terms_per_level,
        })
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn n(&self) -> u64 {
        self.n
    }

    pub fn extent(&self) -> Bounds {
        self.extent
    }

    /// This corpus's term space: every term id [`Corpus::terms`] can emit is less than this value.
    pub fn term_space(&self) -> u32 {
        TERM_LEVELS * self.terms_per_level
    }

    /// One axis: 24 independent bits of the dimension's keyed mix, mapped into the extent at the
    /// step's midpoint. Rounded through `f32`; removing that rounding would move every position
    /// this corpus generates.
    fn axis(&self, axis_salt: u64, e: u64, min: f64, max: f64) -> f64 {
        const AXIS_BITS: u32 = 24;
        let step = (keyed(self.seed, axis_salt, e) >> (64 - AXIS_BITS)) as f64;
        f64::from((min + (step + 0.5) * (max - min) / (1u64 << AXIS_BITS) as f64) as f32)
    }

    /// Item `e`, every declared field evaluated. O(1), defined for every `e`, including `e >= n`.
    pub fn item(&self, e: u64) -> Item {
        let weight = keyed(self.seed, SALT_WEIGHT, e);
        let seen = keyed(self.seed, SALT_SEEN, e);
        let bay = keyed(self.seed, SALT_BAY, e);
        let tag = keyed(self.seed, SALT_TAG, e);
        let blurb = keyed(self.seed, SALT_BLURB, e);
        // 2020-01-01T00:00:00Z, and six years in microseconds.
        const SEEN_BASE_US: i64 = 1_577_836_800_000_000;
        const SEEN_SPAN_US: u64 = 189_388_800_000_000;
        Item {
            e,
            fx_key: keyed(self.seed, SALT_FX, e),
            x: self.axis(SALT_X, e, self.extent.x_min, self.extent.x_max),
            y: self.axis(SALT_Y, e, self.extent.y_min, self.extent.y_max),
            weight: present(weight).then_some(weight as u32),
            // The low 58 bits, disjoint from the presence bits, folded into the span.
            seen_at: present(seen)
                .then(|| SEEN_BASE_US + ((seen & 0x03FF_FFFF_FFFF_FFFF) % SEEN_SPAN_US) as i64),
            bay: present(bay).then(|| BAY_VALUES[((bay & 0xFFFF_FFFF) % 7) as usize]),
            tag: present(tag).then(|| format!("kw-{:05x}", tag & 0xF_FFFF)),
            blurb: present(blurb).then(|| {
                format!(
                    "{} {} {}",
                    LEXICON[(blurb & 63) as usize],
                    LEXICON[((blurb >> 6) & 63) as usize],
                    LEXICON[((blurb >> 12) & 63) as usize]
                )
            }),
        }
    }

    /// Item `e`'s terms: one or two terms, collapsed when they collide, sorted.
    pub fn terms(&self, e: u64) -> Vec<TermId> {
        let a = term_of_draw(keyed(self.seed, SALT_TERM_A, e), self.terms_per_level);
        let b = term_of_draw(keyed(self.seed, SALT_TERM_B, e), self.terms_per_level);
        match a.cmp(&b) {
            std::cmp::Ordering::Less => vec![a, b],
            std::cmp::Ordering::Equal => vec![a],
            std::cmp::Ordering::Greater => vec![b, a],
        }
    }

    /// Whether `grant` sees item `e`: any granted term among the item's.
    pub fn visible(&self, e: u64, grant: &Grant) -> bool {
        self.terms(e).iter().any(|t| grant.contains(*t))
    }

    /// The item whose `fx_key` is `fx_key`. Every `u64` has an answer. A corrupted key gives some
    /// other item, whose values then differ from the served row, and the comparison reports that.
    pub fn item_of_fx_key(&self, fx_key: u64) -> u64 {
        unmix64(fx_key)
            .wrapping_sub(mix64(self.seed ^ SALT_FX))
            .wrapping_mul(GOLDEN_INV)
    }

    /// The expected masked count per depth-`zoom` tile for `grant`. Non-empty tiles only,
    /// ascending by tile id. Single-threaded.
    pub fn census(&self, zoom: u8, grant: &Grant) -> Vec<(TileId, u64)> {
        assert!(zoom <= 16, "census: zoom {zoom} exceeds grid depth 16");
        self.bucket_census(grant, |e| [self.tile_of(e, zoom)])
    }

    /// The depth-`zoom` tile that item `e`'s position falls in.
    pub(crate) fn tile_of(&self, e: u64, zoom: u8) -> TileId {
        let shift = 16 - u32::from(zoom);
        let x = self.axis(SALT_X, e, self.extent.x_min, self.extent.x_max);
        let y = self.axis(SALT_Y, e, self.extent.y_min, self.extent.y_max);
        let cx = cell(x, self.extent.x_min, self.extent.x_max);
        let cy = cell(y, self.extent.y_min, self.extent.y_max);
        // Widened before the shift: at zoom 0 the shift is the full 16 bits, which a `u16`
        // cannot express.
        interleave_bits(u32::from(cx) >> shift, u32::from(cy) >> shift, zoom)
    }

    /// The artifact census's shared driver: one O(n) pass, bucketing each visible entity into the
    /// artifact(s) `holders_of` names for it. An artifact this grant sees nothing of is omitted,
    /// never a zero-count row.
    pub(crate) fn bucket_census<I: IntoIterator<Item = u64>>(
        &self,
        grant: &Grant,
        holders_of: impl Fn(u64) -> I,
    ) -> Vec<(u64, u64)> {
        let mut counts: HashMap<u64, u64> = HashMap::new();
        for e in 0..self.n {
            if !self.visible(e, grant) {
                continue;
            }
            for a in holders_of(e) {
                *counts.entry(a).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(u64, u64)> = counts.into_iter().collect();
        out.sort_unstable();
        out
    }
}

/// Absent iff the draw's top six bits are all zero: P(absent) = 1/64.
fn present(draw: u64) -> bool {
    (draw >> 58) != 0
}

/// A term from one uniform draw: the level from trailing zeros, capped at the deepest level, the
/// slot from high bits masked to `terms_per_level`.
fn term_of_draw(draw: u64, terms_per_level: u32) -> TermId {
    let level = draw.trailing_zeros().min(TERM_LEVELS - 1);
    let slot = ((draw >> 40) & u64::from(terms_per_level - 1)) as u32;
    TermId::new(level * terms_per_level + slot)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub fn corpus(n: u64) -> Corpus {
        let extent = Bounds {
            x_min: 0.0,
            x_max: 1000.0,
            y_min: 0.0,
            y_max: 1000.0,
        };
        Corpus::new(0x5EED, n, extent).unwrap()
    }

    /// Checks a census against `members_of`, the forward direction, by enumeration.
    pub fn assert_census_counts_visible_members(
        corpus: &Corpus,
        grant: &Grant,
        census: Vec<(u64, u64)>,
        artifacts: impl Iterator<Item = u64>,
        members_of: impl Fn(u64) -> Vec<u64>,
    ) {
        let expected: Vec<(u64, u64)> = artifacts
            .map(|a| {
                let visible = members_of(a)
                    .into_iter()
                    .filter(|&e| corpus.visible(e, grant))
                    .count();
                (a, visible as u64)
            })
            .filter(|&(_, visible)| visible > 0)
            .collect();
        assert_eq!(census, expected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The multiplicative constants are inverses.
    #[test]
    fn the_mix_constants_are_modular_inverses() {
        assert_eq!(
            0xBF58_476D_1CE4_E5B9u64.wrapping_mul(0x96DE_1B17_3F11_9089),
            1
        );
        assert_eq!(
            0x94D0_49BB_1331_11EBu64.wrapping_mul(0x3196_42B2_D24D_8EC3),
            1
        );
        assert_eq!(GOLDEN.wrapping_mul(GOLDEN_INV), 1);
    }

    #[test]
    fn the_salts_are_distinct() {
        let salts = [
            SALT_X,
            SALT_Y,
            SALT_FX,
            SALT_TERM_A,
            SALT_TERM_B,
            SALT_WEIGHT,
            SALT_SEEN,
            SALT_BAY,
            SALT_TAG,
            SALT_BLURB,
        ];
        let mut sorted = salts.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), salts.len(), "two dimensions share a salt");
    }

    #[test]
    fn terms_stay_inside_the_term_space_and_sorted() {
        let corpus = Corpus::new(9, 0, grid()).unwrap();
        for e in 0..10_000 {
            let terms = corpus.terms(e);
            assert!(!terms.is_empty() && terms.len() <= 2);
            assert!(
                terms.windows(2).all(|w| w[0] < w[1]),
                "sorted, deduplicated"
            );
            assert!(terms.iter().all(|t| t.raw() < TERM_SPACE));
        }
    }

    #[test]
    fn grant_parse_follows_the_passthrough_rule_and_refuses_typos() {
        let g = Grant::parse(" 3 ,, 17,3 ").unwrap();
        assert_eq!(g.terms(), &[TermId::new(3), TermId::new(17)]);
        assert!(Grant::parse("").unwrap().terms().is_empty());
        assert!(Grant::parse("cs.LG").is_err(), "non-decimal descriptor");
        assert!(Grant::parse("1024").is_err(), "outside the term space");
    }

    /// Widening the slot count keeps the same 16-level spectrum: `16 * 65_536 = 1_048_576`.
    #[test]
    fn a_wider_term_space_keeps_every_term_inside_it() {
        let wide = Corpus::with_terms_per_level(0x5EED, 10_000, grid(), 65_536).unwrap();
        assert_eq!(wide.term_space(), 1_048_576);
        for e in 0..10_000 {
            for t in wide.terms(e) {
                assert!(
                    t.raw() < wide.term_space(),
                    "term {t:?} escapes the widened space"
                );
            }
        }
        assert!(Grant::parse_bounded("1000000", wide.term_space()).is_ok());
        assert!(Grant::parse_bounded("1048576", wide.term_space()).is_err());
    }

    #[test]
    fn terms_per_level_must_be_a_nonzero_power_of_two() {
        assert!(Corpus::with_terms_per_level(1, 100, grid(), 0).is_err());
        assert!(Corpus::with_terms_per_level(1, 100, grid(), 100).is_err());
        assert!(Corpus::with_terms_per_level(1, 100, grid(), 128).is_ok());
    }

    fn grid() -> Bounds {
        Bounds {
            x_min: 0.0,
            x_max: 65536.0,
            y_min: 0.0,
            y_max: 65536.0,
        }
    }
}
