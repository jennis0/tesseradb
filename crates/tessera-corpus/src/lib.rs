//! The correctness suite's corpus: ground truth as a function (`correctness-suite.md` §8, §12.1).
//!
//! At a billion points the expected answer cannot be stored, so it is computed: every property of
//! item *e* — its position, its terms, and every declared field — is a pure function of `(seed, e)`,
//! evaluated in constant time with no I/O and no table. [`Corpus::item`] and [`Corpus::terms`] are
//! those lookups; [`Corpus::census`] is the **one** O(*n*) method, because "nothing is missing or
//! extra" is a statement about all *n* items and cannot cost less — and it is a method (and a CLI
//! verb) of its own so that the O(*n*) pass runs once per census, in Rust, rather than once per
//! row in the Python driver.
//!
//! **One implementation, deliberately.** The generator defines *the corpus*, not Tessera's
//! behaviour, so the two-implementations principle does not apply to it: the second implementation
//! that matters is the oracle's account of *serving*, which stays in Python and reaches this one
//! through the `tessera corpus` CLI verbs. Two generators would give a corpus that disagrees with
//! itself, putting the fixture under test rather than the system (spec §12.1).
//!
//! **The dependency rule is the design.** This crate may depend on `tessera-types` and
//! `tessera-spatial` and nothing else in the workspace (spec §13, enforced by
//! `scripts/check-layers.sh`). A generator that learned to read an artefact — an engine type, a
//! store reader — would become a transcription of the thing it checks, and total verification
//! (spec §9) would then compare the system against itself. `tessera-spatial` is admissible because
//! it is the *definition* of quantisation and tile addressing (contracts §2.5), not artefact
//! machinery: the census must bucket a position into the same cell the build does, and a second
//! statement of `cell()` would be a place for the two to disagree.
//!
//! # The four generator properties (spec §8)
//!
//! Every value is a keyed mix of `(seed, salt_of_dimension, e)` through one fixed 64-bit
//! finaliser: [`keyed`] evaluates SplitMix64's *e*-th output from a stream keyed by
//! `mix64(seed ^ salt)`. Each property below rules out a failure the previous fixture has met,
//! and each is pinned by a test rather than argued here.
//!
//! - **Prefix-stable.** *n* appears nowhere in any derivation — it bounds only the census loop and
//!   the materialisers' row count. The same seed at a smaller *n* is the same corpus truncated,
//!   which is what lets a failure at 10⁹ bisect on *n* (spec §15).
//! - **Spread at any size.** Each axis takes 24 independent bits of its own keyed mix into the
//!   extent — about 1.7 × 10⁷ distinct positions per axis at *every* corpus size. The fixture this
//!   replaces placed item *e* at `((e·37) mod 1000, (e·53) mod 1000)`: exactly 1000 positions
//!   whatever *n* was, measured as a probe-lookup failure at a 250M base with every count exact.
//! - **Decorrelated across dimensions.** Position, terms and each field carry their own salt, so
//!   no two dimensions share a period. A generator whose value function tracked its grant function
//!   would pass every cross-principal check while testing nothing.
//! - **Named in its own row.** `fx_key` — the column the conformance fixtures already declare and
//!   the build already serves — carries a keyed *bijection* of *e*, inverted by
//!   [`Corpus::item_of_fx_key`]. A bijection rather than *e* itself: entity ids are assigned in
//!   signature order by the build and arrival order by ingest, and the source `entity_id` column
//!   is *e*, so a plain copy would let a producer that served the entity id pass the join
//!   vacuously. It is deliberately **not** recoverable from a served `tessera_id` (spec §8 gives
//!   the reason that route was rejected).
//!
//! # The grant structure
//!
//! Each item carries one or two terms drawn from a 16 × 64 term space whose levels halve in
//! probability: a level-*L* term covers about `n / 2^(L+1) / 64` items, so term widths span dense
//! to near-singleton at any size. That spectrum is what grant construction needs — head, tail and
//! crossover principals are all expressible as term sets — without any per-term state. A term's
//! descriptor is its decimal string, which is the `builtin:passthrough` convention the build's
//! dictionary already derives from the pairs relation, so a [`Grant`] here and the access label
//! the server resolves name the same postings.
//!
//! **64 is the default slot width, not the only one.** [`Corpus::new`] always builds the
//! 16 × 64 = 1024-term space every existing fixture is pinned to; [`Corpus::with_terms_per_level`]
//! widens the slot count (the level count stays fixed — see its doc) so a campaign wanting ~10⁶
//! unique terms can have them from the same spectrum.

