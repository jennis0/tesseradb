//! `CURRENT` / `MANIFEST.json` / `SEGMENTS-<n>.json` serde structs (contracts §2.2/§2.3).
//! Unknown JSON fields are ignored — plain `#[derive(Deserialize)]` without
//! `deny_unknown_fields` — so a newer writer can add fields this reader doesn't yet know about
//! without breaking it. What that tolerance must *not* extend to is a field naming state the
//! reader would have to act on; [`HONOURED_STATE`] is where that line is drawn.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use tessera_spatial::tiler::ScalarType;
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
    /// The column's storage type. See [`scalar_type_name`] for the JSON spelling, and for why an
    /// unknown one refuses the whole manifest rather than this one field.
    #[serde(with = "scalar_type_name")]
    pub arrow_type: ScalarType,
    /// For a category column, the [`ManifestVocabulary::name`] its codes index; `None` for a
    /// plain numeric column.
    ///
    /// `default` here is the `Option`'s own absence — a plain scalar genuinely has no vocabulary —
    /// not tolerance of an older manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocabulary: Option<String>,
    /// Whether this column carries an entity-space filter index (`filter-index.md` §2).
    ///
    /// **A flag rather than a descriptor, because everything else about the placement is derivable
    /// from the declaration already here.** The family is `vocabulary.is_some()` — a category or
    /// not — and the record format follows the family: scattered vocabulary codes are addressed by
    /// a keyed file, dense interned ordinals by a positional one (§2.5). Storing either would be a
    /// second copy of a fact this struct can answer, and `wire_type` exists because that kind of
    /// second copy has already disagreed with itself once here.
    ///
    /// **No `serde(default)`**: a manifest that omits it is malformed, not filter-free. Decision
    /// 0048's rule is the whole argument — do not default a field so that an older bundle still
    /// opens, because there is no older bundle. This is the distinction [`DeclaredScalar::vocabulary`]
    /// draws next door: its `default` is the `Option`'s own absence, a plain scalar genuinely having
    /// no vocabulary, and not tolerance of a manifest written before the field existed.
    ///
    /// The failure a default would cause is ordinary rather than disclosure-shaped, and worth stating
    /// as such: a column silently non-filterable is simply absent from `/v1/meta`'s operand list, so
    /// no caller can name it. That is a capability that quietly went missing, not a wrong answer —
    /// still worth refusing, but not for the reason a first draft of this comment gave.
    pub filter: bool,
    /// Declared `render`: this column occupies a slot in every row of `columns.arrow`.
    ///
    /// **The tail is exactly the render columns.** A `filter`-only column is entity-space and must
    /// not appear in the hot column — §10.3 routes by access cadence, and putting a per-query column
    /// in a per-mark structure spends 0.93 GiB per byte per row per 10⁹ for nothing. Flush and merge
    /// take their writer schema from here, so this flag is what keeps a streamed segment's tail
    /// identical to the build's.
    ///
    /// No `serde(default)`: pre-release there is no bundle to stay compatible with (decision 0048),
    /// and a defaulted placement is one that reads as declared when it was inferred.
    pub render: bool,
}

impl DeclaredScalar {
    /// The arrow type an ingest batch must present this column at: `utf8` for a category — whatever
    /// its code width — and the storage type for everything else.
    ///
    /// **The one place the wire/storage split is decided, and it is a function of the declaration
    /// alone.** A category's codes are minted by the server and never supplied
    /// (per-point-attributes §3.1, §5), so its wire form is the value *key*; the declared width
    /// remains the storage type for the row, the WAL scalar and the segment column. The
    /// positional-safety argument in `parse_ingest_batch` rests on "every column at its
    /// **expected** type, where expected is a function of the manifest declaration alone" — never
    /// on wire equalling storage — so the split leaves it intact.
    ///
    /// **Here rather than transcribed at the caller.** `tessera-server` sees engine API types only
    /// (SA §3, enforced by `check-layers.sh`), so ingest validation once carried a second copy of
    /// the type table — and the copies disagreed, one spelling `uint64` where the other spelt
    /// `u64`. Neither had run against a non-empty declaration, so nothing caught it. A second copy
    /// of *this* rule would be worse than that: it would let a `u16` category be validated as a
    /// plain `u16`, which accepts raw codes and silently reopens the hole keys-on-the-wire closes.
    pub fn wire_type(&self) -> ScalarType {
        match self.vocabulary {
            Some(_) => ScalarType::Utf8,
            None => self.arrow_type,
        }
    }

