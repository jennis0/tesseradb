//! `CURRENT` / `MANIFEST.json` / `SEGMENTS-<n>.json` serde structs (contracts §2.2/§2.3).
//! Unknown JSON fields are ignored — plain `#[derive(Deserialize)]` without
//! `deny_unknown_fields` — so a newer writer can add fields this reader doesn't yet know about
//! without breaking it. What that tolerance must *not* extend to is a field naming state the
//! reader would have to act on; [`HONOURED_STATE`] is where that line is drawn.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use tessera_spatial::tiler::ScalarType;
use tessera_spatial::Projection;
use tessera_types::layer::{RegisteredLayer, ServingLayout};
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// For a `text` column, the full `<name>/<version>` identity of the analyser that produced its
    /// terms; `None` for every other type (decision 0070).
    ///
    /// **Per column, not per bundle**, because two `text` columns may be analysed differently — and
    /// because the failure this records is silent. An index built by one analyser and queried by
    /// another matches on precisely the strings whose segmentation differs, with no error anywhere;
    /// there is no way to detect it from the postings, which are individually valid either way. §7's
    /// fold-merge argument reads this too: two layers merge only because the same versioned
    /// analyser produced them over the same values.
    ///
    /// `default` here is the `Option`'s own absence, as [`DeclaredScalar::vocabulary`]'s is — a
    /// numeric column genuinely has no analyser — and not tolerance of an older manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyser: Option<String>,
    /// Whether this column carries an entity-space index (`records-and-search.md` §3;
    /// `filter-index.md` §2). Declared `index = true`, compiled here.
    ///
    /// **A flag rather than a descriptor, because everything else about the placement is derivable
    /// from the declaration already here.** The family is `vocabulary.is_some()` — a category or
    /// not — and the record format follows the family: scattered vocabulary codes are addressed by
    /// a keyed file, dense interned ordinals by a positional one (§2.5). Storing either would be a
    /// second copy of a fact this struct can answer, and `wire_type` exists because that kind of
    /// second copy has already disagreed with itself once here.
    ///
    /// A column with neither this nor [`DeclaredScalar::render`] set is **blob-resident**
    /// (records §3): its values live in the record blob and it is absent from `/v1/meta`'s
    /// operand list. That home is derived from the two flags, never stored — a third flag would
    /// be a second copy of a fact these two already answer.
    ///
    /// **No `serde(default)`**: a manifest that omits it is malformed, not index-free. Decision
    /// 0048's rule is the whole argument — do not default a field so that an older bundle still
    /// opens, because there is no older bundle. This is the distinction [`DeclaredScalar::vocabulary`]
    /// draws next door: its `default` is the `Option`'s own absence, a plain scalar genuinely having
    /// no vocabulary, and not tolerance of a manifest written before the field existed.
    ///
    /// The failure a default would cause is ordinary rather than disclosure-shaped, and worth stating
    /// as such: a column silently non-filterable is simply absent from `/v1/meta`'s operand list, so
    /// no caller can name it. That is a capability that quietly went missing, not a wrong answer —
    /// still worth refusing, but not for the reason a first draft of this comment gave.
    pub index: bool,
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
    /// The arrow type an ingest batch must present this column at: **`utf8` for all three string
    /// families** — a category whatever its code width, a keyword, and a text column — and the
    /// storage type for everything else.
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
            // A keyword is stored as an ordinal into its layer's dictionary and supplied as the
            // value itself — the same split a category makes, for the same reason. The ordinal is
            // a per-layer index internal (`records-and-search.md` §4.3): it is not stable across
            // layers, so a caller could not name one even if the boundary let it.
            None if self.arrow_type == ScalarType::Keyword => ScalarType::Utf8,
            // Text is the third column of the same split, and the widest of the three: it is stored
            // as **no per-entity value at all** — a token dictionary, postings over it, and a blob
            // row — and supplied as the prose. A caller could not name the storage form if the
            // boundary let it, there being nothing per entity to name.
            None if self.arrow_type == ScalarType::Text => ScalarType::Utf8,
            None => self.arrow_type,
        }
    }

    /// Whether this column's index postings are addressed by a scattered identifier (a keyed file)
    /// rather than a dense ordinal (a positional one) — `filter-index.md` §2.5.
    ///
    /// Derived, never stored, for the reason [`DeclaredScalar::index`] gives. A category's codes are
    /// drawn at random over the declared width, so a positional file would need one record per code
    /// point — 4×10⁹ for a `u32`. Every other family's identifiers are interned positions and dense
    /// by construction.
    ///
    /// Meaningless unless [`DeclaredScalar::index`] is set; a caller reaching this on a
    /// non-filterable column has already lost track of what it is doing, which is why this answers
    /// the format question and not the "does it have postings" question.
    pub fn index_is_keyed(&self) -> bool {
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
    pub visibility: Visibility,
    /// The width every code of this vocabulary is stored at (`u8`, `u16` or `u32`). A category
    /// column stores its vocabulary's width.
    #[serde(with = "scalar_type_name")]
    pub width: ScalarType,
    pub values: Vec<ManifestVocabularyValue>,
    /// Retired codes, never reassigned (§3.4). Carried into the manifest rather than left in the
    /// schema file so that a later build reading this bundle's lineage can see which codes are
    /// spent without needing the artifact that retired them.
    #[serde(default)]
    pub reserved: Vec<u32>,
}

/// The two vocabulary discriminants are defined in `tessera-types` so the WAL's
/// `VocabularyDeclare` record (`ingest.md` §1.3, T5) and this manifest read one type; re-exported
/// here so every reader of the manifest keeps its path.
pub use tessera_types::vocabulary::{Visibility, VocabularyKind};

/// One value of a vocabulary: its stable opaque key, its pinned code, and its presentation.
///
/// **The key is not the display name** (§3.4). `sev_1` is the key a row's code stands for;
/// "Critical" is a property of it. Conflating them makes renaming for display a rewrite of every
/// row, which is why `title` is separate and amendable without a build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestVocabularyValue {
    pub key: String,
    pub code: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `views[..].quantisation`: the extent Morton codes are computed against (contracts §2.5), and
/// a property of the **view** rather than the bundle (decision 0040).
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
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x_min && x <= self.x_max && y >= self.y_min && y <= self.y_max
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

/// `views` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewDescriptor {
    pub id: String,
    pub display_name: String,
    /// **Which incarnation of this key the view currently is** (decision 0115) — the one place a
    /// reader with a view id gets the number every artifact of the view is stamped with.
    ///
    /// **Internal, and on no wire.** `/v1/meta` publishes the id and the roster record; this
    /// number is not part of either, and no response carries it, so a principal cannot tell a
    /// recreated key from one created for the first time.
    ///
    /// [`DECLARED_INCARNATION`] for a view the build declared. A create while the service runs
    /// mints the next value and [`Manifest::with_roster`] stamps it here; a drop takes the
    /// descriptor out, and a create of the same key puts back a descriptor at a higher one — which
    /// is what makes every segment, column and derived structure of the predecessor unreachable.
    ///
    /// **Required, not `default`**, on `quantisation`'s rule and with a sharper consequence: an
    /// absent incarnation reads as the build's, which is the one value a leftover artifact of a
    /// dropped-and-recreated key could carry, so a default here would adopt exactly the artifacts
    /// the field exists to keep out.
    pub incarnation: ViewIncarnation,
    /// The frame every position in this view is quantised against (contracts §2.5), immutable
    /// for the view's life — which is what makes a Morton prefix a permanent address in this view
    /// (decision 0040).
    ///
    /// **Here rather than on the bundle, because the frame is declared per view**: two views of
    /// one bundle may quantise differently, and an embedding and a map cannot share a frame
    /// without one of them wasting most of the grid (`views.md` §2). A single bundle-wide key
    /// would have to pick one of them.
    ///
    /// **Required, not `default`, and there is no bundle-level fallback** — the same rule as
    /// `projection` below, for the same reason: a view whose frame went missing is malformed, not
    /// unframed, and every position it holds decodes against whatever a reader guessed. A
    /// manifest omitting it refuses at open, loudly. No bundle predates the move (decision 0048).
    pub quantisation: Quantisation,
    /// What placed every position in this view before the frame did (`projections.md` §3).
    ///
    /// **Here rather than on the bundle, because a projection is declared per view**, exactly as
    /// the frame above it is: two views of one bundle may be projected differently.
    ///
    /// **Required, not `default`.** The frame alone does not imply a projection — a `[0, 1]`
    /// extent is a legal frame for a view with no projection at all — so a bundle that carries
    /// projected positions and cannot say so is one every second reader has to be told about out
    /// of band: the write path, which would otherwise quantise a degree as though it were a frame
    /// coordinate, and the differential oracle, which re-quantises source coordinates against the
    /// recorded frame. Defaulting the field to `none` is exactly the misread the field exists to
    /// stop, so a manifest omitting it is malformed rather than unprojected, and the
    /// `bundle_format` bump that introduced it (4) makes every bundle written before it refuse at
    /// open.
    ///
    /// **The declared name is the format, not the transform's parameters.** An equirectangular
    /// alias differs from its siblings only in the world aspect a client draws
    /// (`projections.md` §5.2), and serialising the standard parallel structurally would put a
    /// display parameter into the artifact — which is what makes that family one entry rather
    /// than five. An unknown name refuses the whole manifest, per [`scalar_type_name`]'s rule: a
    /// reader that cannot resolve the projection cannot invert a stored position, and reading it
    /// as `none` is the misread again.
    #[serde(with = "projection_name")]
    pub projection: Projection,
    /// **This view's own gate** (`views.md` §6): the labels a principal must hold one of to reach
    /// the view at all, each element one term taken verbatim (decision 0132), or `None` for
    /// `public` — the label every principal holds by construction
    /// ([decision 0088](../../../docs/decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md)),
    /// which is why the ordinary case stores nothing rather than storing the word.
    ///
    /// **The one input the gate evaluation reads for a view's own half.** For a view of a group
    /// this is the roster record's own label ([`GroupViewDescriptor::visibility`]) and the two are
    /// checked equal at [`Manifest::validate_groups`], on the discipline
    /// [`ScopedScalar::group`] already sets: the roster is what `/v1/meta` publishes and this is
    /// what `Engine::authorise` evaluates, so a manifest whose two copies disagree refuses at open
    /// rather than serving a view under a gate nobody wrote. The group's own half is
    /// [`GroupDescriptor::visibility`], and the two are conjunctive — a view's gate narrows its
    /// group's and never widens it.
    ///
    /// **Required, not `default`.** A gate that went missing would read as `public`, which is the
    /// one direction a disclosure control must not fail in; a manifest omitting it is malformed
    /// rather than ungated. No bundle predates the field (decision 0048).
    pub visibility: Option<Vec<String>>,
    /// **What a point carrying no access label of its own is given** — the declaration's
    /// `point_visibility.default`, or `None` where the declaration named none (decision 0133).
    ///
    /// The one input `/control/ingest` reads to decide an empty `access` list: under `Some` the
    /// row lands under that label's terms, as the build fills a null or empty label; under `None`
    /// the batch is refused naming the count, as the build refuses the corpus. The two entry
    /// points read one declaration, which is what decision 0091 asks of them.
    ///
    /// **Required, not `default`.** A defaulted `None` would refuse every unlabelled row of a
    /// corpus whose declaration filled them at the build; a defaulted label would fill with one
    /// nobody declared. `BUNDLE_FORMAT` 6 is the guard.
    pub point_default: Option<String>,
}