pub mod artifacts;
pub mod boundary;
pub mod hierarchy;
pub mod materialise;
pub mod partition;

use std::collections::HashMap;

use tessera_spatial::{cell, interleave_bits, Bounds};
use tessera_types::TermId;

// ---------------------------------------------------------------------------------------------
// The keyed mix
// ---------------------------------------------------------------------------------------------

/// SplitMix64's stream increment: the *e*-th state of a stream from key `K` is `K + e·GOLDEN`.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
/// `GOLDEN⁻¹ (mod 2⁶⁴)` — what makes [`Corpus::item_of_fx_key`] exact rather than a search.
const GOLDEN_INV: u64 = 0xF1DE_83E1_9937_733D;

/// The one fixed 64-bit finaliser (spec §12.1): SplitMix64's, a bijection over `u64`.
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The exact inverse of [`mix64`], step by step in reverse. The multiplicative constants are the
/// modular inverses of `mix64`'s; the round-trip is pinned by a property test, so a mistyped
/// constant cannot survive.
const fn unmix64(mut z: u64) -> u64 {
    z ^= (z >> 31) ^ (z >> 62);
    z = z.wrapping_mul(0x3196_42B2_D24D_8EC3);
    z ^= (z >> 27) ^ (z >> 54);
    z = z.wrapping_mul(0x96DE_1B17_3F11_9089);
    z ^= (z >> 30) ^ (z >> 60);
    z
}

/// The keyed mix of `(seed, salt, e)`: SplitMix64's *e*-th output from the stream keyed by
/// `mix64(seed ^ salt)`. Prefix-stable because *n* appears nowhere in it; spread because the
/// finaliser is uniform; decorrelated across dimensions because the salt passes through a full
/// avalanche before *e* joins, so two dimensions' streams differ by key material, not by a
/// constant offset. Bijective in *e* for a fixed `(seed, salt)`, which is what `fx_key` spends.
const fn keyed(seed: u64, salt: u64, e: u64) -> u64 {
    mix64(mix64(seed ^ salt).wrapping_add(e.wrapping_mul(GOLDEN)))
}

/// Dimension salts, spelt as what they salt. Distinctness is by inspection; each constant is the
/// little-endian reading of its own name.
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

// ---------------------------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------------------------

/// The generated corpus: a seed, a size, and the extent positions quantise against.
///
/// `n` bounds [`Corpus::census`] and the materialisers ([`Corpus::write_points_parquet`] and
/// [`Corpus::write_pairs_parquet`]) and nothing else: [`Corpus::item`] and [`Corpus::terms`] are
/// defined for **every** `e`, which is what lets [`Corpus::ingest_batch`] extend the corpus past
/// the built prefix with items drawn from the same functions.
///
/// The declared columns are fixed — [`Corpus::config_toml`] is a constant, not a parameter. A
/// configurable schema would make "the corpus" a family of corpora and put the fixture under
/// configuration; one statement of the five families (number, datetime, category, keyword, text)
/// is the whole point of a generator the suite can trust (spec §8, §12.1).
#[derive(Debug, Clone, Copy)]
pub struct Corpus {
    seed: u64,
    n: u64,
    extent: Bounds,
    /// Slots per term level — [`DEFAULT_TERMS_PER_LEVEL`] unless [`Corpus::with_terms_per_level`]
    /// chose another. Always a power of two: [`term_of_draw`] masks it out of the keyed draw
    /// directly, the same trick a modulus by a non-power-of-two could not do without bias.
    terms_per_level: u32,
}