    /// Whether this column's filter postings are addressed by a scattered identifier (a keyed file)
    /// rather than a dense ordinal (a positional one) — `filter-index.md` §2.5.
    ///
    /// Derived, never stored, for the reason [`DeclaredScalar::filter`] gives. A category's codes are
    /// drawn at random over the declared width, so a positional file would need one record per code
    /// point — 4×10⁹ for a `u32`. Every other family's identifiers are interned positions and dense
    /// by construction.
    ///
    /// Meaningless unless [`DeclaredScalar::filter`] is set; a caller reaching this on a
    /// non-filterable column has already lost track of what it is doing, which is why this answers
    /// the format question and not the "does it have postings" question.
    pub fn filter_is_keyed(&self) -> bool {
        self.vocabulary.is_some()
    }
}

/// `arrow_type`'s JSON spelling: [`ScalarType::arrow_type_name`] out, [`ScalarType::parse`] in.
///
/// **A `serde(with)` module rather than a derive, because the spelling is not serde's to choose.**
/// It belongs to `ScalarType`, whose crate carries no serde dependency and needs none for this.
///
/// **An unknown spelling refuses the whole manifest.** The field was a `String` so that a build
/// meeting a type a later one wrote could refuse it by name rather than fail to deserialise — a
/// tolerance priced against the mixed-version deployment decision 0048 says does not exist, and
/// paid for at four use sites: a fallible parse here, a refusal in `parse_ingest_batch`, a `None`
/// arm in `scalar_schema_of` and a `NoFold` variant, four spellings of one unreachable case.
/// Refusing at the parse is fail-closed in the same direction and costs none of them.
mod scalar_type_name {
    use super::ScalarType;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ty: &ScalarType, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(ty.arrow_type_name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<ScalarType, D::Error> {
        let name = String::deserialize(d)?;
        ScalarType::parse(&name).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "'{name}' is not a scalar type this build can store (contracts §2.2)"
            ))
        })
    }
}

/// One `vocabularies` entry: a named value set, its pinned codes and their presentation.
///
/// **Flat, and per-placement rather than per-capability** (per-point-attributes §4.1). The schema
/// the operator wrote says what each attribute is *for*; the manifest says only what a reader must
/// load. A reader should never have to understand intent to know what a column holds, so nothing
/// of `used_for` survives compilation — only the column, its width, and the vocabulary it indexes.
///
/// **Codes are the compiled artifact and are never re-derived.** `columns.arrow` stores the code,
/// not the key, so a rebuild that re-derived codes from a re-supplied vocabulary file —
/// regenerated, re-sorted, hand-edited — would silently recolour the whole corpus with no error
/// and no digest mismatch (§3.4). The mapping lives here, under the manifest digest, for the same
/// reason `identity.key` does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestVocabulary {
    /// The name a [`DeclaredScalar::vocabulary`] refers to.
    pub name: String,
    /// Whether the value set is closed at build or grows as the corpus supplies keys (§3.4).
    ///
    /// **Required, not defaulted, because both defaults are wrong in a direction that matters.**
    /// This is what ingest consults to decide whether a key it has never seen is a typo or a new
    /// value: defaulting to `declared` refuses valid data, and defaulting to `discovered` mints a
    /// code for a typo and gives it a place in the corpus. No bundle predates the field (decision
    /// 0048), so tolerating its absence buys a reader that does not exist.
    pub kind: VocabularyKind,
    /// Whether the *existence* of a value is sensitive (§3.8) — the disclosure control
    /// `/v1/categories` gates on.
    pub listing: Listing,
    pub values: Vec<ManifestVocabularyValue>,
    /// Retired codes, never reassigned (§3.4). Carried into the manifest rather than left in the
    /// schema file so that a later build reading this bundle's lineage can see which codes are
    /// spent without needing the artifact that retired them.
    #[serde(default)]
    pub reserved: Vec<u32>,
}