/// `groups` entry: one view group and its roster (`views.md` §3.1, §3.2).
///
/// **The roster's durable home is the manifest**, not the WAL: rotation reclaims WAL records, so
/// a roster that lived only in the log is lost at the first rotation, and a reused key silently
/// repoints every client cache keyed on the view (decision 0029, `views.md` §3.2).
///
/// A group is **not** a view: it cannot be named on a viewer verb, has no row space and no
/// permutation. What it carries is the half of a view that is the same for all of them, beside
/// the roster that differs — and each of its views appears in [`Manifest::views`] under the
/// joined `group:key` id, which is what a request names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDescriptor {
    pub name: String,
    /// The group's human-readable title, served on `/v1/meta` as `groups[..].title`
    /// (`configuration.md` §1, contracts §3.2). `None` where the declaration gave none, and
    /// served as `null` there — presentation metadata on an object whose visibility is already
    /// decided, so it discloses nothing the name does not.
    pub title: Option<String>,
    /// The group whose keys these are, where this group declares `members`
    /// (`views.md` §3.3); `None` where it owns them. Chains are refused at the declaration, so
    /// this always names an owner.
    pub members_of: Option<String>,
    /// The frame every view of this group is quantised against, and what placed its positions
    /// before that frame did.
    ///
    /// **Here as well as on each view, because a view of this group may not exist yet.** A group
    /// grows at a running service (`views.md` §3.2) and the view a create mints takes both from
    /// the group — reading them off a sibling view is correct only while the group has one, and a
    /// group whose whole roster was dropped, or which was declared empty, has none. They are the
    /// same values every view of the group already carries: a group's views share every setting
    /// by construction, which is what makes a key set meaningful.
    pub quantisation: Quantisation,
    #[serde(with = "projection_name")]
    pub projection: Projection,
    /// The per-view metadata names and types this group declared, in declaration order. Empty on
    /// a `members` group, whose metadata belongs to the owner, and on a group whose views carry
    /// none — which is the one group a first ingest batch may create a view of (`views.md` §3.2).
    pub metadata: Vec<GroupMetadataField>,
    /// **The group's gate, the outer bound over every view of it** (`views.md` §6): a view of a
    /// group is reachable only where its group is, so this is conjunctive with each view's own
    /// label and a view gate can narrow it and can never widen it — the relation decision 0089
    /// gives an artifact to its layer, and the I12 direction. `None` is `public`.
    ///
    /// **A `members` group carries its own**, not the owner's: two groups sharing one key set are
    /// two layouts, and which principals may see each layout is a fact about the layout
    /// (`views.md` §3.3).
    ///
    /// Required, for the reason [`ViewDescriptor::visibility`] gives, and in the same shape: a
    /// list of labels, each one term.
    pub visibility: Option<Vec<String>>,
    /// The group's `point_visibility.default`, or `None` where it declared none
    /// ([`ViewDescriptor::point_default`]). **Here as well as on each view** for the reason
    /// `quantisation` is: a view created while the service runs takes it from the group, which
    /// may have no view yet to read it off. Every view of the group carries the same value, and
    /// [`Manifest::validate_groups`] refuses a bundle whose copies disagree.
    pub point_default: Option<String>,
    /// The roster, in creation order — which at a build is declaration order, and afterwards is
    /// the order the creations were appended in (`views.md` §3.2). There is no stored number: the
    /// order is the record order (decision 0113).
    pub views: Vec<GroupViewDescriptor>,
    /// The **group-scoped attribute column families** this group owns (`views.md` §5): one
    /// entity-space column per view of the roster above, under
    /// `attrs/<column>/<group>/<key>/`.
    ///
    /// **Here rather than in [`Manifest::declared_scalars`]**, which is one flat bundle-wide list
    /// addressed positionally by the record blob's field tags and by every segment's scalar tail:
    /// a family has no slot in it, and a scoped column placed there would take a slot in every
    /// row and a whole-corpus `attrs/<column>/` of its own, both absent for every entity. The
    /// group is where it belongs instead, beside the keys a pinned leaf's `@<key>` resolves
    /// against.
    ///
    /// Empty is the ordinary case — a group with no attribute scoped to it — and a `members`
    /// group's is always empty: its views are the owner's, so a family over them is the owner's
    /// (`views.md` §3.3).
    pub scoped_scalars: Vec<ScopedScalar>,
}

/// `groups[..].scoped_scalars` entry: one group-scoped attribute's column family (`views.md` §5).
///
/// The declaration is an ordinary attribute's — same types, same `index` and `render` — and what
/// the scope changes is only **which column file** a predicate reads: one per view of the owning
/// group instead of one for the corpus. Evaluation stays in entity space, which is what keeps a
/// scoped attribute inside I2's argument: every value is indexed by entity, a predicate answers a
/// bitmap in entity space, and the mask meets it there before any permutation is applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedScalar {
    /// The column's name, as a filter leaf spells it before any pin — unique bundle-wide across
    /// the entity-scoped columns and the scoped families alike, so a leaf naming it is never
    /// ambiguous about which of the two it means.
    pub name: String,
    /// The group that owns the views this family has a column per — [`GroupDescriptor::name`],
    /// repeated here so the flattened list [`Manifest::scoped_scalars`] hands a reader is
    /// self-contained: a refusal names the group, and a bare leaf resolves against it. Checked
    /// against the descriptor it hangs off at [`Manifest::validate_groups`], so the two cannot
    /// come to disagree.
    pub group: String,
    /// The column's storage type, spelt exactly as [`DeclaredScalar::arrow_type`] is.
    #[serde(with = "scalar_type_name")]
    pub arrow_type: ScalarType,
    /// For a category column, the [`ManifestVocabulary::name`] its codes index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocabulary: Option<String>,
    /// For a `text` column, the analyser identity that produced its terms (decision 0070).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyser: Option<String>,
    /// Declared `index = true` — this family's columns carry an entity-space value column a
    /// filter may be answered from.
    pub index: bool,
    /// Declared `render = true` — this family's column occupies a slot in every row of
    /// `columns.arrow` **in each view of its group**, and of any group sharing those views via
    /// `members`, and in no other view (`views.md` §5).
    ///
    /// **The per-view counterpart of [`DeclaredScalar::render`]**, and the whole difference is the
    /// view set: an entity-scoped column's slot is in every row space the bundle has, a family's
    /// is in the row spaces its scope names. A view the family has no column for — one created
    /// after the build, [`Self::views`] naming those that have one — carries no slot at all, which
    /// a reader sees as the column's absence rather than as a row of placeholders.
    pub render: bool,
    /// The view ids that have a column, in roster order: the joined `group:key`
    /// form, which is what [`crate::view_path_components`] turns into the column's directory.
    ///
    /// **Named rather than derived from the roster**, because the two can differ: a view created
    /// after the build has no column until one is written for it, and reading the roster instead
    /// would make an absent file a missing artefact rather than a view with no values yet.
    pub views: Vec<String>,
}

impl ScopedScalar {
    /// Is this family on the filter surface — an operand `/v1/meta` publishes, a leaf may name,
    /// and `/v1/categories` answers a value list for (`views.md` §5)?
    ///
    /// **`index`, or `render`**, the two being one rule since the asymmetry between them closed
    /// (2026-08-31, owner ruling). `text` is excluded from the render arm because `render` on a
    /// scoped `text` family is refused at the declaration — a manifest carrying the combination
    /// would name a token index no pass produced.
    ///
    /// **Here, at the record, because the build and the engine both decide on it** and neither may
    /// depend on the other: `check-layers.sh` denies the build the engine, so the engine's
    /// `filter::scoped_is_filterable` calls this and the build's `scoped_postings_are_owed` calls
    /// [`Self::licence_of`] over its own declaration. The two agreed by argument until this
    /// existed — the build spelled it `index || render` and the engine spelled it with the `text`
    /// arm — and a divergence would have the open demand a `postings.arrow` no pass wrote.
    pub fn is_filterable(&self) -> bool {
        Self::licence_of(
            self.arrow_type,
            self.vocabulary.is_some(),
            self.index,
            self.render,
        )
    }

    /// [`Self::is_filterable`] over the four facts, rather than over the record that carries them
    /// — what a build's own declaration, which is not a [`ScopedScalar`] yet, asks.
    ///
    /// `vocabulary` is whether the family names one, which is the category arm of the family
    /// classification: a category over a `text` storage type is not the text family, and takes the
    /// render arm like every other category.
    pub fn licence_of(arrow_type: ScalarType, vocabulary: bool, index: bool, render: bool) -> bool {
        index || (render && (vocabulary || arrow_type != ScalarType::Text))
    }

    /// Does this family's per-view column have a **value column and a presence bitmap on disc** —
    /// the pair a drill-down reads one entity's value out of (`views.md` §5)?
    ///
    /// **Every family but `text`**, whatever its flags, which is where this parts from
    /// [`Self::is_filterable`]: the build writes `values.arrow` and `presence.roaring` per view
    /// for a family with neither flag exactly as it does for an indexed one, and a `text` family
    /// has no per-entity slot at all — its entity-space artefacts are a token dictionary and the
    /// postings over it.
    ///
    /// This is what gives a neither-flag declaration its meaning (owner ruling 2026-09-01): stored,
    /// served on `POST /v1/items/{tessera_id}`, on no filter surface and in no row tail.
    pub fn has_value_column(&self) -> bool {
        self.vocabulary.is_some() || self.arrow_type != ScalarType::Text
    }
}

/// One entry of `scoped_columns`: a group-scoped family, and the view whose column of it a flush
/// wrote (`views.md` §5).
///
/// **The pair, because the family's columns share one name.** `column` is the family's own
/// `ScopedScalar::name` and `view` the joined `group:key` id, which is what
/// [`Manifest::with_scoped_columns`] adds to the family's list and what
/// [`crate::view_path_components`] turns into the column's directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedColumn {
    pub column: String,
    pub view: String,
    /// The view's incarnation when the column was written (decision 0115).
    ///
    /// **Carried, not resolved.** This list is complete current state carried forward for ever,
    /// so an entry outlives the drop that made it garbage; without the stamp a key created again
    /// would find its predecessor's pair in the list and publish the old column as its own.
    pub incarnation: ViewIncarnation,
}

/// One view of a group, as the roster records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupViewDescriptor {
    /// The caller's own key. `<group>:<key>` is the view id, and is the [`ViewDescriptor::id`]
    /// this roster entry must have.
    pub key: String,
    /// This view's own gate, as the roster records it and as `/v1/meta` publishes the roster;
    /// `None` is `public` (`views.md` §6).
    ///
    /// **Evaluated through [`ViewDescriptor::visibility`], which carries the same label**, the two
    /// being checked equal at [`Manifest::validate_groups`]: one input decides a view's own half
    /// of the gate whether the view is a plain one or a group's, and the copy that would otherwise
    /// drift is refused at open instead.
    pub visibility: Option<Vec<String>>,
    /// The typed per-view values this view carries, one per name the owning group declared.
    /// Empty on a `members` group's views, whose metadata belongs to the owner.
    pub metadata: BTreeMap<String, ViewMetadataValue>,
}

/// The roster's own types live in `tessera-types`, because the WAL record that makes a create
/// durable travels through `tessera-lifecycle`, which does not depend on this crate
/// (`tessera_types::view`). Re-exported here so a manifest reader still names one module.
pub use tessera_types::view::{
    CreatedView, DeadIncarnation, GroupMetadataField, ViewIncarnation, ViewMetadataType,
    ViewMetadataValue, DECLARED_INCARNATION,
};

/// `views[..].projection` as the name a declaration writes (`projections.md` §5), refusing one
/// outside the set rather than defaulting it.
mod projection_name {
    use super::Projection;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(p: &Projection, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(p.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Projection, D::Error> {
        let name = String::deserialize(d)?;
        Projection::from_name(&name).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "'{name}' is not a projection this build can place points under \
                 (projections.md §5)"
            ))
        })
    }
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
    pub entity_id_high_water: u64,
    pub identity: IdentityDescriptor,
    /// The declared views, each carrying its own frame (decision 0040). There is no bundle-level
    /// extent: [`Manifest::quantisation_of`] is how a caller that has a view id gets one, and a
    /// caller that has no view id is asking a question the bundle cannot answer.
    pub views: Vec<ViewDescriptor>,
    /// The view groups and their rosters (`views.md` §3.2), in declaration order.
    ///
    /// **Required, not `default`**, on `vocabularies`' rule: a manifest that omits it is
    /// malformed rather than group-free, and the two are indistinguishable under `default` — a
    /// bundle whose views are a group's and whose roster went missing would open and serve views
    /// no client can order or name. No bundle predates the field (decision 0048). Empty is the
    /// ordinary case: a declaration of plain views alone.
    pub groups: Vec<GroupDescriptor>,
    pub partitions: Vec<PartitionDescriptor>,
    #[serde(default)]
    pub provenance: serde_json::Value,
    pub files: BTreeMap<String, FileDigest>,
}

/// Everything a side manifest or the write-ahead log can add to a [`Manifest`] after it was
/// written, gathered so one call merges it — see [`Manifest::with_declarations`]. A list left
/// empty adds nothing.
#[derive(Default)]
pub struct Declarations<'a> {
    pub groups: &'a [GroupDescriptor],
    pub plain_views: &'a [ViewDescriptor],
    pub vocabularies: &'a [ManifestVocabulary],
    pub attributes: &'a [DeclaredScalar],
    pub scoped_attributes: &'a [ScopedScalar],
    pub created_views: &'a [CreatedView],
    pub dead_incarnations: &'a [DeadIncarnation],
    /// `(column, view, incarnation)`, as [`Manifest::with_scoped_columns`] takes them.
    pub scoped_columns: &'a [(String, String, ViewIncarnation)],
}

