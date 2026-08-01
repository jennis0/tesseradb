//! `CURRENT` / `MANIFEST.json` / `SEGMENTS-<n>.json` serde structs (contracts §2.2/§2.3).
//! Unknown JSON fields are ignored — plain `#[derive(Deserialize)]` without
//! `deny_unknown_fields` — so a newer writer can add fields this reader doesn't yet know about
//! without breaking it. What that tolerance must *not* extend to is a field naming state the
//! reader would have to act on; [`HONOURED_STATE`] is where that line is drawn.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use tessera_types::{IDENTITY_CONSTRUCTION, IDENTITY_ROUNDS};

use crate::error::{Result, StoreError};

/// `CURRENT`: the bundle's only mutable file. Points at the live prefix directory and the
/// digest `MANIFEST.json` at that prefix must match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentPointer {
    pub prefix: String,
    pub manifest_digest: String,
}

/// One `files` map entry: path (prefix-relative, forward slashes) → size and hex SHA-256.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDigest {
    pub size: u64,
    pub sha256: String,
}

/// `declared_scalars` entry: one caller-declared per-item column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclaredScalar {
    pub name: String,
    pub arrow_type: String,
}

/// `quantisation`: the extent Morton codes are computed against (contracts §2.5).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Quantisation {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

/// A safe-to-print stand-in for a deployment identity key: `fp:` plus the first 8 hex characters
/// of a domain-separated SHA-256 over the key's canonical hex form.
///
/// **Why this exists.** `IdentityKey` has a redacted `Debug` and no hex accessor, but the key's
/// plaintext hex is deliberately carried alongside it (MANIFEST must record it), and that hex
/// then sits in `Debug`-deriving carriers — `IdentityDescriptor`, and through it `Manifest` and
/// `Bundle`. One `tracing::error!("{bundle:?}")` would print the deployment key. Operators still
/// need to be able to say "these two keys differ" (a rotation refusal, a support ticket), so the
/// answer is a fingerprint rather than nothing: it distinguishes keys without disclosing one.
///
/// Domain-separated so a fingerprint can never be confused with, or compared against, one of the
/// bundle's file digests; truncated because 32 bits is ample to tell two keys apart and leaves
/// nothing worth attacking.
pub fn identity_key_fingerprint(key_hex: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"tessera-identity-key-fingerprint-v1\0");
    hasher.update(key_hex.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::from("fp:");
    for byte in &digest[..4] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `MANIFEST.json`'s `identity` object (contracts §2.2/§2.6 r6): the `tessera_id`
/// permutation's construction, round count, per-deployment key and shard id. **Required** —
/// no `#[serde(default)]` — because an absent object cannot invert a `tessera_id`, and a
/// *defaulted* key would invert every identifier to the wrong entity, suppressing the wrong
/// item on `/control/changes`.
///
/// `Debug` is hand-written and redacting — see the impl below.
#[derive(Clone, Serialize, Deserialize)]
pub struct IdentityDescriptor {
    pub construction: String,
    pub rounds: u32,
    /// Exactly 32 lowercase hex characters (readers reject any other case rather than
    /// case-folding — contracts §2.6).
    pub key: String,
    pub shard_id: u32,
    /// The idset — which set of `tessera_id` values this bundle's identifiers belong to
    /// (contracts §2.2, §2.6 r6). Advanced whenever the partitioning or sharding changes,
    /// carried forward verbatim by a normal rebuild, and reset to 1 by a key rotation.
    ///
    /// **Not `#[serde(default)]`, deliberately.** `tessera_id` is stable across rebuilds but
    /// *not* across a repartition, and the churn is **partial** — so without this signal a
    /// stale identifier does not fail, it silently names whichever entity now occupies that
    /// permutation input. A defaulted idset would make every bundle claim idset 0 and defeat
    /// the one mechanism that distinguishes "your identifier is old" from "your identifier
    /// resolved". An absent `idset` is a typed reader error, exactly as an absent `identity`
    /// object is.
    pub idset: u32,
}

/// **Hand-written, not derived: `key` is the deployment's identity key in plaintext hex.**
/// `IdentityKey`'s own `Debug` is redacted, but that redaction is worthless if the same bytes
/// print from the `String` carried beside it — and this struct is reachable from `Manifest` and
/// `Bundle`, both `Debug`, so a single `{:?}` on either would emit the key. `Serialize` is
/// untouched: MANIFEST.json must still contain the key verbatim.
impl std::fmt::Debug for IdentityDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityDescriptor")
            .field("construction", &self.construction)
            .field("rounds", &self.rounds)
            .field("key", &identity_key_fingerprint(&self.key))
            .field("shard_id", &self.shard_id)
            .field("idset", &self.idset)
            .finish()
    }
}