/// One item, every declared field evaluated. `None` is *absence* — the item carries no value for
/// that column, which the points file writes as null, the ingest batch carries as null, and the
/// stores keep out of band (presence bitmaps, the has-row bitmap). Each optional field is absent
/// for about one item in 64, on its own salt's bits, so presence machinery is exercised at every
/// size without correlating with anything.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The item's own identity — the value the `entity_id` column of the points file carries.
    pub e: u64,
    /// The planted join column: a keyed bijection of `e` (see the module doc), inverted by
    /// [`Corpus::item_of_fx_key`]. Never absent — it is the join every other comparison hangs on.
    pub fx_key: u64,
    pub x: f64,
    pub y: f64,
    /// The number family, in both homes (`render = true`, `index = true`).
    pub weight: Option<u32>,
    /// The datetime family (`timestamp_us`): microseconds since the epoch, within 2020–2026.
    pub seen_at: Option<i64>,
    /// The category family: the value *key*, which is what both the points file and the ingest
    /// wire carry — codes are pinned by the declaration and assigned by the build, never supplied.
    pub bay: Option<&'static str>,
    /// The keyword family: a short exact-match string from a 2²⁰-value space.
    pub tag: Option<String>,
    /// The text family: three words from a fixed lexicon, chosen so the `unicode` analyser's
    /// tokens are exactly the words.
    pub blurb: Option<String>,
}

/// The category's value keys, in code order (`config_toml` pins key *i* to code *i + 1*; code 0
/// is the reserved *absent* sentinel).
pub const BAY_VALUES: [&str; 7] = ["amber", "basalt", "cedar", "dune", "ember", "flint", "gale"];

/// The text lexicon: 64 plain lowercase words, so segmentation is the identity under any
/// word-boundary analyser and an expected token list is readable off the value.
const LEXICON: [&str; 64] = [
    "alder", "basin", "cairn", "delta", "eddy", "fjord", "glade", "heath", "islet", "jetty",
    "knoll", "lagoon", "marsh", "nook", "outcrop", "pool", "quarry", "ridge", "shoal", "tarn",
    "upland", "vale", "weir", "yard", "arch", "bluff", "cove", "dune", "esker", "flat", "gorge",
    "hollow", "inlet", "junction", "kettle", "ledge", "mesa", "notch", "oxbow", "pass", "quay",
    "reef", "spur", "trench", "under", "vent", "wash", "expanse", "yield", "zone", "bank", "crest",
    "drift", "edge", "ford", "gap", "hill", "island", "jut", "kame", "loch", "moor", "neck",
    "orbit",
];

/// Term-space shape: 16 levels, each split into a keyed-width slot. Level *L* is drawn with
/// probability `2^-(L+1)` (trailing zeros of a uniform draw, capped), so term widths span about
/// `n/128` down to near-singleton — the spectrum grant construction needs, with no per-term state
/// anywhere.
///
/// **The level count stays fixed; only the slot width is a corpus parameter.** `trailing_zeros`
/// on a `u64` draw is at most 63 and level *L*'s probability is already `2^-64` by `L = 63` — going
/// past ~20 levels reaches depths a corpus this side of 10¹⁸ items would never populate, so
/// widening `TERM_LEVELS` would only add levels that are permanently empty. Reaching the campaign's
/// ~10⁶ terms is [`Corpus::terms_per_level`] widened instead, which keeps the same 16-level
/// dense→singleton spectrum and just gives each level more slots to spread across
/// ([`Corpus::with_terms_per_level`]).
const TERM_LEVELS: u32 = 16;
/// The default width — 1024 unique terms total — every existing test, fixture and doc comment
/// (`crate::TERM_SPACE`, [`Grant::parse`]) is pinned to. [`Corpus::new`] always uses this; only
/// [`Corpus::with_terms_per_level`] can choose another.
const DEFAULT_TERMS_PER_LEVEL: u32 = 64;
/// One past the largest term id the *default*-width generator can emit. A grant naming a term
/// outside this space is a typo, and [`Grant::parse`] refuses it rather than counting nothing in
/// silence. A corpus built with [`Corpus::with_terms_per_level`] has its own, wider space —
/// [`Corpus::term_space`] and [`Grant::parse_bounded`] are what that path uses instead; this
/// constant is unchanged so every existing caller of the unparameterised path keeps its bound.
pub const TERM_SPACE: u32 = TERM_LEVELS * DEFAULT_TERMS_PER_LEVEL;