impl Manifest {
    /// The frame a named view's positions are quantised against, or `None` for a view this bundle
    /// does not declare.
    ///
    /// **Keyed by view, never bundle-wide** (decision 0040): the extent is the view's, so a single
    /// answer would have to pick one of two differently framed views. Reading the *first* declared
    /// view instead is correct only while a bundle carries one and fails silently rather than
    /// loudly on the day one carries two — every position decoded against the wrong frame, with
    /// nothing to notice afterwards.
    ///
    /// An unknown name is `None` and the caller refuses; there is no default frame to fall back
    /// on, for the reason [`ViewDescriptor::quantisation`] gives.
    /// Refuse a manifest whose roster and whose views disagree (`views.md` §3.2).
    ///
    /// **The roster is not a second list of views; it is what orders and names them.** Every
    /// roster entry must have its `group:key` view declared, and every view whose id carries the
    /// group separator must be on a roster — either direction failing leaves a view a client can
    /// see and cannot address, or a roster entry that resolves to nothing.
    pub fn validate_groups(&self) -> std::result::Result<(), String> {
        let mut rostered: Vec<String> = Vec::new();
        for group in &self.groups {
            if group.name.contains(crate::GROUP_SEPARATOR) {
                return Err(format!(
                    "group '{}' carries the reserved separator '{}'",
                    group.name,
                    crate::GROUP_SEPARATOR
                ));
            }
            let mut keys: Vec<&str> = group.views.iter().map(|v| v.key.as_str()).collect();
            keys.sort_unstable();
            if keys.windows(2).any(|w| w[0] == w[1]) {
                return Err(format!(
                    "group '{}' carries a key twice; a key is a view's only address and is never \
                     reused (views §3.2)",
                    group.name
                ));
            }
            for view in &group.views {
                let id = format!("{}{}{}", group.name, crate::GROUP_SEPARATOR, view.key);
                let Some(descriptor) = self.views.iter().find(|v| v.id == id) else {
                    return Err(format!(
                        "the roster of group '{}' names view '{id}', which the manifest does not \
                         declare",
                        group.name
                    ));
                };
                // **The gate is written twice and must be written once** (`views.md` §6): the
                // roster record is what `/v1/meta` publishes and `ViewDescriptor::visibility` is
                // what `Engine::authorise` evaluates, so a manifest whose two copies disagree
                // would serve a view under a gate nobody wrote — and the direction that matters
                // is the one where the descriptor says `public` and the roster says otherwise,
                // which is a control accepted and never enforced. Refused at open, in the
                // direction that costs a load rather than a disclosure.
                if descriptor.visibility != view.visibility {
                    return Err(format!(
                        "view '{id}' records the gate {:?} and the roster of group '{}' records \
                         {:?}; a view's gate is one list of labels, published on the roster and \
                         evaluated from the view (views §6)",
                        descriptor.visibility, group.name, view.visibility
                    ));
                }
                if descriptor.point_default != group.point_default {
                    return Err(format!(
                        "view '{id}' records the point default {:?} and group '{}' records {:?}; \
                         a group's views share one point default, read from the view by the \
                         ingest plane and from the group by a create (decision 0133)",
                        descriptor.point_default, group.name, group.point_default
                    ));
                }
                rostered.push(id);
            }
            // **A family's columns are its group's views.** A named view the roster does not
            // carry would send the opener at a directory outside the group's own, and a family on
            // a `members` group would duplicate the owner's columns under a second name.
            for family in &group.scoped_scalars {
                if group.members_of.is_some() {
                    return Err(format!(
                        "group '{}' declares `members` and carries the scoped column family \
                         '{}'; a family over shared views belongs to the group that owns them \
                         (views §3.3, §5)",
                        group.name, family.name
                    ));
                }
                if family.group != group.name {
                    return Err(format!(
                        "the scoped column family '{}' of group '{}' records the group '{}'",
                        family.name, group.name, family.group
                    ));
                }
                for view in &family.views {
                    let key = view
                        .split_once(crate::GROUP_SEPARATOR)
                        .filter(|(g, _)| *g == group.name)
                        .map(|(_, key)| key);
                    if !key.is_some_and(|key| group.views.iter().any(|v| v.key == key)) {
                        return Err(format!(
                            "the scoped column family '{}' of group '{}' names view '{view}', \
                             which is not a view of that group (views §5)",
                            family.name, group.name
                        ));
                    }
                }
            }
        }
        for view in &self.views {
            if view.id.contains(crate::GROUP_SEPARATOR) && !rostered.contains(&view.id) {
                return Err(format!(
                    "view '{}' is a group's view and no roster carries it, so nothing gives it a \
                     key (views §3.2)",
                    view.id
                ));
            }
        }
        Ok(())
    }

    /// Every group's scoped column families, in manifest order (`views.md` §5).
    ///
    /// **Flattened, because the group is already inside each family's view ids**: a reader that
    /// opens or publishes a family needs the family, not the roster it hangs off, and the
    /// `<group>:<key>` id carries the group's own name. Cloned rather than borrowed so a caller
    /// can hold the list across a generation swap, which is what both openers do.
    pub fn scoped_scalars(&self) -> Vec<ScopedScalar> {
        self.groups
            .iter()
            .flat_map(|g| g.scoped_scalars.iter().cloned())
            .collect()
    }

    /// The group that **owns** `group`'s keys — itself, unless it declares `members`
    /// (`views.md` §3.3). A group this manifest does not declare owns its own keys, which is the
    /// answer a caller can act on: it names no sharing groups either.
    pub fn owner_of_group(&self, group: &str) -> String {
        self.groups
            .iter()
            .find(|g| g.name == group)
            .and_then(|g| g.members_of.clone())
            .unwrap_or_else(|| group.to_string())
    }

    /// Every view id one key of `owner` resolves to: the owning group's, and one for **every group
    /// whose views are the owner's** (`members`, `views.md` §3.3).
    ///
    /// **One definition, because a key is not one view.** A create lands on every sharing group at
    /// the same moment and a drop takes it off every one of them, so anything that acts on "the
    /// views of this key" — [`Self::with_roster`]'s death loop, the drop's buffer prune, its
    /// `delete_dangling` probe, and the WAL replay's own prune — must expand the same way. Three
    /// copies of the expansion is how one of them comes to prune a single spelling and leave the
    /// other's rows to be adopted by whatever takes the key next
    /// ([decision 0115](../../../docs/decisions/0115-a-dropped-view-key-is-reusable.md)).
    ///
    /// The caller passes the **owner**: `owner_of_group` is what turns the group a request named
    /// into it.
    pub fn view_ids_for_key(&self, owner: &str, key: &str) -> Vec<String> {
        self.groups
            .iter()
            .filter(|g| g.name == owner || g.members_of.as_deref() == Some(owner))
            .map(|g| format!("{}{}{}", g.name, crate::GROUP_SEPARATOR, key))
            .collect()
    }

    /// This manifest as the **live roster** makes it: the views a build declared, plus every view
    /// created while the service runs, minus every key that has been dropped (`views.md` §3.2,
    /// §3.4).
    ///
    /// **One derivation, two callers.** `Engine::open` builds it after replay, and the create and
    /// drop operations build it again when they publish — so the roster a request resolves against
    /// is the same function of the same state whether it was reached by a restart or by a `PUT`.
    /// Everything downstream — `/v1/meta`, view resolution on both planes, the flush's frame
    /// lookup — reads the manifest and needs no second notion of which views exist.
    ///
    /// **A create names the owner group and lands on every group sharing its views**
    /// (`views.md` §3.3): a key belongs to the group that owns it, so
    /// `quarter:2026-Q5` creates `quarter_map:2026-Q5` at the same moment, empty, and a request
    /// naming it is answered rather than 404ed. A drop of the key takes both away.
    ///
    /// A create naming a group this manifest does not declare is **dropped rather than expanded**:
    /// it cannot arise from the create operation, which refuses an unknown group, and a rebuild is
    /// free to remove a group — in which case its views are not views of this bundle either.
    pub fn with_roster(&self, created: &[CreatedView], dead: &[DeadIncarnation]) -> Manifest {
        let mut manifest = self.clone();
        // **The deaths first, then the creations** (decision 0115). A key may be dropped and
        // created again, and both lists are complete current state rather than a diff — so this
        // order is what decides whether the recreate survives. Taking the dead incarnation's
        // descriptors out first lets the creation put back a descriptor at the live incarnation;
        // the other order would delete the view the caller was just told it had. A death whose key
        // nothing recreated simply leaves the group without it.
        for stone in dead {
            // A death takes away only the incarnation it names. The list is never pruned and is
            // applied again at every roster publication and every open, so it goes on naming
            // earlier incarnations of a key created again; the recreated key keeps its place and
            // its families' columns.
            let owner_id = format!("{}{}{}", stone.group, crate::GROUP_SEPARATOR, stone.key);
            if manifest
                .incarnation_of(&owner_id)
                .is_some_and(|live| live != stone.incarnation)
            {
                continue;
            }
            // **The owner's groups and every group sharing its views** — the one expansion
            // `Self::view_ids_for_key` defines, which the drop's own prunes take too.
            let ids = manifest.view_ids_for_key(&stone.group, &stone.key);
            for group in &mut manifest.groups {
                if group.name == stone.group
                    || group.members_of.as_deref() == Some(stone.group.as_str())
                {
                    group.views.retain(|v| v.key != stone.key);
                    // **And the families' own lists** (`views.md` §5). A family names the views
                    // that have a column, and [`Self::validate_groups`] holds every one of them to
                    // being a view of the group — so a list that kept a dropped key would make the
                    // manifest refuse to load at the next restart. The column's files are left
                    // behind with the prefix, exactly as the view's segments are: §3.4's
                    // reclamation is by omission, and the incarnation is what keeps a key created
                    // again from adopting them.
                    let dropped = format!("{}{}{}", group.name, crate::GROUP_SEPARATOR, stone.key);
                    for family in &mut group.scoped_scalars {
                        family.views.retain(|v| *v != dropped);
                    }
                }
            }
            manifest.views.retain(|v| !ids.contains(&v.id));
        }
        for view in created {
            // The owner, then every group whose views are the owner's.
            let sharing: Vec<String> = manifest
                .groups
                .iter()
                .filter(|g| {
                    g.name == view.group || g.members_of.as_deref() == Some(view.group.as_str())
                })
                .map(|g| g.name.clone())
                .collect();
            for group_name in sharing {
                let group = manifest
                    .groups
                    .iter_mut()
                    .find(|g| g.name == group_name)
                    .expect("named from this list");
                if group.views.iter().any(|v| v.key == view.key) {
                    continue;
                }
                group.views.push(GroupViewDescriptor {
                    key: view.key.clone(),
                    visibility: view.visibility.clone(),
                    // Metadata belongs to the group that owns the views; a sharing group's copies
                    // carry none, exactly as a build writes them.
                    metadata: if group.name == view.group {
                        view.metadata.clone()
                    } else {
                        BTreeMap::new()
                    },
                });
                let (quantisation, projection) = (group.quantisation, group.projection);
                let point_default = group.point_default.clone();
                let id = format!("{group_name}{}{}", crate::GROUP_SEPARATOR, view.key);
                if !manifest.views.iter().any(|v| v.id == id) {
                    manifest.views.push(ViewDescriptor {
                        display_name: id.clone(),
                        id,
                        // **The record's own gate, on both copies** (`views.md` §6): the roster
                        // entry above and this descriptor carry one label, which
                        // `Manifest::validate_groups` holds them to.
                        visibility: view.visibility.clone(),
                        // **The record's incarnation** (decision 0115), which is what every
                        // consumer with a view id resolves an artifact's stamp against. A key
                        // created again lands here at a higher number than the segments and
                        // columns its predecessor left behind, so none of them is composed.
                        incarnation: view.incarnation,
                        // **The group's frame and the group's projection**: a view of a group
                        // shares every setting with its siblings, which is what makes a key set
                        // one coordinate system observed at several keys.
                        quantisation,
                        projection,
                        // **The group's point default** (decision 0133), for the reason the frame
                        // is the group's: a key set is one declaration observed at several keys.
                        point_default,
                    });
                }
            }
        }
        manifest
    }

    /// Which incarnation the view with this id currently is, or `None` if this manifest declares
    /// no such view (decision 0115).
    ///
    /// **The one resolution site for an artifact's stamp, and it fails closed**: a caller that
    /// gets `None` must treat the artifact as unreachable, never as live.
    pub fn incarnation_of(&self, view: &str) -> Option<ViewIncarnation> {
        self.views
            .iter()
            .find(|v| v.id == view)
            .map(|v| v.incarnation)
    }

    /// [`Self::incarnation_of`] for the view `key` of `group`.
    pub fn incarnation_of_key(&self, group: &str, key: &str) -> Option<ViewIncarnation> {
        self.incarnation_of(&format!("{group}{}{key}", crate::GROUP_SEPARATOR))
    }