/// Whether a vocabulary's value set is closed at build or grows as the corpus supplies keys.
///
/// The distinction is only ever consulted at a *write*: it decides what happens to a key nothing
/// has bound yet. Every read path treats the two identically, because a bound key is a bound key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VocabularyKind {
    /// The value set is closed: every key is authored, and an unknown one is refused
    /// (declare-then-use, §5). A typo must not create a category.
    Declared,
    /// The value set grows: a key nothing has bound acquires a scattered code at the commit-window
    /// close, recorded beside it and pinned forever (§3.4).
    Discovered,
}

/// Whether the *existence* of a value is sensitive — the disclosure control of §3.8, orthogonal to
/// [`VocabularyKind`]'s operational question.
///
/// **Typed rather than a string, because it is now load-bearing.** It decides whether
/// `/v1/categories` filters a value set per principal, so a spelling no reader recognises must
/// refuse the manifest at the parse rather than fall through to a default — and both defaults are
/// wrong in a direction that matters: `public` publishes a gated value set, `per_viewer` withholds
/// a published one and looks like a permission bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Listing {
    /// The value set is filtered per principal: a value appears only if the principal can see at
    /// least one item carrying it (per-point-attributes §3.3).
    PerViewer,
    /// The value set is published as authored, to every principal with a session. Legal only for a
    /// `declared` vocabulary, where an accountable party wrote the names down (§3.8).
    Public,
}

impl Listing {
    pub fn as_str(self) -> &'static str {
        match self {
            Listing::PerViewer => "per_viewer",
            Listing::Public => "public",
        }
    }
}

/// One value of a vocabulary: its stable opaque key, its pinned code, and its presentation.
///
/// **The key is not the display name** (§3.4). `sev_1` is the key a row's code stands for;
/// "Critical" is a property of it. Conflating them makes renaming for display a rewrite of every
/// row, which is why `label` is separate and amendable without a build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestVocabularyValue {
    pub key: String,
    pub code: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// `quantisation`: the extent Morton codes are computed against (contracts §2.5).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Quantisation {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

impl Quantisation {
    /// Whether `(x, y)` has a cell in this extent — **the one definition**, because a second copy
    /// is how ingest and flush come to disagree about which points exist.
    ///
    /// Morton codes are a *fraction of the declared extent* (contracts §2.5), so a point outside it
    /// has no cell. The quantiser clamps rather than failing, which is why this must be checked
    /// before a point ever reaches it: a clamped point at the boundary is indistinguishable from
    /// one that legitimately sits there, so clamping silently moves data with nothing left to
    /// notice afterwards.
    ///
    /// Inclusive of the maxima, matching the quantiser's own domain: a point exactly at `x_max`
    /// occupies the top of the grid and belongs there. **NaN fails in both directions** and is
    /// therefore outside — right, because a NaN coordinate has no cell either, and `as u32`
    /// saturates it to zero rather than erroring.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        (x as f64) >= self.x_min
            && (x as f64) <= self.x_max
            && (y as f64) >= self.y_min
            && (y as f64) <= self.y_max
    }
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
    /// The value sets `declared_scalars`' category columns draw their codes from; empty in a
    /// bundle whose schema declares no category.
    ///
    /// **Required, not `default`.** A manifest that omits it is malformed rather than
    /// category-free: the two are indistinguishable under `default`, and the one that matters —
    /// a bundle whose rows carry codes and whose bindings went missing — would open and serve
    /// marks that decode to nothing. No bundle predates the field (decision 0048), so tolerating
    /// its absence buys a reader that does not exist and costs the check that does.
    pub vocabularies: Vec<ManifestVocabulary>,
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