/// A depth-`zoom` Morton prefix over the cell grid — the value [`tessera_spatial::Tile::prefix`]
/// holds at that depth, and the census's tile key.
pub type TileId = u64;

/// A principal's granted term set, in the encoding the mask catalogue and the `builtin:passthrough`
/// plugin already use: comma-separated decimal term descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Sorted, deduplicated.
    terms: Vec<TermId>,
}

impl Grant {
    /// Parse the catalogue's principal encoding against the default 1024-wide term space —
    /// comma-separated decimal descriptors, empties dropped (the passthrough rule). A descriptor
    /// that is not a decimal term id, or names a term outside [`TERM_SPACE`], is refused: the
    /// census exists to state exact expected counts, and the server-side behaviour for an unknown
    /// descriptor is a silent drop — a typo'd grant here would otherwise produce a confidently
    /// wrong comparison instead of an error.
    ///
    /// **Every existing caller uses this**, unchanged, because every existing corpus is the
    /// default width. A corpus built with [`Corpus::with_terms_per_level`] has a wider term space
    /// and must check a grant against it with [`Grant::parse_bounded`] instead.
    pub fn parse(encoded: &str) -> Result<Grant, String> {
        Self::parse_bounded(encoded, TERM_SPACE)
    }

    /// [`Grant::parse`], against `term_space` rather than the default 1024 — what a corpus built
    /// with [`Corpus::with_terms_per_level`] checks a grant against, since its term space is wider.
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
    /// A corpus of `n` items under `seed`, positioned within `extent`, at the default 1024-wide
    /// term space. Refuses a degenerate extent for [`tessera_spatial::Bounds::validate`]'s reason:
    /// quantisation is undefined over one.
    pub fn new(seed: u64, n: u64, extent: Bounds) -> Result<Corpus, String> {
        Self::with_terms_per_level(seed, n, extent, DEFAULT_TERMS_PER_LEVEL)
    }

    /// [`Corpus::new`], with the term space widened (or narrowed) by choosing a different slot
    /// count per level rather than the default 64 — [`Corpus::term_space`] is `16 * terms_per_level`
    /// after this. Refuses a `terms_per_level` that is zero or not a power of two: [`term_of_draw`]
    /// carves the slot out of a keyed draw with a bitmask, which only distributes evenly over a
    /// power-of-two width.
    ///
    /// **This is the one place the corpus's term-space width is chosen.** The level count
    /// ([`TERM_LEVELS`]) stays fixed — see its doc for why widening it further is not useful —
    /// so this is how the campaign reaches ~10⁶ unique terms: `terms_per_level = 65_536` gives a
    /// term space of 1_048_576, the same 16-level dense→singleton spectrum spread over far more
    /// slots per level.
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

    /// This corpus's term space: every term id [`Corpus::terms`] can emit is `< term_space`, and
    /// [`Grant::parse_bounded`] against this value is the check that agrees with it. `1024` unless
    /// constructed with [`Corpus::with_terms_per_level`].
    pub fn term_space(&self) -> u32 {
        TERM_LEVELS * self.terms_per_level
    }