    /// Is this artifact's `(view, incarnation)` stamp the live one?
    ///
    /// The predicate every carry-forward and every open filters on. A stamp naming a view this
    /// manifest does not declare, or naming an incarnation that is not the live one, is an
    /// artifact of a dropped view: unreachable, and the fold's to reclaim.
    pub fn is_live_incarnation(&self, view: &str, incarnation: ViewIncarnation) -> bool {
        self.incarnation_of(view) == Some(incarnation)
    }

    /// This manifest with each `(family, view)` pair added to the family's own `views` list —
    /// what a flush that wrote the **first** column of a family for a view publishes
    /// (`views.md` §5).
    ///
    /// **The list means "the views that have a column", and only a writer can extend it.** A view
    /// created while the service runs has none until a flush of it carries a value; that flush
    /// writes the base and the extent, and this is where the manifest starts saying so — which is
    /// what `/v1/meta`'s `scoped_scalars[..].views` reports and what `FilterColumns::open` walks
    /// at the next restart. A pair the list already holds is a no-op, and a pair naming a family
    /// or a view this manifest does not declare is **dropped rather than expanded**, on
    /// [`Self::with_roster`]'s rule: a rebuild is free to remove either, in which case the column
    /// is not this bundle's either.
    /// **A pair of a dead incarnation is dropped, not published** (decision 0115): the column is
    /// on disc under the same path a key created again would use, and adding it to the family's
    /// list would serve the predecessor's values as the new view's. The pair is checked against
    /// this manifest's own incarnation, which is the live one by construction.
    /// This manifest with the attribute columns declared at a running service appended
    /// (`ingest.md` §1.3, §6.3): each entity-scoped column at the tail of `declared_scalars`, in
    /// declaration order, and each group-scoped family at the tail of its group's
    /// `scoped_scalars`.
    ///
    /// **One list to every reader.** The served schema is the build's columns followed by the
    /// runtime ones, and nothing downstream can tell the two apart: a buffered row's scalars, a
    /// record blob's field tags and a flush's writer schema are all positional against this list,
    /// which is why a runtime column appends and never inserts. A name the list already holds is
    /// skipped, not compared: the door refuses a differing redeclaration, so a repeat here is the
    /// same column, seen twice because a fold moved it into `MANIFEST.json` while the log or the
    /// side manifest still names it. A family naming a group this manifest does not declare is
    /// dropped, on [`Self::with_scoped_columns`]' rule.
    pub fn with_attributes(
        &self,
        attributes: &[DeclaredScalar],
        scoped_attributes: &[ScopedScalar],
    ) -> Manifest {
        let mut manifest = self.clone();
        for attribute in attributes {
            if manifest
                .declared_scalars
                .iter()
                .any(|d| d.name == attribute.name)
            {
                continue;
            }
            manifest.declared_scalars.push(attribute.clone());
        }
        for family in scoped_attributes {
            let Some(group) = manifest.groups.iter_mut().find(|g| g.name == family.group) else {
                continue;
            };
            if group.scoped_scalars.iter().any(|f| f.name == family.name) {
                continue;
            }
            group.scoped_scalars.push(family.clone());
        }
        manifest
    }

    /// This manifest with the vocabularies declared at a running service appended
    /// (`ingest.md` §1.3), each with its values as last published.
    ///
    /// **A name this manifest already holds is skipped, not merged.** The door refuses a
    /// redeclaration under another identity, so a repeat here is the same vocabulary seen twice —
    /// which is what a fold that moved it into `MANIFEST.json` while the side manifest still
    /// names it leaves behind. Values minted into a *built* vocabulary travel as
    /// [`VocabularyExtension`]s and are not this list's business.
    pub fn with_vocabularies(&self, vocabularies: &[ManifestVocabulary]) -> Manifest {
        let mut manifest = self.clone();
        for vocabulary in vocabularies {
            if manifest
                .vocabularies
                .iter()
                .any(|v| v.name == vocabulary.name)
            {
                continue;
            }
            manifest.vocabularies.push(vocabulary.clone());
        }
        manifest
    }

    /// This manifest with the view groups declared at a running service appended
    /// (`ingest.md` §1.3), each carrying the group's own half and an empty roster.
    ///
    /// **Called before [`Self::with_roster`] at every open**, which is the whole of the ordering
    /// rule: a create names its group, and a roster record whose group this manifest does not
    /// declare is dropped rather than expanded. A name this manifest already holds is skipped, on
    /// [`Self::with_vocabularies`]' rule — the door refuses a redeclaration under another
    /// identity, so a repeat is the same group seen twice.
    pub fn with_groups(&self, groups: &[GroupDescriptor]) -> Manifest {
        let mut manifest = self.clone();
        for group in groups {
            if manifest.groups.iter().any(|g| g.name == group.name) {
                continue;
            }
            manifest.groups.push(group.clone());
        }
        manifest
    }

    /// This manifest with the plain views declared at a running service appended
    /// (`ingest.md` §1.3, §10 R9), each with the empty row space every view created at a running
    /// service starts with.
    ///
    /// A name this manifest already holds is skipped, on [`Self::with_groups`]' rule.
    pub fn with_plain_views(&self, views: &[ViewDescriptor]) -> Manifest {
        let mut manifest = self.clone();
        for view in views {
            if manifest.views.iter().any(|v| v.id == view.id) {
                continue;
            }
            manifest.views.push(view.clone());
        }
        manifest
    }

    pub fn with_scoped_columns(&self, columns: &[(String, String, ViewIncarnation)]) -> Manifest {
        let mut manifest = self.clone();
        let mut grown: BTreeSet<(usize, usize)> = BTreeSet::new();
        for (column, view, incarnation) in columns {
            if !manifest.is_live_incarnation(view, *incarnation) {
                continue;
            }
            let Some((group_name, key)) = view.split_once(crate::GROUP_SEPARATOR) else {
                continue;
            };
            let Some(g) = manifest.groups.iter().position(|g| g.name == group_name) else {
                continue;
            };
            let group = &mut manifest.groups[g];
            if !group.views.iter().any(|v| v.key == key) {
                continue;
            }
            if let Some(f) = group.scoped_scalars.iter().position(|f| f.name == *column) {
                let family = &mut group.scoped_scalars[f];
                if !family.views.contains(view) {
                    family.views.push(view.clone());
                    grown.insert((g, f));
                }
            }
        }
        // Roster order, as a build lists them, whichever view a flush reached first.
        let mut position: Option<(usize, HashMap<String, usize>)> = None;
        for (g, f) in grown {
            let group = &mut manifest.groups[g];
            if position.as_ref().is_none_or(|(held, _)| *held != g) {
                let ids = group.views.iter().enumerate().map(|(i, v)| {
                    (format!("{}{}{}", group.name, crate::GROUP_SEPARATOR, v.key), i)
                });
                position = Some((g, ids.collect()));
            }
            let (_, of) = position.as_ref().expect("set above");
            group.scoped_scalars[f]
                .views
                .sort_by_key(|id| of.get(id).copied());
        }
        manifest
    }

    /// This manifest with every list in `declarations` merged, in the one order that keeps them
    /// all: groups, plain views, vocabularies, attributes, roster, scoped columns.
    ///
    /// Each step drops what the manifest cannot place, so a step reached too early loses a
    /// declaration that arrived in the same value. A roster creation whose group the manifest
    /// does not yet declare is dropped; a column naming a vocabulary the manifest does not yet
    /// carry refuses to seed; a scoped column extends a family's list, which the attributes step
    /// puts there and the roster step decides the live incarnation of.
    pub fn with_declarations(&self, declarations: &Declarations<'_>) -> Manifest {
        self.with_groups(declarations.groups)
            .with_plain_views(declarations.plain_views)
            .with_vocabularies(declarations.vocabularies)
            .with_attributes(declarations.attributes, declarations.scoped_attributes)
            .with_roster(declarations.created_views, declarations.dead_incarnations)
            .with_scoped_columns(declarations.scoped_columns)
    }

    pub fn quantisation_of(&self, view: &str) -> Option<Quantisation> {
        self.views
            .iter()
            .find(|v| v.id == view)
            .map(|v| v.quantisation)
    }

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

    /// Where each render column sits in the **full** declaration.
    ///
    /// A buffered row carries one scalar per declared column, positionally, while a segment's tail
    /// is [`Self::render_scalars`] — so a flush has to select before it writes, and selecting by
    /// position is the only form of that which cannot silently pair a value with another column's
    /// name. Defined beside `render_scalars` because the two share one predicate: a flush that
    /// selected on a different one would write a `utf8` title into an `i32` score's slot, which the
    /// segment writer refuses by type only when the two happen to differ.
    pub fn render_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.declared_scalars
            .iter()
            .enumerate()
            .filter(|(_, d)| d.render)
            .map(|(index, _)| index)
    }
}

/// One entry of `SEGMENTS-<n>.json`'s `segments` array — one build (or streamed) segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    pub view: String,
    /// The view's incarnation when this segment was written (decision 0115).
    ///
    /// **Carried rather than resolved, and this is the class the stamp exists for.** A dropped
    /// view's segments stay in the live side-manifest until a fold reclaims them; a key created
    /// again is declared once more, so the "is this view still declared" test that omits them
    /// today would start answering yes and the new view would serve the predecessor's points —
    /// the silent wrong answer. The restart path can check this with no ordering argument: it has
    /// the manifest and the roster, and needs nothing about when either was written.
    pub incarnation: ViewIncarnation,
    pub seg_id: String,
    pub row_count: u32,
    pub entity_lo: u64,
    pub entity_hi: u64,
}

/// A set of entity ids under a deny, as `deny` and `tombstones` carry one: the portable Roaring
/// serialisation, base64 in the JSON.
///
/// The bytes are decoded once, when the manifest is deserialised, and an id set that does not
/// decode is held as undecodable rather than as the empty set. [`SegmentsManifest::honourability`]
/// then refuses the manifest, because a reader that took undecodable bytes for "nothing is
/// denied" would serve every entity the field names.
#[derive(Debug, Clone)]
pub struct DenySet {
    encoded: String,
    entities: Option<croaring::Bitmap>,
}

impl DenySet {
    /// The set `entities` names, encoded for a manifest about to be written.
    pub fn of(entities: &croaring::Bitmap) -> Self {
        DenySet {
            encoded: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                entities.serialize::<croaring::Portable>(),
            ),
            entities: Some(entities.clone()),
        }
    }

    /// The ids, or `None` where the field's bytes did not decode.
    pub fn entities(&self) -> Option<&croaring::Bitmap> {
        self.entities.as_ref()
    }

    /// Whether this manifest exists because a deny was accepted. Undecodable counts as carrying:
    /// the field was written by something, and what it said cannot be read.
    pub fn carries(&self) -> bool {
        self.entities
            .as_ref()
            .is_none_or(|entities| !entities.is_empty())
    }

    fn undecodable(&self) -> bool {
        self.entities.is_none()
    }

    fn decode(encoded: &str) -> Option<croaring::Bitmap> {
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).ok()?;
        croaring::Bitmap::try_deserialize::<croaring::Portable>(&bytes)
    }
}

impl Default for DenySet {
    fn default() -> Self {
        DenySet::of(&croaring::Bitmap::new())
    }
}

impl Serialize for DenySet {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded)
    }
}

impl<'de> Deserialize<'de> for DenySet {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let entities = DenySet::decode(&encoded);
        Ok(DenySet { encoded, entities })
    }
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

impl DictExtent {
    /// Every file this extent owns.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.path.as_str())
    }
}