impl IdentityDescriptor {
    /// Reject an unknown `construction` or a `rounds` other than [`IDENTITY_ROUNDS`]: a bundle
    /// written by a different construction must not be silently read by this one (contracts
    /// §2.6 r6 — "changing the construction, the round count or the round function is a
    /// `bundle_format` bump").
    pub fn validate(&self) -> Result<()> {
        if self.construction != IDENTITY_CONSTRUCTION {
            return Err(StoreError::InvalidIdentity {
                detail: format!(
                    "unknown identity construction '{}' (expected '{IDENTITY_CONSTRUCTION}')",
                    self.construction
                ),
            });
        }
        if self.rounds != IDENTITY_ROUNDS {
            return Err(StoreError::InvalidIdentity {
                detail: format!(
                    "identity rounds {} does not match this reader's {IDENTITY_ROUNDS}",
                    self.rounds
                ),
            });
        }
        // Contracts §2.2: the idset is "reset to 1 by a key rotation" and advanced from there,
        // so 0 is not a value any conforming writer produces. Refusing it here means a
        // hand-edited or partially-written manifest fails closed rather than presenting an
        // idset that no client can meaningfully compare against.
        if self.idset == 0 {
            return Err(StoreError::InvalidIdentity {
                detail: "idset is 0; conforming writers start at 1 and advance \
                         (contracts §2.2)"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// `slices` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliceDescriptor {
    pub id: String,
    pub display_name: String,
}

/// `partitions` entry. This build writes exactly one, `phash == "default"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionDescriptor {
    pub phash: String,
    #[serde(default)]
    pub required_terms: Vec<String>,
}

/// `MANIFEST.json` (contracts §2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub bundle_format: u32,
    pub created_at: String,
    pub data_plugin_hash: String,
    #[serde(default)]
    pub declared_bounds: serde_json::Value,
    #[serde(default)]
    pub declared_scalars: Vec<DeclaredScalar>,
    pub small_term_threshold: u32,
    pub quantisation: Quantisation,
    pub entity_id_high_water: u64,
    pub identity: IdentityDescriptor,
    pub slices: Vec<SliceDescriptor>,
    pub partitions: Vec<PartitionDescriptor>,
    #[serde(default)]
    pub provenance: serde_json::Value,
    pub files: BTreeMap<String, FileDigest>,
}

/// One entry of `SEGMENTS-<n>.json`'s `segments` array — one build (or streamed) segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    pub slice: String,
    pub seg_id: String,
    pub row_count: u32,
    pub entity_lo: u64,
    pub entity_hi: u64,
}

/// One entry of `deny`: the current suppression set (contracts §2.3's publication rule).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenyEntry {
    pub entity_id: u64,
    pub cause: String,
}

/// One entry of `dict_extents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictExtent {
    pub path: String,
    pub records: u64,
}

/// `SEGMENTS-<n>.json` (contracts §2.3): complete current state for one partition, written by
/// that partition's worker only after every file it names is durable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentsManifest {
    pub segments_version: u64,
    pub watermark: u64,
    pub entity_id_high_water: u64,
    pub segments: Vec<SegmentDescriptor>,
    #[serde(default)]
    pub deltas: Vec<u64>,
    #[serde(default)]
    pub dict_extents: Vec<DictExtent>,
    #[serde(default)]
    pub external_id_extents: Vec<String>,
    #[serde(default)]
    pub tombstones: Vec<u64>,
    #[serde(default)]
    pub deny: Vec<DenyEntry>,
    pub files: BTreeMap<String, FileDigest>,
}

