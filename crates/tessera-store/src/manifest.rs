//! `CURRENT` / `MANIFEST.json` / `SEGMENTS-<n>.json` serde structs (contracts §2.2/§2.3,
//! Reference Sheet R4). Unknown JSON fields are ignored — plain `#[derive(Deserialize)]`
//! without `deny_unknown_fields` — so a newer writer can add fields this reader doesn't yet
//! know about without breaking it (shared-context constraint per the task brief).

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
    /// The transport-identity epoch (contracts §2.2, §2.6 r6). Advanced whenever the
    /// partitioning or sharding changes, carried forward verbatim by a normal rebuild, and
    /// reset to 1 by a key rotation.
    ///
    /// **Not `#[serde(default)]`, deliberately.** `tessera_id` is stable across rebuilds but
    /// *not* across a repartition, and the churn is **partial** — so without this signal a
    /// stale identifier does not fail, it silently names whichever entity now occupies that
    /// permutation input. A defaulted epoch would make every bundle claim epoch 0 and defeat
    /// the one mechanism that distinguishes "your identifier is old" from "your identifier
    /// resolved". An absent `epoch` is a typed reader error, exactly as an absent `identity`
    /// object is.
    pub epoch: u32,
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
            .field("epoch", &self.epoch)
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
        // Contracts §2.2: the epoch is "reset to 1 by a key rotation" and advanced from there,
        // so 0 is not a value any conforming writer produces. Refusing it here means a
        // hand-edited or partially-written manifest fails closed rather than presenting an
        // epoch that no client can meaningfully compare against.
        if self.epoch == 0 {
            return Err(StoreError::InvalidIdentity {
                detail: "identity epoch is 0; conforming writers start at 1 and advance \
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

/// `partitions` entry. Phase 1 has exactly one, `phash == "default"`.
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
/// no deny application in the loader or the query path. Stage 2.2 adds each name here as, and
/// only as, the corresponding behaviour lands — the entry is the claim "a manifest carrying
/// this is served correctly", so adding one ahead of the code re-opens the fail-open this
/// list exists to close.
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
pub const DENY_DISPOSITION_STATE: &[&str] = &["tombstones", "deny"];

impl SegmentsManifest {
    /// The state fields this manifest carries that [`HONOURED_STATE`] does not cover, by name.
    ///
    /// **A list of names, never a bool**, because the caller has to make two decisions from
    /// one answer: *what to tell the operator* (which build capability is missing) and *what
    /// posture to take* (step down, or refuse the partition — see [`DENY_DISPOSITION_STATE`]).
    /// A bool collapses a suppression that must not be stepped past into the same value as a
    /// missing delta tier, which is exactly the conflation that makes the refusal fail-open.
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
            epoch: 1,
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
    }

    /// Each field is reported by name and independently — a reader that collapsed them would
    /// hand `load_verifying_segments_manifest` no way to tell "step down" from "unready".
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

    /// A manifest carrying deltas *and* a deny must still be classified as a deny — which it is
    /// only because every carried field is reported, not just the first. The ordering is part of
    /// the contract: the field deciding the posture is named first.
    #[test]
    fn a_deny_alongside_deltas_is_still_named() {
        let mut manifest = empty_segments_manifest();
        manifest.deltas.push(1);
        manifest.deny.push(DenyEntry {
            entity_id: 17,
            cause: "suppress".to_string(),
        });
        let fields = manifest.unhonourable_state();
        assert_eq!(fields, vec!["deny", "deltas"]);
        assert!(fields.iter().any(|f| DENY_DISPOSITION_STATE.contains(f)));
    }

    /// `HONOURED_STATE` is the claim "the read path acts on this field". It is empty today, and
    /// this test is the tripwire on an entry being added ahead of the behaviour it asserts —
    /// stage 2.2 must delete or amend it deliberately, with the code to justify it.
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