/// One entry of `attr_extents`: one flush's values for one filterable column
/// (`filter-index.md` §2.1, §2.5).
///
/// A value column is written once by the batch build and covers `[0, entity_id_high_water)`. An
/// entity allocated afterwards has no slot in it, so each flush appends the values of the entities
/// it publishes and the reader composes `base ∪ extents`. Entity ids are permanent (**I9**), so an
/// extent never renumbers anything and the layers it joins are disjoint in entity space.
///
/// **Named here rather than derived from a path convention**, unlike the `deltas` list's first
/// shape: a column's name is a path segment in `attrs/<column>/extents/`, and a reader that
/// recovered it by splitting the path would be inferring the artefact's identity from its
/// filename. It is also the difference between a missing file being an error and being an absence
/// — a directory scan finds what is there, and this says what must be.
///
/// **The presence bitmap is not optional here**, where it is for a base column. A flush publishes
/// an entity *set*, which need not be contiguous and never starts at zero, so positional addressing
/// — "the entity id is the array index" — would read every value against the wrong entity. The
/// base column's rule (the file's absence means positional) is exactly what must not apply to an
/// extent, so the path is carried rather than probed for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttrExtent {
    /// The [`DeclaredScalar::name`] this extends — the column's declared name, never a path
    /// segment to be parsed back. For a **group-scoped** family (`views.md` §5) it is the
    /// family's name, and [`Self::view`] says which of its columns this extends.
    pub column: String,
    /// The view whose column of a **group-scoped family** this extends — `None` for the ordinary
    /// entity-scoped column, which has one column bundle-wide.
    ///
    /// **Named rather than parsed out of the values path.** A family has one column per view of
    /// its group and the column *name* is shared between them, so every consumer that keys on
    /// `column` alone — the coalesce's window selection, the fold's per-column merge — would
    /// otherwise treat two views' extents as one column's layers and merge Q3's values into Q4's.
    /// The pair `(column, view)` is the identity; the path is the artefact.
    ///
    /// The `Option`'s absence is the field's own — an entity-scoped column belongs to no view —
    /// and not tolerance of an older manifest ([`AttrExtent::dict`]'s note).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    /// The incarnation of [`Self::view`] when this extent was written — `Some` exactly when
    /// `view` is, an entity-scoped column belonging to no view (decision 0115). Carried for
    /// [`SegmentDescriptor::incarnation`]'s reason: the list is carried forward for ever, so an
    /// extent outlives the drop that orphaned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<ViewIncarnation>,
    /// Prefix-relative path of the values file.
    pub values: String,
    /// Prefix-relative path of the presence bitmap.
    pub presence: String,
    /// Prefix-relative path of the layer's front-coded sorted dictionary — keyword and text
    /// columns only (`records-and-search.md` §4.3/§4.4). `None` for every family whose values
    /// file carries the values themselves rather than ordinals into a dictionary.
    ///
    /// **⊘ Written by nobody yet.** The slot is reserved here rather than added when the keyword
    /// family lands so that the three string epics change this struct once, not three times —
    /// each layer's files must swap atomically (records §7), and that is a property of the
    /// *struct*, not of any one family. The `Option`'s absence is the field's own (a number has
    /// no dictionary), not tolerance of an older manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dict: Option<String>,
    /// Prefix-relative path of the layer's per-term postings — keyword (derived, decision 0067)
    /// and text columns (records §4.3/§4.4). Same reservation as [`AttrExtent::dict`].
    ///
    /// **⊘ Written by nobody yet.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postings: Option<String>,
    /// Prefix-relative path of the CSR offsets file — multi-valued columns only (records §5).
    /// Same reservation as [`AttrExtent::dict`].
    ///
    /// **⊘ Written by nobody yet.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offsets: Option<String>,
}

impl AttrExtent {
    /// Every file this extent owns, the optional ones when the extent names them.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        [self.values.as_str(), self.presence.as_str()]
            .into_iter()
            .chain(
                [&self.dict, &self.postings, &self.offsets]
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            )
    }
}

/// One entry of `text_extents`: one flush's text layer — **dictionary, postings and presence, and
/// no value column** (`records-and-search.md` §4.4).
///
/// **A separate list from [`AttrExtent`] because the shape genuinely differs**, as the record
/// blob's does. Every other indexed family stores one value per entity, so its extent is a value
/// view plus presence with the dictionary beside it; a text field has *many* terms per entity, so
/// there is no per-entity slot to store and the postings are the whole index. Widening `AttrExtent`
/// instead would make `values` optional for one family and force every reader of every other family
/// to handle an absence that cannot occur.
///
/// The three files are **one record**, which is what makes them one atomic unit (§7, review B2):
/// this extent's postings are positions in *this* extent's dictionary and name nothing against
/// another's, so a reader that saw a new dictionary beside old postings would recolour the layer
/// with no symptom.
///
/// ⊘ §7's flush paragraph reads "for keyword and text the extent's own sorted dictionary, ordinals
/// against it, and postings" — the *ordinals* clause is a keyword's and cannot be a text column's,
/// there being no single ordinal per entity to hold. §4.4 is the family's own section and is
/// explicit that `index = true` adds "only the per-layer token dictionary and hybrid postings";
/// this follows §4.4, and §7 owes the narrowing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TextExtent {
    /// The column this extent belongs to — a declared column's name, or a **group-scoped**
    /// family's, in which case [`Self::view`] says which of its columns this extends.
    pub column: String,
    /// The view whose column of a group-scoped family this extends — `None` for the ordinary
    /// entity-scoped column. [`AttrExtent::view`]'s field, for its reason: a family's columns
    /// share one name, so `(column, view)` is the identity and the path is the artefact. The
    /// `Option`'s absence is the field's own, not tolerance of an older manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    /// The incarnation of [`Self::view`] when this extent was written — `Some` exactly when
    /// `view` is. [`AttrExtent::incarnation`]'s field, for its reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<ViewIncarnation>,
    /// Prefix-relative path of the extent's **own** front-coded token dictionary. An extent's
    /// postings are positions in this dictionary and name nothing against another's.
    pub dict: String,
    /// Prefix-relative path of the extent's postings over that dictionary.
    pub postings: String,
    /// Prefix-relative path of the entities this extent holds a value for.
    ///
    /// **Not derivable from the postings**, which is why it is stored: an entity whose text
    /// analysed to no terms at all — an empty string, a field of pure punctuation — carries a value
    /// and appears in no posting. Without this the layer would report it absent, and a later
    /// extent could claim it.
    pub presence: String,
}

impl TextExtent {
    /// Every file this extent owns.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        [
            self.dict.as_str(),
            self.postings.as_str(),
            self.presence.as_str(),
        ]
        .into_iter()
    }
}

/// One entry of `record_extents`: one flush's record-blob layer (`records-and-search.md` §3, §7).
///
/// The record blob is not a column, so its extents cannot live in [`AttrExtent`]'s list — that
/// list is keyed by [`DeclaredScalar::name`] and `record` is a reserved column name precisely so
/// this namespace cannot collide with a declaration. Each entry names one flush's three files
/// explicitly, for [`AttrExtent`]'s own reason: a reader that recovered them from a path
/// convention would be inferring an artefact's identity from its filename, and a missing file
/// must be an error rather than an absence — **a blob file that is missing, short, or fails its
/// digest refuses at open** (records §3's fail-closed rule), never "those entities have no
/// record".
///
/// Not part of the honoured-state machinery, for the reason [`SegmentsManifest::attr_extents`]
/// is not: that list gates state a reader might not be able to act on, and this field lands in
/// the same epic as the code that reads it. A reader that ignored it would answer drill-down
/// short over post-build entities — the failure the extent exists to remove — so there is no
/// version of this reader for which ignoring it is a posture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordExtent {
    /// Prefix-relative path of the extent's zstd blocks, rows in entity order (records §3).
    pub blocks: String,
    /// Prefix-relative path of the extent's has-row Roaring bitmap — the entities whose rows
    /// this extent holds. Rank in it addresses the within-block offsets.
    pub hasrow: String,
    /// Prefix-relative path of the extent's block directory and rank-indexed offsets.
    pub directory: String,
}

impl RecordExtent {
    /// Every file this extent owns.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        [
            self.blocks.as_str(),
            self.hasrow.as_str(),
            self.directory.as_str(),
        ]
        .into_iter()
    }
}

/// One entry of `entity_terms_extents`: one flush's slice of the entity→term transpose
/// (`entities/terms/`, contracts §2.4; `crate::entity_terms` for the format).
///
/// The same shape and the same argument as [`RecordExtent`]: every file named explicitly rather
/// than recovered from a path convention, layers disjoint in entity space by **I9**, and a
/// missing or short file refusing the open rather than reading as "those entities carry no
/// terms". The stakes differ from the blob's by direction, not by degree — a record read short
/// omits a field from a drill-down, a term list read short omits a *label*, which is what the
/// join rule's `409` compares against.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EntityTermsExtent {
    /// Prefix-relative path of the extent's has-row Roaring bitmap — the entities this flush
    /// minted a term list for. Rank in it addresses `offsets`.
    pub hasrow: String,
    /// Prefix-relative path of the extent's `(cardinality + 1)` `u32` offsets, each relative to
    /// its own block's base.
    pub offsets: String,
    /// Prefix-relative path of the extent's concatenated `u32` term ordinals.
    pub terms: String,
    /// Prefix-relative path of the extent's `u64` block bases, one per 65,536 ranks of
    /// `offsets`, which is what keeps a layer's pair count off a `u32` ceiling.
    pub bases: String,
}

impl EntityTermsExtent {
    /// Every file this extent owns.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        [
            self.hasrow.as_str(),
            self.offsets.as_str(),
            self.terms.as_str(),
            self.bases.as_str(),
        ]
        .into_iter()
    }
}

/// One entry of `membership_extents`: one publication's packed artifact memberships for one level
/// of one layer (`annotation-representation.md` §2.4, and `membership.rs` for the format).
///
/// **The extent is the unit because publication is append-only.** Ordinals are claimed contiguously
/// from a level's cursor, so a publication covers `[ordinal_lo, ordinal_lo + count)` and no earlier
/// extent's range is disturbed. A reader unions the extents of a level in any order and gets the
/// whole; the fold rewrites them into one.
///
/// **This is what took membership out of the WAL.** Before it, the log's only route to reclamation
/// was pinned from the first publication onwards, because nothing else on disk carried a membership
/// — segments carry rows and postings, and the registry above carries declarations. Reclaiming a
/// member holding a publication would have destroyed the only copy, leaving the artifact registered,
/// still addressable, and served as absent.
///
/// **One file per publication per level, not one per artifact.** §2.4 sketches
/// `members/<ordinal>.roaring` and immediately marks it *"a shape, not a layout"*: every bundle file
/// is a manifest entry, so 10⁷ artifacts would be 10⁷ entries. The bytes were always affordable and
/// the packaging was the open question.
///
/// Not part of the honoured-state machinery, for the reason [`SegmentsManifest::attr_extents`] is
/// not: that list gates state a reader might not be able to act on, and this field lands with the
/// code that reads it. A reader carrying the field and ignoring it would serve every published
/// artifact as absent, which is the failure the extent exists to remove.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipExtent {
    /// Prefix-relative path of the packed extent.
    pub path: String,
    pub layer: String,
    pub level: u32,
    /// The first ordinal this extent covers. **Also carried inside the file**, and checked against
    /// it at open: a manifest and a file that disagree about which artifacts a range names would
    /// serve one cluster's members under another's identity.
    pub ordinal_lo: u32,
    /// How many ordinals this extent covers.
    pub count: u32,
}

/// One `(layer, level)`'s artifact-write counter, as of the publication this manifest describes.
///
/// **The counter is what a derived structure is valid *for*, and until now it lived only in
/// memory.** `ArtifactStore` counts the writes that have landed on each level, and everything
/// derived from a level — its row-space projection, its lineage, its containment partition — is
/// correct only for the version it was derived from. A restart rebuilt the store from these
/// manifests and started every level's counter at whatever the seeding happened to produce, so a
/// coordinate recorded before the restart could not be compared with one after it.
///
/// **Per `(layer, level)` and not one counter for the store**, for `ArtifactStore::versions`' own
/// reason: a store-wide counter makes one publication anywhere invalidate every level's derived
/// form everywhere (`design/artifact-serving-at-scale.md` §8.1).
///
/// A level present in `membership_extents` and absent here is a level whose version is unknown,
/// which is not the same as zero — see [`SegmentsManifest::level_versions`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LevelVersion {
    pub layer: String,
    pub level: u32,
    /// How many artifact writes had landed on this level when this manifest was written.
    pub version: u64,
}

/// One file derived from an artifact level: a containment partition, a tile index, a row-major
/// column, a segment's shape row form, or a level's held shapes.
///
/// A reader adopts the file only where the level it seeded is at exactly `level_version`, and
/// derives the structure again otherwise. A stale file is narrow (a growth added members it does
/// not cover), so nothing weaker than equality is safe.
///
/// Every form but containment is addressed by row, so it names the view and the incarnation whose
/// row space it was written over; a containment partition names entities' terms and answers for
/// every view of the level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DerivedExtent {
    /// Prefix-relative path of the file.
    pub path: String,
    pub layer: String,
    pub level: u32,
    pub level_version: u64,
    pub view: Option<String>,
    pub incarnation: Option<ViewIncarnation>,
    pub form: DerivedForm,
}

/// Which structure a [`DerivedExtent`] holds, and what else a reader checks before adopting it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DerivedForm {
    Containment,
    TileIndex,
    /// `layout` says which of the two column formats the file is in, and is checked against the
    /// file's magic at open. Never [`ServingLayout::ArtifactMajor`], which has no column.
    RowColumn {
        layout: ServingLayout,
    },
    /// The rows of one segment. A `seg_id` is never reused, so the file answers for that segment
    /// in every generation that carries it; `row_count` catches a file written for another.
    ShapeRows {
        seg_id: String,
        row_count: u32,
    },
    ShapeHeld,
}