/// The `SEGMENTS-<n>.json` state fields this reader **honours** — reads and acts on.
///
/// **Empty, and that is the accurate value.** `deltas`, `tombstones` and `deny` all parse
/// above and no read path consults any of them: there is no delta tier, no tombstone fold and
/// no deny application in the loader or the query path. A name is added here as, and only as,
/// the corresponding behaviour lands — the entry is the claim "a manifest carrying this is
/// served correctly", so adding one ahead of the code re-opens the fail-open this list exists
/// to close.
///
/// **Why an honoured list and not a forbidden list.** The default for a field this reader does
/// not understand has to be *refuse*, not *ignore*. A forbidden list is a list someone must
/// remember to extend when the format grows; an honoured list is one someone must remember to
/// extend when the *reader* grows, and forgetting it costs availability rather than
/// correctness. Note the deliberate asymmetry with this module's `#[derive(Deserialize)]`
/// without `deny_unknown_fields`: an unknown *JSON* field is ignored so a newer writer can add
/// one, but a **known** field carrying state this reader cannot act on is not.
pub const HONOURED_STATE: &[&str] = &[];

/// The subset of state fields a manifest carries **because a deny was accepted** (contracts
/// §2.3's publication rule: "any accepted deny-disposition change (delete, suppress) triggers
/// immediate publication of a new side-manifest").
///
/// This is what separates the two reader responses, and the separation is not decoration. An
/// unhonourable `deltas` means *items are missing* — staleness in the fail-safe direction, so
/// stepping down to an older manifest is legitimate and the availability argument for a
/// mid-sync replica applies. An unhonourable `tombstones` or `deny` means *items are meant to
/// be gone*, so stepping down past it re-exposes every entity suppressed or deleted since the
/// older manifest was written — the precise state §2.3 forbids a syncing replica to
/// reconstruct — and there is no bound on how long it lasts, because the freshness gate §2.3
/// pairs with step-down is a stage-2.2 obligation.
///
/// **Membership is consulted in exactly one place** — [`SegmentsManifest::honourability`]. A
/// caller that re-derives the posture from a field list and this constant is re-implementing
/// the classification, and the fail-open is one identifier wide: over `["deny", "deltas"]`,
/// `any` says unready and `all` says step down, and stepping down past an accepted suppression
/// re-exposes it. Dispatch on [`Honourability`] instead.
pub const DENY_DISPOSITION_STATE: &[&str] = &["tombstones", "deny"];