impl Manifest {
    /// The declared scalars that occupy a slot in every row — `columns.arrow`'s tail, in order.
    ///
    /// **Every segment-facing consumer must use this rather than `declared_scalars` directly.** The
    /// full list is the compiled schema and includes `filter`-only columns, which live in entity
    /// space and are deliberately absent from the hot column. A flush or merge taking its writer
    /// schema from the full list would give a per-query column a slot in every row; worse,
    /// `gather_scalars` refuses a segment missing a declared column, so a schema built from the full
    /// list would make a merge refuse the **build's own** segment for correctly omitting one.
    ///
    /// The ingest plane is the deliberate exception and uses the full list: a caller supplies values
    /// for every declared column, filterable ones included.
    pub fn render_scalars(&self) -> impl Iterator<Item = &DeclaredScalar> {
        self.declared_scalars.iter().filter(|d| d.render)
    }
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

/// One entry of `vocabulary_extensions`: the bindings one named vocabulary has acquired since the
/// build or fold that wrote `MANIFEST.vocabularies`.
///
/// **Carried forward and appended to, never restated fresh** — the opposite discipline to `deny`,
/// and the difference is what each field must be able to do. `deny` is re-derived from the live
/// overlay at every write *because it must be able to shrink*: an unsuppress has to reach disc. A
/// binding must never shrink, and restate-fresh is the one shape that can silently drop one — every
/// write re-derives the whole set, so any gap in that derivation deletes bindings with no error and
/// no digest mismatch, and every row carrying a dropped code becomes a code no key explains.
/// Carry-forward cannot do that, because the previous manifest's bindings are present by
/// construction.
///
/// **Not a `dict_extents` counterpart.** Everything that makes `dict_extents` subtle — the
/// append-only list order, the no-repeat rule (decision 0042), `coalesce_dict_extents`' contiguous
/// window, the moved-under discard — exists to protect *positions*. A binding carries its code
/// explicitly, so none of that machinery has anything to protect here; adopting the shape would
/// import the obligations without the need, and add files, digests and a coalesce policy for a
/// quantity bounded by the code space of a declared width.
///
/// The fold folds these verbatim into the new prefix's `MANIFEST.vocabularies` and writes an empty
/// set (§3.3) — verbatim being the whole rule, since a fold that re-derived, re-sorted or
/// re-numbered would recolour the corpus with nothing to notice.
///
/// Honoured: the loader seeds [`crate::vocabulary::Vocabularies`] from this before WAL replay, so a
/// binding minted between builds survives a restart and stays out of the next draw.
///
/// **⊘ Written by nobody yet** ([#82](https://github.com/jennis0/tessera-index/issues/82)): nothing
/// mints between builds until the commit window does, so every manifest currently carries this
/// empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VocabularyExtension {
    /// The [`ManifestVocabulary::name`] these values extend.
    pub name: String,
    pub values: Vec<ManifestVocabularyValue>,
}

/// One entry of `dict_extents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictExtent {
    pub path: String,
    pub records: u64,
}

/// One entry of `locator_extents`: the **reverse** external-id direction for one flush segment's
/// entity range (§3.6).
///
/// The build's `entities/ext-locator.u32` is one file whose length is the entity space *at build
/// time*, so it says nothing about an entity a flush created. Without a durable reverse path for
/// those, an item visible on the map would answer `/v1/items` with a typed error forever once its
/// WAL region is reclaimed — contracts §2.4 serves that direction live-map-first,
/// locator-second, and rotation empties the live map at restart.
///
/// The file is a dense `u32` array over `[entity_lo, entity_hi]`, no header, `0xFFFFFFFF` for an
/// entity with no caller-supplied external id (contracts §3.4 r6 makes it optional). Each slot is
/// an **ordinal into `external_id_run`**, named here rather than inferred, because a segment's
/// extent is its own file and the concatenation order that gives the base locator its meaning does
/// not extend across flushes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocatorExtent {
    pub path: String,
    pub entity_lo: u64,
    /// Inclusive.
    pub entity_hi: u64,
    /// Prefix-relative path of the `external_id_runs` entry these ordinals index.
    pub external_id_run: String,
}