impl DerivedForm {
    /// The directory under a partition that files of this form are written to.
    pub fn dir(&self) -> &'static str {
        match self {
            DerivedForm::Containment => "containment",
            DerivedForm::TileIndex => "tile-index",
            DerivedForm::RowColumn { .. } => "row-column",
            DerivedForm::ShapeRows { .. } => "shape-rows",
            DerivedForm::ShapeHeld => "shape-held",
        }
    }
}

/// One entry of `term_image_extents`: one `(partition, view)`'s term images — every
/// authorisation term's base posting projected into that view's row space
/// (`tessera_store::term_images`).
///
/// One file per view rather than one per term: the table is dense over term ids, so a term with no
/// image still reports the size the route chooser prices its walk from.
///
/// `dict_len` and `keep_rows_per_container` are the two figures a reader must agree with the
/// writer about before it maps anything. The dictionary length fixes where the table ends and the
/// payload begins, and the keep rule fixes which terms the chooser may read rather than walk, so a
/// file written under either of them and opened under the other would be read against the wrong
/// layout or priced against the wrong cost model. Both are also in the file's own header, and the
/// open compares the pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TermImageExtent {
    /// Prefix-relative path of the term-image file.
    pub path: String,
    /// The view whose row space the images are in.
    pub view: String,
    /// The view's incarnation when the images were derived (decision 0115). Carried for
    /// [`SegmentDescriptor::incarnation`]'s reason: an image is a set of *rows*, so one derived
    /// over a dropped incarnation's row space names the rows of a key created again.
    pub incarnation: ViewIncarnation,
    /// Terms the table covers, which is the dictionary length the file was derived against.
    pub dict_len: u32,
    /// The keep rule the file was derived under, in rows per Roaring container.
    pub keep_rows_per_container: u32,
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

impl LocatorExtent {
    /// Every file this extent names, the run it indexes included — which is also named by
    /// `external_id_runs`, so a caller walking both lists sees it twice.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        [self.path.as_str(), self.external_id_run.as_str()].into_iter()
    }
}