/// What a reader may do with a `SEGMENTS-<n>.json`, given the state fields it carries.
///
/// **The type exists so the posture is decided once, at the definition of the fields, rather
/// than at each call site.** The classification is an intersection of two lists
/// ([`SegmentsManifest::unhonourable_state`] against [`DENY_DISPOSITION_STATE`]), and the shape
/// that matters is the *common* one: contracts §2.3 makes a side-manifest complete for its
/// partition — "full current state, not a diff" — so every manifest published while any
/// suppression is live carries `deny` **and** whatever `deltas` exist. An `any`/`all` slip over
/// that pair classifies it as steppable, and stepping down past an accepted suppression is the
/// fail-open the whole guard exists to close. Behind this enum a caller has nothing left to get
/// wrong but the arm it takes, and each arm is a distinct reader behaviour with its own test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Honourability {
    /// Carries no state this reader cannot act on. Open it.
    Honourable,
    /// Carries only state whose absence leaves items *missing* (`deltas`). Stepping down to an
    /// older manifest is legitimate: staleness in the fail-safe direction.
    Steppable { fields: Vec<&'static str> },
    /// Carries state that exists *because a deny was accepted* ([`DENY_DISPOSITION_STATE`]).
    /// The partition is **unready** — not stale, not steppable (SA §9: "a worker that cannot
    /// verify its partition marks itself unready rather than serving partial data").
    Unready { fields: Vec<&'static str> },
}

impl SegmentsManifest {
    /// The state fields this manifest carries that [`HONOURED_STATE`] does not cover, by name.
    ///
    /// **A list of names, never a bool**, because the operator has to be told *which* build
    /// capability is missing. It deliberately says nothing about the posture to take — that is
    /// [`Self::honourability`]'s single job, so that "carries a deny" and "may be stepped past"
    /// cannot drift apart at a call site.
    ///
    /// Empty is the ordinary case: a bundle straight out of `tessera build` carries none of
    /// these, so the guard is invisible until something writes them.
    pub fn unhonourable_state(&self) -> Vec<&'static str> {
        // Deny-disposition fields first, so a truncated message still names the field that
        // decided the posture.
        [
            ("tombstones", !self.tombstones.is_empty()),
            ("deny", !self.deny.is_empty()),
            ("deltas", !self.deltas.is_empty()),
        ]
        .into_iter()
        .filter(|(name, carried)| *carried && !HONOURED_STATE.contains(name))
        .map(|(name, _)| name)
        .collect()
    }

    /// The posture a reader must take towards this manifest — **the only place the two
    /// dispositions are told apart.**
    ///
    /// `any`, not `all`: a manifest carrying a deny *and* deltas is a deny-carrying manifest.
    /// That is not a nicety about set operators, it is the ordinary published shape (see
    /// [`Honourability`]), and `all` would step down past every live suppression the moment a
    /// delta existed alongside it.
    pub fn honourability(&self) -> Honourability {
        let fields = self.unhonourable_state();
        if fields.is_empty() {
            Honourability::Honourable
        } else if fields.iter().any(|f| DENY_DISPOSITION_STATE.contains(f)) {
            Honourability::Unready { fields }
        } else {
            Honourability::Steppable { fields }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

    fn descriptor() -> IdentityDescriptor {
        IdentityDescriptor {
            construction: IDENTITY_CONSTRUCTION.to_string(),
            rounds: IDENTITY_ROUNDS,
            key: KEY_HEX.to_string(),
            shard_id: 0,
            idset: 1,
        }
    }

    /// `IdentityKey`'s `Debug` is redacted, but the key's plaintext hex is deliberately carried
    /// beside it, and `IdentityDescriptor` is reachable from `Manifest` and `Bundle` — both
    /// `Debug`. One `tracing::error!("{bundle:?}")` would otherwise print the deployment key.
    #[test]
    fn identity_descriptor_debug_does_not_print_the_key() {
        let printed = format!("{:?}", descriptor());
        assert!(
            !printed.contains(KEY_HEX),
            "Debug must not print key material, got: {printed}"
        );
        assert!(
            printed.contains(&identity_key_fingerprint(KEY_HEX)),
            "Debug should still distinguish keys by fingerprint, got: {printed}"
        );
    }

    /// The redaction must not touch `Serialize`: MANIFEST.json must still record the key verbatim,
    /// or no rebuild can carry the lineage forward.
    #[test]
    fn identity_descriptor_serialises_the_key_verbatim() {
        let json = serde_json::to_string(&descriptor()).unwrap();
        assert!(
            json.contains(KEY_HEX),
            "MANIFEST must carry the key: {json}"
        );
    }

    fn empty_segments_manifest() -> SegmentsManifest {
        SegmentsManifest {
            segments_version: 0,
            watermark: 0,
            entity_id_high_water: 0,
            segments: Vec::new(),
            deltas: Vec::new(),
            dict_extents: Vec::new(),
            external_id_extents: Vec::new(),
            tombstones: Vec::new(),
            deny: Vec::new(),
            files: BTreeMap::new(),
        }
    }

    /// The guard must be invisible on the shape `tessera build` writes, or every bundle in the
    /// project stops opening.
    #[test]
    fn a_manifest_with_no_state_carries_nothing_unhonourable() {
        assert!(empty_segments_manifest().unhonourable_state().is_empty());
        assert_eq!(
            empty_segments_manifest().honourability(),
            Honourability::Honourable
        );
    }

    /// Each deny-disposition field alone makes the partition unready, and `deltas` alone does
    /// not. This is the classification the reader dispatches on, so it is asserted here on the
    /// *type* rather than left to be re-derived from the field list at the call site.
    #[test]
    fn each_field_alone_gets_its_own_disposition() {
        let mut with_tombstone = empty_segments_manifest();
        with_tombstone.tombstones.push(17);
        assert_eq!(
            with_tombstone.honourability(),
            Honourability::Unready {
                fields: vec!["tombstones"]
            }
        );

        let mut with_deny = empty_segments_manifest();
        with_deny.deny.push(DenyEntry {
            entity_id: 17,
            cause: "suppress".to_string(),
        });
        assert_eq!(
            with_deny.honourability(),
            Honourability::Unready {
                fields: vec!["deny"]
            }
        );

        let mut with_delta = empty_segments_manifest();
        with_delta.deltas.push(1);
        assert_eq!(
            with_delta.honourability(),
            Honourability::Steppable {
                fields: vec!["deltas"]
            }
        );
    }

    /// Each field is reported by name and independently — the operator has to be told *which*
    /// build capability is missing, and [`SegmentsManifest::honourability`] classifies from this
    /// list, so a reader that collapsed it would have nothing to classify from.
    #[test]
    fn each_carried_state_field_is_reported_by_name() {
        let mut with_tombstone = empty_segments_manifest();
        with_tombstone.tombstones.push(17);
        assert_eq!(with_tombstone.unhonourable_state(), vec!["tombstones"]);

        let mut with_deny = empty_segments_manifest();
        with_deny.deny.push(DenyEntry {
            entity_id: 17,
            cause: "suppress".to_string(),
        });
        assert_eq!(with_deny.unhonourable_state(), vec!["deny"]);

        let mut with_delta = empty_segments_manifest();
        with_delta.deltas.push(1);
        assert_eq!(with_delta.unhonourable_state(), vec!["deltas"]);
    }

    /// **The common shape, and the one-identifier fail-open.** Contracts §2.3 makes a
    /// side-manifest complete for its partition, so every manifest published while a
    /// suppression is live carries `deny` *and* whatever `deltas` exist — `deny` alone is the
    /// rarer case. Over `["deny", "deltas"]`, `any` says unready and `all` says steppable, and
    /// steppable here means serving the pre-suppression state indefinitely.
    ///
    /// Asserted on [`Honourability`], not on the field list: a list that merely *contains*
    /// "deny" proves nothing about what the reader then does with it.
    #[test]
    fn a_deny_alongside_deltas_is_unready_not_steppable() {
        let mut manifest = empty_segments_manifest();
        manifest.deltas.push(1);
        manifest.deny.push(DenyEntry {
            entity_id: 17,
            cause: "suppress".to_string(),
        });
        assert_eq!(
            manifest.honourability(),
            Honourability::Unready {
                // Deny-disposition fields first, so a truncated message still names the field
                // that decided the posture.
                fields: vec!["deny", "deltas"]
            }
        );

        // And the same for a tombstone beside deltas — the other deny-disposition field.
        let mut manifest = empty_segments_manifest();
        manifest.deltas.push(1);
        manifest.tombstones.push(17);
        assert_eq!(
            manifest.honourability(),
            Honourability::Unready {
                fields: vec!["tombstones", "deltas"]
            }
        );
    }

    /// `HONOURED_STATE` is the claim "the read path acts on this field". It is empty today, and
    /// this test is the tripwire on an entry being added ahead of the behaviour it asserts:
    /// whoever adds the first honoured field must delete or amend this deliberately, with the
    /// code to justify it.
    #[test]
    fn no_state_field_is_claimed_as_honoured_yet() {
        assert!(
            HONOURED_STATE.is_empty(),
            "the read path consults no delta tier, no tombstone fold and no deny set; \
             adding a name here without the behaviour re-opens the fail-open"
        );
    }

    #[test]
    fn fingerprints_distinguish_keys_and_are_not_the_key() {
        let a = identity_key_fingerprint(KEY_HEX);
        let b = identity_key_fingerprint("100f0e0d0c0b0a090807060504030201");
        assert_ne!(a, b);
        assert!(a.starts_with("fp:") && a.len() == 3 + 8);
    }
}