    /// One axis: 24 independent bits of the dimension's keyed mix, mapped into the extent at the
    /// step's midpoint (so no value lands exactly on the extent minimum or a cell edge by
    /// construction).
    ///
    /// **The round trip through `f32` is the definition of the value, and removing it would move
    /// every position this corpus has ever generated.** The coordinate path is `f64` now (the
    /// points file, the ingest wire and the log all carry it — `projections.md` §6), so the value
    /// is *stored* at the wider width; but the value itself is still the `f32` this expression
    /// rounded to when the fixtures and their golden digests were written, and a generator is only
    /// useful while it keeps answering the same. A wider draw is a different corpus, not a more
    /// precise one.
    fn axis(&self, axis_salt: u64, e: u64, min: f64, max: f64) -> f64 {
        const AXIS_BITS: u32 = 24;
        let step = (keyed(self.seed, axis_salt, e) >> (64 - AXIS_BITS)) as f64;
        f64::from((min + (step + 0.5) * (max - min) / (1u64 << AXIS_BITS) as f64) as f32)
    }

    /// Item `e`, every declared field evaluated — O(1), no I/O, no state, and no `n` anywhere
    /// (defined for every `e`, not only `e < n`; see the type's doc).
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
            // Each optional field: absent iff the top six bits of its own draw are zero (1 in 64);
            // the value spends only the low bits, so presence and value share no bit.
            weight: present(weight).then_some(weight as u32),
            // The low 58 bits — disjoint from the presence bits — folded into the span; the
            // modulo bias at 2⁵⁸ over ~1.9 × 10¹⁴ is a part in ~10³, irrelevant to any check.
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

    /// Item `e`'s terms — its grant structure, independently salted. One or two terms (two draws,
    /// collapsed when they collide), sorted; O(1), no I/O, no `n`.
    pub fn terms(&self, e: u64) -> Vec<TermId> {
        let a = term_of_draw(keyed(self.seed, SALT_TERM_A, e), self.terms_per_level);
        let b = term_of_draw(keyed(self.seed, SALT_TERM_B, e), self.terms_per_level);
        match a.cmp(&b) {
            std::cmp::Ordering::Less => vec![a, b],
            std::cmp::Ordering::Equal => vec![a],
            std::cmp::Ordering::Greater => vec![b, a],
        }
    }

    /// Whether `grant` sees item `e` — the passthrough rule: any granted term among the item's.
    pub fn visible(&self, e: u64, grant: &Grant) -> bool {
        self.terms(e).iter().any(|t| grant.contains(*t))
    }

    /// Invert a served `fx_key` back to its item. Total over `u64` — a corrupted key inverts to
    /// *some* `e`, whose properties then disagree with the served row, which is exactly the
    /// failure total verification exists to surface; refusing here would need `n`, which the
    /// lookup deliberately does not take.
    pub fn item_of_fx_key(&self, fx_key: u64) -> u64 {
        unmix64(fx_key)
            .wrapping_sub(mix64(self.seed ^ SALT_FX))
            .wrapping_mul(GOLDEN_INV)
    }

    /// The expected masked count per depth-`zoom` tile for `grant` — the one O(*n*) method, one
    /// pass, no I/O. Non-empty tiles only, ascending by tile id.
    ///
    /// This is the generator half of spec §9.2: per tile rather than one global total, because a
    /// single number passes any defect that moves rows between tiles while preserving the sum.
    /// The denies a harness has had accepted are its own to subtract, and barriers are its
    /// business too — this function states what the *corpus* holds.
    ///
    /// Single-threaded deliberately: at 10⁹ items this is seconds of CPU (spec §9.2's modelled
    /// figure), and the audit value of a ten-line loop that obviously implements "bucket, apply
    /// the grant rule, count" outweighs a parallel gather nobody has measured a need for.
    pub fn census(&self, zoom: u8, grant: &Grant) -> Vec<(TileId, u64)> {
        assert!(zoom <= 16, "census: zoom {zoom} exceeds grid depth 16");
        let shift = 16 - u32::from(zoom);
        let mut counts: HashMap<TileId, u64> = HashMap::new();
        for e in 0..self.n {
            if !self.visible(e, grant) {
                continue;
            }
            let x = self.axis(SALT_X, e, self.extent.x_min, self.extent.x_max);
            let y = self.axis(SALT_Y, e, self.extent.y_min, self.extent.y_max);
            let cx = cell(x, self.extent.x_min, self.extent.x_max);
            let cy = cell(y, self.extent.y_min, self.extent.y_max);
            // Widened before the shift: at zoom 0 the shift is the full 16 bits, which a `u16`
            // cannot express.
            let tile = interleave_bits(u32::from(cx) >> shift, u32::from(cy) >> shift, zoom);
            *counts.entry(tile).or_insert(0) += 1;
        }
        let mut out: Vec<(TileId, u64)> = counts.into_iter().collect();
        out.sort_unstable();
        out
    }

