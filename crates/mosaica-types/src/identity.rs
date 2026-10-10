//! The `mosaica_id` construction: a keyed **blinding permutation**, not encryption.
//!
//! Threat model, in three sentences (design memo
//! `docs/evidence/memos/2026-07-30-tessera-id-construction.md`, normative): the key is not
//! secret against a bundle-holder, who can already invert every `mosaica_id` trivially and
//! gains nothing from doing so; the property actually defended is that a **viewer-plane**
//! client — holding `mosaica_id`s and no bundle — cannot derive entity IDs, cannot order
//! them, and cannot count the gaps between them; and the key must never leave the server,
//! on any plane, in any response, log line or metric label.
//!
//! `mosaica_id = FPE_k(high: u32 ‖ number: u32) -> u64`: a balanced Feistel network,
//! 8 rounds, 32-bit halves, round function `splitmix64` (a non-cryptographic mixer — see
//! the memo §8 for the ruling that keeps it, and §3 for why a cryptographic PRF is not
//! required here). Do not change the round count, the round function, the packing or the
//! key schedule: see the memo §1.
//!
//! An item's high half is [`ItemHigh`]: from the most significant bit, a kind bit of 0, the
//! shard in 19 bits and the item's [`Tenancy`] in 12. The low half is the item's number, the
//! entity id it holds. An identifier whose kind bit is 1 names no item.

use crate::EntityId;

/// Number of Feistel rounds in the `mosaica_id` construction. Fixed by the memo §1 — do
/// not change.
pub const IDENTITY_ROUNDS: u32 = 8;

/// Identifies the construction, as recorded in MANIFEST's `identity.construction` field.
pub const IDENTITY_CONSTRUCTION: &str = "feistel-splitmix64-v1";

/// Errors from key parsing and from the checked `high ‖ entity_id -> mosaica_id` conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// The hex string was not exactly 32 lowercase hexadecimal characters.
    InvalidHexLength { len: usize },
    /// The hex string contained a character outside `0-9a-f` (including `A-F`).
    InvalidHexChar { ch: char },
    /// The key is all-zero (a special case of `k1 == 0`, named separately -- memo §1.3 --
    /// so the message says which of the two an operator hit).
    DegenerateKeyAllZero,
    /// The key's second 64-bit half (`k1`) is zero (and `k0` is not), collapsing the key
    /// schedule to a single repeated round key for all eight rounds (memo §1.3).
    DegenerateKeyK1Zero,
    /// `forward`'s entity input exceeded `u32::MAX`. Plan Important I-1: a truncating
    /// cast would make "collision-free by construction" false. See memo §1.8.
    EntityOutOfRange { entity: u64 },
    /// A shard id does not fit the 19 bits [`ItemHigh`] gives it.
    ShardOutOfRange { shard: u32 },
    /// A tenancy does not fit the 12 bits [`ItemHigh`] gives it.
    TenancyOutOfRange { tenancy: u16 },
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::InvalidHexLength { len } => {
                write!(
                    f,
                    "identity key must be exactly 32 hex characters, got {len}"
                )
            }
            IdentityError::InvalidHexChar { ch } => {
                write!(
                    f,
                    "identity key contains non-lowercase-hex character {ch:?}"
                )
            }
            IdentityError::DegenerateKeyAllZero => {
                write!(f, "identity key is degenerate: all-zero key (k1 == 0)")
            }
            IdentityError::DegenerateKeyK1Zero => {
                write!(
                    f,
                    "identity key is degenerate: k1 == 0 (round schedule collapses)"
                )
            }
            IdentityError::EntityOutOfRange { entity } => {
                write!(
                    f,
                    "entity id {entity} exceeds u32::MAX; cannot form a mosaica_id"
                )
            }
            IdentityError::ShardOutOfRange { shard } => {
                write!(
                    f,
                    "shard id {shard} does not fit in 19 bits; a shard id must be below {}",
                    ItemHigh::SHARD_LIMIT
                )
            }
            IdentityError::TenancyOutOfRange { tenancy } => {
                write!(
                    f,
                    "tenancy {tenancy} does not fit in 12 bits; a tenancy must be at most {}",
                    Tenancy::MAX.0
                )
            }
        }
    }
}