/// `SEGMENTS-<n>.json` (contracts §2.3): complete current state for one partition, written by
/// that partition's worker only after every file it names is durable.
///
/// **`n` lives in the filename and nowhere else.** This struct carried a `segments_version` field
/// defined as `= n`, which was redundant on its face and actively misleading in its name: the
/// *geometry* version — the counter a row-projection cache key rotates on — is a different
/// quantity, and it must **not** advance when an overlay publication writes a manifest carrying
/// only new deny state (write-path §5.6; bumping it would cost every live session a
/// measured 1 277 ms row-projection rebuild per deny burst). Two counters with one name is how the
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
    /// The row-less region's mark: one past the lowest entity any layer registration has claimed,
    /// or `ROWLESS_CEILING` when none has.
    ///
    /// **Required, and this is the field decision 0074 calls the part to get right.** The WAL
    /// carries the same mark in its `LayerCreate` records, and rotation reclaims those — so a mark
    /// that lived only there is lost at the first rotation, and the next registration is handed ids
    /// a live layer already holds: two entities, one `tessera_id`. This is the home that survives,
    /// exactly as `entity_id_high_water` is for the point region.
    ///
    /// **No `serde(default)`, and the reason is sharper here than elsewhere.** A default would make
    /// an absent mark and a *lost* mark the same value, which is the fail-open the field exists to
    /// close — and the safe-looking default is the ceiling, which is precisely the value that
    /// reissues everything. A manifest omitting it is malformed, not layer-free.
    pub entity_id_low_water: u64,
    /// The annotation layer registry as of this publication, complete current state rather than a
    /// diff — the posture contracts §2.3 already fixes for `deny` and `tombstones`.
    ///
    /// No `serde(default)`, per [`SegmentsManifest::attr_extents`]'s argument: a manifest that
    /// omits it is malformed, not layer-free. The distinction matters because the two are
    /// indistinguishable under a default and only one of them is safe to serve.
    pub layers: Vec<RegisteredLayer>,
    /// Every layer name that has ever been dropped. **Carried for ever and never pruned**:
    /// bookmarks, edges and suppressions all travel by name, so a name that once meant something
    /// must not come to mean something else. A tombstone list that forgot would let a recreated
    /// layer silently inherit every stale reference to the old one.
    pub layer_tombstones: Vec<String>,
    /// Every view **created while the service runs**, complete current state (`views.md` §3.2).
    ///
    /// **This is the roster's durable home, and the WAL is not.** The create and drop records are
    /// WAL entries for replay, but rotation reclaims them — so a roster that lived only in the log
    /// is lost at the first rotation, and a reused key silently repoints every client cache keyed
    /// on the view (decision 0029). Carried forward for ever, exactly as
    /// [`SegmentsManifest::entity_id_low_water`] and [`SegmentsManifest::layer_tombstones`] are
    /// and for the same reason.
    ///
    /// The views a *build* declared are in `MANIFEST.json` and are not restated here: this list
    /// is the additions, and the served roster is the two together plus the WAL's own overlay.
    ///
    /// No `serde(default)`, on `layers`' argument: a manifest omitting it is malformed, not
    /// creation-free, and the two are indistinguishable under a default while only one is safe to
    /// serve — an absent list reads as *no view was ever created*, which is what a lost list looks
    /// like, and the roster then serves a group as though nothing had ever been added to it.
    pub views: Vec<CreatedView>,
    /// Every `(group-scoped family, view)` pair a **flush** has written a column or a render lane
    /// for, complete current state (`views.md` §5).
    ///
    /// **The durable half of `scoped_scalars[..].views`, and the reason it cannot be derived.** A
    /// family's list in `MANIFEST.json` names the views the *build* wrote a column for; a view
    /// created while the service runs acquires one at its first flush carrying values, and
    /// `MANIFEST.json` is rewritten only by a fold. Deriving the pairs from `attr_extents` instead
    /// would recover a filterable family's — its extents name their view — and lose a
    /// **render-only** family's, which writes a row lane and no entity-space extent at all: the
    /// column would come back from a restart as one the manifest does not know exists, and its
    /// values would be served as the ordinary absence below.
    ///
    /// Carried forward for ever and never pruned, exactly as [`Self::views`] is: a view's column
    /// is on disc until a fold rewrites the prefix, and a fold writes the derived list into the
    /// new `MANIFEST.json` rather than leaving it here.
    ///
    /// No `serde(default)`, on `layers`' argument: a manifest omitting it is malformed, not
    /// column-free, and the two are indistinguishable under a default while only one is safe to
    /// serve.
    pub scoped_columns: Vec<ScopedColumn>,
    /// Every entity-scoped attribute column **declared while the service runs**
    /// (`ingest.md` §1.3, §6.3), complete current state, in declaration order.
    ///
    /// **This is the declaration's durable home, and the WAL is not**, on [`Self::views`]'
    /// argument: rotation reclaims the `AttributeDeclare` record, and a column whose declaration
    /// lived only there comes back from a restart as one no reader knows exists. The columns a
    /// build declared are in `MANIFEST.json` and are not restated here; the served schema is the
    /// two together ([`Manifest::with_attributes`]), and the fold writes the union into the next
    /// `MANIFEST.json` and leaves here only the declarations made after it planned.
    ///
    /// **The list is also the set of columns whose base artefacts do not exist yet.** A column
    /// declared at a running service has no base value column, no base postings and no slot in
    /// any segment written before it; a fold writes all of those and moves the column into
    /// `MANIFEST.json` in the same publication, so a name here is one the opener must not demand
    /// a base for (`FilterColumns::open`) and the fold must not read one from.
    ///
    /// No `serde(default)`, on `layers`' argument: a manifest omitting it is malformed, not
    /// declaration-free, and the two are indistinguishable under a default while only one is safe
    /// to serve.
    pub attributes: Vec<DeclaredScalar>,
    /// Every group-scoped attribute family declared while the service runs, on
    /// [`Self::attributes`]' argument. Separate from it because a family has no slot in the flat
    /// list ([`GroupDescriptor::scoped_scalars`]). A family's `views` list here is what the
    /// flushes since the declaration have given a column (`with_scoped_columns` extends it), and
    /// the fold writes the family into the next `MANIFEST.json` with that list.
    pub scoped_attributes: Vec<ScopedScalar>,
    /// Every vocabulary declared while the service runs (`ingest.md` §1.3), complete current state
    /// with its values as last published, on [`Self::attributes`]' argument. A value minted into
    /// one of these at a window close reaches the log as a `VocabularyMint` and this list at the
    /// next publication, as [`Self::vocabulary_extensions`] does for a built vocabulary. Not built
    /// yet: empty from every writer; T5.
    pub vocabularies: Vec<ManifestVocabulary>,
    /// Every view group declared while the service runs (`ingest.md` §1.3), complete current
    /// state, on [`Self::attributes`]' argument. Its roster is [`Self::views`], which already
    /// carries every view created at a running service whichever group owns it, so a descriptor
    /// here carries the group's own half — frame, projection, gate, point default, metadata — and
    /// its `views` list is rebuilt from the roster at every open.
    pub groups: Vec<GroupDescriptor>,
    /// Every **plain view** declared while the service runs (`ingest.md` §1.3 and §10, R9),
    /// complete current state, on [`Self::attributes`]' argument.
    ///
    /// **Its own list rather than [`Self::views`]**, which carries roster records: a plain view
    /// belongs to no group and carries its frame, projection, gate and point default itself,
    /// where a roster record carries a key and metadata under a group's. Merged into
    /// `MANIFEST.views` at every open ([`Manifest::with_plain_views`]), so nothing downstream
    /// tells a plain view the build declared from one declared since.
    ///
    /// No `serde(default)`, on `layers`' argument: a manifest omitting it is malformed, not
    /// view-free, and the two are indistinguishable under a default while only one is safe to
    /// serve.
    pub plain_views: Vec<ViewDescriptor>,
    /// Every **incarnation of a key that has died** and whose artifacts a fold has not yet
    /// reclaimed (`views.md` §3.4, decision 0115).
    ///
    /// **This is bookkeeping, not a refusal.** It was a tombstone list, and a create measured
    /// itself against it; a dropped key is now reusable, and what remains is what the reuse needs
    /// — the record that an incarnation's row spaces, columns and derived structures are on disc
    /// and unreachable. Renamed rather than repurposed under the old name, so that nothing reads
    /// it as the burn it no longer is.
    ///
    /// Carried forward at every publication, on `layer_tombstones`' argument: a mark that lives
    /// only in the log is lost at the first rotation, and a reclaim that forgot an incarnation
    /// would leave its files on disc for ever.
    pub dead_view_incarnations: Vec<DeadIncarnation>,
    /// Every packed membership extent this partition holds — see [`MembershipExtent`]. Empty in a
    /// bundle straight out of `tessera build`, which registers no layers and publishes no artifacts.
    ///
    /// No `serde(default)`, per [`SegmentsManifest::attr_extents`]'s argument: a manifest omitting it
    /// is malformed, not artifact-free. The two are indistinguishable under a default and only one of
    /// them is safe to serve — an absent list reads as *no artifact was ever published*, which is
    /// exactly what a lost list looks like, and the artifacts are then served as absent with nothing
    /// anywhere reporting a fault.
    pub membership_extents: Vec<MembershipExtent>,
    /// Every `(layer, level)`'s artifact-write counter as of this publication — see
    /// [`LevelVersion`].
    ///
    /// No `serde(default)`, on `membership_extents`' argument and with the same shape of
    /// consequence: an absent list and a lost list are indistinguishable under a default, and a
    /// lost one restarts every level at zero — which is a coordinate a derived structure written
    /// under the *old* numbering could compare equal to. A manifest omitting it is malformed, not
    /// version-free.
    pub level_versions: Vec<LevelVersion>,
    /// Every derived artifact file this partition holds. See [`DerivedExtent`]. No
    /// `serde(default)`: a lost list must not read as an empty one.
    pub derived_extents: Vec<DerivedExtent>,
    /// Every view's term images this partition holds. See [`TermImageExtent`]. Empty in a bundle
    /// whose views hold no rows, in one whose dictionary carries no terms, and in one published
    /// before a build or a fold derived them.
    ///
    /// No `serde(default)`, on `membership_extents`' argument: an absent list and a lost list are
    /// indistinguishable under a default. The consequence here is that every session builds its
    /// row projection by walking its permutation, which answers the same rows and takes the time
    /// the images exist to remove, with nothing reporting a fault.
    pub term_image_extents: Vec<TermImageExtent>,
    /// Every record-blob extent holding **artifact supplied content** — the same format, reader and
    /// store as [`SegmentsManifest::record_extents`], listed separately.
    ///
    /// **Separate because ownership differs, not because the bytes do.** A point extent is written
    /// by a flush and consumed by the coalesce and the fold; an artifact extent is written by an
    /// artifact publication, which clones a *stale* manifest and so must restate its whole list
    /// rather than extend one — the posture `membership_extents` takes above and for the same
    /// reason. Keeping them on one list would put a held list and a maintained list in the same
    /// field, where a coalesce consuming an entry and a publication restating it would each undo
    /// the other.
    ///
    /// ⊘ **They are therefore not coalesced or folded**, and accumulate one file per publication
    /// until the fold's artifact pass rewrites them (Stage 4, which also owns the fold refusal a
    /// node with published artifacts already takes). A reader opens them alongside the point
    /// extents; the two never share an entity, artifact ids descending from the ceiling and point
    /// ids ascending from zero.
    ///
    /// No `serde(default)`, on `membership_extents`' argument: an absent list and an empty one are
    /// indistinguishable under a default, and only one of them is safe to serve — an artifact whose
    /// content extent went missing is withheld from every viewer with nothing reporting a fault.
    pub artifact_record_extents: Vec<RecordExtent>,
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
    /// Every filter-column extent this partition holds — see [`AttrExtent`]. Empty in a bundle
    /// straight out of `tessera build`, whose value columns cover every entity it knows about.
    ///
    /// **Not in [`HONOURED_STATE`], for the reason `dict_extents` and `locator_extents` are not:**
    /// that list gates *state a reader might not be able to act on*, and this landed with the code
    /// that reads it. A reader that carried the field and ignored it would answer filters short
    /// over post-build entities, which is the failure the extent exists to remove — so there is no
    /// version of this reader for which ignoring it is a posture.
    pub attr_extents: Vec<AttrExtent>,
    /// Every record-blob extent this partition holds — see [`RecordExtent`]. Empty in a bundle
    /// straight out of `tessera build`, whose base blob (`attrs/record/*`) covers every entity
    /// it knows about. Ordered oldest-first, like [`SegmentsManifest::attr_extents`]; the layers
    /// are disjoint in entity space (**I9**), so order only decides which layer answers first.
    ///
    /// No `serde(default)`, per [`SegmentsManifest::attr_extents`]'s argument: a manifest that
    /// omits it is malformed, not extent-free.
    pub record_extents: Vec<RecordExtent>,
    /// Every entity→term transpose extent this partition holds — see [`EntityTermsExtent`].
    /// Empty in a bundle straight out of `tessera build`, whose base layer
    /// (`entities/terms/*`) covers every entity it knows about. Oldest first, and disjoint in
    /// entity space (**I9**), so order decides only which layer answers first.
    ///
    /// No `serde(default)`, per [`SegmentsManifest::attr_extents`]'s argument: a manifest that
    /// omits it is malformed, not extent-free.
    pub entity_terms_extents: Vec<EntityTermsExtent>,
    /// One flush's text layer per entry, oldest first — the base build's index is not in this list
    /// and is opened from the column's own directory, exactly as `record_extents` treats the base
    /// blob.
    ///
    /// The layers are **disjoint in entity space** (**I9**), so a `match` unions across them and
    /// order decides nothing. That disjointness is why this list needs no coverage check of the
    /// kind [`SegmentsManifest::attr_extents`] carries: an entity id is never reused, so no two
    /// text layers can hold the same entity's terms.
    ///
    /// No `serde(default)`, per [`SegmentsManifest::attr_extents`]'s argument: a manifest that
    /// omits it is malformed, not extent-free.
    pub text_extents: Vec<TextExtent>,
    #[serde(default)]
    pub external_id_runs: Vec<String>,
    /// The reverse external-id direction for each flush segment — see [`LocatorExtent`]. Empty in
    /// a bundle straight out of `tessera build`, whose one `ext-locator.u32` covers every entity
    /// it knows about.
    #[serde(default)]
    pub locator_extents: Vec<LocatorExtent>,
    /// The entities already deleted whose rows a fold has not yet removed — see [`DenySet`].
    #[serde(default)]
    pub tombstones: DenySet,
    /// The suppression set as it stood when this manifest was written — see [`DenySet`]. A
    /// separate field from [`SegmentsManifest::tombstones`] and never its union: publishing the
    /// union would make every deletion look retirable by an unsuppress.
    #[serde(default)]
    pub deny: DenySet,
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
pub const HONOURED_STATE: &[&str] = &[
    "deltas",
    "deny",
    "tombstones",
    "vocabulary_extensions",
    // Both arrived with the code that reads them (`WritePath::reconstruct` seeds the registry from
    // the manifest and replays the WAL on top). A reader carrying a layer list and ignoring it
    // would serve a bundle as though no layer had ever been registered — every gate absent, every
    // reserved run available for reissue — which is the fail-open this list exists to close.
    "layers",
    "layer_tombstones",
    // The roster's runtime half, on the same argument: a reader carrying created views and
    // ignoring them would serve a bundle as though the views did not exist — every request naming
    // one a 404 — and, worse, would admit a create on a key a live or tombstoned view already
    // holds, when a key is a view's only address and is never reused (`views.md` §3.2, §3.4).
    "views",
    "dead_view_incarnations",
    // **Every declaration made while the service ran** (`ingest.md` §1.3, §6.3), on the roster's
    // argument and with the same consequence: a reader carrying these and ignoring them would
    // serve a bundle as though the column, the vocabulary, the group or the view had never been
    // declared — the column's values read as absent for every entity, a row's code explained by
    // no key, and a request naming the view a 404. Each arrived with the code that reads it
    // (`Engine::open` merges them into the manifest before anything reads the schema).
    "attributes",
    "scoped_attributes",
    "vocabularies",
    "groups",
    "plain_views",
];

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
    /// A side-manifest that names nothing.
    pub fn empty() -> SegmentsManifest {
        SegmentsManifest {
            watermark: 0,
            entity_id_high_water: 0,
            entity_id_low_water: tessera_types::layer::ROWLESS_CEILING,
            layers: Vec::new(),
            layer_tombstones: Vec::new(),
            views: Vec::new(),
            scoped_columns: Vec::new(),
            attributes: Vec::new(),
            scoped_attributes: Vec::new(),
            vocabularies: Vec::new(),
            groups: Vec::new(),
            plain_views: Vec::new(),
            dead_view_incarnations: Vec::new(),
            membership_extents: Vec::new(),
            level_versions: Vec::new(),
            derived_extents: Vec::new(),
            term_image_extents: Vec::new(),
            artifact_record_extents: Vec::new(),
            segments: Vec::new(),
            deltas: Vec::new(),
            dict_extents: Vec::new(),
            attr_extents: Vec::new(),
            record_extents: Vec::new(),
            entity_terms_extents: Vec::new(),
            text_extents: Vec::new(),
            external_id_runs: Vec::new(),
            locator_extents: Vec::new(),
            tombstones: DenySet::default(),
            deny: DenySet::default(),
            vocabulary_extensions: Vec::new(),
            files: BTreeMap::new(),
        }
    }

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
            ("tombstones", self.tombstones.carries()),
            ("deny", self.deny.carries()),
        ]
        .into_iter()
        .filter(|(name, carried)| *carried && DENY_DISPOSITION_STATE.contains(name))
        .map(|(name, _)| name)
        .collect()
    }

    pub fn unhonourable_state(&self) -> Vec<&'static str> {
        // Deny-disposition fields first, so a truncated message still names the field that
        // decided the posture.
        //
        // A deny field is honoured, and honouring it means acting on the ids it carries. Bytes
        // that do not decode are ids this reader cannot act on, whatever the field name says, so
        // they are listed here past the honoured-name filter below and the manifest is refused
        // rather than served as though nothing were denied.
        let mut fields: Vec<&'static str> = [
            ("tombstones", self.tombstones.undecodable()),
            ("deny", self.deny.undecodable()),
        ]
        .into_iter()
        .filter(|(_, undecodable)| *undecodable)
        .map(|(name, _)| name)
        .collect();
        fields.extend([
            ("deltas", !self.deltas.is_empty()),
            (
                "vocabulary_extensions",
                !self.vocabulary_extensions.is_empty(),
            ),
            ("layers", !self.layers.is_empty()),
            ("layer_tombstones", !self.layer_tombstones.is_empty()),
            ("views", !self.views.is_empty()),
            (
                "dead_view_incarnations",
                !self.dead_view_incarnations.is_empty(),
            ),
            ("attributes", !self.attributes.is_empty()),
            ("scoped_attributes", !self.scoped_attributes.is_empty()),
            ("vocabularies", !self.vocabularies.is_empty()),
            ("groups", !self.groups.is_empty()),
            ("plain_views", !self.plain_views.is_empty()),
        ]
        .into_iter()
        .filter(|(name, carried)| *carried && !HONOURED_STATE.contains(name))
        .map(|(name, _)| name));
        fields
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
        let good: DeclaredScalar = serde_json::from_str(
            r#"{"name": "score", "arrow_type": "f32", "index": false, "render": true}"#,
        )
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
            analyser: None,
            index: false,
            render: true,
        };
        assert_eq!(category.wire_type(), ScalarType::Utf8);
        assert_eq!(category.arrow_type, ScalarType::U16);

        let plain = DeclaredScalar {
            name: "score".to_string(),
            arrow_type: ScalarType::U16,
            vocabulary: None,
            analyser: None,
            index: false,
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

    /// The term-image list survives a round trip, and a manifest that omits it is refused.
    ///
    /// The refusal is the half worth testing. There is no `serde(default)` on the field, so a
    /// manifest written without it fails to parse rather than parsing as a partition whose views
    /// have no images, which is what a list lost in transit would look like.
    #[test]
    fn a_term_image_list_round_trips_and_an_absent_one_is_refused() {
        let mut manifest = SegmentsManifest::empty();
        manifest.term_image_extents.push(TermImageExtent {
            path: "partitions/default/term-images/term-images-000000-000.timg".to_string(),
            view: "s0".to_string(),
            incarnation: DECLARED_INCARNATION,
            dict_len: 41,
            keep_rows_per_container: crate::term_images::KEEP_ROWS_PER_CONTAINER as u32,
        });
        let bytes = serde_json::to_vec(&manifest).expect("a manifest serialises");
        let parsed: SegmentsManifest = serde_json::from_slice(&bytes).expect("and parses back");
        assert_eq!(parsed.term_image_extents, manifest.term_image_extents);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("as JSON");
        value
            .as_object_mut()
            .expect("an object")
            .remove("term_image_extents");
        let without = serde_json::to_vec(&value).expect("re-serialises");
        assert!(
            serde_json::from_slice::<SegmentsManifest>(&without).is_err(),
            "a manifest with no term-image list must not parse"
        );
    }

    #[test]
    fn every_derived_form_round_trips_and_an_absent_list_is_refused() {
        let entry = |form: DerivedForm, view: Option<&str>| DerivedExtent {
            path: format!("partitions/default/{}/x", form.dir()),
            layer: "regions".to_string(),
            level: 0,
            level_version: 3,
            view: view.map(str::to_string),
            incarnation: view.map(|_| DECLARED_INCARNATION),
            form,
        };
        let mut manifest = SegmentsManifest::empty();
        manifest.derived_extents = vec![
            entry(DerivedForm::Containment, None),
            entry(DerivedForm::TileIndex, Some("s0")),
            entry(
                DerivedForm::RowColumn {
                    layout: ServingLayout::RowMajorList,
                },
                Some("s0"),
            ),
            entry(
                DerivedForm::ShapeRows {
                    seg_id: "base".to_string(),
                    row_count: 7,
                },
                Some("s0"),
            ),
            entry(DerivedForm::ShapeHeld, Some("s0")),
        ];
        let bytes = serde_json::to_vec(&manifest).expect("a manifest serialises");
        let parsed: SegmentsManifest = serde_json::from_slice(&bytes).expect("and parses back");
        assert_eq!(parsed.derived_extents, manifest.derived_extents);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("as JSON");
        value
            .as_object_mut()
            .expect("an object")
            .remove("derived_extents");
        let without = serde_json::to_vec(&value).expect("re-serialises");
        assert!(serde_json::from_slice::<SegmentsManifest>(&without).is_err());
    }

    /// The guard must be invisible on the shape `tessera build` writes, or every bundle in the
    /// project stops opening.
    #[test]
    fn a_manifest_with_no_state_carries_nothing_unhonourable() {
        assert!(SegmentsManifest::empty().unhonourable_state().is_empty());
        assert_eq!(
            SegmentsManifest::empty().honourability(),
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
        let mut with_tombstone = SegmentsManifest::empty();
        with_tombstone.tombstones = deny_set(&[17]);
        assert_eq!(with_tombstone.honourability(), Honourability::Honourable);
        assert_eq!(with_tombstone.deny_disposition_state(), vec!["tombstones"]);

        let mut with_deny = SegmentsManifest::empty();
        with_deny.deny = deny_set(&[17]);
        assert_eq!(with_deny.honourability(), Honourability::Honourable);
        assert_eq!(with_deny.deny_disposition_state(), vec!["deny"]);

        // `deltas` is not deny-disposition state: its absence leaves items *missing*, which is
        // staleness in the fail-safe direction, so a deltas-only candidate whose files do not
        // verify may still be stepped past.
        let mut with_delta = SegmentsManifest::empty();
        with_delta.deltas.push("d.arrow".to_string());
        assert_eq!(with_delta.honourability(), Honourability::Honourable);
        assert!(with_delta.deny_disposition_state().is_empty());
    }

    /// Nothing this reader knows about is unhonourable any more. The machinery stays because it
    /// is the tripwire for the *next* state field added to [`SegmentsManifest`], which must
    /// arrive with the code that acts on it or be refused.
    #[test]
    fn no_known_state_field_is_unhonourable_any_more() {
        let mut all_three = SegmentsManifest::empty();
        all_three.tombstones = deny_set(&[17]);
        all_three.deltas.push("d.arrow".to_string());
        all_three.deny = deny_set(&[18]);
        assert!(all_three.unhonourable_state().is_empty());
    }

    /// A `deny` set for the ids given.
    fn deny_set(ids: &[u32]) -> DenySet {
        DenySet::of(&ids.iter().copied().collect::<croaring::Bitmap>())
    }

    /// A deny field whose bytes do not decode is refused, never read as the empty set. Reading it
    /// as empty would serve every entity it named; stepping past it would serve an older manifest
    /// that predates the deny.
    #[test]
    fn a_deny_field_that_does_not_decode_makes_the_manifest_unready() {
        for encoded in ["not base64 at all", "", "AAAA"] {
            let mut manifest = SegmentsManifest::empty();
            manifest.deny = serde_json::from_value(serde_json::json!(encoded)).unwrap();
            assert!(manifest.deny.entities().is_none(), "{encoded} decoded");
            assert_eq!(
                manifest.honourability(),
                Honourability::Unready {
                    fields: vec!["deny"]
                }
            );
            assert_eq!(manifest.deny_disposition_state(), vec!["deny"]);
        }
    }

    /// A round trip through the JSON encoding keeps every id and keeps an empty set empty.
    #[test]
    fn a_deny_set_round_trips_through_its_json_encoding() {
        for ids in [vec![], vec![0u32], vec![1, 2, 3, 70_000, u32::MAX]] {
            let written = serde_json::to_value(deny_set(&ids)).unwrap();
            let read: DenySet = serde_json::from_value(written).unwrap();
            assert_eq!(
                read.entities().unwrap().to_vec(),
                ids,
                "the ids a manifest carries must survive its encoding"
            );
            assert_eq!(read.carries(), !ids.is_empty());
        }
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
        let mut manifest = SegmentsManifest::empty();
        manifest.deltas.push("d.arrow".to_string());
        manifest.deny = deny_set(&[17]);
        assert_eq!(manifest.deny_disposition_state(), vec!["deny"]);

        // And the same for a tombstone beside deltas — the other deny-disposition field.
        let mut manifest = SegmentsManifest::empty();
        manifest.deltas.push("d.arrow".to_string());
        manifest.tombstones = deny_set(&[17]);
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
            [
                "deltas",
                "deny",
                "tombstones",
                "vocabulary_extensions",
                "layers",
                "layer_tombstones",
                "views",
                "dead_view_incarnations",
                "attributes",
                "scoped_attributes",
                "vocabularies",
                "groups",
                "plain_views",
            ],
            "deltas: `build_fragment_with_deltas` unions every live tier into a fragment. \
             deny/tombstones: the loader seeds the initial overlay from them and WAL replay \
             unions on top. vocabulary_extensions: the loader seeds the live \
             `vocabulary::Vocabularies` from them before replay, which is what makes a minted \
             code survive a restart and what keeps it out of the next draw. \
             layers/layer_tombstones: `WritePath::reconstruct` seeds the `LayerRegistry` from them \
             before replaying the WAL over the top, which is what makes a registration survive the \
             rotation that reclaims its `LayerCreate` record — a reader carrying them and ignoring \
             them would open a bundle as though no layer had ever been registered, every gate \
             absent and every reserved run free for reissue. views/dead_view_incarnations: \
             `Engine::open` seeds the `ViewRoster` from them and amends the bundle's own manifest \
             with what it holds, before replaying the WAL over the top — which is what makes a \
             view created while the service ran survive the rotation that reclaims its \
             `ViewCreate` record; a reader carrying them and ignoring them would 404 every \
             request naming such a view and would admit a create on a key a live or tombstoned \
             view already holds. attributes/scoped_attributes/vocabularies/groups/plain_views: \
             `Engine::open` merges each into the bundle's own manifest before anything reads the \
             schema, the roster or the vocabulary table, and replays the WAL over the top — which \
             is what makes a declaration made while the service ran survive the rotation that \
             reclaims its record (`ingest.md` §1.3, §6.3); a reader carrying them and ignoring \
             them would serve the column absent for every entity, a row's code explained by no \
             key, and a 404 for every request naming the view"
        );
    }

    #[test]
    fn fingerprints_distinguish_keys_and_are_not_the_key() {
        let a = identity_key_fingerprint(KEY_HEX);
        let b = identity_key_fingerprint("100f0e0d0c0b0a090807060504030201");
        assert_ne!(a, b);
        assert!(a.starts_with("fp:") && a.len() == 3 + 8);
    }

    fn bare_manifest() -> Manifest {
        Manifest {
            bundle_format: tessera_types::BUNDLE_FORMAT,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            data_plugin_hash: "builtin".to_string(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: Vec::new(),
            vocabularies: Vec::new(),
            small_term_threshold: 32,
            entity_id_high_water: 0,
            identity: descriptor(),
            views: Vec::new(),
            groups: Vec::new(),
            partitions: Vec::new(),
            provenance: serde_json::json!({}),
            files: BTreeMap::new(),
        }
    }

    /// A group, a family over it, a view of it and a column of that view all arriving together
    /// survive the merge — which they do only in [`Manifest::with_declarations`]' order. Each of
    /// the three assertions fails under a merge that ran its step before the one it depends on:
    /// the roster drops a creation whose group is not yet declared, the scoped column drops a
    /// pair whose family or whose view is not yet there.
    #[test]
    fn a_group_its_family_its_view_and_its_column_all_land_from_one_declarations() {
        let group = GroupDescriptor {
            name: "quarter".to_string(),
            title: None,
            members_of: None,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            projection: Projection::None,
            metadata: Vec::new(),
            visibility: None,
            point_default: None,
            views: Vec::new(),
            scoped_scalars: Vec::new(),
        };
        let family = ScopedScalar {
            name: "rank".to_string(),
            group: "quarter".to_string(),
            arrow_type: ScalarType::I32,
            vocabulary: None,
            analyser: None,
            index: true,
            render: false,
            views: Vec::new(),
        };
        let created = CreatedView {
            group: "quarter".to_string(),
            key: "2026-Q1".to_string(),
            incarnation: 1,
            visibility: None,
            metadata: BTreeMap::new(),
        };
        let view_id = format!("quarter{}2026-Q1", crate::GROUP_SEPARATOR);

        let merged = bare_manifest().with_declarations(&Declarations {
            groups: std::slice::from_ref(&group),
            scoped_attributes: std::slice::from_ref(&family),
            created_views: std::slice::from_ref(&created),
            scoped_columns: &[("rank".to_string(), view_id.clone(), 1)],
            ..Declarations::default()
        });

        let group = merged
            .groups
            .iter()
            .find(|g| g.name == "quarter")
            .expect("the declared group");
        assert!(
            group.views.iter().any(|v| v.key == "2026-Q1"),
            "the creation lands on the group that arrived with it"
        );
        assert!(
            merged.views.iter().any(|v| v.id == view_id),
            "and the view it names is declared"
        );
        assert_eq!(
            group
                .scoped_scalars
                .iter()
                .find(|f| f.name == "rank")
                .expect("the declared family")
                .views,
            vec![view_id],
            "and the column of that view is on the family's list"
        );
    }

    /// **A death takes away the incarnation it names and no other.** The list of deaths is never
    /// pruned, so a key created again meets its predecessor's death at every later merge; a key
    /// dropped a second time meets its own.
    #[test]
    fn a_death_removes_only_the_incarnation_it_names() {
        let group = GroupDescriptor {
            name: "quarter".to_string(),
            title: None,
            members_of: None,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            projection: Projection::None,
            metadata: Vec::new(),
            visibility: None,
            point_default: None,
            views: Vec::new(),
            scoped_scalars: Vec::new(),
        };
        let family = ScopedScalar {
            name: "rank".to_string(),
            group: "quarter".to_string(),
            arrow_type: ScalarType::I32,
            vocabulary: None,
            analyser: None,
            index: true,
            render: false,
            views: Vec::new(),
        };
        let created = |incarnation| CreatedView {
            group: "quarter".to_string(),
            key: "2026-Q1".to_string(),
            incarnation,
            visibility: None,
            metadata: BTreeMap::new(),
        };
        let stone = |incarnation| DeadIncarnation {
            group: "quarter".to_string(),
            key: "2026-Q1".to_string(),
            incarnation,
        };
        let view_id = format!("quarter{}2026-Q1", crate::GROUP_SEPARATOR);
        // The key as a fold writes it into `MANIFEST.json` once incarnation 1 has a column.
        let folded = bare_manifest().with_declarations(&Declarations {
            groups: std::slice::from_ref(&group),
            scoped_attributes: std::slice::from_ref(&family),
            created_views: &[created(1)],
            scoped_columns: &[("rank".to_string(), view_id.clone(), 1)],
            ..Declarations::default()
        });
        let listed = |manifest: &Manifest| {
            manifest.groups[0]
                .scoped_scalars
                .iter()
                .any(|f| f.views.contains(&view_id))
        };
        let rostered = |manifest: &Manifest| manifest.groups[0].views.iter().any(|v| v.key == "2026-Q1");

        // Recreated: the predecessor's death leaves it alone.
        let recreated = folded.with_roster(&[created(1)], &[stone(0)]);
        assert_eq!(recreated.incarnation_of(&view_id), Some(1));
        assert!(rostered(&recreated) && listed(&recreated));

        // Dropped again: both deaths are on the list, and its own takes it away.
        let dropped_again = folded.with_roster(&[], &[stone(0), stone(1)]);
        assert_eq!(dropped_again.incarnation_of(&view_id), None);
        assert!(!rostered(&dropped_again));
        assert!(!listed(&dropped_again));
    }

    /// A family lists its views in the group's roster order, whichever view's first column a
    /// flush wrote first, as a build lists them.
    #[test]
    fn a_family_lists_its_views_in_roster_order() {
        let group = GroupDescriptor {
            name: "quarter".to_string(),
            title: None,
            members_of: None,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            projection: Projection::None,
            metadata: Vec::new(),
            visibility: None,
            point_default: None,
            views: Vec::new(),
            scoped_scalars: Vec::new(),
        };
        let family = ScopedScalar {
            name: "rank".to_string(),
            group: "quarter".to_string(),
            arrow_type: ScalarType::I32,
            vocabulary: None,
            analyser: None,
            index: true,
            render: false,
            views: Vec::new(),
        };
        let keys = ["2026-Q1", "2026-Q2", "2026-Q3"];
        let created: Vec<CreatedView> = keys
            .iter()
            .map(|key| CreatedView {
                group: "quarter".to_string(),
                key: key.to_string(),
                incarnation: 1,
                visibility: None,
                metadata: BTreeMap::new(),
            })
            .collect();
        let id = |key: &str| format!("quarter{}{key}", crate::GROUP_SEPARATOR);
        let column = |key: &str| ("rank".to_string(), id(key), 1);

        let first = bare_manifest().with_declarations(&Declarations {
            groups: std::slice::from_ref(&group),
            scoped_attributes: std::slice::from_ref(&family),
            created_views: &created,
            scoped_columns: &[column("2026-Q3")],
            ..Declarations::default()
        });
        let later = first.with_scoped_columns(&[column("2026-Q2"), column("2026-Q1")]);
        assert_eq!(
            later.groups[0].scoped_scalars[0].views,
            keys.iter().map(|k| id(k)).collect::<Vec<_>>()
        );
    }
}