    /// The artifact census's shared driver (`artifacts.rs`, `partition.rs`, `boundary.rs`): one
    /// O(*n*) pass, bucketing each visible entity into the artifact(s) `holders_of` names for it.
    ///
    /// **One pass rather than one membership walk per artifact**, which is what makes this the
    /// oracle rather than a restatement of `artifact_members`: it asks the *reverse* direction for
    /// every entity once, exactly as an engine answering "which artifacts does this masked session
    /// see, and how much of each" would, so an artifact absent from the output is one this grant
    /// sees nothing of — never a zero-count row (spec §9.2's rule, restated per artifact).
    pub(crate) fn bucket_census(
        &self,
        grant: &Grant,
        holders_of: impl Fn(u64) -> Vec<u64>,
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

/// Absent iff the draw's top six bits are all zero — P(absent) = 1/64, on bits no value spends.
fn present(draw: u64) -> bool {
    (draw >> 58) != 0
}

/// A term from one uniform draw: the level from trailing zeros (halving probabilities, capped at
/// the deepest level), the slot from high bits the level test cannot touch — masked to
/// `terms_per_level` rather than shifted, so widening the slot count changes nothing about which
/// bits decide the level.
///
/// **Bit-exact at the default width.** `terms_per_level = 64` masks with `63`, which is what this
/// function did before it took a parameter — the reason [`Corpus::new`]'s corpus is unchanged.
fn term_of_draw(draw: u64, terms_per_level: u32) -> TermId {
    let level = draw.trailing_zeros().min(TERM_LEVELS - 1);
    let slot = ((draw >> 40) & u64::from(terms_per_level - 1)) as u32;
    TermId::new(level * terms_per_level + slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The multiplicative constants really are inverses — the property the exactness of
    /// [`Corpus::item_of_fx_key`] rests on, cheap enough to assert directly.
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

    /// **The parameterisation must not move a single existing answer.** `Corpus::new` is defined
    /// as `with_terms_per_level(.., DEFAULT_TERMS_PER_LEVEL)`, and the default mask (`63`) is
    /// bit-identical to the one `term_of_draw` used before it took a parameter — so this asserts
    /// the thing the campaign's whole licence to widen the space depends on: nobody's fixture
    /// moved.
    #[test]
    fn the_default_term_space_is_exactly_1024_and_unwidened_by_the_parameter() {
        assert_eq!(TERM_SPACE, 1024);
        let default = Corpus::new(0x5EED, 10_000, grid()).unwrap();
        assert_eq!(default.term_space(), TERM_SPACE);
        let explicit =
            Corpus::with_terms_per_level(0x5EED, 10_000, grid(), DEFAULT_TERMS_PER_LEVEL).unwrap();
        for e in 0..10_000 {
            assert_eq!(
                default.terms(e),
                explicit.terms(e),
                "the explicit default-width constructor disagrees with `new` at {e}"
            );
        }
    }

    /// Widening the slot count keeps the same 16-level spectrum — the halving-probability level
    /// draw is untouched — and reaches the campaign's ~10⁶-term target: `16 * 65_536 = 1_048_576`.
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