impl std::error::Error for IdentityError {}

/// `mosaica_id`: the opaque, blinded wire identity of a point (invariant I10 — entity IDs
/// never cross the trust boundary). A newtype with no conversions to or from any other ID
/// newtype (invariant I4); see `IdentityKey::forward` / `IdentityKey::invert` for the only
/// way to produce or unwrap one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MosaicaId(u64);

impl MosaicaId {
    #[inline]
    pub fn new(raw: u64) -> Self {
        MosaicaId(raw)
    }

    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// The leading 16 bits of the identity — contracts §2.6's `priority`, and the **one
    /// and only** definition of the priority column (2026-07-30 fold). It lives beside the
    /// construction it is a prefix of because a second inline `>> 48` elsewhere is exactly
    /// how the column and the sort key drift apart.
    #[inline]
    pub fn priority(&self) -> u16 {
        (self.0 >> 48) as u16
    }
}

/// How many items have held an entity id as their number before the item that holds it now.
/// Twelve bits, so at most [`Tenancy::MAX`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Tenancy(u16);

impl Tenancy {
    pub const BITS: u32 = 12;
    pub const ZERO: Tenancy = Tenancy(0);
    pub const MAX: Tenancy = Tenancy((1 << Self::BITS) - 1);

    pub fn new(raw: u16) -> Result<Tenancy, IdentityError> {
        if raw > Self::MAX.0 {
            return Err(IdentityError::TenancyOutOfRange { tenancy: raw });
        }
        Ok(Tenancy(raw))
    }

    #[inline]
    pub fn raw(self) -> u16 {
        self.0
    }

    /// The tenancy one higher, or `None` at [`Tenancy::MAX`].
    pub fn next(self) -> Option<Tenancy> {
        (self < Self::MAX).then_some(Tenancy(self.0 + 1))
    }
}

/// The high 32 bits of an item's `mosaica_id` input: `0 << 31 | shard << 12 | tenancy`.
/// [`ItemHigh::pack`] refuses a shard of [`ItemHigh::SHARD_LIMIT`] or more.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ItemHigh {
    pub shard: u32,
    pub tenancy: Tenancy,
}

impl ItemHigh {
    pub const SHARD_BITS: u32 = 19;
    /// The lowest shard id that does not fit.
    pub const SHARD_LIMIT: u32 = 1 << Self::SHARD_BITS;
    const KIND_BIT: u32 = 1 << (Self::SHARD_BITS + Tenancy::BITS);

    #[inline]
    pub const fn new(shard: u32, tenancy: Tenancy) -> Self {
        ItemHigh { shard, tenancy }
    }

    pub fn pack(self) -> Result<u32, IdentityError> {
        if self.shard >= Self::SHARD_LIMIT {
            return Err(IdentityError::ShardOutOfRange { shard: self.shard });
        }
        Ok((self.shard << Tenancy::BITS) | self.tenancy.0 as u32)
    }

    /// The item high half packed in `high`, or `None` when its kind bit is set and it names
    /// something other than an item.
    pub fn unpack(high: u32) -> Option<ItemHigh> {
        if high & Self::KIND_BIT != 0 {
            return None;
        }
        Some(ItemHigh {
            shard: high >> Tenancy::BITS,
            tenancy: Tenancy((high & Tenancy::MAX.0 as u32) as u16),
        })
    }
}

/// The bundle's 128-bit key for the `mosaica_id` blinding permutation, generated when the bundle
/// is created and stored in its manifest. `Debug` is redacted. The key never leaves the server.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IdentityKey {
    k0: u64,
    k1: u64,
}

impl std::fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IdentityKey").field(&"<redacted>").finish()
    }
}