/// `SEGMENTS-<n>.json` (contracts §2.3): complete current state for one partition, written by
/// that partition's worker only after every file it names is durable.
///
/// **`n` lives in the filename and nowhere else.** This struct carried a `segments_version` field
/// defined as `= n`, which was redundant on its face and actively misleading in its name: the
/// *geometry* version — the counter a row-projection cache key rotates on — is a different
/// quantity, and it must **not** advance when an overlay publication writes a manifest carrying
/// only new deny state (write-path §5.6; bumping it would cost every live session a
/// measured 10.7 s row-projection rebuild per deny burst). Two counters with one name is how the
/// two came to be conflated, so the redundant one is gone: the reader takes `n` from the filename,
/// the writer allocates it, and the geometry version is process-local, carried by
/// `Generation::segments_version` and no format field.
///
/// Old bundles still open: this type parses without `deny_unknown_fields`, so a `segments_version`
/// key in an existing manifest is ignored rather than refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentsManifest {
    pub watermark: u64,
    pub entity_id_high_water: u64,
    pub segments: Vec<SegmentDescriptor>,
    /// Every live delta postings tier, **by prefix-relative path**, in serving order.
    ///
    /// **A path list rather than the count declaration it was.** Until r18 this carried the
    /// manifest sequence number each tier arrived at and the reader *derived* the paths from
    /// `segments` (`…/segments/<seg_id>/delta.arrow`), checking only that the two counts agreed.
    /// That derivation makes tier coalescence unexpressible: a coalesced tier covers several
    /// segments' entities, so it sits beside none of them, and a manifest naming it would open
    /// with the tiers it could derive and refuse the count. Naming the files is also what lets a
    /// coalesce publication drop the consumed tiers from `files` and add one, which is the whole
    /// of its manifest edit on this axis.
    #[serde(default)]
    pub deltas: Vec<String>,
    #[serde(default)]
    pub dict_extents: Vec<DictExtent>,
    #[serde(default)]
    pub external_id_runs: Vec<String>,
    /// The reverse external-id direction for each flush segment — see [`LocatorExtent`]. Empty in
    /// a bundle straight out of `tessera build`, whose one `ext-locator.u32` covers every entity
    /// it knows about.
    #[serde(default)]
    pub locator_extents: Vec<LocatorExtent>,
    #[serde(default)]
    pub tombstones: Vec<u64>,
    #[serde(default)]
    pub deny: Vec<DenyEntry>,
    /// Category bindings minted since the last build or fold — see [`VocabularyExtension`]. Empty
    /// in a bundle straight out of `tessera build`, and emptied again by every fold.
    #[serde(default)]
    pub vocabulary_extensions: Vec<VocabularyExtension>,
    pub files: BTreeMap<String, FileDigest>,
}

