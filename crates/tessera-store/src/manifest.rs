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

/// `MANIFEST.json`'s `identity` object (contracts §2.2/§2.6 r6): the `tessera_id`
/// permutation's construction, round count, per-deployment key and shard id. **Required** —
/// no `#[serde(default)]` — because an absent object cannot invert a `tessera_id`, and a
/// *defaulted* key would invert every identifier to the wrong entity, suppressing the wrong
/// item on `/control/changes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityDescriptor {
    pub construction: String,
    pub rounds: u32,
    /// Exactly 32 lowercase hex characters (readers reject any other case rather than
    /// case-folding — contracts §2.6).
    pub key: String,
    pub shard_id: u32,
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