/// `splitmix64`, exactly as contracts §2.6 fixes it: all arithmetic wrapping `u64`, all
/// shifts logical right shifts on unsigned values. Shift amounts 30, 27, 31, in that order.
#[inline]
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl IdentityKey {
    /// Parse a 128-bit key from **exactly 32 lowercase hexadecimal characters**
    /// (`0-9a-f`), byte 0 first. Rejects, does not normalise: uppercase, wrong length, any
    /// non-hex character, and any leading `0x`, whitespace or separator are all typed
    /// errors (memo §1.2). Also rejects degenerate keys — `k1 == 0` (including the
    /// all-zero key) — because they collapse the key schedule to one repeated round key
    /// without failing loudly anywhere else (memo §1.3).
    pub fn from_hex(s: &str) -> Result<Self, IdentityError> {
        if s.len() != 32 {
            return Err(IdentityError::InvalidHexLength { len: s.len() });
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let hi = s.as_bytes()[i * 2] as char;
            let lo = s.as_bytes()[i * 2 + 1] as char;
            let hi_val = lowercase_hex_digit(hi)?;
            let lo_val = lowercase_hex_digit(lo)?;
            *byte = (hi_val << 4) | lo_val;
        }
        let k0 = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let k1 = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        Self::from_parts(k0, k1)
    }

    /// A new key from the operating system's random source, for a bundle being created. A draw
    /// that [`IdentityKey::from_hex`] would refuse as degenerate (`k1 == 0`) is drawn again.
    pub fn generate() -> Self {
        loop {
            let mut bytes = [0u8; 16];
            getrandom::getrandom(&mut bytes).expect("the operating system's random source");
            let k0 = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let k1 = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
            if let Ok(key) = Self::from_parts(k0, k1) {
                return key;
            }
        }
    }

    /// The canonical form [`IdentityKey::from_hex`] reads: 32 lowercase hexadecimal characters,
    /// byte 0 first.
    pub fn to_hex(&self) -> String {
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&self.k0.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.k1.to_le_bytes());
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Construct directly from the two little-endian `u64` halves, refusing degenerate
    /// keys (`k1 == 0`, which subsumes the all-zero key).
    fn from_parts(k0: u64, k1: u64) -> Result<Self, IdentityError> {
        if k0 == 0 && k1 == 0 {
            return Err(IdentityError::DegenerateKeyAllZero);
        }
        if k1 == 0 {
            return Err(IdentityError::DegenerateKeyK1Zero);
        }
        Ok(IdentityKey { k0, k1 })
    }

    /// `round_key(i) = splitmix64(k0 ^ (k1 *wrapping (i + 1)))` for `i = 0..8`, multiplier
    /// taking the values `1..=8` (memo §1.4).
    #[inline]
    fn round_key(&self, i: u32) -> u64 {
        let multiplier = (i as u64) + 1;
        splitmix64(self.k0 ^ self.k1.wrapping_mul(multiplier))
    }

    /// `F(i, r) = (splitmix64(r as u64 ^ round_key(i)) >> 32) as u32` — the **high** 32
    /// bits (memo §1.5). Taking the low half instead is still a bijection but disagrees
    /// with every vector in the file.
    #[inline]
    fn f(&self, i: u32, r: u32) -> u32 {
        (splitmix64((r as u64) ^ self.round_key(i)) >> 32) as u32
    }

    /// The `mosaica_id` of the item at `high` holding `entity` as its number. Refuses a shard
    /// [`ItemHigh::pack`] refuses, and an entity above `u32::MAX` (plan Important I-1, memo
    /// §1.8): a truncating cast would let two entities differing only above bit 32 share a
    /// `mosaica_id` and make `invert` return the wrong entity.
    pub fn forward(&self, high: ItemHigh, entity: EntityId) -> Result<MosaicaId, IdentityError> {
        self.forward_raw(high.pack()?, entity)
    }

    /// The item high half and number `id` was formed from, or `None` when `id` names no item.
    /// Every `u64` inverts (memo §1.7); the caller compares the shard and the tenancy with the
    /// ones it holds.
    pub fn invert(&self, id: MosaicaId) -> Option<(ItemHigh, EntityId)> {
        let (high, entity) = self.invert_raw(id);
        Some((ItemHigh::unpack(high)?, entity))
    }

    /// `FPE_k(high ‖ entity)` over any 32-bit high half.
    fn forward_raw(&self, high: u32, entity: EntityId) -> Result<MosaicaId, IdentityError> {
        let entity_raw = entity.raw();
        if entity_raw > u32::MAX as u64 {
            return Err(IdentityError::EntityOutOfRange { entity: entity_raw });
        }
        let mut l = high;
        let mut r = entity_raw as u32;
        for i in 0..IDENTITY_ROUNDS {
            let new_l = r;
            let new_r = l ^ self.f(i, r);
            l = new_l;
            r = new_r;
        }
        Ok(MosaicaId(((l as u64) << 32) | (r as u64)))
    }

    /// The inverse of [`IdentityKey::forward_raw`], total over the `u64` space.
    fn invert_raw(&self, id: MosaicaId) -> (u32, EntityId) {
        let raw = id.raw();
        let mut l = (raw >> 32) as u32;
        let mut r = raw as u32;
        for i in (0..IDENTITY_ROUNDS).rev() {
            let new_l = r ^ self.f(i, l);
            let new_r = l;
            l = new_l;
            r = new_r;
        }
        (l, EntityId::new(r as u64))
    }
}