/// The `SEGMENTS-<n>.json` state fields this reader **honours** — reads and acts on.
///
/// **Each name here is the claim "a manifest carrying this is served correctly", and each
/// arrived with the code that serves it.** `deltas` is unioned into every fragment build
/// (`build_fragment_with_deltas`); `deny` and `tombstones` are applied to the initial overlay by
/// the loader, which WAL replay then unions on top of. Adding a name ahead of its code re-opens
/// the fail-open this list exists to close.
///
/// **Honouring a field changes what the reader does with a *valid* manifest and must not change
/// what it does with an invalid one.** [`SegmentsManifest::unhonourable_state`] filters honoured
/// fields out before [`DENY_DISPOSITION_STATE`] is consulted, so honouring `"deny"` alone would
/// reclassify a deny-carrying manifest as `Honourable`, send it to `verify_files`, and let a
/// digest failure step the candidate walk past accepted denies. That is why
/// [`SegmentsManifest::deny_disposition_state`] exists and is consulted at the verification
/// failure too — the two must land together, and did.
///
/// **Why an honoured list and not a forbidden list.** The default for a field this reader does
/// not understand has to be *refuse*, not *ignore*. A forbidden list is a list someone must
/// remember to extend when the format grows; an honoured list is one someone must remember to
/// extend when the *reader* grows, and forgetting it costs availability rather than
/// correctness. Note the deliberate asymmetry with this module's `#[derive(Deserialize)]`
/// without `deny_unknown_fields`: an unknown *JSON* field is ignored so a newer writer can add
/// one, but a **known** field carrying state this reader cannot act on is not.
pub const HONOURED_STATE: &[&str] = &["deltas", "deny", "tombstones", "vocabulary_extensions"];

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
    /// The deny-disposition fields this manifest carries, **regardless of what this reader
    /// honours**.
    ///
    /// [`Self::unhonourable_state`] answers "what can this reader not act on"; this answers "does
    /// this manifest exist because a deny was accepted". Once `deny` and `tombstones` are
    /// honoured the first list is empty for a manifest the second is non-empty for, and it is the
    /// second that decides whether a candidate may be stepped past — because a manifest whose
    /// *files* fail to verify is one this reader cannot serve either, and stepping past it
    /// re-exposes every entity denied since the older manifest was written.
    pub fn deny_disposition_state(&self) -> Vec<&'static str> {
        [
            ("tombstones", !self.tombstones.is_empty()),
            ("deny", !self.deny.is_empty()),
        ]
        .into_iter()
        .filter(|(name, carried)| *carried && DENY_DISPOSITION_STATE.contains(name))
        .map(|(name, _)| name)
        .collect()
    }

    pub fn unhonourable_state(&self) -> Vec<&'static str> {
        // Deny-disposition fields first, so a truncated message still names the field that
        // decided the posture.
        [
            ("tombstones", !self.tombstones.is_empty()),
            ("deny", !self.deny.is_empty()),
            ("deltas", !self.deltas.is_empty()),
            (
                "vocabulary_extensions",
                !self.vocabulary_extensions.is_empty(),
            ),
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

    /// **A declared type this build cannot store refuses the manifest**, rather than deserialising
    /// into a column the flush path then has to guard against.
    ///
    /// The direction is what matters. A flush that dropped the unknown column would write a
    /// `columns.arrow` shorter than its schema — a bundle that no longer opens — so the answer was
    /// always refusal; the only question was where. Refusing at the parse makes it one refusal
    /// instead of the four the fallible spelling needed, and makes the *whole* manifest
    /// unavailable rather than one field, which is what makes a caller's refusal total.
    #[test]
    fn an_unknown_arrow_type_refuses_the_declaration() {
        let good: DeclaredScalar =
            serde_json::from_str(r#"{"name": "score", "arrow_type": "f32", "filter": false, "render": true}"#)
                .expect("f32 parses");
        assert_eq!(good.arrow_type, ScalarType::F32);
        assert_eq!(
            serde_json::to_value(&good).unwrap()["arrow_type"],
            serde_json::json!("f32"),
            "the spelling round-trips through ScalarType::arrow_type_name"
        );

        let err = serde_json::from_str::<DeclaredScalar>(
            r#"{"name": "decimal128", "arrow_type": "d128"}"#,
        )
        .expect_err("a type this build cannot store is refused");
        assert!(
            err.to_string().contains("d128"),
            "the refusal names the spelling it could not parse, got: {err}"
        );
    }

    /// A category's wire type is its key, never its code — the one place that split is decided.
    ///
    /// A second copy of this rule would let a `u16` category be validated as a plain `u16`, which
    /// accepts raw codes and reopens the hole keys-on-the-wire closes.
    #[test]
    fn a_category_declares_utf8_on_the_wire_and_its_width_in_storage() {
        let category = DeclaredScalar {
            name: "department".to_string(),
            arrow_type: ScalarType::U16,
            vocabulary: Some("departments".to_string()),
            filter: false,
            render: true,
        };
        assert_eq!(category.wire_type(), ScalarType::Utf8);
        assert_eq!(category.arrow_type, ScalarType::U16);

        let plain = DeclaredScalar {
            name: "score".to_string(),
            arrow_type: ScalarType::U16,
            vocabulary: None,
            filter: false,
            render: true,
        };
        assert_eq!(
            plain.wire_type(),
            ScalarType::U16,
            "a plain scalar of the same width is unchanged, and is distinguishable on the wire \
             from the category above"
        );
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
            watermark: 0,
            entity_id_high_water: 0,
            segments: Vec::new(),
            deltas: Vec::new(),
            dict_extents: Vec::new(),
            external_id_runs: Vec::new(),
            locator_extents: Vec::new(),
            tombstones: Vec::new(),
            deny: Vec::new(),
            vocabulary_extensions: Vec::new(),
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

    /// **All three fields are honoured, so classification alone no longer refuses any of them.**
    /// What refuses a deny-carrying candidate is [`SegmentsManifest::deny_disposition_state`],
    /// consulted where its files fail to verify.
    ///
    /// This is the test that catches the two halves being separated: it asserts, in one place,
    /// that a deny-carrying manifest is now `Honourable` *and* that it is still identifiable as
    /// deny-carrying. Landing the constant without the second property is the fail-open.
    #[test]
    fn a_honoured_field_no_longer_refuses_a_manifest_but_deny_state_stays_visible() {
        let mut with_tombstone = empty_segments_manifest();
        with_tombstone.tombstones.push(17);
        assert_eq!(with_tombstone.honourability(), Honourability::Honourable);
        assert_eq!(with_tombstone.deny_disposition_state(), vec!["tombstones"]);

        let mut with_deny = empty_segments_manifest();
        with_deny.deny.push(DenyEntry {
            entity_id: 17,
            cause: "suppress".to_string(),
        });
        assert_eq!(with_deny.honourability(), Honourability::Honourable);
        assert_eq!(with_deny.deny_disposition_state(), vec!["deny"]);

        // `deltas` is not deny-disposition state: its absence leaves items *missing*, which is
        // staleness in the fail-safe direction, so a deltas-only candidate whose files do not
        // verify may still be stepped past.
        let mut with_delta = empty_segments_manifest();
        with_delta.deltas.push("d.arrow".to_string());
        assert_eq!(with_delta.honourability(), Honourability::Honourable);
        assert!(with_delta.deny_disposition_state().is_empty());
    }

    /// Nothing this reader knows about is unhonourable any more. The machinery stays because it
    /// is the tripwire for the *next* state field added to [`SegmentsManifest`], which must
    /// arrive with the code that acts on it or be refused.
    #[test]
    fn no_known_state_field_is_unhonourable_any_more() {
        let mut all_three = empty_segments_manifest();
        all_three.tombstones.push(17);
        all_three.deltas.push("d.arrow".to_string());
        all_three.deny.push(DenyEntry {
            entity_id: 18,
            cause: "suppress".to_string(),
        });
        assert!(all_three.unhonourable_state().is_empty());
    }

    /// **The common shape, and the one-identifier fail-open.** Contracts §2.3 makes a
    /// side-manifest complete for its partition, so every manifest published while a suppression
    /// is live carries `deny` *and* whatever `deltas` exist — `deny` alone is the rarer case.
    ///
    /// The fail-open stayed one identifier wide across the move. It was `any` vs `all` over
    /// `["deny", "deltas"]` in the classification; it is now whether
    /// [`SegmentsManifest::deny_disposition_state`] filters by [`DENY_DISPOSITION_STATE`] at all
    /// — a version returning every carried field would make a deltas-only candidate unready (an
    /// availability loss), and one returning none would step past the suppression.
    #[test]
    fn a_deny_beside_deltas_is_still_deny_disposition_state() {
        let mut manifest = empty_segments_manifest();
        manifest.deltas.push("d.arrow".to_string());
        manifest.deny.push(DenyEntry {
            entity_id: 17,
            cause: "suppress".to_string(),
        });
        assert_eq!(manifest.deny_disposition_state(), vec!["deny"]);

        // And the same for a tombstone beside deltas — the other deny-disposition field.
        let mut manifest = empty_segments_manifest();
        manifest.deltas.push("d.arrow".to_string());
        manifest.tombstones.push(17);
        assert_eq!(manifest.deny_disposition_state(), vec!["tombstones"]);
    }

    /// [`HONOURED_STATE`] is the claim "the read path acts on this field", and each name here is
    /// pinned beside the code that acts on it. A name added without its code re-opens the
    /// fail-open the list exists to close; a name removed while its code stays makes every
    /// manifest carrying it unready, which is an outage rather than a leak but is still wrong.
    #[test]
    fn every_honoured_field_names_code_that_acts_on_it() {
        assert_eq!(
            HONOURED_STATE,
            ["deltas", "deny", "tombstones", "vocabulary_extensions"],
            "deltas: `build_fragment_with_deltas` unions every live tier into a fragment. \
             deny/tombstones: the loader seeds the initial overlay from them and WAL replay \
             unions on top. vocabulary_extensions: the loader seeds the live \
             `vocabulary::Vocabularies` from them before replay, which is what makes a minted \
             code survive a restart and what keeps it out of the next draw"
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