#[inline]
fn lowercase_hex_digit(ch: char) -> Result<u8, IdentityError> {
    match ch {
        '0'..='9' => Ok(ch as u8 - b'0'),
        'a'..='f' => Ok(ch as u8 - b'a' + 10),
        _ => Err(IdentityError::InvalidHexChar { ch }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL_KEY: &str = "000102030405060708090a0b0c0d0e0f";
    const ITEM: ItemHigh = ItemHigh::new(0, Tenancy::ZERO);

    /// Loads `reference/vectors/mosaica_id.json`, shared with every test below that reads
    /// from it -- a single load point so a future correction to the file reaches every
    /// block, not just `vectors`/`inverse_only`/`secondary_key`/`rejected_keys` (task-5
    /// review minor: `splitmix64_known_answers` and `key_schedule_matches_vectors` used to
    /// hardcode these values instead).
    fn load_vectors_doc() -> serde_json::Value {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../reference/vectors/mosaica_id.json"
        ))
        .expect("reference/vectors/mosaica_id.json must exist");
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn splitmix64_known_answers() {
        let doc = load_vectors_doc();
        let cases = doc["splitmix64"].as_array().unwrap();
        assert_eq!(cases.len(), 6);
        for case in cases {
            let input = parse_hex_u64(case["input"].as_str().unwrap());
            let expected = parse_hex_u64(case["output"].as_str().unwrap());
            assert_eq!(splitmix64(input), expected, "splitmix64({input:#x})");
        }
    }

    #[test]
    fn key_schedule_matches_vectors() {
        let doc = load_vectors_doc();
        let ks = &doc["key_schedule"];
        let key = IdentityKey::from_hex(ks["key"].as_str().unwrap()).unwrap();
        assert_eq!(key.k0, parse_hex_u64(ks["k0"].as_str().unwrap()));
        assert_eq!(key.k1, parse_hex_u64(ks["k1"].as_str().unwrap()));
        let expected: Vec<u64> = ks["round_keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| parse_hex_u64(v.as_str().unwrap()))
            .collect();
        assert_eq!(expected.len(), IDENTITY_ROUNDS as usize);
        for (i, exp) in expected.iter().enumerate() {
            assert_eq!(key.round_key(i as u32), *exp, "round key {i}");
        }
    }

    #[test]
    fn forward_and_invert_round_trip_over_the_whole_low_space() {
        // A Feistel is a permutation for any round function, so this cannot fail for a
        // correct transcription -- which is exactly why it is worth running: the realistic
        // failure is a swapped half or an off-by-one round index, and both break here.
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        for e in (0u32..1 << 22).step_by(7) {
            let id = key.forward(ITEM, EntityId::new(e as u64)).unwrap();
            assert_eq!(key.invert(id), Some((ITEM, EntityId::new(e as u64))));
        }
    }

    #[test]
    fn it_is_injective_over_a_large_contiguous_run() {
        // Collision-freedom is structural, not probabilistic -- assert it anyway over a
        // run large enough that a broken round loop would show, because the whole design
        // rests on there being no collision-detection pass anywhere.
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let seen: std::collections::HashSet<u64> = (0u32..1 << 21)
            .map(|e| key.forward(ITEM, EntityId::new(e as u64)).unwrap().raw())
            .collect();
        assert_eq!(seen.len(), 1 << 21);
    }

    #[test]
    fn it_matches_the_shared_known_answer_vectors() {
        // reference/vectors/mosaica_id.json was generated from the spec text before either
        // implementation existed. The Python oracle tests against the same file. Disagreement
        // here means the Rust is wrong; agreement between two independent implementations and
        // the file is the evidence the construction is reproducible.
        let doc = load_vectors_doc();

        // Catches the file being swapped to a different construction: without this, neither
        // implementation's tests would assert these two top-level
        // fields against anything.
        assert_eq!(doc["construction"].as_str().unwrap(), IDENTITY_CONSTRUCTION);
        assert_eq!(doc["rounds"].as_u64().unwrap(), IDENTITY_ROUNDS as u64);

        let key_hex = doc["key"].as_str().unwrap();
        let key = IdentityKey::from_hex(key_hex).unwrap();

        let main_vectors = doc["vectors"].as_array().unwrap();
        assert_eq!(main_vectors.len(), 33);
        for v in main_vectors {
            let shard = v["shard_id"].as_u64().unwrap() as u32;
            let entity = v["entity_id"].as_u64().unwrap();
            let expected = parse_hex_u64(v["mosaica_id"].as_str().unwrap());
            let id = key.forward_raw(shard, EntityId::new(entity)).unwrap();
            assert_eq!(id.raw(), expected, "forward shard={shard} entity={entity}");
            assert_eq!(
                key.invert_raw(MosaicaId::new(expected)),
                (shard, EntityId::new(entity)),
                "invert shard={shard} entity={entity}"
            );
        }

        // `inverse_only`: asserted in BOTH directions (task-5 review minor -- Rust used
        // to assert the inverse direction only). Valid because the construction is a
        // total bijection over 2**64 (memo §1.7): `forward(*invert(x)) == x` must hold
        // even for a `mosaica_id` not drawn from a forward-generated vector.
        let inverse_only = doc["inverse_only"].as_array().unwrap();
        assert_eq!(inverse_only.len(), 6);
        for v in inverse_only {
            let id = parse_hex_u64(v["mosaica_id"].as_str().unwrap());
            let shard = v["shard_id"].as_u64().unwrap() as u32;
            let entity = v["entity_id"].as_u64().unwrap();
            assert_eq!(
                key.invert_raw(MosaicaId::new(id)),
                (shard, EntityId::new(entity))
            );
            assert_eq!(
                key.forward_raw(shard, EntityId::new(entity)).unwrap().raw(),
                id,
                "forward(*invert({id:#x})) should round-trip"
            );
        }

        let sk = &doc["secondary_key"];
        let sk_key = IdentityKey::from_hex(sk["key"].as_str().unwrap()).unwrap();
        assert_eq!(sk_key.k0, parse_hex_u64(sk["k0"].as_str().unwrap()));
        assert_eq!(sk_key.k1, parse_hex_u64(sk["k1"].as_str().unwrap()));
        // `secondary_key.vectors`: asserted in BOTH directions too (same minor).
        let sk_vectors = sk["vectors"].as_array().unwrap();
        assert_eq!(sk_vectors.len(), 6);
        for v in sk_vectors {
            let shard = v["shard_id"].as_u64().unwrap() as u32;
            let entity = v["entity_id"].as_u64().unwrap();
            let expected = parse_hex_u64(v["mosaica_id"].as_str().unwrap());
            let id = sk_key.forward_raw(shard, EntityId::new(entity)).unwrap();
            assert_eq!(
                id.raw(),
                expected,
                "secondary_key shard={shard} entity={entity}"
            );
            assert_eq!(
                sk_key.invert_raw(MosaicaId::new(expected)),
                (shard, EntityId::new(entity)),
                "secondary_key invert shard={shard} entity={entity}"
            );
        }

        let rejected_keys = doc["rejected_keys"].as_array().unwrap();
        assert_eq!(rejected_keys.len(), 6);
        for v in rejected_keys {
            let key_str = v["key"].as_str().unwrap();
            assert!(
                IdentityKey::from_hex(key_str).is_err(),
                "expected rejection for key {key_str}: {}",
                v["reason"].as_str().unwrap_or("")
            );
        }
    }

    fn parse_hex_u64(s: &str) -> u64 {
        u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
    }

    #[test]
    fn consecutive_entities_do_not_yield_ordered_identities() {
        // The property C6 rests on: the fraction of adjacent pairs that ascend must sit
        // near 1/2. An identity-like or lightly-perturbed permutation lands at ~1.0.
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let n = 10_000u32;
        let mut ascending = 0u32;
        let mut prev = key.forward(ITEM, EntityId::new(0)).unwrap().raw();
        for e in 1..n {
            let cur = key.forward(ITEM, EntityId::new(e as u64)).unwrap().raw();
            if cur > prev {
                ascending += 1;
            }
            prev = cur;
        }
        let fraction = ascending as f64 / (n - 1) as f64;
        assert!(
            (0.4..0.6).contains(&fraction),
            "ascending fraction {fraction} is not near 1/2"
        );
    }

    #[test]
    fn a_different_key_gives_a_different_identity_for_the_same_entity() {
        let key_a = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let key_b = IdentityKey::from_hex("100f0e0d0c0b0a090807060504030201").unwrap();
        let id_a = key_a.forward(ITEM, EntityId::new(42)).unwrap();
        let id_b = key_b.forward(ITEM, EntityId::new(42)).unwrap();
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn the_shard_and_the_tenancy_separate_identity_spaces() {
        // Every caller in this build passes shard 0, and every number is at tenancy 0, so
        // this keeps both fields from being tidied out of the input encoding.
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let shard_one = ItemHigh::new(1, Tenancy::ZERO);
        let tenancy_one = ItemHigh::new(0, Tenancy::ZERO.next().unwrap());
        for e in 0u32..1000 {
            let e = EntityId::new(e as u64);
            let id0 = key.forward(ITEM, e).unwrap();
            let id1 = key.forward(shard_one, e).unwrap();
            let id2 = key.forward(tenancy_one, e).unwrap();
            assert_ne!(id0, id1);
            assert_ne!(id0, id2);
            assert_ne!(id1, id2);
            assert_eq!(key.invert(id1), Some((shard_one, e)));
            assert_eq!(key.invert(id2), Some((tenancy_one, e)));
        }
    }

    #[test]
    fn it_matches_the_packed_vectors() {
        let doc = load_vectors_doc();
        let packed = &doc["packed"];
        let key = IdentityKey::from_hex(doc["key"].as_str().unwrap()).unwrap();
        let sk_key = IdentityKey::from_hex(doc["secondary_key"]["key"].as_str().unwrap()).unwrap();

        let vectors = packed["vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), 8);
        let sk_vectors = packed["secondary_key_vectors"].as_array().unwrap();
        assert_eq!(sk_vectors.len(), 2);
        let cases = vectors
            .iter()
            .map(|v| (&key, v))
            .chain(sk_vectors.iter().map(|v| (&sk_key, v)));
        let mut kinds = [0usize; 2];
        for (key, v) in cases {
            let kind = v["kind"].as_u64().unwrap();
            let shard = v["shard"].as_u64().unwrap() as u32;
            let tenancy = Tenancy::new(v["tenancy"].as_u64().unwrap() as u16).unwrap();
            let number = EntityId::new(v["number"].as_u64().unwrap());
            let high = v["high"].as_u64().unwrap() as u32;
            let id = MosaicaId::new(parse_hex_u64(v["mosaica_id"].as_str().unwrap()));
            kinds[kind as usize] += 1;

            assert_eq!(key.forward_raw(high, number), Ok(id), "{v}");
            assert_eq!(key.invert_raw(id), (high, number), "{v}");
            if kind == 0 {
                let item = ItemHigh::new(shard, tenancy);
                assert_eq!(item.pack(), Ok(high), "{v}");
                assert_eq!(ItemHigh::unpack(high), Some(item), "{v}");
                assert_eq!(key.forward(item, number), Ok(id), "{v}");
                assert_eq!(key.invert(id), Some((item, number)), "{v}");
            } else {
                assert_eq!(kind, 1);
                assert_eq!(ItemHigh::unpack(high), None, "{v}");
                assert_eq!(key.invert(id), None, "{v}");
            }
        }
        assert!(kinds[0] > 0 && kinds[1] > 0);

        let rejected = packed["rejected_packings"].as_array().unwrap();
        assert_eq!(rejected.len(), 2);
        for v in rejected {
            let shard = v["shard"].as_u64().unwrap() as u32;
            let tenancy = v["tenancy"].as_u64().unwrap() as u16;
            let refused = Tenancy::new(tenancy).and_then(|t| ItemHigh::new(shard, t).pack());
            assert!(refused.is_err(), "{v}");
        }
    }

    #[test]
    fn an_item_at_shard_zero_and_tenancy_zero_has_the_raw_identifier() {
        let doc = load_vectors_doc();
        let key = IdentityKey::from_hex(doc["key"].as_str().unwrap()).unwrap();
        for v in doc["vectors"].as_array().unwrap() {
            if v["shard_id"].as_u64() == Some(0) {
                let entity = EntityId::new(v["entity_id"].as_u64().unwrap());
                let id = parse_hex_u64(v["mosaica_id"].as_str().unwrap());
                assert_eq!(key.forward(ITEM, entity).unwrap().raw(), id);
            }
        }
    }

    #[test]
    fn the_tenancy_counts_to_its_maximum_and_stops() {
        assert_eq!(Tenancy::MAX.raw(), 4095);
        assert_eq!(Tenancy::new(4095), Ok(Tenancy::MAX));
        assert!(Tenancy::new(4096).is_err());
        assert_eq!(Tenancy::MAX.next(), None);
        let mut t = Tenancy::ZERO;
        for _ in 0..4095 {
            t = t.next().unwrap();
        }
        assert_eq!(t, Tenancy::MAX);
    }

    #[test]
    fn a_shard_that_does_not_fit_is_refused() {
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let last = ItemHigh::new(ItemHigh::SHARD_LIMIT - 1, Tenancy::MAX);
        assert_eq!(last.pack(), Ok(0x7fff_ffff));
        let id = key.forward(last, EntityId::new(9)).unwrap();
        assert_eq!(key.invert(id), Some((last, EntityId::new(9))));
        assert!(matches!(
            key.forward(
                ItemHigh::new(ItemHigh::SHARD_LIMIT, Tenancy::ZERO),
                EntityId::new(9)
            ),
            Err(IdentityError::ShardOutOfRange { .. })
        ));
    }

    #[test]
    fn forward_refuses_an_entity_above_u32_max_rather_than_truncating() {
        // IMPORTANT I-1. A truncating cast makes "collision-free by construction" FALSE:
        // 0x1_0000_0000 and 0x0 would share a mosaica_id, and `invert` would name the wrong
        // entity -- a /control/changes suppression against the wrong item. The allocator's
        // u32 cap makes this unreachable; this makes a bypass loud.
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        assert!(matches!(
            key.forward(ITEM, EntityId::new(1u64 << 32)),
            Err(IdentityError::EntityOutOfRange { .. })
        ));
        assert!(key
            .forward(ITEM, EntityId::new(u32::MAX as u64 - 1))
            .is_ok());
    }

    #[test]
    fn degenerate_keys_are_refused() {
        // k1 == 0 collapses the schedule to ONE constant round key for all eight rounds;
        // the all-zero key does the same and is additionally what an uninitialised buffer
        // supplies. Both are still permutations, so nothing fails loudly -- which is exactly
        // why they must be refused at the door.
        assert!(matches!(
            IdentityKey::from_hex("0f0e0d0c0b0a09080000000000000000").unwrap_err(),
            IdentityError::DegenerateKeyK1Zero
        ));
        assert!(matches!(
            IdentityKey::from_hex("00000000000000000000000000000000").unwrap_err(),
            IdentityError::DegenerateKeyAllZero
        ));
    }

    #[test]
    fn the_hex_form_is_lowercase_only_and_is_not_case_folded() {
        // MANIFEST has one canonical spelling because a digest is taken over it. Uppercase
        // is a typed error, not a synonym -- folding would let two MANIFESTs that differ
        // byte-wise claim the same key.
        assert!(IdentityKey::from_hex("000102030405060708090A0B0C0D0E0F").is_err());
        assert!(IdentityKey::from_hex(CANONICAL_KEY).is_ok());
    }

    #[test]
    fn priority_is_the_leading_sixteen_bits_of_the_identity() {
        // Contracts §2.6 r6. The point of the redefinition is that the sort prefix and the
        // full sort key are the same value, so this is not a formatting detail: if priority
        // is ever anything but a prefix, "k lowest by priority then by mosaica_id" stops
        // being "k lowest by mosaica_id" and the sampler acquires a composite comparator.
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        for e in (0u32..1 << 16).step_by(13) {
            let id = key.forward(ITEM, EntityId::new(e as u64)).unwrap();
            assert_eq!(id.priority(), (id.raw() >> 48) as u16);
        }
    }

    #[test]
    fn ordering_by_priority_then_id_is_ordering_by_id() {
        // Assert the equivalence the prefix argument rests on over a shuffled sample:
        // sort_by(|a,b| a.priority().cmp(&b.priority()).then(a.raw().cmp(&b.raw()))) must
        // produce exactly sort_by_key(|x| x.raw()).
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let mut ids: Vec<MosaicaId> = (0u32..5000)
            .map(|e| key.forward(ITEM, EntityId::new(e as u64)).unwrap())
            .collect();

        let mut by_raw = ids.clone();
        by_raw.sort_by_key(|x| x.raw());

        // Shuffle deterministically (reverse + interleave) before sorting by the composite
        // key, so the composite sort is doing real work rather than confirming a no-op.
        ids.reverse();
        let mut by_priority_then_raw = ids;
        by_priority_then_raw
            .sort_by(|a, b| a.priority().cmp(&b.priority()).then(a.raw().cmp(&b.raw())));

        assert_eq!(by_priority_then_raw, by_raw);
    }

    #[test]
    fn a_generated_key_is_valid_and_differs_from_the_next() {
        let key = IdentityKey::generate();
        assert_eq!(IdentityKey::from_hex(&key.to_hex()), Ok(key));
        assert_ne!(key, IdentityKey::generate());
    }

    #[test]
    fn the_hex_form_round_trips() {
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        assert_eq!(key.to_hex(), CANONICAL_KEY);
    }

    #[test]
    fn debug_does_not_print_key_material() {
        let key = IdentityKey::from_hex(CANONICAL_KEY).unwrap();
        let printed = format!("{key:?}");
        assert!(!printed.contains("0706050403020100"));
        assert!(printed.contains("redacted"));
    }
}
