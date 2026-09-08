//! What an artifact's membership is, and where the canonical copy lives.
//!
//! **Entity space, always, for the durable form.** Entity ids are permanent and view-invariant;
//! row space is per view, derived, and renumbered globally by every fold. A membership stored in
//! row space would be a frozen projection — correct until the first fold, then silently naming
//! other people's documents. The row form is built from this one at open and rebuilt when the
//! generation moves (`annotation-representation.md` §2.1), and it never travels.
//!
//! **The row form is built member-wise and never range-wise**, which is a disclosure rule rather
//! than an implementation note. Translating an entity *range* to a row range would let a Morton
//! neighbour — a document that happens to sit next to a member in row order and belongs to nobody's
//! membership — join the set, and a single extra member can lift an artifact over its existence
//! criterion. The write cycle already forbids range-wise translation for exactly this reason; the
//! same rule reaches the build of the resident form.
//!
//! ## What is not stored, and why the absence is the design
//!
//! **No bounding box.** An earlier draft kept a build-time box over full membership and served an
//! artifact wherever that box intersected the viewport — which discloses the unmasked extent by
//! panning: a viewer sees the edge of a shape in a region holding nothing they may see. Candidacy is
//! a masked question instead, answered from the row form against the viewer's own mask, so no
//! representation here can express the fault.
//!
//! **No unmasked count on any wire shape.** [`ArtifactRecord::declared_size`] is the artifact's
//! full membership size and it exists for exactly one consumer: the proportional existence
//! criterion, which divides by it. It is a **predicate input** — the build computes it, the test
//! consumes it, and nothing serialises it to a client — because a corpus-wide count over items a
//! principal may not see is C8's row, one careless line from being served beside a masked one.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use croaring::{Bitmap, BitmapView, Portable};
use tessera_types::EntityId;

/// Withdraw every content of one record whose generating set names a member this fold retired
/// ([decision 0135](../../../docs/decisions/0135-a-generating-set-is-the-callers-claim-i8-withdrawn.md)).
///
/// The content and its set leave together. A deleted member is outside every mask, so the set
/// already fails containment for every principal; what the fold adds is that nothing durable goes
/// on naming the freed slot (decision 0072), and the caller is told through the fold's report
/// ([`ArtifactStore::degradations`]) and re-declares the set or the content through ingest. The
/// service neither shrinks the set nor keeps the content on the survivors: what a content was
/// derived from is the caller's claim, and the fold does not edit it.
///
/// **An artifact left with no contents is not an artifact with no content** — it is one the
/// serving path withholds, because its layer declares supplied content and it has none to serve.
/// That is [decision 0076](../../../docs/decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)
/// reached from the write side: the alternative is serving the identity and the count with the
/// description missing, which is the in-between state the decision forbids.
///
/// Content that requires only inherited visibility carries no generating set at all (C28) and is
/// never reached. A content moves exactly when its generating set meets `retired`, which is the
/// third term of [`record_moved_by`]; whether a record moved is asked there, not here.
fn withdraw_content_of_retired_members(record: &mut ArtifactRecord, retired: &Bitmap) {
    record
        .contents
        .retain(|content| content.generated_from.and_cardinality(retired) == 0);
}

/// Whether retiring `retired` changes `record`: the artifact's own entity is retired, one of its
/// members is, or a member of one of its contents' generating sets is.
///
/// **The one predicate [`ArtifactStore::levels_moved_by`] and [`ArtifactStore::retire`] both
/// read.** A fold asks the first before it writes its manifest and runs the second after the
/// flip, and it stamps a reported level's derived structures with the version the level will
/// have once the retirement has moved it by one (`write.rs`'s `artifact_coordinates`). The two
/// have to agree level for level: a level reported and not moved, or moved and not reported,
/// would leave a structure held at a version that describes other records.
fn record_moved_by(record: &ArtifactRecord, retired: &Bitmap) -> bool {
    retired.contains(record.entity.raw() as u32)
        || record.members.and_cardinality(retired) != 0
        || record
            .contents
            .iter()
            .any(|content| content.generated_from.and_cardinality(retired) != 0)
}

/// One level's not-yet-published artifacts, ready to pack: `(layer, level, ordinal_lo, blobs)`.
///
/// A tuple alias rather than a struct because it is a *transfer* between two modules that both
/// already name these four things — a struct would be a third name for the same tuple, and the
/// packer takes them apart again immediately.
pub type PendingExtent = (String, u32, u32, Vec<Vec<u8>>);

/// One level's not-yet-published **range**: `(layer, level, ordinal_lo, count)` — the same four
/// things a [`PendingExtent`] names, with the blobs left where they are for a caller that will
/// encode them one at a time. See [`ArtifactStore::pending_ranges`].
pub type PendingRange = (String, u32, u32, u32);

/// What [`ArtifactStore::fill`] did with one part, under the fill rule (`ingest.md` §1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    /// The part was absent and is now held.
    Filled,
    /// The part was held identically; nothing changed.
    Identical,
    /// The part was held differently; nothing changed. Refused on the live path before the
    /// record is appended, so at replay this is damage.
    Differs,
    /// The ordinal names no record, so there is nothing to fill: a hole a fold left.
    NoRecord,
    /// The part's digest does not match its bytes, or a content names a rank past the next.
    Undecodable,
}

/// One artifact's canonical shapes — **one per view of its layer, as bytes this crate stores and
/// never interprets** (`polygon-membership.md` §4.3, §6.6).
///
/// A shape is declared once and resolved per view, because each view quantises in its own frame:
/// the canonical form is grid units, so the same boundary is a different byte sequence in each
/// view it is drawn in. What travels here is the engine's own encoding (`tessera_spatial::shape`,
/// `Shape::encode`): the reader that decodes it is the one that resolves it, and this crate holds
/// the pair `(view, bytes)` exactly as it holds a membership's Roaring bytes — addressed, never
/// arithmetic on.
///
/// **Never empty, and never an empty entry.** An artifact of a shape layer with no shape has no
/// membership rule at all — it counts zero for every viewer and is absent under any criterion,
/// which no client can tell from an artifact whose members are simply invisible to them — so the
/// constructor refuses the shapes that would produce that state, and the blob decoder refuses the
/// bytes that would.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactShapes {
    /// `(view, canonical bytes)`, ascending by view, no view twice.
    by_view: Vec<(String, Vec<u8>)>,
    /// SHA-256 over the per-view encoding ([`Self::content_text`]'s bytes), computed at
    /// construction and stored with the shape in the log and the record blob (`ingest.md` §1.5).
    /// A later record carrying a shape for an artifact that holds one is compared digest to
    /// digest, which is what makes a repeated publication safe after the level is repacked.
    /// The blob decoder checks it against the bytes it decodes; the log path does not, since
    /// postcard restores the struct whole, so `ArtifactStore::fill` routes the log's copy through
    /// [`Self::new`] and compares before it trusts the number.
    digest: [u8; 32],
}

impl ArtifactShapes {
    /// `None` for no views, a view named twice, or a view whose bytes are empty.
    pub fn new(mut by_view: Vec<(String, Vec<u8>)>) -> Option<Self> {
        if by_view.is_empty() || by_view.iter().any(|(_, bytes)| bytes.is_empty()) {
            return None;
        }
        by_view.sort_by(|a, b| a.0.cmp(&b.0));
        if by_view.windows(2).any(|w| w[0].0 == w[1].0) {
            return None;
        }
        let mut shapes = ArtifactShapes {
            by_view,
            digest: [0; 32],
        };
        let mut encoded = Vec::with_capacity(shapes.byte_len() + 8);
        shapes.encode_into(&mut encoded);
        shapes.digest = sha256(&encoded);
        Some(shapes)
    }

    /// The stored digest ([`Self::digest`]'s field).
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// The canonical bytes for one view, or `None` where the shape was not canonicalised for it.
    pub fn for_view(&self, view: &str) -> Option<&[u8]> {
        self.by_view
            .binary_search_by(|(v, _)| v.as_str().cmp(view))
            .ok()
            .map(|i| self.by_view[i].1.as_slice())
    }

    pub fn views(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.by_view.iter().map(|(v, b)| (v.as_str(), b.as_slice()))
    }

    /// The bytes held, summed over the views — what a shape layer costs at rest, for the reports.
    pub fn byte_len(&self) -> usize {
        self.by_view.iter().map(|(v, b)| v.len() + b.len()).sum()
    }

    /// The per-view bytes as an **authored content** carries them (`polygon-membership.md` §6.1,
    /// §6.6): the same per-view wrapper the record's shape tail holds, hex-spelled so that it
    /// fits the utf8 content slot every supplied kind occupies. A content value is text by the
    /// record blob's construction, and the shape's canonical bytes are not, so the spelling is
    /// the whole of what this adds; nothing about the geometry changes. Read back by
    /// [`Self::from_content_text`], and never served — the wire carries the rings.
    pub fn content_text(&self) -> String {
        let mut bytes = Vec::with_capacity(self.byte_len() + 8);
        self.encode_into(&mut bytes);
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// The inverse of [`Self::content_text`]; `None` for anything that is not one — a supplied
    /// `polygon` that reached the store as an opaque string under the earlier reading, or a
    /// truncated value — which withholds the shape rather than drawing a guess.
    pub fn from_content_text(text: &str) -> Option<Self> {
        if !text.len().is_multiple_of(2) || !text.is_ascii() {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
            .collect();
        let bytes = bytes?;
        let mut at = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let end = at.checked_add(n)?;
            let view = bytes.get(at..end)?;
            at = end;
            Some(view)
        };
        let views = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
        let mut by_view = Vec::with_capacity(views);
        for _ in 0..views {
            let view_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
            let view = std::str::from_utf8(take(view_len)?).ok()?.to_string();
            let shape_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            by_view.push((view, take(shape_len)?.to_vec()));
        }
        if at != bytes.len() {
            return None;
        }
        ArtifactShapes::new(by_view)
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.by_view.len() as u16).to_le_bytes());
        for (view, bytes) in &self.by_view {
            let view = view.as_bytes();
            out.extend_from_slice(&(view.len() as u16).to_le_bytes());
            out.extend_from_slice(view);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}

/// One artifact as a caller offers it, before the engine has given it an ordinal or an entity.
///
/// **Members are entities, resolved at admission.** A caller names them by `tessera_id` and the
/// control plane inverts them once, at the boundary, exactly as `/control/changes` does — so no
/// blinded identifier reaches durable state, where a key rotation would silently redirect it (I10).
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingArtifact {
    /// The caller's own name for this artifact. **Effectively mandatory for a layer another
    /// layer's edges point into**: an edge names its target, and at publish time the caller holds
    /// no `tessera_id` for it.
    pub key: Option<String>,
    /// The view this artifact belongs to, on a layer scoped to a group — **part of the identity,
    /// required at publish and never a fillable part** (`ingest.md` §1.5, `views.md` §3.5): keys
    /// are unique per `(layer, view)`, so one key in two views is two artifacts, and an edge may
    /// not cross views. `None` on an entity-scoped layer, whose one artifact set is drawn on
    /// every view it names, and refused there.
    pub view: Option<String>,
    pub members: Bitmap,
    /// The entities the membership leaves out, where the caller spelled it by exclusion
    /// (`ingest.md` §2.3), and `None` for the inclusion spelling `members` carries.
    ///
    /// **It does not survive admission.** The complement is taken on the executor against the
    /// view's entity set before the record is written and `members` becomes the set an inclusion
    /// would have carried; the list itself travels no further than
    /// [`crate::LayerRegistry::prepare_put`], which reads it for one thing only — a held key,
    /// whose second exclusion is a `409` — and writes it into no record. **There is no type below
    /// this one that can carry the spelling**, so no serving path can learn it and none can
    /// evaluate a complement against a viewer's mask, which would disclose the existence of items
    /// outside it (`annotation-write-cycle.md` §6.1).
    pub excluding: Option<Bitmap>,
    /// The artifact's supplied content, as **ranked contents** — most specific first. Empty on a
    /// layer that declares no supplied content, which is every layer Stage 2 could publish.
    ///
    /// A viewer is served the first content whose generating set they contain, entire, or the
    /// artifact is absent ([decision 0076](../../../docs/decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)).
    /// The order is the caller's ranking and the service takes no opinion on it
    /// ([decision 0078](../../../docs/decisions/0078-the-service-takes-no-opinion-on-which-variation.md)).
    pub contents: Vec<IncomingContent>,
    /// The artifact this one exists only as an attachment to — a toponymy label on a cluster.
    ///
    /// **Named by the target's own key, because an ordinal is never disclosed.** A response
    /// carries a `tessera_id` and never a position in a dense level (C8), so the caller holds no
    /// address for the target beyond the key they published it under.
    pub attached_to: Option<IncomingAttachment>,
    /// The parent artifacts in a hierarchical layer, each named by the parent's own key. Empty at
    /// a root; at most one on a `nested` or `tiered` layer, which refuse a second; as many as the
    /// child sits beneath on a `dag` layer (`dag-hierarchies.md` §4, decision 0117). A key named
    /// twice is one edge.
    ///
    /// **The lineage is declared upward only, and the downward list is deliberately absent.** A
    /// `children_keys` beside this was read, validated for cross-row agreement, and never walked:
    /// every consumer — containment, coverage, cycle detection, the cut — derives children by
    /// inverting the parent edges, because that is the direction an artifact can state without
    /// knowing what will later point at it. Two spellings of one edge is one more place for them
    /// to disagree.
    pub parent_keys: Vec<String>,
    /// The artifact's canonical shapes, one per view — required on a layer whose `shape` declares
    /// one, refused on every other kind. It **is** the membership: `members` stays empty on such a
    /// layer, because the rows inside the shape are resolved from it at every segment's
    /// publication.
    pub shape: Option<ArtifactShapes>,
}

/// The target of an attachment, as a caller names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingAttachment {
    pub layer: String,
    pub level: u32,
    pub key: String,
}

/// One entry in an artifact's ranked `contents`, as a caller offers it. Its position in that
/// list is its **rank**.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingContent {
    /// One value per kind the layer declares, **positionally**. Every declared kind must be
    /// supplied: `/v1/meta` publishes the kinds so a client knows what to draw, and that is only
    /// safe because no served artifact ever lacks one its layer declared.
    pub values: Vec<String>,
    /// The documents this content was generated from — the set a viewer must contain **entirely**
    /// to be served it.
    ///
    /// Empty exactly where the layer declares no corpus-derived kind, in which case containment is
    /// vacuous and the content serves unconditionally. A set supplied where none is tested is
    /// refused rather than stored: a claim the service carries and never checks is worse than no
    /// claim, because a reader takes its presence for a control.
    pub generated_from: Bitmap,
}

/// One membership, built from resolved entities — **sorted once, then appended**.
///
/// Every caller below took the same loop of `Bitmap::add` over whatever order its source happened
/// to hand it, and that is the expensive shape: an out-of-order value lands in the middle of its
/// container's array, so a container of *c* elements pays a `memmove` of *c* per insertion. Sorting
/// first means every value is larger than the container's last, which is Roaring's append path.
/// **Measured at the campaign's own membership shape** — 3.1×10⁶ scattered entities over a 5×10⁷
/// row space, 763 containers of ~4 000 — 458 ms scattered against 72 ms sorted, a factor of 6.4,
/// with the two bitmaps compared equal (`probes/2026-08-22-artifact-serving-e2e/README.md` finding
/// 3's follow-up).
///
/// The order a set is built in is not observable in the set, so this changes no result.
fn bitmap_of_entities(entities: impl IntoIterator<Item = EntityId>) -> Bitmap {
    // Entity space is `u32` by I9, so the narrowing is total.
    let mut values: Vec<u32> = entities.into_iter().map(|e| e.raw() as u32).collect();
    values.sort_unstable();
    values.dedup();
    let mut bitmap = Bitmap::new();
    bitmap.add_many(&values);
    bitmap
}

impl IncomingContent {
    /// Builds one from resolved entities — the constructor exists for
    /// [`IncomingArtifact::from_entities`]'s reason: `tessera-server` names a set without being
    /// able to do arithmetic on one.
    pub fn new(values: Vec<String>, generated_from: impl IntoIterator<Item = EntityId>) -> Self {
        IncomingContent {
            values,
            generated_from: bitmap_of_entities(generated_from),
        }
    }
}

impl IncomingArtifact {
    /// Builds one from resolved entities.
    ///
    /// **The constructor exists so the bitmap type stays inside this crate.** `tessera-server`
    /// assembles these from a resolved batch and carries no Roaring dependency — a layering
    /// `check-layers.sh` holds, and one worth holding: the request plane should be able to name a
    /// membership without being able to do arithmetic on one.
    pub fn from_entities(key: Option<String>, members: impl IntoIterator<Item = EntityId>) -> Self {
        IncomingArtifact {
            key,
            view: None,
            members: bitmap_of_entities(members),
            excluding: None,
            contents: Vec::new(),
            attached_to: None,
            parent_keys: Vec::new(),
            shape: None,
        }
    }

    /// Spell this artifact's membership by exclusion: `excluded` names the entities it leaves
    /// out, and `members` stays empty until the executor complements it (`ingest.md` §2.3).
    ///
    /// **The method exists so the bitmap type stays inside this crate**, on
    /// [`Self::from_entities`]'s argument: the request plane names a list without being able to
    /// complement one.
    pub fn exclude(&mut self, excluded: impl IntoIterator<Item = EntityId>) {
        self.excluding = Some(bitmap_of_entities(excluded));
    }

    /// The membership an exclusion means: `entities` — the view's entity set as of this step —
    /// minus the excluded list, materialised here and the spelling dropped.
    ///
    /// One `andnot` over the view's entity bitmap, which is what the bound on the *list* buys:
    /// the complement itself is not bounded and need not be (`ingest.md` §2.3). Answers how large
    /// the membership became, for the caller's log, or `None` where the artifact carried no
    /// exclusion. The list is left in place: the held-key refusal is what reads it, and it
    /// reaches no record.
    pub fn complement_against(&mut self, entities: &Bitmap) -> Option<u64> {
        let excluded = self.excluding.as_ref()?;
        self.members = entities.andnot(excluded);
        Some(self.members.cardinality())
    }

    /// The same, attached to another layer's artifact — the shape a label layer publishes.
    pub fn attached(
        key: Option<String>,
        members: impl IntoIterator<Item = EntityId>,
        contents: Vec<IncomingContent>,
        attached_to: IncomingAttachment,
    ) -> Self {
        let mut artifact = IncomingArtifact::with_content(key, members, contents);
        artifact.attached_to = Some(attached_to);
        artifact
    }

    /// The same, carrying supplied content.
    pub fn with_content(
        key: Option<String>,
        members: impl IntoIterator<Item = EntityId>,
        contents: Vec<IncomingContent>,
    ) -> Self {
        let mut artifact = IncomingArtifact::from_entities(key, members);
        artifact.contents = contents;
        artifact
    }
}

/// Entities joining an artifact that already exists, and the fixed parts a caller supplies for
/// it, as a caller offers them (`ingest.md` §1.5).
///
/// **Addressed by the caller's own key, and resolved on the executor.** An ordinal never crosses
/// the wire (C8) and the caller holds none; the key is the address they published under, and
/// `ArtifactStore::ordinal_of_key` is what resolves it — a lookup in the *store*, never in what is
/// served, so a suppressed artifact resolves like any other and a growth against it leaves it
/// suppressed (`artifacts-from-points.md` §5).
///
/// **Members are entities, resolved at admission**, on [`IncomingArtifact`]'s rule: no blinded
/// identifier reaches durable state, where a key rotation would silently redirect it (I10).
///
/// **The fixed parts follow the fill rule** (`ingest.md` §1.1): a part the artifact does not hold
/// is filled, a part it holds identically is accepted with no effect, and a part it holds
/// differently refuses the batch naming the part. Each fill is its own record
/// ([`crate::wal::WalRecord::ArtifactFill`]) beside the growth's.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingGrowth {
    /// The key the artifact was published under. **An unknown one is refused rather than minted**,
    /// on this route whatever the layer's value set says: a growth names an artifact to add members
    /// to, and there is no point whose column declared the key, so an unknown one is a typo with
    /// nothing behind it. Minting is what a *membership column* does — at a build from a member
    /// source, and at ingest from a column named for the layer (`artifacts-from-points.md` §6.3).
    pub key: String,
    /// The entities joining. Empty is a no-op rather than a refusal: nothing joining is a thing a
    /// caller can honestly say, and it discloses nothing.
    pub joining: Bitmap,
    /// The fixed parts supplied for the artifact; every field empty on a growth that only joins.
    pub parts: FixedParts,
}

/// The fixed parts of an artifact a record may supply, each on the fill rule (`ingest.md` §1.5):
/// the parent list, the attachment, the shape, and each content's values at its rank.
///
/// A content here carries values and no generating set: the set is a set part and travels as a
/// growth at the same rank ([`crate::wal::GrownSet::GeneratingSet`], track T2b). Until that
/// lands, a content on a layer whose content requires every member visible has no set to be
/// tested against and is refused, as a publication carrying an empty set is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FixedParts {
    /// The parents, each named by the parent's own key, on [`IncomingArtifact::parent_keys`]'s
    /// terms.
    pub parent_keys: Vec<String>,
    /// The attachment, named by the target's key, on [`IncomingArtifact::attached_to`]'s terms.
    pub attached_to: Option<IncomingAttachment>,
    /// Contents by rank: `(rank, values)`, the values positional to the layer's declared kinds.
    pub contents: Vec<(u16, Vec<String>)>,
    /// The canonical shapes, on [`IncomingArtifact::shape`]'s terms.
    pub shape: Option<ArtifactShapes>,
}

impl FixedParts {
    /// Whether no part is supplied.
    pub fn is_empty(&self) -> bool {
        self.parent_keys.is_empty()
            && self.attached_to.is_none()
            && self.contents.is_empty()
            && self.shape.is_none()
    }
}

impl IncomingGrowth {
    /// Builds one from resolved entities — [`IncomingArtifact::from_entities`]'s reason: the
    /// bitmap type stays inside this crate, so a request plane can name a set without being able
    /// to do arithmetic on one.
    pub fn from_entities(key: String, joining: impl IntoIterator<Item = EntityId>) -> Self {
        IncomingGrowth {
            key,
            joining: bitmap_of_entities(joining),
            parts: FixedParts::default(),
        }
    }
}

/// What one fold's deletions took from one artifact — a row of the fold's report.
///
/// **Addressed by the caller's own key where they supplied one**, because that is the name they can
/// act on: a `tessera_id` is what a *viewer* holds, and the ordinal is an internal address that no
/// response carries. A caller who published without a key gets the address and can still find the
/// artifact by it on the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    pub key: Option<String>,
    /// How many of this artifact's members the fold retired. Zero where only its content lost
    /// sources — the two losses are independent.
    pub members_lost: u64,
    /// What the membership held before this fold, so a caller can see the proportion rather than
    /// having to hold the previous number themselves.
    pub declared_members: u64,
    /// `(rank, members of that generating set the fold retired)`, for the contents that lost any.
    /// Every content listed here is withdrawn by this fold (decision 0135), so the list's length
    /// is the number of contents the artifact lost. Empty on a layer that declares no supplied
    /// content.
    pub contents_lost: Vec<(u32, u64)>,
}

/// One artifact's membership — **the bitmap, or a read-only view of the same bitmap's bytes
/// somewhere the heap is not**.
///
/// # Why the field is not simply a `Bitmap`
///
/// A membership is a bitmap for its whole serving life and is never anything else; what this adds
/// is where the *containers* live. At a build the store is filled at the layers stage and read
/// again at the artifact pass, four stages later, and in between it holds one Roaring bitmap per
/// artifact over the whole corpus. Measured on the 10⁷ MedCPT sample with 471,778,374 closed MeSH
/// member rows, that is **+1.2 GB of anonymous memory** carried across the build's peak — about
/// 2.7 bytes per closed member row, array containers almost throughout
/// (`probes/2026-09-02-mapped-memberships/README.md`).
///
/// The bytes are written to the bundle's own membership extent a moment later, and the build maps
/// that file back ([`Members::mapped`]). A view costs the heap its container *descriptors* and
/// nothing else: the two-byte values themselves stay page cache the kernel may evict. There is no
/// second format and no second write — the mapped bytes are the extent's, in the portable Roaring
/// form [`serialise_members`] already wrote.
///
/// # What a view may and may not do
///
/// Reads go through [`Deref`](std::ops::Deref), so every caller that asks a membership a question is unchanged and
/// cannot tell the two apart. A *write* takes [`Members::to_mut`], which materialises an owned
/// bitmap first — growth and retirement therefore behave identically on either form, which is what
/// keeps write-path §5.4's two removal rules the only routes a bit leaves a membership.
///
/// ⊘ **The mapping's lifetime is the owner's, and the owner is held here.** `bytes` points into an
/// allocation `owner` keeps alive — at a build, the mapped extent file — so the view is valid for
/// exactly as long as this value is. Nothing outside [`Members::mapped`] can construct one.
pub struct Members(MembersInner);

enum MembersInner {
    Owned(Bitmap),
    /// Declared in drop order: the view's header is freed before the bytes it addresses can go.
    Mapped {
        view: BitmapView<'static>,
        bytes: &'static [u8],
        owner: Arc<dyn Any + Send + Sync>,
    },
}

/// `Bitmap` carries croaring's own `Send`/`Sync`, and a view over bytes nothing else may write is
/// no weaker: every operation reachable through [`Deref`](std::ops::Deref) is a read of a `roaring_bitmap_t` and of
/// the immutable slice behind it, and the one route to a mutation ([`Members::to_mut`]) needs
/// `&mut self`. The owner is `Send + Sync` by its own bound.
unsafe impl Send for Members {}
unsafe impl Sync for Members {}

impl Members {
    /// The membership on the heap — what a publication, a WAL replay and every test produce.
    pub fn owned(bitmap: Bitmap) -> Self {
        Members(MembersInner::Owned(bitmap))
    }

    /// The membership read through `bytes`, which `owner` keeps alive.
    ///
    /// `bytes` must be the **portable** Roaring form [`serialise_members`] writes — the form the
    /// packed extent carries — and must live inside an allocation `owner` owns. Answers `None`
    /// where the bytes are not a bitmap at all, on [`deserialise_members`]'s rule: a membership
    /// that decodes short is one with a low masked count for every viewer, which the existence
    /// criterion renders as absent with nothing to notice.
    ///
    /// # Safety
    ///
    /// `bytes` must remain valid and unwritten for as long as `owner` is held, and dropping
    /// `owner` must not free them earlier. Both hold for a slice of a read-only file mapping the
    /// `owner` itself keeps open.
    pub unsafe fn mapped(bytes: &[u8], owner: Arc<dyn Any + Send + Sync>) -> Option<Self> {
        // The checked deserialiser first: `BitmapView::deserialize` is unchecked, and bytes that
        // are not a bitmap would be read as containers at whatever the header claimed. This costs
        // one decode of the header region and is the same validation the WAL replay applies.
        let checked = deserialise_members(bytes)?;
        drop(checked);
        // SAFETY: the caller's contract above pins the bytes for `owner`'s life, and `owner` is
        // moved into the value that holds the view, so the two cannot be separated.
        let bytes: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(bytes) };
        let view = unsafe { BitmapView::deserialize::<Portable>(bytes) };
        Some(Members(MembersInner::Mapped { view, bytes, owner }))
    }

    /// The membership as something that can be written to, materialising an owned copy where this
    /// was a view. Every mutation in this module goes through it.
    pub fn to_mut(&mut self) -> &mut Bitmap {
        if let MembersInner::Mapped { view, .. } = &self.0 {
            self.0 = MembersInner::Owned(view.to_bitmap());
        }
        match &mut self.0 {
            MembersInner::Owned(bitmap) => bitmap,
            MembersInner::Mapped { .. } => unreachable!("the arm above replaced it"),
        }
    }

    /// Whether this membership is read through a mapping rather than held on the heap — for the
    /// build's own accounting and its tests, and for nothing on a serving path.
    pub fn is_mapped(&self) -> bool {
        matches!(self.0, MembersInner::Mapped { .. })
    }
}

impl std::ops::Deref for Members {
    type Target = Bitmap;

    fn deref(&self) -> &Bitmap {
        match &self.0 {
            MembersInner::Owned(bitmap) => bitmap,
            MembersInner::Mapped { view, .. } => view,
        }
    }
}

impl From<Bitmap> for Members {
    fn from(bitmap: Bitmap) -> Self {
        Members::owned(bitmap)
    }
}

impl Default for Members {
    fn default() -> Self {
        Members::owned(Bitmap::new())
    }
}

impl Clone for Members {
    /// A clone of a view is a view: the owner is an `Arc` and the bytes outlive both.
    fn clone(&self) -> Self {
        match &self.0 {
            MembersInner::Owned(bitmap) => Members::owned(bitmap.clone()),
            MembersInner::Mapped { bytes, owner, .. } => Members(MembersInner::Mapped {
                // SAFETY: `bytes` is the slice this value's own view already addresses, and
                // `owner` — cloned beside it — is what keeps it alive.
                view: unsafe { BitmapView::deserialize::<Portable>(bytes) },
                bytes,
                owner: Arc::clone(owner),
            }),
        }
    }
}

impl std::fmt::Debug for Members {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Members({}{})",
            self.cardinality(),
            if self.is_mapped() { ", mapped" } else { "" }
        )
    }
}

impl PartialEq for Members {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Members {}

impl PartialEq<Bitmap> for Members {
    fn eq(&self, other: &Bitmap) -> bool {
        **self == *other
    }
}

impl PartialEq<Members> for Bitmap {
    fn eq(&self, other: &Members) -> bool {
        *self == **other
    }
}

/// `layer → level → view → key → ordinal`, the key index of [`ArtifactStore`]. The view level is
/// the identity's (`ingest.md` §1.5); see the field's own note for why the map is nested rather
/// than keyed by a tuple.
type KeyIndex = BTreeMap<String, BTreeMap<u32, BTreeMap<Option<String>, BTreeMap<String, u32>>>>;

/// One artifact's durable state, as the registry holds it.
#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    /// This artifact's own entity — its address for the deny lane, and what `tessera_id` blinds.
    pub entity: EntityId,
    /// The caller's own key, if they supplied one. **Effectively mandatory for a layer another
    /// layer's edges point into**: an edge names its target, and at publish time the caller holds
    /// no `tessera_id` for it.
    pub key: Option<String>,
    /// The view this artifact belongs to, on a layer scoped to a group (`views.md` §3.5), and
    /// `None` on an entity-scoped layer. Part of the key's uniqueness scope: `ArtifactStore::keys`
    /// indexes `(layer, level, view, key)`, so one key in two views is two artifacts.
    ///
    /// **The build's rows and the wire's records take this one field.** A build reads it from the
    /// artifact source's `view` column and the publication route from the record's `view`; below
    /// this type there is one shape and no second structure saying which view an artifact is
    /// drawn in.
    ///
    /// **A packed extent carries it** ([`encode_record`], `bundle_format` 8): a level folded and
    /// reopened comes back with each artifact in the view it was published into, which is what
    /// keeps two views' keys apart across a fold.
    pub view: Option<String>,
    /// Entity-space membership — the canonical, view-invariant record. Owned, or read through a
    /// mapping of the bytes that carry it (see [`Members`]).
    pub members: Members,
    /// The ranked contents of this artifact's supplied content, most specific first.
    ///
    /// **The values are not here.** This carries each content's *generating set* — the thing the
    /// serving path does bitmap arithmetic on for every request — while the content bytes live in
    /// the record blob at this artifact's entity
    /// ([decision 0077](../../../docs/decisions/0077-supplied-content-lives-in-the-record-blob.md)).
    /// The split follows from what each is for: a generating set is projected into row space once
    /// per generation and intersected per request, and a form that had to be decompressed to be
    /// tested would pay that cost on every artifact of every viewport.
    pub contents: Vec<ContentSet>,
    /// What this artifact exists as an attachment to, resolved at publication.
    ///
    /// **A visibility term, not a navigation aid.** An artifact carrying one is tested on its
    /// target's disposition and reachability as well as on its own conjuncts, on **every** route —
    /// see [`Attachment`].
    pub attached_to: Option<Attachment>,
    /// This artifact's parents in its layer's hierarchy — ascending by `(level, ordinal)`,
    /// deduplicated; empty at a root, one on a tree, several on a `dag` layer (decision 0117).
    ///
    /// **The opposite of an attachment in the one way that matters**: it is *not* a visibility term.
    /// A node's verdict is its own masked count against its own criterion, with no input from its
    /// lineage and none from the viewport
    /// ([decision 0080](../../../docs/decisions/0080-the-frontier-is-a-per-artifact-test.md)) — so
    /// a parent that is suppressed, deleted or below its bar withholds itself and nothing else. What
    /// the edge decides is only which of two artifacts that *both* passed is the one drawn.
    ///
    /// A parent whose ordinal is now a hole is a parent this node does not have, and a node whose
    /// every parent is one is a root, which serves it: correct, since it passed its own test, and
    /// the reason the fold does not have to rewrite these.
    ///
    /// **The level is carried because a layer's edges are one of two shapes.** A nested layer's run
    /// within one level, and the cut climbs them; a tiered layer's run between levels, and
    /// the cut does not — those are information about what contains what, not a ladder to coarsen
    /// along (owner ruling, 2026-08-18).
    pub parents: Vec<crate::wal::ParentRef>,
}

/// The resolved target of an attachment: the edge `annotation-representation.md` §2.4 names, with
/// the target's entity carried beside it.
///
/// **The entity is stored rather than re-derived, because it is what the predicate reads.** The
/// extra term an attached artifact carries is one `verdict` lookup — the same lookup the
/// predicate's first branch already performs on the artifact's own entity — and re-deriving it from
/// the target layer's reserved runs on every request would put the registry in a path that needs
/// nothing but an identifier. The address `(layer, level, ordinal)` travels with it because that is
/// the edge's identity, and traversal will read it.
///
/// **The target exists before the edge does** (§5.0.4). An edge names a position in a dense level,
/// so one written ahead of its target would name whatever later landed there; publication refuses
/// an unresolvable target rather than storing one. The target's layer must be one the attaching
/// layer declared in `depends_on`, which is what makes the layer-level refusal of a dangling
/// replacement sound: a dependency nobody declared is one nothing checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    /// The target's own entity — its address for the deny lane, and the identifier the extra
    /// predicate term is evaluated on.
    pub entity: EntityId,
}

/// One entry of an artifact's ranked `contents`, as the registry holds it: the generating set,
/// and the content it gates.
#[derive(Debug, Clone)]
pub struct ContentSet {
    /// The content values, positional to the layer's declared kinds.
    ///
    /// **`None` where this copy does not carry them**, which means *restored from a packed extent
    /// rather than replayed from the log*: the extent carries generating sets and not values,
    /// because values live in the record blob
    /// ([decision 0077](../../../docs/decisions/0077-supplied-content-lives-in-the-record-blob.md)).
    ///
    /// It is not an absence of content — the serving path reads the blob at the artifact's own
    /// entity when this is `None`, and the two copies are written by the same publication. What it
    /// means is *ask the blob*, and a blob that cannot answer **withholds the artifact**: served
    /// with its identity and its count and no description is the in-between state decision 0076
    /// forbids.
    pub values: Option<Vec<String>>,
    /// SHA-256 over the values ([`content_digest`]), carried in every copy including the one a
    /// packed extent restores, which holds no values: it is what a later record carrying this
    /// content is compared against (`ingest.md` §1.5; [`ArtifactStore::fill`]).
    pub digest: [u8; 32],
    /// Entity-space, canonical. **Empty means corpus-independent** — containment is vacuous and the
    /// content serves to everyone who reaches the layer — and that is a real declaration rather
    /// than a missing one: a layer whose kinds are all corpus-independent is refused a generating
    /// set at publish, so an empty set here cannot be an omission.
    pub generated_from: Bitmap,
    /// The set's **stored cardinality** (`ingest.md` §1.1): `generated_from`'s at publication,
    /// moved afterwards only by a page at this content's rank, and published beside the set's
    /// row-space operator at the tick so a containment test never reads a cardinality from one
    /// version of the set against an operator from another. Read by T2b.
    pub cardinality: u64,
}

/// The digest a content's values are stored with: SHA-256 over each value's UTF-8 bytes, each
/// preceded by its length as a `u32` LE, so that two value lists that concatenate to the same
/// bytes digest differently.
pub fn content_digest(values: &[String]) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(values.iter().map(|v| v.len() + 4).sum());
    for value in values {
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    sha256(&bytes)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

impl ArtifactRecord {
    /// The artifact's **declared** membership size: how many members it was published with,
    /// unmasked.
    ///
    /// **A predicate input and never a field.** The proportional criterion divides by it; nothing
    /// else may read it, and nothing serialises it. Deriving it from `members` rather than storing
    /// it separately is deliberate — a stored copy is a number someone can reach for, and this one
    /// cannot drift from the set it describes.
    pub fn declared_size(&self) -> u64 {
        self.members.cardinality()
    }
}

/// Every artifact of every layer, keyed by `(layer, level, ordinal)`.
///
/// **Ordinals are dense within a level**, so a level is a vector rather than a map: the address is
/// `entity − run.start` arithmetic, and an ordinal that names no artifact is a hole rather than a
/// lookup miss. A hole is a real state — an artifact whose publication is still in flight, or one
/// a fold has yet to remove — and it must answer *absent* rather than panic.
#[derive(Debug, Clone, Default)]
pub struct ArtifactStore {
    levels: BTreeMap<(String, u32), Vec<Option<ArtifactRecord>>>,
    /// Per `(layer, level)`, each ordinal's canonical shapes — the membership of a layer whose
    /// `shape` declares one, and `None` everywhere else.
    ///
    /// **Beside the records rather than inside them, on [`ArtifactRecords`]' own split**
    /// (`tessera_engine::artifacts`): the two halves are read at different cadences. A record is
    /// dereferenced on every verdict; a shape is read **once per level per publication**, by the
    /// pass that decomposes it and holds the decomposition, and never by a verdict at all. Keeping
    /// it out of the record keeps the type every serving path walks the same shape it was.
    ///
    /// **Written wherever a record is, in [`ArtifactStore::put`], so the two cannot come apart.**
    /// There is no route that sets one without the other, and a level's vectors are grown together;
    /// a shape for an ordinal with no record would be a membership rule for an artifact that does
    /// not exist, and a record with no shape on a shape layer is refused at publication.
    /// Nested for [`ArtifactStore::keys`]' reason: `shape_of` is asked once per artifact by
    /// `unpublished`, and a tuple key made every one of those a `String` allocation.
    shapes: BTreeMap<String, BTreeMap<u32, Vec<Option<ArtifactShapes>>>>,
    /// `(layer, level, key) → ordinal`. **An index, not a second copy of the truth**: it
    /// exists so a batch of ten thousand artifacts can be checked for duplicate keys in
    /// `O(n log n)` rather than rescanning the level per artifact, which is `O(n²)` and reachable
    /// at the sizes this stage publishes.
    /// Nested `layer → level → key` rather than a flat `(String, u32, String)` tuple, and that is
    /// the whole reason for the shape: a `BTreeMap<String, _>` answers a `&str`, so a lookup
    /// borrows where a tuple key forced two `String` allocations at every probe — on a path the
    /// serving side takes as well as the build.
    /// The third level is the artifact's view (`ingest.md` §1.5): a group-scoped layer's keys are
    /// unique per `(layer, view)` and an entity-scoped layer's sit under `None`, so the same key
    /// in two views resolves to two ordinals and neither can be reached from the other's view.
    keys: KeyIndex,
    /// Where the oldest surviving publication sits in the log — the bound rotation may not reclaim
    /// past. See [`ArtifactStore::oldest_wal_pos`].
    oldest_wal_pos: Option<u64>,
    /// Where the oldest **growth** not yet covered by a whole-level rewrite sits in the log — the
    /// second half of the same bound, held separately because it is released by a different event.
    ///
    /// A publication's records are free once the level's tail is packed and marked; a growth's are
    /// not, because it lands *below* the high-water the packer starts from and no append-only pack
    /// will ever reach it. Only the fold's whole rewrite does, so only
    /// [`ArtifactStore::mark_growth_packed`] clears this. See [`ArtifactStore::grow`].
    grown_wal_pos: Option<u64>,
    /// Per `(layer, level)`, the ordinal high-water already durable in a manifest. Everything at or
    /// above it lives only in the WAL, which is what the rotation pin holds the log for.
    published_through: BTreeMap<(String, u32), u32>,
    /// `target entity → the entities of the artifacts attached to it`.
    ///
    /// **The inverse of [`Attachment`], maintained here because the deny lane reads it.** Deleting
    /// an artifact deletes the artifacts depending on it
    /// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)),
    /// and the lane holds an entity rather than an address — so the alternative to this index is a
    /// scan of every level per deletion, on the one lane whose ack latency is a guarantee. It is an
    /// index and not a second copy of the truth: every entry is derived from a record's
    /// `attached_to`, added where the record enters and removed where it leaves.
    dependents: BTreeMap<EntityId, Vec<EntityId>>,
    /// Per `(layer, level)`, how many artifact writes have landed on it. **A derived structure is
    /// valid only for the version of the level it was derived from**: a cache that missed a bump
    /// would serve a level with its newest artifacts absent, which a viewer cannot tell from
    /// artifacts that failed their existence criterion.
    ///
    /// **Per level rather than one counter for the store**, which is what it was until the scale
    /// campaign measured the difference (`design/artifact-serving-at-scale.md` §8.1): a global
    /// counter makes one suppression, one growth or one publication *anywhere* invalidate every
    /// level's row form in every view — 138 s of rebuild at 10⁷ artifacts over 10⁹ rows, so under
    /// any read-write load the cache never survives to be used. Everything derived from a level
    /// reads that level's records and nothing else, so the level is the grain at which a
    /// derivation can go wrong.
    ///
    /// **Monotonic, and an entry is never removed.** A version that went backwards — by erasing a
    /// dropped layer's entry and starting again at zero when the name is re-registered — would
    /// make a cached form built over the *old* artifacts compare equal to the new level and be
    /// served. [`Self::remove_layer`] therefore bumps what it drops rather than forgetting it.
    versions: BTreeMap<(String, u32), u64>,
    /// Per `(layer, level)`, how many writes have moved its **edges**: the second version counter
    /// of `ingest.md` §1.5 and §4.1. A publication, a fill of a parent list, a retirement and a
    /// layer drop move it; a growth and a fill of any other part move [`Self::versions`] alone. The
    /// served lineage is keyed on this one, so a page of members joining never rebuilds the
    /// hierarchy, which at rung 3's 30,954 nodes is about half a second per page.
    ///
    /// In-process only: no derived structure filed under it survives a restart, so it starts at
    /// zero at open and is never seeded. Monotone in a running process, on [`Self::versions`]'
    /// argument.
    lineage_versions: BTreeMap<(String, u32), u64>,
    /// Where the oldest **fill** of a fixed part sits in the log — the third half of the rotation
    /// bound ([`Self::oldest_wal_pos`]), released when the fold has rewritten every level whole
    /// ([`Self::mark_growth_packed`]), because a filled record sits below its level's high-water
    /// as a grown one does and only the fold's rewrite carries a filled part into an extent.
    filled_wal_pos: Option<u64>,
    /// Where the oldest fill of a **content's values** sits in the log, released only once the
    /// content extent carrying them is named by a durable manifest
    /// ([`Self::mark_content_published`]). Held apart from `filled_wal_pos` because it is released
    /// by a different event: the fold rewrites membership extents and carries content extents
    /// forward unchanged, so a fold does not make filled values durable and must not release the
    /// record that holds them.
    content_wal_pos: Option<u64>,
    /// The artifacts whose content values were filled since their last content extent, as
    /// `(layer, level, ordinal)`. [`Self::unpublished_content`] writes them beside the level's
    /// tail, and [`Self::mark_content_published`] clears the set once the extent is durable.
    content_pending: std::collections::BTreeSet<(String, u32, u32)>,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace one artifact, and the shapes it declared if its layer declares a kind.
    /// Growing the level's vectors to fit is what makes a publication that arrives out of ordinal
    /// order land correctly.
    ///
    /// **One call sets both halves** — see [`ArtifactStore::shapes`] for why the shape is beside
    /// the record rather than in it, and why there is no route that writes one without the other.
    pub fn put(
        &mut self,
        layer: &str,
        level: u32,
        ordinal: u32,
        record: ArtifactRecord,
        shape: Option<ArtifactShapes>,
    ) {
        if let Some(key) = &record.key {
            self.keys
                .entry(layer.to_string())
                .or_default()
                .entry(level)
                .or_default()
                .entry(record.view.clone())
                .or_default()
                .insert(key.clone(), ordinal);
        }
        let edge = record
            .attached_to
            .as_ref()
            .map(|attachment| (attachment.entity, record.entity));
        let idx = ordinal as usize;
        let previous = {
            let slots = self.levels.entry((layer.to_string(), level)).or_default();
            if slots.len() <= idx {
                slots.resize(idx + 1, None);
            }
            // The slot's previous occupant, if any, takes its edge with it — replay applies the
            // same publication twice on a re-read prefix, and an index that accumulated a
            // duplicate would cascade one deletion into the same dependent twice.
            let previous = slots[idx].take();
            slots[idx] = Some(record);
            previous
        };
        if let Some(previous) = previous {
            self.forget_dependency(&previous);
        }
        if let Some((target, dependent)) = edge {
            let entry = self.dependents.entry(target).or_default();
            if !entry.contains(&dependent) {
                entry.push(dependent);
            }
        }
        let shapes = self
            .shapes
            .entry(layer.to_string())
            .or_default()
            .entry(level)
            .or_default();
        if shapes.len() <= idx {
            shapes.resize(idx + 1, None);
        }
        shapes[idx] = shape;
    }

    /// The shapes the artifact at `ordinal` declared, or `None` where it declared none — which is
    /// every artifact of every layer whose membership is not a shape.
    pub fn shape_of(&self, layer: &str, level: u32, ordinal: u32) -> Option<&ArtifactShapes> {
        self.shapes
            .get(layer)
            .and_then(|levels| levels.get(&level))
            .and_then(|shapes| shapes.get(ordinal as usize))
            .and_then(Option::as_ref)
    }

    /// Drop one record's outgoing dependency edge from the index.
    fn forget_dependency(&mut self, record: &ArtifactRecord) {
        let Some(attachment) = &record.attached_to else {
            return;
        };
        if let Some(entry) = self.dependents.get_mut(&attachment.entity) {
            entry.retain(|dependent| *dependent != record.entity);
            if entry.is_empty() {
                self.dependents.remove(&attachment.entity);
            }
        }
    }

    /// Every artifact that depends, directly or transitively, on one of `roots` — the deletions
    /// rule 1 of [decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)
    /// adds to a caller's own.
    ///
    /// **Never contains a root**, so a caller can submit these beside the deletions it was given
    /// without deleting anything twice. The walk terminates without a visited set of its own on
    /// the dedup below plus the acyclicity the registry enforces — a layer is registered after
    /// every layer it names in `depends_on`, so no edge can point back up the chain — and the
    /// dedup is what makes it safe anyway: an entity already collected is never expanded again.
    ///
    /// An entity that is not an artifact — a point, a layer — has no dependents and answers
    /// empty, which is what lets the deny lane ask this of every deletion it carries.
    pub fn cascade_from(&self, roots: &[EntityId]) -> Vec<EntityId> {
        let mut collected: std::collections::BTreeSet<EntityId> = std::collections::BTreeSet::new();
        let mut frontier: Vec<EntityId> = roots.to_vec();
        while let Some(entity) = frontier.pop() {
            let Some(dependents) = self.dependents.get(&entity) else {
                continue;
            };
            for dependent in dependents {
                if roots.contains(dependent) || !collected.insert(*dependent) {
                    continue;
                }
                frontier.push(*dependent);
            }
        }
        collected.into_iter().collect()
    }

    /// The next ordinal a publication into this level would claim.
    ///
    /// **Derived from the level's extent, never stored**, so replay reconstructs it exactly rather
    /// than needing a durable cursor. A hole left by a removal is *not* reused: the entity behind
    /// it is not reclaimed either (decision 0072 is settled and unbuilt), and handing the ordinal
    /// back while the entity stays spent is how the two would come to disagree.
    pub fn next_ordinal(&self, layer: &str, level: u32) -> u32 {
        self.levels
            .get(&(layer.to_string(), level))
            .map(|slots| slots.len() as u32)
            .unwrap_or(0)
    }

    /// The ordinal a caller's own key names in this level of this view, if any.
    ///
    /// **The view is part of the address** (`ingest.md` §1.5): a group-scoped layer's key is
    /// unique per `(layer, view)`, so a probe under one view never reaches another's artifact —
    /// which is what keeps an edge inside its view and lets one key name two artifacts.
    pub fn ordinal_of_key(
        &self,
        layer: &str,
        level: u32,
        view: Option<&str>,
        key: &str,
    ) -> Option<u32> {
        // Probed without allocating: the map is keyed by `Option<String>` and `Option<&str>`
        // does not borrow as one, so the view level is found by comparing what it holds — a
        // level holds one bucket per view, which is a handful, and the nested shape exists
        // precisely so a lookup allocates nothing (see the field's note).
        self.keys
            .get(layer)?
            .get(&level)?
            .iter()
            .find(|(held, _)| held.as_deref() == view)?
            .1
            .get(key)
            .copied()
    }

    /// The views, other than `view`, whose set of this level holds `key` — what a cross-view edge
    /// is reported with (`views.md` §3.5). Empty on an entity-scoped layer and wherever the key
    /// is held nowhere else.
    pub fn views_holding_key(
        &self,
        layer: &str,
        level: u32,
        view: Option<&str>,
        key: &str,
    ) -> Vec<String> {
        let Some(by_view) = self.keys.get(layer).and_then(|held| held.get(&level)) else {
            return Vec::new();
        };
        by_view
            .iter()
            .filter(|(held, _)| held.as_deref() != view)
            .filter(|(_, keys)| keys.contains_key(key))
            .map(|(held, _)| {
                held.as_deref()
                    .unwrap_or("(the layer's one set)")
                    .to_string()
            })
            .collect()
    }

    /// The log position of the oldest surviving publication, or `None` if none survives.
    ///
    /// **Rotation may not reclaim past this, and today that pins the log from the first
    /// publication onwards.** Nothing but the WAL carries a membership: a manifest carries the
    /// registry, segments carry rows and postings, and neither carries a Roaring bitmap of who
    /// belongs to a cluster. So reclaiming a member holding an `ArtifactPublish` destroys the only
    /// copy — a served cluster that comes back from a restart with no members, which the existence
    /// criterion then renders as *absent* rather than as an error.
    ///
    /// ⊘ **This is an open question answered fail-closed, not a design.** Where membership lives on
    /// disk is the owner's decision and the one layout question decisions 0074–0081 left open;
    /// until it lands, an unbounded log is the safe direction and a visible one. It is the same
    /// posture `oldest_wal_pos`'s unknown-position arm takes in the ingest buffer, and for the same
    /// reason: a sequence that grows is noticed, a record that vanishes is not.
    ///
    /// **Four bounds, one answer.** The oldest unpacked publication, the oldest growth and the
    /// oldest fill no whole rewrite has covered, and the oldest content fill no content extent
    /// carries — the minimum, because rotation takes a single bound and each is released by a
    /// different event (`grown_wal_pos`, `filled_wal_pos`, `content_wal_pos`).
    pub fn oldest_wal_pos(&self) -> Option<u64> {
        [
            self.oldest_wal_pos,
            self.grown_wal_pos,
            self.filled_wal_pos,
            self.content_wal_pos,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Applies a durable publication, a durable growth or a durable fill — the **three** paths by
    /// which artifact state enters, taken by both the live write path and replay.
    ///
    /// `position` is where the record sits in the log. **Replay applies the recorded ordinals and
    /// entities rather than re-deriving them**, on [`crate::LayerRegistry::apply`]'s contract: a
    /// re-derived ordinal would move an artifact under every suppression naming it.
    ///
    /// Records other than these three are ignored, so a caller can hand the whole replay stream to
    /// this and to the registry alike.
    ///
    /// Returns how many memberships **did not decode** — always zero in any healthy log. The count
    /// is returned rather than logged because this crate carries no tracing dependency by design
    /// (see `check-layers.sh`), and a silent skip is the one outcome this must not have: an
    /// artifact whose members were lost is served as absent, which is indistinguishable from one
    /// that never cleared its criterion.
    #[must_use]
    pub fn apply(&mut self, record: &crate::wal::WalRecord, position: u64) -> usize {
        match record {
            crate::wal::WalRecord::ArtifactPublish {
                layer,
                level,
                artifacts,
                ..
            } => self.apply_publish(layer, *level, artifacts, position),
            crate::wal::WalRecord::ArtifactGrow {
                layer,
                level,
                growth,
            } => self.apply_growth(layer, *level, growth, position),
            crate::wal::WalRecord::ArtifactFill {
                layer,
                level,
                ordinal,
                part,
            } => self.apply_fill(layer, *level, *ordinal, part, position),
            _ => 0,
        }
    }

    fn apply_fill(
        &mut self,
        layer: &str,
        level: u32,
        ordinal: u32,
        part: &crate::wal::ArtifactPart,
        position: u64,
    ) -> usize {
        let refused = match self.fill(layer, level, ordinal, part) {
            FillOutcome::Filled | FillOutcome::Identical => 0,
            // A record the log carries that disagrees with the record it names, or that does not
            // decode, is damage on the publication's argument: it was compared before it was
            // appended, so a live disagreement cannot reach here, and a replayed one means the
            // durable prefix and the seeded extents describe two different artifacts.
            FillOutcome::Differs | FillOutcome::NoRecord | FillOutcome::Undecodable => 1,
        };
        // Held until the fold's whole rewrite on the growth's argument: a filled record sits
        // below the level's high-water, which the tail pack never rewrites. Content values are
        // held further, until the content extent carrying them is durable.
        self.filled_wal_pos = Some(match self.filled_wal_pos {
            Some(existing) => existing.min(position),
            None => position,
        });
        if refused == 0
            && matches!(part, crate::wal::ArtifactPart::Content { .. })
            && self
                .content_pending
                .contains(&(layer.to_string(), level, ordinal))
        {
            self.content_wal_pos = Some(match self.content_wal_pos {
                Some(existing) => existing.min(position),
                None => position,
            });
        }
        // Unconditional, on the growth's argument: the refusal has been counted for a caller who
        // will act on it, and one rebuild of one level is the cheaper mistake.
        self.bump(layer, level);
        refused
    }

    /// **The one way a fixed part is filled**, taken by every route through the durable record
    /// above and by no other caller (`ingest.md` §1.5).
    ///
    /// The fill rule, applied to the record the ordinal names: an absent part is filled, a present
    /// identical part is [`FillOutcome::Identical`] and changes nothing, a present differing part
    /// is [`FillOutcome::Differs`] and changes nothing. Identity is by the stored digest for a
    /// content and a shape, by the resolved references for parents and an attachment. The live
    /// path makes the same comparison before it appends, so on that path this answers `Filled` or
    /// `Identical`; the other answers are replay's, where they count as damage.
    ///
    /// **A content's values are stored beside its digest and the record is queued for the next
    /// content extent** ([`Self::unpublished_content`]). An identical content on a record restored
    /// from an extent takes the values back into memory and is not queued: its row is already in
    /// a durable extent, and the record stack holds one row per entity.
    ///
    /// **The log's copy of a shape is rebuilt through [`ArtifactShapes::new`]** and its digest
    /// compared with the recorded one: postcard restores the struct whole, so the stored digest is
    /// trusted only once it has been recomputed from the bytes beside it.
    ///
    /// **An ordinal naming no record fills nothing**, on [`Self::grow`]'s argument: creating a
    /// record here would resurrect an artifact a fold retired.
    ///
    /// The level's version moves in the caller ([`Self::apply_fill`]); the lineage version moves
    /// here, for a parent list alone, since that is the one part the served hierarchy reads.
    pub fn fill(
        &mut self,
        layer: &str,
        level: u32,
        ordinal: u32,
        part: &crate::wal::ArtifactPart,
    ) -> FillOutcome {
        use crate::wal::ArtifactPart;
        let key = (layer.to_string(), level);
        let Some(record) = self
            .levels
            .get_mut(&key)
            .and_then(|slots| slots.get_mut(ordinal as usize))
            .and_then(Option::as_mut)
        else {
            return FillOutcome::NoRecord;
        };
        match part {
            ArtifactPart::Parents(parents) => {
                if !record.parents.is_empty() {
                    return if record.parents == *parents {
                        FillOutcome::Identical
                    } else {
                        FillOutcome::Differs
                    };
                }
                record.parents = parents.clone();
                *self.lineage_versions.entry(key).or_insert(0) += 1;
                FillOutcome::Filled
            }
            ArtifactPart::AttachedTo(attachment) => {
                let wanted = Attachment {
                    layer: attachment.layer.clone(),
                    level: attachment.level,
                    ordinal: attachment.ordinal,
                    entity: attachment.entity,
                };
                if let Some(held) = &record.attached_to {
                    return if *held == wanted {
                        FillOutcome::Identical
                    } else {
                        FillOutcome::Differs
                    };
                }
                let dependent = record.entity;
                record.attached_to = Some(wanted);
                let entry = self.dependents.entry(attachment.entity).or_default();
                if !entry.contains(&dependent) {
                    entry.push(dependent);
                }
                FillOutcome::Filled
            }
            ArtifactPart::Shape(shape) => {
                let Some(rebuilt) = ArtifactShapes::new(shape.by_view.clone()) else {
                    return FillOutcome::Undecodable;
                };
                if rebuilt.digest() != shape.digest() {
                    return FillOutcome::Undecodable;
                }
                let shapes = self
                    .shapes
                    .entry(layer.to_string())
                    .or_default()
                    .entry(level)
                    .or_default();
                if shapes.len() <= ordinal as usize {
                    shapes.resize(ordinal as usize + 1, None);
                }
                match &shapes[ordinal as usize] {
                    Some(held) if held.digest() == rebuilt.digest() => FillOutcome::Identical,
                    Some(_) => FillOutcome::Differs,
                    None => {
                        shapes[ordinal as usize] = Some(rebuilt);
                        FillOutcome::Filled
                    }
                }
            }
            ArtifactPart::Content {
                rank,
                values,
                digest,
            } => {
                if content_digest(values) != *digest {
                    return FillOutcome::Undecodable;
                }
                let rank = *rank as usize;
                if let Some(held) = record.contents.get_mut(rank) {
                    if held.digest != *digest {
                        return FillOutcome::Differs;
                    }
                    if held.values.is_none() {
                        held.values = Some(values.clone());
                    }
                    return FillOutcome::Identical;
                }
                if rank != record.contents.len() {
                    // Ranks are positions in a list, so a fill past the next one names a content
                    // between two that does not exist. The live path refuses this before the
                    // append; at replay it is damage.
                    return FillOutcome::Undecodable;
                }
                record.contents.push(ContentSet {
                    values: Some(values.clone()),
                    digest: *digest,
                    generated_from: Bitmap::new(),
                    cardinality: 0,
                });
                self.content_pending
                    .insert((layer.to_string(), level, ordinal));
                FillOutcome::Filled
            }
        }
    }

    /// See [`Self::lineage_versions`]: what a lineage derived from this level is valid for.
    pub fn lineage_version(&self, layer: &str, level: u32) -> u64 {
        self.lineage_versions
            .get(&(layer.to_string(), level))
            .copied()
            .unwrap_or(0)
    }

    /// Whether the artifact at `ordinal` has a row in a durable content extent, or is in a
    /// manifest whose content extent would already hold one if it had content: **an artifact
    /// packed at or below the level's high-water with content it was packed with.** A further
    /// content rank on such an artifact cannot be written, because the record stack holds one row
    /// per entity and reads the first layer that has it (`ingest.md` §1.4 gives the per-column
    /// read to track T3). The registry refuses the fill on this answer.
    pub fn content_row_is_packed(&self, layer: &str, level: u32, ordinal: u32) -> bool {
        let through = self
            .published_through
            .get(&(layer.to_string(), level))
            .copied()
            .unwrap_or(0);
        ordinal < through
            && self
                .get(layer, level, ordinal)
                .is_some_and(|record| !record.contents.is_empty())
            && !self
                .content_pending
                .contains(&(layer.to_string(), level, ordinal))
    }

    /// Record that the content extent naming every pending content fill is durable, releasing the
    /// log from the records that carried the values. Called beside [`Self::mark_published`], after
    /// the manifest naming the extent is durable, and from nowhere else.
    pub fn mark_content_published(&mut self) {
        self.content_pending.clear();
        self.content_wal_pos = None;
    }

    fn apply_publish(
        &mut self,
        layer: &str,
        level: u32,
        artifacts: &[crate::wal::PublishedArtifact],
        position: u64,
    ) -> usize {
        let mut refused = 0;
        for published in artifacts {
            // Damage is a refusal, not an empty membership — see `deserialise_members`. Skipping
            // leaves a hole, which answers *absent*; the alternative decodes a corrupt record to a
            // legitimately emptied artifact and serves it.
            let Some(members) = deserialise_members(&published.members) else {
                refused += 1;
                continue;
            };
            // Every generating set decodes or the artifact is refused whole. A content whose set
            // decoded short is one a viewer may be served without containing what it was generated
            // from — the disclosure containment exists to prevent — so the failure may not be
            // localised to the one content and skipped. The digest is recomputed from the values
            // the record carries and checked against the stored one, so the log is self-checking
            // and a record whose two halves disagree is refused as damage.
            let sets: Option<Vec<ContentSet>> = published
                .contents
                .iter()
                .map(|v| {
                    let digest = content_digest(&v.values);
                    if digest != v.digest {
                        return None;
                    }
                    deserialise_members(&v.generated_from).map(|generated_from| ContentSet {
                        values: Some(v.values.clone()),
                        digest,
                        generated_from,
                        cardinality: v.cardinality,
                    })
                })
                .collect();
            let Some(contents) = sets else {
                refused += 1;
                continue;
            };
            let shape = published.shape.clone();
            self.put(
                layer,
                level,
                published.ordinal,
                ArtifactRecord {
                    entity: published.entity,
                    key: published.key.clone(),
                    view: published.view.clone(),
                    members: Members::owned(members),
                    contents,
                    attached_to: published.attached_to.clone().map(|a| Attachment {
                        layer: a.layer,
                        level: a.level,
                        ordinal: a.ordinal,
                        entity: a.entity,
                    }),
                    parents: published.parents.clone(),
                },
                shape,
            );
        }
        self.oldest_wal_pos = Some(match self.oldest_wal_pos {
            Some(existing) => existing.min(position),
            None => position,
        });
        self.bump(layer, level);
        self.bump_lineage(layer, level);
        refused
    }

    fn apply_growth(
        &mut self,
        layer: &str,
        level: u32,
        growth: &[crate::wal::MembershipGrowth],
        position: u64,
    ) -> usize {
        let mut refused = 0;
        for grown in growth {
            // A leave, or a delta to a generating set, has no apply path until T2b (`ingest.md`
            // §1.1, §8). Refused and counted on the fill's argument: the replay refuses to open
            // before it reaches here (`crate::wal::unbuilt_track`), and applying the joins of
            // such a record while passing over its leaves would serve a set nobody declared.
            if !grown.leaving.is_empty()
                || matches!(grown.set, crate::wal::GrownSet::GeneratingSet { .. })
            {
                refused += 1;
                continue;
            }
            // Damage is a refusal, not an empty delta, on the publication's argument: a growth
            // decoded short is an acked join that silently did not happen, and the artifact then
            // serves the count it had before — which nothing distinguishes from a criterion it
            // failed to clear.
            let Some(joining) = deserialise_members(&grown.joining) else {
                refused += 1;
                continue;
            };
            self.grow(layer, level, grown.ordinal, &joining);
        }
        // **Held from here until a whole rewrite covers it, and `mark_published` does not release
        // it.** The append-only packer starts at the level's high-water and a grown record sits
        // below it, so this record is the only durable copy of the join until the fold.
        self.grown_wal_pos = Some(match self.grown_wal_pos {
            Some(existing) => existing.min(position),
            None => position,
        });
        // **Unconditional, including where every delta was refused.** A refusal has already
        // returned a count to a caller who will act on it; spending one rebuild of one level to
        // keep the bump off a decision about damaged input is the cheaper of the two mistakes.
        self.bump(layer, level);
        refused
    }

    /// **The one way a membership grows**, taken by every route through the durable record above
    /// and by no other caller.
    ///
    /// ## How this stands to the two removal rules
    ///
    /// It does not touch them, and that is the whole of its relationship to them. Write-path §5.4's
    /// rules govern *retirement* — a suppression retires only on unsuppress and never touches a
    /// stored structure (Rule S); a deletion retires only at the compaction fold that executes it
    /// (Rule F) — and the hazard they exist against is a second route by which a bit **leaves** a
    /// membership. This adds bits. A member added here is retired by exactly the routes every other
    /// member is retired by, having no separate provenance once it is in the set: [`Self::retire`]
    /// and [`Self::repack_all`] cannot tell it from a declared one, which is the property that keeps
    /// growth from becoming a third removal rule by the back door.
    ///
    /// What it must not become is a second *entry* route with its own rules, which is why it is one
    /// method and not one per caller: an unsuppress that restored a member by re-growing it, say,
    /// would give a suppression a retirement route through the growth path. A suppression's bit
    /// never leaves, so it never needs putting back.
    ///
    /// **An ordinal naming no record adds nothing.** That is a hole — an artifact a fold retired —
    /// and creating a record here would resurrect it under an identity a caller's `tessera_id`
    /// still names. Nothing is counted for it either: the state is reachable and legitimate (a
    /// growth still in the log for an artifact this fold removed), so alarming on it would alarm on
    /// every restart after such a fold.
    fn grow(&mut self, layer: &str, level: u32, ordinal: u32, joining: &Bitmap) {
        let Some(record) = self
            .levels
            .get_mut(&(layer.to_string(), level))
            .and_then(|slots| slots.get_mut(ordinal as usize))
            .and_then(Option::as_mut)
        else {
            return;
        };
        record.members.to_mut().or_inplace(joining);
    }

    /// Replace one artifact's membership with the **same** membership held somewhere else — the
    /// build's route from a heap bitmap to a view over the extent it has just written
    /// ([`Members::mapped`]).
    ///
    /// **It refuses a membership that is not equal to the one it replaces**, by cardinality, which
    /// a Roaring container answers from its own header and so costs the containers and not the
    /// members. That check is the whole of what stands between a mis-sliced blob and an artifact
    /// whose masked count is low for every viewer — the state the existence criterion renders as
    /// absent with nothing anywhere to notice. `false` leaves the record exactly as it was.
    ///
    /// Nothing about what the artifact *is* changes: the same bits, addressed through a mapping.
    pub fn rehouse_members(
        &mut self,
        layer: &str,
        level: u32,
        ordinal: u32,
        members: Members,
    ) -> bool {
        let Some(record) = self
            .levels
            .get_mut(&(layer.to_string(), level))
            .and_then(|slots| slots.get_mut(ordinal as usize))
            .and_then(Option::as_mut)
        else {
            return false;
        };
        if members.cardinality() != record.members.cardinality() {
            return false;
        }
        record.members = members;
        true
    }

    pub fn get(&self, layer: &str, level: u32, ordinal: u32) -> Option<&ArtifactRecord> {
        self.levels
            .get(&(layer.to_string(), level))
            .and_then(|slots| slots.get(ordinal as usize))
            .and_then(Option::as_ref)
    }

    /// Every artifact of one level, with its ordinal. Holes are skipped.
    pub fn level(&self, layer: &str, level: u32) -> impl Iterator<Item = (u32, &ArtifactRecord)> {
        self.levels
            .get(&(layer.to_string(), level))
            .into_iter()
            .flat_map(|slots| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| slot.as_ref().map(|r| (i as u32, r)))
            })
    }

    /// The artifacts of one level a given view is **drawn** (`views.md` §3.5): every record of an
    /// entity-scoped layer, whose one set is drawn on every view it names, and on a group-scoped
    /// layer only the records belonging to this view.
    ///
    /// **`view` is the view's own key**, not its path: a group's several layouts over one key set
    /// draw the same artifact in each, which is what the build's per-view pass reads too.
    ///
    /// A group-scoped artifact drawn in every view would be a real, wrong artifact in the others
    /// — a Q1 cluster's frame and count on Q2's map — so the filter is here, where every form is
    /// projected, rather than at each caller.
    pub fn level_in_view<'a>(
        &'a self,
        layer: &str,
        level: u32,
        view: &'a str,
    ) -> impl Iterator<Item = (u32, &'a ArtifactRecord)> {
        self.level(layer, level)
            .filter(move |(_, record)| record.view.as_deref().is_none_or(|own| own == view))
    }

    /// Is the artifact at `ordinal` drawn in `view` — every record of an entity-scoped layer,
    /// and on a group-scoped one only its own view's ([`Self::level_in_view`], `views.md` §3.5)?
    /// A hole is drawn nowhere.
    pub fn drawn_in_view(&self, layer: &str, level: u32, ordinal: u32, view: &str) -> bool {
        self.get(layer, level, ordinal)
            .is_some_and(|record| record.view.as_deref().is_none_or(|own| own == view))
    }

    /// Every artifact of every level of one layer, as `(level, ordinal, record)`.
    pub fn layer<'a>(
        &'a self,
        layer: &'a str,
    ) -> impl Iterator<Item = (u32, u32, &'a ArtifactRecord)> + 'a {
        self.levels
            .range((layer.to_string(), 0)..)
            .take_while(move |((l, _), _)| l == layer)
            .flat_map(|((_, level), slots)| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(move |(i, slot)| slot.as_ref().map(|r| (*level, i as u32, r)))
            })
    }

    /// Drop every artifact of a layer — what a layer drop leaves behind otherwise.
    ///
    /// **The log pin is not lowered with them.** A rotation that reclaimed back to where this
    /// layer's publications sat would also reclaim every *other* layer's records in between, and
    /// the pin is a single bound rather than a set. Holding it costs a longer log; recomputing it
    /// wrongly costs a membership.
    pub fn remove_layer(&mut self, layer: &str) {
        // Rebuilt from what survives rather than patched from what left: a dropped layer is both
        // ends of an edge — its artifacts' own outgoing edges, and the edges of layers that
        // attached into it — and one full pass over the remaining records is simpler to audit than
        // two removals whose union has to be argued. A layer drop is rare and never on a request
        // path.
        //
        // **Its levels' versions move before the levels do**, and the entries stay behind: see
        // [`Self::versions`] on why a dropped level's version may not be forgotten.
        let dropped: Vec<u32> = self
            .levels
            .keys()
            .filter(|(l, _)| l == layer)
            .map(|(_, level)| *level)
            .collect();
        for level in dropped {
            self.bump(layer, level);
            self.bump_lineage(layer, level);
        }
        self.levels.retain(|(l, _), _| l != layer);
        self.keys.remove(layer);
        self.content_pending.retain(|(l, _, _)| l != layer);
        self.dependents.clear();
        let edges: Vec<(EntityId, EntityId)> = self
            .levels
            .values()
            .flatten()
            .flatten()
            .filter_map(|record| {
                record
                    .attached_to
                    .as_ref()
                    .map(|attachment| (attachment.entity, record.entity))
            })
            .collect();
        for (target, dependent) in edges {
            self.dependents.entry(target).or_default().push(dependent);
        }
    }

    /// See [`Self::versions`]: what a structure derived from this level is valid for.
    ///
    /// **A level nothing has ever written to is version 0**, and so is a caller's first read of
    /// it — which is right rather than a coincidence: there are no records, so the empty
    /// derivation a cache would hold is the correct one, and the first write moves it off zero.
    pub fn level_version(&self, layer: &str, level: u32) -> u64 {
        self.versions
            .get(&(layer.to_string(), level))
            .copied()
            .unwrap_or(0)
    }

    /// Move one level's version — called by every route that changes what a level's records say.
    fn bump(&mut self, layer: &str, level: u32) {
        *self.versions.entry((layer.to_string(), level)).or_insert(0) += 1;
    }

    /// Move one level's lineage version — called by the routes that change what a level's edges
    /// say ([`Self::lineage_versions`]); a parent fill moves it inside [`Self::fill`].
    fn bump_lineage(&mut self, layer: &str, level: u32) {
        *self
            .lineage_versions
            .entry((layer.to_string(), level))
            .or_insert(0) += 1;
    }

    /// Set one level's version to what the manifest that published it recorded. **Open only.**
    ///
    /// **This is the one route that may move a version backwards, and it is safe for exactly one
    /// reason: nothing has been derived yet.** [`Self::versions`] is monotone in a running process
    /// because a version that went backwards would make a cached form built over the *old*
    /// artifacts compare equal to the new level. At open there is no such form — the store is being
    /// built. Without it a restart starts every level at zero however many publications the
    /// manifest carries, so no coordinate written before the restart could be compared with one
    /// after it, and every derived structure the prefix holds would be rejected on every open.
    ///
    /// Called after the level's records are seeded and **before** the WAL is replayed. Seeding
    /// itself does not move a version — [`Self::seed`] goes through [`Self::put`], which writes a
    /// slot and counts nothing — but replay does, through `apply`, so a record the log carries past
    /// the manifest moves the version off the published value. That is exactly the signal a reader
    /// deciding whether to adopt a derived structure needs (`manifest::ContainmentExtent`).
    pub fn seed_level_version(&mut self, layer: &str, level: u32, version: u64) {
        self.versions.insert((layer.to_string(), level), version);
    }

    /// Every level [`Self::retire`] would move, given the same `retired` set: the read-only twin
    /// of its `changed`, over the same predicate ([`record_moved_by`]), so the two agree level for
    /// level.
    ///
    /// **Why a publication needs it.** A fold writes its manifest before it retires, because the
    /// retirement is not reversible and a manifest that would not commit must leave it undone. At
    /// that moment this store reports a level's pre-retirement version while the records the
    /// manifest names are the post-retirement ones. For a level reported here the retirement will
    /// move the version by exactly one, and that is the version the fold stamps the level's
    /// derived structures with: see `write.rs`'s `artifact_coordinates`.
    pub fn levels_moved_by(&self, retired: &Bitmap) -> Vec<(String, u32)> {
        if retired.is_empty() {
            return Vec::new();
        }
        let mut moved = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let touched = slots
                .iter()
                .flatten()
                .any(|record| record_moved_by(record, retired));
            if touched {
                moved.push((layer.clone(), *level));
            }
        }
        moved
    }

    /// Every level this store holds a version for, as `(layer, level, version)` — what a
    /// publication records in `manifest::SegmentsManifest::level_versions`.
    pub fn level_versions(&self) -> impl Iterator<Item = (&str, u32, u64)> {
        self.versions
            .iter()
            .map(|((layer, level), version)| (layer.as_str(), *level, *version))
    }

    /// Every level's artifacts that are **not yet in a manifest**, as
    /// `(layer, level, ordinal_lo, blobs)` ready to pack — see [`encode_record`].
    ///
    /// **A level with a hole in its unpublished range is skipped whole and reported**, rather than
    /// packed around: an extent addresses `[ordinal_lo, ordinal_lo + count)` densely, so a hole
    /// would shift every later artifact's identity by one. A hole here means a publication landed
    /// out of order, which nothing does today.
    pub fn unpublished(&self) -> (Vec<PendingExtent>, Vec<(String, u32)>) {
        let (ranges, skipped) = self.pending_ranges();
        let ready = ranges
            .into_iter()
            .map(|(layer, level, ordinal_lo, count)| {
                let blobs: Vec<Vec<u8>> = self
                    .encode_pending(&layer, level, ordinal_lo, count)
                    .map(|blob| blob.expect("pending_ranges returned a dense range"))
                    .collect();
                (layer, level, ordinal_lo, blobs)
            })
            .collect();
        (ready, skipped)
    }

    /// The same levels [`Self::unpublished`] answers, as **ranges rather than blobs**:
    /// `(layer, level, ordinal_lo, count)`, and the levels skipped for a hole.
    ///
    /// **What a caller that means to stream them asks instead**, [`Self::encode_at`] being the
    /// other half. A build publishes every level of a corpus in one pass, so encoding them all
    /// before the first is written holds the whole corpus's memberships a second time, beside the
    /// bitmaps they came from; a level's ordinal range is enough to open the extent and take them
    /// one at a time. The online publication materialises instead, and not from preference: it
    /// reads this store under a lock it may not hold across an fsync.
    pub fn pending_ranges(&self) -> (Vec<PendingRange>, Vec<(String, u32)>) {
        let mut ready = Vec::new();
        let mut skipped = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let from = *self
                .published_through
                .get(&(layer.clone(), *level))
                .unwrap_or(&0) as usize;
            if from >= slots.len() {
                continue;
            }
            if slots[from..].iter().any(Option::is_none) {
                skipped.push((layer.clone(), *level));
                continue;
            }
            ready.push((
                layer.clone(),
                *level,
                from as u32,
                (slots.len() - from) as u32,
            ));
        }
        (ready, skipped)
    }

    /// The blobs of one pending range, **encoded as they are taken** — the other half of
    /// [`Self::pending_ranges`].
    ///
    /// An item is `None` where the level holds no record at that ordinal. Inside a range
    /// `pending_ranges` returned that is a bug rather than a hole: it reports a level with a hole
    /// as skipped and never as a range.
    ///
    /// The level is looked up once, not once an artifact: the store is keyed by an owned
    /// `(String, u32)`, so a lookup per ordinal would be a `String` allocation per artifact on the
    /// one path that walks every artifact of the corpus.
    pub fn encode_pending<'a>(
        &'a self,
        layer: &'a str,
        level: u32,
        ordinal_lo: u32,
        count: u32,
    ) -> impl Iterator<Item = Option<Vec<u8>>> + 'a {
        let slots = self.levels.get(&(layer.to_string(), level));
        (ordinal_lo..ordinal_lo + count).map(move |ordinal| {
            let record = slots?.get(ordinal as usize)?.as_ref()?;
            Some(encode_record(record, self.shape_of(layer, level, ordinal)))
        })
    }

    /// What this fold's deletions took away from every artifact that held one — the sweep behind
    /// the fold's report (`annotation-write-cycle.md` §4.2).
    ///
    /// **One `and_cardinality` per artifact, against a set the fold already holds.** No inverted
    /// index and no traversal: a superseded draft found the affected sets as a by-product of a pass
    /// that had to visit every (artifact, member) pair, which is the coupling this replaces.
    ///
    /// Two kinds of loss, and they are not the same event:
    ///
    /// - a **generating set** that lost a member describes content generated from a document that
    ///   no longer exists. The content and its set are withdrawn at this fold (decision 0135), and
    ///   the caller is owed the notice, because only they can decide whether the text still says
    ///   something true and re-declare it.
    /// - a **membership** that lost members is smaller than the caller declared it. Nothing is
    ///   wrong with it — every count was already correct at the ack — but a caller planning a
    ///   refresh wants to know which of their sets have drifted.
    ///
    /// **Unmasked counts, deliberately.** This is control-plane output behind the operator
    /// credential and outside the leak register's viewer scope
    /// ([decision 0024](../../../docs/decisions/0024-operator-credential-is-out-of-scope.md)); a
    /// viewer-facing route carrying these numbers would be C8.
    pub fn degradations(&self, retired: &Bitmap) -> Vec<Degradation> {
        let mut out = Vec::new();
        if retired.is_empty() {
            return out;
        }
        for ((layer, level), slots) in &self.levels {
            for (ordinal, slot) in slots.iter().enumerate() {
                let Some(record) = slot else { continue };
                // An artifact this fold retires outright is not *degraded* — it is gone, and its
                // own deletion is what the caller already knows about.
                if retired.contains(record.entity.raw() as u32) {
                    continue;
                }
                let members_lost = record.members.and_cardinality(retired);
                let mut contents_lost = Vec::new();
                for (rank, content) in record.contents.iter().enumerate() {
                    let lost = content.generated_from.and_cardinality(retired);
                    if lost > 0 {
                        contents_lost.push((rank as u32, lost));
                    }
                }
                if members_lost == 0 && contents_lost.is_empty() {
                    continue;
                }
                out.push(Degradation {
                    layer: layer.clone(),
                    level: *level,
                    ordinal: ordinal as u32,
                    key: record.key.clone(),
                    members_lost,
                    declared_members: record.declared_size(),
                    contents_lost,
                });
            }
        }
        out
    }

    /// How many Roaring **containers** every membership holds, summed across every level.
    ///
    /// **The one quantity the fold's artifact pass can be priced from**, and the reason it is
    /// counted rather than modelled: resident cost is ~90 B per container — not per artifact and
    /// not per member — so a membership of a hundred members costs what its *scatter* says and
    /// nothing a manifest holds can predict that
    /// ([the probe](../../../probes/2026-08-16-membership-residency/README.md)). A planner charging
    /// per declared member would overcharge a compact clustering by an order and refuse folds that
    /// fit.
    ///
    /// Counted over the **entity**-space form, which is what this store holds; the pass produces the
    /// row-space form, whose container count differs but tracks it, both being decided by how
    /// scattered a membership is in a Morton-ranked space. That is the approximation in this number,
    /// and it is the honest one available before the pass has run.
    ///
    /// `O(containers)`, on the fold's planning path only — tens of milliseconds at 10⁷ artifacts,
    /// against a fold measured in minutes.
    pub fn membership_containers(&self) -> u64 {
        self.levels
            .values()
            .flat_map(|slots| slots.iter().flatten())
            .map(|record| record.members.statistics().n_containers as u64)
            .sum()
    }

    /// Every level's artifacts **whole**, with `retired` dropped from each membership — what the
    /// fold repacks into the prefix it is publishing.
    ///
    /// **Not [`Self::unpublished`] with a wider range.** A fold publishes a new prefix and extent
    /// paths are prefix-relative, so every artifact has to be written again whether or not it was
    /// already durable somewhere else; the high-water this rewrite leaves behind is the level's own
    /// length. It is also not a copy: a fold retires entities, and a membership carried forward
    /// unchanged goes on counting members that no longer exist — in the very size the proportional
    /// existence criterion divides by.
    ///
    /// **`retired` is the fold's executed deletions and nothing else.** A *suppressed* member stays
    /// in the set: a suppression retires only on unsuppress and never touches a stored structure
    /// (Rule S), so dropping its bit here would give it a second retirement route, which is
    /// fail-open.
    ///
    /// A content whose generating set lost a retired member is **dropped whole**, content and set
    /// together ([`withdraw_content_of_retired_members`], decision 0135): containment is
    /// all-or-nothing and a set that lost a member fails it for every principal for ever; the
    /// caller re-declares.
    ///
    /// A level with a hole is reported rather than packed around, exactly as in
    /// [`Self::unpublished`] — but the consequence differs and the caller must not treat it as a
    /// skip: an extent this rewrite omits is a level the new prefix does not carry at all, whose
    /// artifacts come back registered, addressable and served as absent.
    pub fn repack_all(&self, retired: &Bitmap) -> Vec<PendingExtent> {
        let mut ready = Vec::new();
        for ((layer, level), slots) in &self.levels {
            if slots.is_empty() {
                continue;
            }
            let blobs: Vec<Vec<u8>> = slots
                .iter()
                .enumerate()
                .map(|(ordinal, slot)| {
                    // **An empty blob is a hole, and a hole is a real state** — an artifact this
                    // fold retired, or one whose publication is still in flight. It has to be
                    // *written* rather than packed around: an ordinal is identity, so closing a gap
                    // would hand every later artifact in the level the identity of its neighbour,
                    // and every `tessera_id` a caller holds would name the wrong cluster.
                    let Some(record) = slot else {
                        return Vec::new();
                    };
                    // **Rule F's artifact arm.** An artifact whose own entity this fold executed
                    // leaves the level here, in the same publication that retires the overlay entry
                    // hiding it — which is the ordering the rule is about, not reclamation. A
                    // deleted artifact has no rows and no postings, so compaction's derivation
                    // would otherwise call it executed *vacuously* at the first fold and retire the
                    // entry while its slot went on being served.
                    if retired.contains(record.entity.raw() as u32) {
                        return Vec::new();
                    }
                    // **The shape survives a fold unchanged**: it is geometry, not rows, so nothing
                    // the fold renumbers reaches it; the fold re-resolves the rows against it. What
                    // a deletion removes from such a layer is the *point*, which leaves the mask —
                    // the artifact's rule is untouched.
                    let shape = self.shape_of(layer, *level, ordinal as u32);
                    if retired.is_empty() {
                        return encode_record(record, shape);
                    }
                    let mut record = record.clone();
                    record.members.to_mut().andnot_inplace(retired);
                    withdraw_content_of_retired_members(&mut record, retired);
                    encode_record(&record, shape)
                })
                .collect();
            ready.push((layer.clone(), *level, 0, blobs));
        }
        ready
    }

    /// Drop every artifact this fold executed, and the entities it retired from what survives.
    ///
    /// The resident half of [`Self::repack_all`], applied after the flip for the reason
    /// [`Self::mark_published`] is: until the manifest naming the rewritten extents is durable, the
    /// old prefix is what a restart opens.
    ///
    /// **A retired artifact's slot becomes a hole rather than disappearing**, and its key
    /// goes with it — the key indexes an ordinal, and a key left behind would resolve a caller's
    /// republication onto the identity of the artifact this fold just removed.
    ///
    /// **Only the levels this actually changed have their version moved**, each by one, and those
    /// levels are returned. A fold walks every level and most folds touch few of them; bumping the
    /// ones it read would be the global grain [`Self::versions`] exists to escape, in the one place
    /// where it is least affordable. Which levels change is [`record_moved_by`]'s answer, the one
    /// [`Self::levels_moved_by`] gave the fold before its manifest was written.
    pub fn retire(&mut self, retired: &Bitmap) -> Vec<(String, u32)> {
        if retired.is_empty() {
            return Vec::new();
        }
        // Collected during the walk and applied after it: both indexes are fields beside `levels`,
        // which is borrowed mutably here.
        let mut gone: Vec<(EntityId, EntityId)> = Vec::new();
        let mut moved: Vec<(String, u32)> = Vec::new();
        for ((layer, level), slots) in self.levels.iter_mut() {
            let mut changed = false;
            for slot in slots.iter_mut() {
                let Some(record) = slot else { continue };
                if !record_moved_by(record, retired) {
                    continue;
                }
                changed = true;
                if retired.contains(record.entity.raw() as u32) {
                    if let Some(key) = &record.key {
                        if let Some(keys) = self
                            .keys
                            .get_mut(layer.as_str())
                            .and_then(|levels| levels.get_mut(level))
                            .and_then(|views| views.get_mut(&record.view))
                        {
                            keys.remove(key.as_str());
                        }
                    }
                    if let Some(attachment) = &record.attached_to {
                        gone.push((attachment.entity, record.entity));
                    }
                    *slot = None;
                    changed = true;
                    continue;
                }
                record.members.to_mut().andnot_inplace(retired);
                withdraw_content_of_retired_members(record, retired);
            }
            if changed {
                moved.push((layer.clone(), *level));
            }
        }
        for (layer, level) in &moved {
            self.bump(layer, *level);
            // A retired artifact's slot is a hole, and a hole has no edges: the lineage over the
            // level has moved.
            self.bump_lineage(layer, *level);
        }
        // A retired artifact's edge leaves with it, in both directions: its own outgoing edge here,
        // and any edges pointing *at* it — nothing can attach to an artifact that is gone, and a
        // stale entry would cascade a later deletion into an ordinal a republication now holds.
        for (target, dependent) in gone {
            if let Some(entry) = self.dependents.get_mut(&target) {
                entry.retain(|e| *e != dependent);
                if entry.is_empty() {
                    self.dependents.remove(&target);
                }
            }
        }
        self.dependents
            .retain(|target, _| !retired.contains(target.raw() as u32));
        moved
    }

    /// The supplied content of every artifact not yet in a manifest, as `(entity, tagged values)`.
    ///
    /// **Tags are `rank × kinds + kind`, positions in the artifact's own layer declaration** —
    /// the same idiom a point row's tags follow, where a tag is a position in the manifest's
    /// declared scalars. Artifact rows and point rows therefore share one store and one reader
    /// while each reads its tags against its own declaration, which is safe because the two never
    /// share an entity: the allocator issues artifact ids downward from the ceiling and point ids
    /// upward from zero, so which declaration governs a row is a range check on its entity.
    ///
    /// Every content of one artifact carries a value for every declared kind — refused at
    /// publication otherwise — so the stride is the same for all of them and is recoverable from
    /// the layer's declaration alone.
    ///
    /// **A record whose content was filled since its last content extent is written beside the
    /// tail** ([`Self::content_pending`]), whichever side of the high-water it sits. Such a record
    /// held no content when it was packed, so its entity is in no content extent and the row this
    /// writes is the first; the registry refuses a fill that would need a second
    /// ([`Self::content_row_is_packed`]).
    pub fn unpublished_content(&self) -> Vec<(EntityId, Vec<(u16, String)>)> {
        let mut out = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let from = *self
                .published_through
                .get(&(layer.clone(), *level))
                .unwrap_or(&0) as usize;
            let pending = (0..from.min(slots.len())).filter(|ordinal| {
                self.content_pending
                    .contains(&(layer.clone(), *level, *ordinal as u32))
            });
            let tail = from..slots.len();
            for slot in pending.chain(tail).map(|ordinal| &slots[ordinal]) {
                let Some(record) = slot else { continue };
                let mut fields = Vec::new();
                for (v, content) in record.contents.iter().enumerate() {
                    let Some(values) = &content.values else {
                        continue;
                    };
                    for (k, value) in values.iter().enumerate() {
                        let tag = v * values.len() + k;
                        // A layer whose kinds and contents multiply past the tag space cannot be
                        // written back. **The whole artifact's row is abandoned, not the one
                        // field**: the reader requires every declared kind or none, so a row
                        // missing one withholds the artifact — while the *in-memory* copy, tried
                        // first, would go on serving it in full. Dropping the field alone therefore
                        // makes the two copies disagree, and which one a viewer gets depends on
                        // whether the process has restarted since publication.
                        let Ok(tag) = u16::try_from(tag) else {
                            // No logging facade in this crate; the withholding is what a reader
                            // sees, and `supplied_content` refuses on the same condition at the
                            // other end.
                            fields.clear();
                            break;
                        };
                        fields.push((tag, value.clone()));
                    }
                    if fields.is_empty() && !content.values.as_ref().is_none_or(Vec::is_empty) {
                        // The break above cleared it: abandon this artifact entirely rather than
                        // writing the contents that happened to fit.
                        break;
                    }
                }
                if !fields.is_empty() {
                    out.push((record.entity, fields));
                }
            }
        }
        out
    }

    /// Every level and how many ordinals it currently spans, holes included — what a publication
    /// marks as published once its manifest is durable.
    pub fn levels_and_extents(&self) -> impl Iterator<Item = (&str, u32, u32)> {
        self.levels
            .iter()
            .map(|((layer, level), slots)| (layer.as_str(), *level, slots.len() as u32))
    }

    /// Record that `[0, through)` of a level is durable in a manifest.
    ///
    /// **Called only after the manifest naming the extent is itself durable.** Marking earlier would
    /// let rotation reclaim the log records behind memberships whose file a crash could still lose —
    /// which is the one ordering this whole mechanism exists to get right.
    pub fn mark_published(&mut self, layer: &str, level: u32, through: u32) {
        let entry = self
            .published_through
            .entry((layer.to_string(), level))
            .or_insert(0);
        *entry = (*entry).max(through);
        self.recompute_pin();
    }

    /// Record that every level has been rewritten **whole** into a durable manifest, releasing the
    /// log from the growths that rewrite carried.
    ///
    /// **Called only from the fold, and only after its flip.** [`Self::mark_published`] is the
    /// wrong home for this and calling it from there would be the silent failure this bookkeeping
    /// exists against: that one records how far the *append-only* packer has reached, and the
    /// append-only packer never touches a record below the high-water. A growth marked published by
    /// a tail pack is a join that is durable nowhere — reclaimable in the log, absent from every
    /// extent, and back to its pre-growth membership at the next restart, with an ack already given
    /// and nothing anywhere reporting a fault.
    ///
    /// One flag rather than a per-level map, because the fold rewrites every level in one
    /// publication ([`Self::repack_all`]): a mark that could be half-set would need an argument
    /// about which half, and the executor is single-threaded, so nothing grows between the rewrite
    /// and this call.
    pub fn mark_growth_packed(&mut self) {
        self.grown_wal_pos = None;
        // A filled parent, attachment, shape or content digest is in the rewritten membership
        // extents on the same argument. Filled content *values* are not: the fold carries content
        // extents forward unchanged, so `content_wal_pos` waits for the content extent.
        self.filled_wal_pos = None;
    }

    /// The log position of the oldest publication whose memberships are not yet in a manifest.
    ///
    /// **Recomputed from scratch rather than advanced**, because the alternative is an increment
    /// that has to be right at every call site. A level fully published contributes nothing; a level
    /// with anything outstanding contributes the position it was applied at.
    fn recompute_pin(&mut self) {
        let outstanding = self.levels.iter().any(|((layer, level), slots)| {
            let through = *self
                .published_through
                .get(&(layer.clone(), *level))
                .unwrap_or(&0) as usize;
            through < slots.len()
        });
        if !outstanding {
            self.oldest_wal_pos = None;
        }
    }

    /// Seed one artifact from a published extent, **before** WAL replay unions what came after.
    ///
    /// The ordering is the rule and not a preference, exactly as it is for the registry and the
    /// overlay: every WAL record postdates any state a manifest carries, so seeding afterwards would
    /// overwrite a later publication with an earlier one. Seeded artifacts are published by
    /// definition, so this advances the high-water and never the pin.
    /// Declare that a seeded level covers `[0, len)` ordinals, holes included.
    ///
    /// **A hole at the top of a level is invisible from its records alone**, and that is an identity
    /// bug rather than an untidiness: a level seeded only from the blobs that decoded ends *shorter*
    /// than the extent that was written, [`Self::next_ordinal`] regresses onto the hole, and the
    /// next publication is handed the ordinal — and therefore the entity, which is a function of it
    /// — that the artifact this fold deleted was published under. Two artifacts, one `tessera_id`,
    /// with the second answering for the first.
    ///
    /// The extent's own `count` is the authority on how far a level reaches, so it is carried here
    /// rather than inferred. A hole in the *middle* survives without this, which is exactly why it
    /// has to be explicit: the case that hides is the one at the end.
    pub fn seed_extent_bound(&mut self, layer: &str, level: u32, len: u32) {
        let slots = self.levels.entry((layer.to_string(), level)).or_default();
        if slots.len() < len as usize {
            slots.resize_with(len as usize, || None);
        }
        let entry = self
            .published_through
            .entry((layer.to_string(), level))
            .or_insert(0);
        *entry = (*entry).max(len);
    }

    pub fn seed(
        &mut self,
        layer: &str,
        level: u32,
        ordinal: u32,
        record: ArtifactRecord,
        shape: Option<ArtifactShapes>,
    ) {
        self.put(layer, level, ordinal, record, shape);
        let entry = self
            .published_through
            .entry((layer.to_string(), level))
            .or_insert(0);
        *entry = (*entry).max(ordinal + 1);
    }

    /// How many artifacts are held, across every layer. **Operator-facing only**: a per-layer count
    /// is a corpus-wide count over objects a principal may not individually see, which is C8's row,
    /// and this deliberately offers no way to ask for one.
    pub fn total(&self) -> usize {
        self.levels
            .values()
            .map(|slots| slots.iter().filter(|s| s.is_some()).count())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Encode one artifact for a packed extent: its caller key, its membership and its contents'
/// generating sets, in one blob.
///
/// ```text
/// blob       := u16 LE key_len | key bytes (UTF-8)
///             | u16 LE view_len | view bytes (UTF-8)   -- 0 on an entity-scoped layer
///             | u16 LE content_count
///             | u32 LE members_len | membership bytes (portable Roaring)
///             | content*
///             | attachment
///             | parents
///             | shape
/// content    := digest (32 bytes, SHA-256 of the values)
///             | u64 LE cardinality                       -- the set's stored cardinality
///             | u32 LE set_len | generating-set bytes (portable Roaring)
/// attachment := u8 0                                     -- unattached
///             | u8 1 | u16 LE layer_len | layer bytes (UTF-8)
///                    | u32 LE level | u32 LE ordinal | u64 LE target entity
/// parents    := u16 LE parent_count                      -- 0 at a root
///             | per parent, ascending by (level, ordinal): u32 LE level | u32 LE ordinal
/// shape      := u8 0                                     -- no declared shape
///             | u8 3 | digest (32 bytes, SHA-256 of what follows)
///                    | u16 LE views
///                    | per view: u16 LE view_len | view bytes (UTF-8)
///                                | u32 LE shape_len | canonical shape bytes
/// ```
///
/// The canonical shape bytes are `tessera_spatial::shape`'s own encoding (`polygon-membership.md`
/// §6.6 — tag 1 a box, 2 a conic, 4 a polygon), held here opaquely; tag 3 is the per-view wrapper
/// and is this blob's, which is why the shape module leaves it unused.
///
/// **A content's digest and cardinality, and a shape's digest, are stored** (`ingest.md` §1.5,
/// `bundle_format` 7). The extent carries no content values, so the digest is the only thing a
/// repeated publication can be compared against once the level is repacked; the cardinality is
/// the set's stored property, moved by a page and published beside the set's operator. The
/// decoder checks a shape's digest against the bytes it decodes and refuses a disagreement.
///
/// **The attachment is stored and not re-derived**, on the reason [`Attachment`] gives: it is a
/// term of the visibility predicate, so an artifact restored without it is one that serves where
/// the live copy would withhold — the fail-open the term exists to close, reappearing at a restart.
///
/// **Every length is explicit, including the membership's.** The membership used to be the blob's
/// tail and its length was *"whatever is left"*, which is exactly the shape that decodes a truncated
/// blob as a shorter membership — an artifact with a low masked count for every viewer, which the
/// criterion renders as absent with nothing to notice. With a length in front, short is short.
///
/// **The view travels beside the key, and for the same reason** (`bundle_format` 8): it is part
/// of an artifact's identity on a group-scoped layer (`ingest.md` §1.5), keys are unique per
/// `(layer, view)`, and a level folded into an extent and reopened without it would collide two
/// views' keys under one index and draw one view's artifacts on every view of the group. A
/// zero length is the entity-scoped layer's one set, which is a complete statement rather than
/// an unfilled one.
///
/// **The key travels with the membership because nothing else durable carries it.** An artifact's
/// entity is derivable from its layer's reserved runs and its ordinal, so the extent need not carry
/// it; a caller's key is derivable from nothing. Putting it in the manifest instead would put
/// one JSON string per artifact in a document parsed at every open — the entry-count problem the
/// packing exists to solve, in another guise.
///
/// `tessera-store` holds this as an opaque blob and addresses it by ordinal. **That split is the
/// layering**: the store owns which bytes belong to which artifact, this crate owns what the bytes
/// mean, and the bitmap library stays on one side of the boundary.
pub fn encode_record(record: &ArtifactRecord, shape: Option<&ArtifactShapes>) -> Vec<u8> {
    let key = record.key.as_deref().unwrap_or_default().as_bytes();
    let members = serialise_members(&record.members);
    let sets: Vec<Vec<u8>> = record
        .contents
        .iter()
        .map(|v| serialise_members(&v.generated_from))
        .collect();
    let mut out = Vec::with_capacity(
        8 + key.len() + members.len() + sets.iter().map(|s| s.len() + 44).sum::<usize>(),
    );
    // A key longer than a `u16` cannot round-trip, and truncating one would silently rename an
    // artifact. The control plane bounds the request body long before this, so the clamp is a
    // backstop; it refuses at encode rather than writing a key it cannot read back.
    let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&key_len.to_le_bytes());
    if key_len != u16::MAX {
        out.extend_from_slice(key);
    }
    // The view, on the key's rule and with the same consequence one step further: a view name
    // that could not round-trip would restore a group-scoped artifact as though it belonged to
    // every view of its group.
    let view = record.view.as_deref().unwrap_or_default().as_bytes();
    let view_len = u16::try_from(view.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&view_len.to_le_bytes());
    if view_len != u16::MAX {
        out.extend_from_slice(view);
    }
    // Same argument, one level up: more contents than a `u16` can count is a publication this
    // encoding cannot read back, so it refuses rather than writing a prefix of the ranking. A
    // dropped content is a viewer served a *different* description from the one the caller
    // ranked for them.
    let count = u16::try_from(sets.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(members.len() as u32).to_le_bytes());
    out.extend_from_slice(&members);
    if count != u16::MAX {
        for (content, set) in record.contents.iter().zip(&sets) {
            out.extend_from_slice(&content.digest);
            out.extend_from_slice(&content.cardinality.to_le_bytes());
            out.extend_from_slice(&(set.len() as u32).to_le_bytes());
            out.extend_from_slice(set);
        }
    }
    match &record.attached_to {
        None => out.push(0),
        Some(attachment) => {
            let layer = attachment.layer.as_bytes();
            // Same argument as the key's, and stricter in consequence: a layer name that could not
            // round-trip would restore an attached artifact as an unattached one, which serves it
            // where the live copy withholds it. Refused at encode, which the decoder then reports.
            let Ok(layer_len) = u16::try_from(layer.len()) else {
                out.push(u8::MAX);
                return out;
            };
            out.push(1);
            out.extend_from_slice(&layer_len.to_le_bytes());
            out.extend_from_slice(layer);
            out.extend_from_slice(&attachment.level.to_le_bytes());
            out.extend_from_slice(&attachment.ordinal.to_le_bytes());
            out.extend_from_slice(&attachment.entity.raw().to_le_bytes());
        }
    }
    // The parents, counted: a root is an explicit zero and never an absent field, so *this node
    // is a root* and *this reader does not know whether it had a parent* cannot encode the same.
    // Here the second answer would serve a child beside the ancestor that should have replaced it
    // — a duplicate on the map rather than a disclosure, but wrong either way. More parents than a
    // `u16` counts is a record this encoding cannot read back, refused at encode on the key's and
    // the contents' rule rather than written as a prefix of the lineage.
    let parent_count = u16::try_from(record.parents.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&parent_count.to_le_bytes());
    if parent_count != u16::MAX {
        for parent in &record.parents {
            out.extend_from_slice(&parent.level.to_le_bytes());
            out.extend_from_slice(&parent.ordinal.to_le_bytes());
        }
    }
    // **The shape, on the same discriminant rule** — and here the fail-closed reading is the loud
    // one. A spatial artifact restored *without* its shape has no membership rule at all, so it
    // counts zero for every viewer and is absent under any criterion; the decoder refuses such a
    // blob rather than restoring a shapeless artifact, which is what makes the absence a fault
    // somebody sees instead of a boundary that quietly stopped holding anything.
    match shape {
        None => out.push(0),
        Some(shapes) => {
            out.push(3);
            out.extend_from_slice(&shapes.digest);
            shapes.encode_into(&mut out);
        }
    }
    out
}

/// Where one packed blob's membership bytes are, without decoding a bitmap.
///
/// **The build's one use for it**: an extent it has just written is mapped back, and a record's
/// heap bitmap is replaced by a view over these bytes ([`Members::mapped`]). Nothing is copied and
/// nothing is parsed but the four lengths [`encode_record`] puts in front — which is the whole
/// reason the membership carries an explicit length rather than being the blob's tail.
///
/// `None` where the framing does not hold, on [`decode_record`]'s rule: the caller keeps the
/// bitmap it already has rather than adopting bytes it could not locate.
pub fn members_bytes(blob: &[u8]) -> Option<&[u8]> {
    let key_len = u16::from_le_bytes(blob.get(0..2)?.try_into().ok()?) as usize;
    if key_len == u16::MAX as usize {
        return None;
    }
    // The view sits between the key and the content count (`bundle_format` 8), and is skipped
    // here for the same reason the key is: this reader wants the membership's offset and parses
    // only the lengths in front of it.
    let at = 2 + key_len;
    let view_len = u16::from_le_bytes(blob.get(at..at + 2)?.try_into().ok()?) as usize;
    if view_len == u16::MAX as usize {
        return None;
    }
    let at = at + 2 + view_len;
    let count = u16::from_le_bytes(blob.get(at..at + 2)?.try_into().ok()?) as usize;
    if count == u16::MAX as usize {
        return None;
    }
    let at = at + 2;
    let members_len = u32::from_le_bytes(blob.get(at..at + 4)?.try_into().ok()?) as usize;
    blob.get(at + 4..at + 4 + members_len)
}

/// The inverse, refusing anything it cannot read back exactly.
///
/// **A refusal and never a partial record**, on [`deserialise_members`]'s argument: an artifact whose
/// key was lost is one no edge can name, and an artifact whose membership decoded short is one with
/// a low masked count for every viewer — which the existence criterion renders as *absent*, with no
/// error anywhere to notice. Both must be a decode failure the caller alarms on.
pub fn decode_record(
    entity: EntityId,
    blob: &[u8],
) -> Option<(ArtifactRecord, Option<ArtifactShapes>)> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let end = at.checked_add(n)?;
        let view = blob.get(at..end)?;
        at = end;
        Some(view)
    };
    let key_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if key_len == u16::MAX as usize {
        return None;
    }
    let key = if key_len == 0 {
        None
    } else {
        Some(std::str::from_utf8(take(key_len)?).ok()?.to_string())
    };
    let view_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if view_len == u16::MAX as usize {
        return None;
    }
    let view = if view_len == 0 {
        None
    } else {
        Some(std::str::from_utf8(take(view_len)?).ok()?.to_string())
    };
    let count = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if count == u16::MAX as usize {
        return None;
    }
    let members_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let members = deserialise_members(take(members_len)?)?;
    let mut contents = Vec::with_capacity(count);
    for _ in 0..count {
        let digest: [u8; 32] = take(32)?.try_into().ok()?;
        let cardinality = u64::from_le_bytes(take(8)?.try_into().ok()?);
        let set_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        contents.push(ContentSet {
            // ⊘ The extent carries no values — see [`ContentSet::values`]. A restored content is
            // therefore unservable until the blob write lands, which is fail-closed and loud rather
            // than an artifact served with its description missing.
            values: None,
            digest,
            generated_from: deserialise_members(take(set_len)?)?,
            cardinality,
        });
    }
    // An attachment absent is one byte and never zero bytes: *unattached* and *this reader does not
    // know whether it was attached* must not encode the same, since the second answer is one that
    // serves a label whose cluster is hidden.
    let attached_to = match take(1)?[0] {
        0 => None,
        1 => {
            let layer_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
            let layer = std::str::from_utf8(take(layer_len)?).ok()?.to_string();
            let level = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let ordinal = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let entity = EntityId::new(u64::from_le_bytes(take(8)?.try_into().ok()?));
            Some(Attachment {
                layer,
                level,
                ordinal,
                entity,
            })
        }
        _ => return None,
    };
    let parent_count = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if parent_count == u16::MAX as usize {
        return None;
    }
    // **Strictly ascending, or a decode failure.** The writer keeps the list sorted and
    // deduplicated, so a list that is not is one this reader did not write — and reading it
    // would hand the engine a lineage whose duplicate edges count twice and whose order is
    // whatever the bytes happened to say.
    let mut parents = Vec::with_capacity(parent_count);
    for _ in 0..parent_count {
        let parent = crate::wal::ParentRef {
            level: u32::from_le_bytes(take(4)?.try_into().ok()?),
            ordinal: u32::from_le_bytes(take(4)?.try_into().ok()?),
        };
        if parents.last().is_some_and(|last| *last >= parent) {
            return None;
        }
        parents.push(parent);
    }
    // **The shapes go through [`ArtifactShapes::new`] rather than being assembled from the
    // bytes**, so a blob carrying no view or an empty shape is a decode failure and not an artifact
    // whose membership is a region nobody wrote. One constructor, at both ends. Whether the bytes
    // *decode as a shape* is the engine's question, asked where the shape is held; a stale format
    // refuses there, loudly, rather than here silently.
    let shape = match take(1)?[0] {
        0 => None,
        3 => {
            let digest: [u8; 32] = take(32)?.try_into().ok()?;
            let views = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
            let mut by_view = Vec::with_capacity(views);
            for _ in 0..views {
                let view_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
                let view = std::str::from_utf8(take(view_len)?).ok()?.to_string();
                let shape_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
                by_view.push((view, take(shape_len)?.to_vec()));
            }
            let shapes = ArtifactShapes::new(by_view)?;
            // The stored digest names the bytes the writer held; a disagreement is a blob this
            // reader did not write, refused as any other framing fault is.
            if shapes.digest != digest {
                return None;
            }
            Some(shapes)
        }
        _ => return None,
    };
    // **Trailing bytes are a decode failure**, not slack to ignore: a blob longer than its own
    // structure means the writer and this reader disagree about the format, and the half that
    // decoded cleanly is the more dangerous outcome of the two.
    if at != blob.len() {
        return None;
    }
    Some((
        ArtifactRecord {
            entity,
            key,
            view,
            members: Members::owned(members),
            contents,
            attached_to,
            parents,
        },
        shape,
    ))
}

/// Serialise a membership for the WAL, in CRoaring's portable form.
///
/// Portable rather than the frozen form the fragment cache uses: frozen is an mmap-oriented layout
/// with alignment padding and no cross-version guarantee, and this goes into a log that must be
/// readable by the process that reopens it. The bytes are self-describing enough that a corrupt
/// record fails to deserialise rather than yielding a plausible wrong set.
pub fn serialise_members(members: &Bitmap) -> Vec<u8> {
    members.serialize::<Portable>()
}

/// **The durable record one growth becomes** — the one construction site, taken by the control
/// plane's `GrowMemberships` and by an ingest batch's membership column alike.
///
/// `None` where nothing is joining: no record is owed for a no-op, and appending an empty one would
/// pin the log at a growth that changed nothing (`artifacts-from-points.md` §6.1). Ordinals are
/// already resolved — see `LayerRegistry::resolve_growth_key` — because what replay applies must be
/// what was decided, not a key re-read against an index that has since moved.
pub fn growth_record<'a>(
    layer: &str,
    level: u32,
    joins: impl IntoIterator<Item = (u32, &'a Bitmap)>,
) -> Option<crate::wal::WalRecord> {
    let growth: Vec<crate::wal::MembershipGrowth> = joins
        .into_iter()
        .filter(|(_, joining)| !joining.is_empty())
        .map(|(ordinal, joining)| crate::wal::MembershipGrowth {
            ordinal,
            joining: serialise_members(joining),
            // A membership never shrinks (`ingest.md` §10, R7); a generating-set page is T2b's
            // record and takes the other variant.
            leaving: Vec::new(),
            set: crate::wal::GrownSet::Membership,
        })
        .collect();
    (!growth.is_empty()).then(|| crate::wal::WalRecord::ArtifactGrow {
        layer: layer.to_string(),
        level,
        growth,
    })
}

/// The inverse, refusing bytes that are not a bitmap.
///
/// **A refusal, not a default.** An empty membership is a real and meaningful state — an artifact
/// every one of whose members has been deleted — so decoding damage to "empty" would make a
/// corrupted record indistinguishable from a legitimately emptied artifact, and the second is
/// served rather than refused.
pub fn deserialise_members(bytes: &[u8]) -> Option<Bitmap> {
    Bitmap::try_deserialize::<Portable>(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(entity: u64, members: &[u32]) -> ArtifactRecord {
        ArtifactRecord {
            entity: EntityId::new(entity),
            key: None,
            view: None,
            members: Members::owned(Bitmap::of(members)),
            contents: Vec::new(),
            attached_to: None,
            parents: Vec::new(),
        }
    }

    /// A record attached to `target`.
    fn attached(entity: u64, target: u64, layer: &str, ordinal: u32) -> ArtifactRecord {
        ArtifactRecord {
            attached_to: Some(Attachment {
                layer: layer.to_string(),
                level: 0,
                ordinal,
                entity: EntityId::new(target),
            }),
            ..record(entity, &[1])
        }
    }

    /// **A chain, not a level.** A label on a cluster and a label on that label both go when the
    /// cluster does, which is what makes rule 1 transitive — and the roots are never in the answer,
    /// so the deny lane can submit it beside the deletions it was given without deleting one twice.
    #[test]
    fn a_cascade_follows_the_whole_chain_and_never_returns_a_root() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1, 2, 3]), None);
        store.put("clusters/a", 0, 1, record(101, &[4]), None);
        store.put("topics/x", 0, 0, attached(200, 100, "clusters/a", 0), None);
        store.put("topics/x", 0, 1, attached(201, 101, "clusters/a", 1), None);
        store.put("glosses/y", 0, 0, attached(300, 200, "topics/x", 0), None);

        assert_eq!(
            store.cascade_from(&[EntityId::new(100)]),
            vec![EntityId::new(200), EntityId::new(300)],
            "the label and the label on the label, and neither of the untouched cluster's"
        );
        assert_eq!(
            store.cascade_from(&[EntityId::new(100), EntityId::new(200)]),
            vec![EntityId::new(300)],
            "a dependent already being deleted is not deleted a second time"
        );
        assert!(
            store.cascade_from(&[EntityId::new(999)]).is_empty(),
            "an entity that is not an artifact has no dependents"
        );
    }

    /// A retired artifact takes its edges with it, so a later deletion of something else does not
    /// cascade into an ordinal that is now a hole — or into whatever a republication put there.
    #[test]
    fn retiring_an_artifact_takes_its_dependency_edges_with_it() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1, 2, 3]), None);
        store.put("topics/x", 0, 0, attached(200, 100, "clusters/a", 0), None);
        store.retire(&Bitmap::of(&[200]));
        assert!(
            store.cascade_from(&[EntityId::new(100)]).is_empty(),
            "the label is gone, so deleting its cluster cascades into nothing"
        );
    }

    /// **The streaming route and the materialising one answer the same thing.** A build takes
    /// ranges and encodes an artifact at a time; the online publication takes the blobs. Two routes
    /// to one set of bytes is one place for them to drift, so the equality is asserted rather than
    /// argued — including which levels are skipped for a hole, where the two must agree exactly or
    /// a build would pack around one.
    #[test]
    fn the_ranges_and_the_blobs_describe_the_same_pending_levels() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1, 2, 3]), None);
        store.put("clusters/a", 0, 1, record(101, &[4]), None);
        store.put("clusters/a", 1, 0, record(102, &[]), None);
        // A level with a hole: reported as skipped by both, and never as a range.
        store.put("holed/h", 0, 1, record(103, &[7]), None);

        let (ready, skipped) = store.unpublished();
        let (ranges, skipped_ranges) = store.pending_ranges();
        assert_eq!(skipped, skipped_ranges);
        assert_eq!(skipped, vec![("holed/h".to_string(), 0)]);
        assert_eq!(ready.len(), ranges.len());
        for ((layer, level, lo, blobs), (r_layer, r_level, r_lo, count)) in
            ready.iter().zip(ranges.iter())
        {
            assert_eq!((layer, level, lo), (r_layer, r_level, r_lo));
            assert_eq!(blobs.len(), *count as usize);
            let streamed: Vec<Vec<u8>> = store
                .encode_pending(r_layer, *r_level, *r_lo, *count)
                .map(|blob| blob.expect("a range holds a record at every ordinal"))
                .collect();
            assert_eq!(*blobs, streamed);
        }
    }

    #[test]
    fn a_level_is_dense_and_a_hole_answers_absent() {
        // A publication that skips an ordinal — one artifact still in flight, or one a fold has
        // removed — must leave a hole that answers `None`, not a panic and not a neighbour.
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 5, record(100, &[1, 2, 3]), None);
        assert!(store.get("clusters/a", 0, 5).is_some());
        assert!(store.get("clusters/a", 0, 0).is_none());
        assert!(store.get("clusters/a", 0, 9).is_none());
        assert_eq!(store.level("clusters/a", 0).count(), 1);
        assert_eq!(store.total(), 1);
    }

    #[test]
    fn the_declared_size_is_derived_and_cannot_drift() {
        // It is the proportional criterion's denominator and nothing else. Deriving it means a
        // deletion that shrinks the membership shrinks the denominator with it, in one place.
        let mut r = record(100, &[1, 2, 3, 4]);
        assert_eq!(r.declared_size(), 4);
        r.members.to_mut().remove(3);
        assert_eq!(r.declared_size(), 3);
    }

    #[test]
    fn dropping_a_layer_takes_its_artifacts_with_it() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1]), None);
        store.put("clusters/a", 1, 0, record(101, &[2]), None);
        store.put("clusters/b", 0, 0, record(102, &[3]), None);
        assert_eq!(store.total(), 3);

        store.remove_layer("clusters/a");
        assert_eq!(store.total(), 1);
        assert!(store.get("clusters/b", 0, 0).is_some());
        assert_eq!(store.layer("clusters/a").count(), 0);
    }

    /// **A level's version is monotone for the life of the store, and a drop is not an exception.**
    ///
    /// The hazard the per-level grain would otherwise have: a form cached for
    /// `(view, layer, level)` outlives the layer, and a name arriving at that address again with a
    /// version the cache has already seen would be answered from the previous incarnation's
    /// members. It is closed twice over — the registry tombstones a dropped name for ever, so the
    /// address is never reoccupied at all — and this is the half that belongs here, because it is
    /// the half a later decision to allow reuse would not silently invalidate.
    #[test]
    fn a_dropped_layers_version_moves_and_is_never_forgotten() {
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("clusters/a", 0, 100, &[1]), 0), 0);
        let published = store.level_version("clusters/a", 0);
        assert!(published > 0);

        store.remove_layer("clusters/a");
        let dropped = store.level_version("clusters/a", 0);
        assert!(
            dropped > published,
            "the drop moves it, so a form cached under the published version is stale"
        );

        // The address occupied again, as a re-registration would occupy it.
        assert_eq!(store.apply(&publication("clusters/a", 0, 200, &[9]), 8), 0);
        assert!(
            store.level_version("clusters/a", 0) > dropped,
            "and it counts on from where the drop left it rather than starting again — a version \
             that restarted at zero would let a form built over the artifacts that are gone \
             compare equal to the level that replaced them"
        );
    }

    /// A membership of `span` entities scattered over a `rows`-wide entity space, which is the
    /// shape a treed node's interval takes once signature order has permuted it.
    fn scattered_membership(span: std::ops::Range<u64>, rows: u64) -> Bitmap {
        let scramble = |e: u64| -> u32 {
            let mut x = e.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 29;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 32;
            (x % rows) as u32
        };
        let mut b = Bitmap::new();
        let mut values: Vec<u32> = span.map(scramble).collect();
        values.sort_unstable();
        values.dedup();
        b.add_many(&values);
        b
    }

    /// Every array container's payload, strictly increasing? Parsed out of the portable bytes here
    /// rather than asked of the decoder, so a failure says *the writer emitted an out-of-order
    /// container* and not merely *the decoder refused*.
    fn out_of_order_containers(bytes: &[u8]) -> Vec<(usize, u32, usize)> {
        let cookie = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let hasrun = (cookie & 0xFFFF) == 12347;
        let (containers, mut at) = if hasrun {
            (((cookie >> 16) + 1) as usize, 4usize)
        } else {
            (
                u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
                8usize,
            )
        };
        let run_flags = at;
        if hasrun {
            at += containers.div_ceil(8);
        }
        let descriptors = at;
        at += containers * 4;
        if !hasrun || containers >= 4 {
            at += containers * 4; // the offset header
        }
        let mut out = Vec::new();
        for k in 0..containers {
            let card = u16::from_le_bytes(
                bytes[descriptors + 4 * k + 2..descriptors + 4 * k + 4]
                    .try_into()
                    .unwrap(),
            ) as u32
                + 1;
            let isrun = hasrun && (bytes[run_flags + k / 8] & (1 << (k % 8))) != 0;
            if isrun {
                let runs = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
                at += 2 + runs * 4;
            } else if card > 4096 {
                at += 8192;
            } else {
                let word = |i: usize| {
                    u16::from_le_bytes(bytes[at + 2 * i..at + 2 * i + 2].try_into().unwrap())
                };
                for i in 1..card as usize {
                    if word(i) <= word(i - 1) {
                        out.push((k, card, i));
                        break;
                    }
                }
                at += 2 * card as usize;
            }
        }
        out
    }

    /// **A membership read through bytes is the membership**, and a write to it materialises
    /// first. The two together are what let the build swap where a membership lives without any
    /// caller being able to tell — and what keep growth and retirement the only routes a bit
    /// changes, whichever form the record happens to be in.
    #[test]
    fn a_mapped_membership_answers_as_the_bitmap_it_views() {
        let bitmap = Bitmap::of(&[1, 2, 3, 70_000, 70_001]);
        let bytes: Arc<Vec<u8>> = Arc::new(serialise_members(&bitmap));
        let owner: Arc<dyn Any + Send + Sync> = bytes.clone();
        let mapped = unsafe { Members::mapped(&bytes, owner) }.expect("the bytes are a bitmap");
        assert!(mapped.is_mapped());
        assert_eq!(mapped, bitmap);
        assert_eq!(mapped.cardinality(), 5);
        assert_eq!(
            mapped.iter().collect::<Vec<u32>>(),
            vec![1, 2, 3, 70_000, 70_001]
        );
        // A clone of a view is a view over the same bytes, and outlives the value it came from.
        let clone = mapped.clone();
        drop(mapped);
        assert_eq!(clone, bitmap);

        let mut written = clone;
        written.to_mut().add(9);
        assert!(!written.is_mapped(), "a write materialises before it lands");
        assert_eq!(written.cardinality(), 6);
        assert!(written.contains(9));
        // And the bytes it viewed are untouched — the mapping is read-only by construction.
        assert_eq!(deserialise_members(&bytes).expect("still a bitmap"), bitmap);
    }

    /// **Bytes that are not a bitmap are refused rather than read as containers.** The view
    /// constructor is unchecked in croaring; this is the check in front of it, and it is the same
    /// refusal [`deserialise_members`] makes for the same reason.
    #[test]
    fn a_mapped_membership_refuses_bytes_that_are_not_a_bitmap() {
        let bytes: Arc<Vec<u8>> = Arc::new(vec![0xAB; 32]);
        let owner: Arc<dyn Any + Send + Sync> = bytes.clone();
        assert!(unsafe { Members::mapped(&bytes, owner) }.is_none());
    }

    /// **A rehousing that is not the same membership is refused**, and the record keeps what it
    /// had. A view over another artifact's bytes would be a masked count that is low for every
    /// viewer — served as absent, with nothing to notice.
    #[test]
    fn rehousing_refuses_a_membership_that_is_not_the_one_it_replaces() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1, 2, 3]), None);

        let wrong: Arc<Vec<u8>> = Arc::new(serialise_members(&Bitmap::of(&[1, 2])));
        let owner: Arc<dyn Any + Send + Sync> = wrong.clone();
        let members = unsafe { Members::mapped(&wrong, owner) }.expect("a bitmap");
        assert!(!store.rehouse_members("clusters/a", 0, 0, members));
        assert_eq!(
            store.get("clusters/a", 0, 0).expect("still there").members,
            Bitmap::of(&[1, 2, 3])
        );

        let right: Arc<Vec<u8>> = Arc::new(serialise_members(&Bitmap::of(&[1, 2, 3])));
        let owner: Arc<dyn Any + Send + Sync> = right.clone();
        let members = unsafe { Members::mapped(&right, owner) }.expect("a bitmap");
        assert!(store.rehouse_members("clusters/a", 0, 0, members));
        let record = store.get("clusters/a", 0, 0).expect("still there");
        assert!(record.members.is_mapped());
        assert_eq!(record.members, Bitmap::of(&[1, 2, 3]));
        // An ordinal naming no record adopts nothing, exactly as a growth does not resurrect one.
        let spare: Arc<Vec<u8>> = Arc::new(serialise_members(&Bitmap::new()));
        let owner: Arc<dyn Any + Send + Sync> = spare.clone();
        let members = unsafe { Members::mapped(&spare, owner) }.expect("a bitmap");
        assert!(!store.rehouse_members("clusters/a", 0, 7, members));
    }

    /// **The membership's bytes are where [`members_bytes`] says they are**, over a record with a
    /// key, contents and an attachment in front of and behind them — the framing the build's
    /// rehousing walks without decoding a bitmap.
    #[test]
    fn members_bytes_finds_the_membership_encode_record_wrote() {
        let mut record = record(100, &[1, 2, 3, 70_000]);
        record.key = Some("a-key".to_string());
        record.contents = vec![ContentSet {
            values: Some(vec!["topic".to_string()]),
            digest: content_digest(&["topic".to_string()]),
            generated_from: Bitmap::of(&[2, 3]),
            cardinality: 2,
        }];
        record.attached_to = Some(Attachment {
            layer: "labels/x".to_string(),
            level: 0,
            ordinal: 3,
            entity: EntityId::new(400),
        });
        let blob = encode_record(&record, None);
        let bytes = members_bytes(&blob).expect("the framing holds");
        assert_eq!(
            deserialise_members(bytes).expect("a bitmap"),
            Bitmap::of(&[1, 2, 3, 70_000]),
            "the slice must be the membership and not a content's generating set"
        );
        // **A blob cut inside the membership answers None rather than a shorter one** — which is
        // the whole point of the explicit length `encode_record` puts in front of it.
        let end = bytes.as_ptr() as usize - blob.as_ptr() as usize + bytes.len();
        assert!(members_bytes(&blob[..end - 1]).is_none());
    }

    fn check_round_trip(members: &Bitmap) {
        let bytes = serialise_members(members);
        let broken = out_of_order_containers(&bytes);
        assert!(
            broken.is_empty(),
            "a container was written out of order — (container, cardinality, index): {broken:?}"
        );
        assert_eq!(
            deserialise_members(&bytes).as_ref(),
            Some(members),
            "a membership of {} did not survive its own encoding",
            members.cardinality()
        );
    }

    /// **The shape the 5×10⁷ tier refused, pinned** — `probes/2026-08-22-artifact-serving-e2e/`
    /// finding 3: `tessera build` stopped with *"1 membership(s) of generator/treed did not survive
    /// their own encoding"*, a bitmap failing its own `Portable` round trip in the process that
    /// wrote it.
    ///
    /// What the campaign met was not an encoding fault. The bytes carry what the container held;
    /// what the decoder's validation rejected was *"array elements not strictly increasing"* — a
    /// container that was **already out of order before it was serialised**. This test therefore
    /// checks both halves at the shape that produced it: an array container's payload is strictly
    /// increasing in the bytes we wrote, and the bytes decode.
    ///
    /// ⊘ **The corruption itself was not attributed.** It appeared under repeated `Bitmap::add` of
    /// out-of-order values — the insert-into-the-middle path this crate no longer uses
    /// ([`bitmap_of_entities`]) — at a rate of a few per thousand memberships, bursty, and not a
    /// function of the data: the same input corrupted a different membership on each run and none
    /// at all on most. It is below this crate, in CRoaring or in the machine, and stopped
    /// reproducing before it could be told which. The check that caught it stays.
    ///
    /// Two memberships of that shape — 3.1×10⁶ entities scattered over a 5×10⁷ row space, which
    /// is 763 array containers of about four thousand each.
    #[test]
    fn a_scattered_membership_of_the_campaigns_shape_survives_its_own_encoding() {
        for block in 0..2u64 {
            let lo = block * 3_125_000;
            check_round_trip(&scattered_membership(lo..lo + 3_125_000, 50_000_000));
        }
    }

    /// The whole treed level the 5×10⁷ tier refused — five thousand memberships, 1.8×10⁸ member
    /// rows, the root holding the corpus.
    ///
    /// `#[ignore]`d for its runtime: it builds every membership of the level and takes tens of
    /// seconds, where the un-ignored test above exercises the same construction and the same two
    /// checks at eight of them.
    #[test]
    #[ignore = "builds a whole 5x10^7 treed level; the eight-membership variant covers the path"]
    fn the_refusing_tiers_whole_treed_level_survives_its_own_encoding() {
        const ROWS: u64 = 50_000_000;
        const BRANCH: u64 = 3;
        let count = ROWS / 10_000;
        let child_span = |(lo, hi): (u64, u64), index: u64| {
            let each = (hi - lo) * 3 / 4 / BRANCH;
            let start = lo + index * each;
            (start.min(hi), (start + each).min(hi))
        };
        for a in 0..count {
            let mut chain = Vec::new();
            let mut node = a;
            while node > 0 {
                node = (node - 1) / BRANCH;
                chain.push(node);
            }
            chain.reverse();
            chain.push(a);
            let mut span = (0, ROWS);
            for pair in chain.windows(2) {
                span = child_span(span, pair[1] - pair[0] * BRANCH - 1);
            }
            check_round_trip(&scattered_membership(span.0..span.1, ROWS));
        }
    }

    #[test]
    fn a_membership_round_trips_and_damage_is_refused_rather_than_emptied() {
        let members = Bitmap::of(&[1, 2, 3, 70_000, 4_000_000]);
        let bytes = serialise_members(&members);
        assert_eq!(deserialise_members(&bytes), Some(members));

        // An empty membership is a real state — every member deleted — so damage must not decode
        // to it. That would make a corrupt record indistinguishable from a legitimately emptied
        // artifact, and the second is served.
        let empty = Bitmap::new();
        assert_eq!(deserialise_members(&serialise_members(&empty)), Some(empty));
        assert_eq!(deserialise_members(&[0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(deserialise_members(&[]), None);
    }

    /// **An artifact's view survives the packed extent** (`bundle_format` 8): a group-scoped
    /// level folded and reopened comes back with each record in the view it was published into,
    /// which is what keeps two views' keys apart across a fold — restored without it, the level's
    /// key index would hold one entry per key and the artifacts of one view would be drawn on
    /// every view of the group. An entity-scoped record's absent view round-trips as absent, the
    /// two being different states.
    #[test]
    fn an_artifacts_view_survives_the_packed_extent_and_absence_is_its_own_state() {
        let mut scoped = record(100, &[1, 2, 3]);
        scoped.key = Some("c1".into());
        scoped.view = Some("q1".into());
        let (restored, _) = decode_record(scoped.entity, &encode_record(&scoped, None))
            .expect("the blob round-trips");
        assert_eq!(restored.view.as_deref(), Some("q1"));
        assert_eq!(restored.key.as_deref(), Some("c1"));
        assert_eq!(restored.members, scoped.members);

        let mut plain = record(101, &[4]);
        plain.key = Some("c1".into());
        let (restored, _) = decode_record(plain.entity, &encode_record(&plain, None))
            .expect("the blob round-trips");
        assert_eq!(restored.view, None);

        // The two keys are one key in two views, and the store that seeds from the extents keeps
        // them apart on the field it just read back.
        let mut store = ArtifactStore::default();
        store.seed("clusters/q", 0, 0, scoped, None);
        let mut second = record(102, &[9]);
        second.key = Some("c1".into());
        second.view = Some("q2".into());
        store.seed("clusters/q", 0, 1, second, None);
        assert_eq!(
            store.ordinal_of_key("clusters/q", 0, Some("q1"), "c1"),
            Some(0)
        );
        assert_eq!(
            store.ordinal_of_key("clusters/q", 0, Some("q2"), "c1"),
            Some(1)
        );
    }

    /// **An attachment survives the packed extent, and a lost one is a decode failure.** A label
    /// restored as unattached is a label that serves when its cluster is suppressed — the fail-open
    /// the term exists to close, reappearing at a restart, with nothing anywhere reporting a fault.
    /// **Several parents round-trip, and a list the writer could not have produced is refused**
    /// (`dag-hierarchies.md` §7, decision 0117). The list is what `BUNDLE_FORMAT` 5 guards: the
    /// previous encoding was one byte and at most one parent.
    #[test]
    fn several_parents_round_trip_and_an_unsorted_list_is_refused() {
        use crate::wal::ParentRef;
        let mut r = record(100, &[1, 2, 3]);
        r.parents = vec![
            ParentRef {
                level: 0,
                ordinal: 3,
            },
            ParentRef {
                level: 0,
                ordinal: 7,
            },
            ParentRef {
                level: 1,
                ordinal: 0,
            },
        ];
        let blob = encode_record(&r, None);
        let (back, _) = decode_record(r.entity, &blob).expect("a whole blob decodes");
        assert_eq!(back.parents, r.parents, "every parent, in order");

        let mut root = r.clone();
        root.parents.clear();
        let (back, _) = decode_record(root.entity, &encode_record(&root, None)).unwrap();
        assert!(back.parents.is_empty(), "a root is an explicit zero");

        // A duplicate edge or an out-of-order list is not one this writer produced: the record is
        // kept ascending and deduplicated, so the reader treats anything else as another format.
        let mut twice = r.clone();
        twice.parents.push(ParentRef {
            level: 1,
            ordinal: 0,
        });
        assert!(
            decode_record(twice.entity, &encode_record(&twice, None)).is_none(),
            "a duplicate edge refuses"
        );
        let mut backwards = r.clone();
        backwards.parents.reverse();
        assert!(
            decode_record(backwards.entity, &encode_record(&backwards, None)).is_none(),
            "an unsorted list refuses"
        );

        // Every truncation inside the list refuses rather than decoding as fewer parents.
        let whole = decode_record(root.entity, &encode_record(&root, None)).is_some();
        assert!(whole);
        for len in 0..blob.len() {
            assert!(
                decode_record(r.entity, &blob[..len]).is_none(),
                "a blob truncated to {len} of {} bytes must refuse",
                blob.len()
            );
        }
    }

    #[test]
    fn an_attachment_round_trips_and_a_truncated_one_is_refused() {
        let mut r = record(100, &[1, 2, 3]);
        r.key = Some("l0".into());
        r.contents = vec![ContentSet {
            values: Some(vec!["a label".into()]),
            digest: content_digest(&["a label".to_string()]),
            generated_from: Bitmap::of(&[1, 2]),
            cardinality: 2,
        }];
        r.attached_to = Some(Attachment {
            layer: "clusters/a".into(),
            level: 0,
            ordinal: 17,
            entity: EntityId::new(4_294_901_759),
        });

        let shape = ArtifactShapes::new(vec![
            ("world".into(), vec![1, 2, 3]),
            ("map".into(), vec![9]),
        ]);
        let blob = encode_record(&r, shape.as_ref());
        let (back, back_shape) = decode_record(r.entity, &blob).expect("a whole blob decodes");
        assert_eq!(back.attached_to, r.attached_to);
        assert_eq!(back.key, r.key);
        assert_eq!(
            back_shape, shape,
            "the shapes are the membership of a shape layer"
        );
        assert_eq!(
            back_shape.as_ref().unwrap().for_view("map"),
            Some(&[9u8][..])
        );
        assert_eq!(back_shape.as_ref().unwrap().for_view("nowhere"), None);

        // An unattached artifact with no shape round-trips too, each carrying its own absence byte
        // — *unattached* and *this reader could not tell* must not encode the same, and neither
        // must *no shape* and *a shape this reader could not read*.
        let mut plain = r.clone();
        plain.attached_to = None;
        let plain_blob = encode_record(&plain, None);
        let (restored, restored_shape) = decode_record(plain.entity, &plain_blob).unwrap();
        assert_eq!(restored.attached_to, None);
        assert_eq!(restored.members, plain.members);
        assert_eq!(restored_shape, None);

        // **A shape with no view, or an empty one, is a decode failure.** The blob is written by
        // hand here because the constructor refuses to build one — which is the point: the only
        // way such a blob exists is a writer that did not go through the constructor, and the
        // reader must not accept what the writer could not have produced.
        let mut viewless = encode_record(&plain, None);
        viewless.pop();
        viewless.push(3);
        viewless.extend_from_slice(&0u16.to_le_bytes());
        assert!(
            decode_record(plain.entity, &viewless).is_none(),
            "a shape for no view is an artifact with no membership rule"
        );
        assert!(ArtifactShapes::new(vec![("a".into(), vec![])]).is_none());
        assert!(ArtifactShapes::new(vec![("a".into(), vec![1]), ("a".into(), vec![2])]).is_none());

        // Every truncation from the end of the contents onwards refuses. The one that matters is
        // the shortest: it is byte-for-byte the unattached artifact's blob without its absence
        // byte, and a reader that shrugged at a missing tail would decode it as unattached.
        for len in (plain_blob.len() - 1)..blob.len() {
            assert!(
                decode_record(r.entity, &blob[..len]).is_none(),
                "a blob truncated to {len} of {} bytes must refuse",
                blob.len()
            );
        }
    }

    /// A growth record for one artifact of one level.
    fn growth(layer: &str, level: u32, ordinal: u32, joining: &[u32]) -> crate::wal::WalRecord {
        crate::wal::WalRecord::ArtifactGrow {
            layer: layer.to_string(),
            level,
            growth: vec![crate::wal::MembershipGrowth {
                ordinal,
                joining: serialise_members(&Bitmap::of(joining)),
                leaving: Vec::new(),
                set: crate::wal::GrownSet::Membership,
            }],
        }
    }

    /// A publication of one artifact at one ordinal, so the pin cases start from the state a live
    /// deployment is in rather than from a hand-placed record.
    fn publication(
        layer: &str,
        ordinal: u32,
        entity: u64,
        members: &[u32],
    ) -> crate::wal::WalRecord {
        crate::wal::WalRecord::ArtifactPublish {
            layer: layer.to_string(),
            level: 0,
            extend_runs: Vec::new(),
            artifacts: vec![crate::wal::PublishedArtifact {
                ordinal,
                entity: EntityId::new(entity),
                key: Some(format!("c{ordinal}")),
                view: None,
                members: serialise_members(&Bitmap::of(members)),
                contents: Vec::new(),
                attached_to: None,
                parents: Vec::new(),
                shape: None,
            }],
        }
    }

    /// [`publication`] with one supplied content generated from `sources`.
    fn publication_with_content(
        layer: &str,
        ordinal: u32,
        entity: u64,
        members: &[u32],
        sources: &[u32],
    ) -> crate::wal::WalRecord {
        let mut record = publication(layer, ordinal, entity, members);
        if let crate::wal::WalRecord::ArtifactPublish { artifacts, .. } = &mut record {
            artifacts[0].contents = vec![crate::wal::PublishedContent {
                values: vec!["a label".to_string()],
                digest: content_digest(&["a label".to_string()]),
                generated_from: serialise_members(&Bitmap::of(sources)),
                cardinality: sources.len() as u64,
            }];
        }
        record
    }

    /// **`levels_moved_by` reports exactly the levels `retire` moves, and `retire` moves each by
    /// one.** The fold stamps a reported level's derived structures with `version + 1` before the
    /// retirement runs (`write.rs`'s `artifact_coordinates`), so a level reported and not moved, or
    /// moved and not reported, would hold a structure at a version describing other records.
    /// Every way a record can move is here: its own entity retired, a member retired, a
    /// generating-set member retired, and a described artifact that lost a member outside its
    /// generating set, beside a level nothing touches.
    #[test]
    fn retire_moves_exactly_the_levels_levels_moved_by_reports_and_each_by_one() {
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("own", 0, 100, &[1, 2]), 0), 0);
        assert_eq!(store.apply(&publication("member", 0, 101, &[3, 4]), 8), 0);
        assert_eq!(
            store.apply(
                &publication_with_content("withdrawn", 0, 102, &[5], &[5, 6]),
                16
            ),
            0
        );
        assert_eq!(
            store.apply(
                &publication_with_content("described", 0, 103, &[7, 8], &[7]),
                24
            ),
            0
        );
        assert_eq!(store.apply(&publication("untouched", 0, 104, &[9]), 32), 0);
        let before: BTreeMap<(String, u32), u64> = store
            .level_versions()
            .map(|(layer, level, version)| ((layer.to_string(), level), version))
            .collect();

        let retired = Bitmap::of(&[100, 4, 6, 8]);
        let mut reported = store.levels_moved_by(&retired);
        reported.sort();
        let mut moved = store.retire(&retired);
        moved.sort();

        let expected: Vec<(String, u32)> = ["described", "member", "own", "withdrawn"]
            .iter()
            .map(|layer| (layer.to_string(), 0))
            .collect();
        assert_eq!(reported, expected, "every way a record moves is reported");
        assert_eq!(moved, reported, "and the retirement moved exactly those");
        for (layer, level, after) in store.level_versions() {
            let key = (layer.to_string(), level);
            let step = u64::from(reported.contains(&key));
            assert_eq!(
                after,
                before[&key] + step,
                "{layer}: a reported level moves by one and an unreported one not at all"
            );
        }
        assert!(
            store.get("own", 0, 0).is_none(),
            "the artifact whose own entity was retired is gone"
        );
        assert_eq!(
            store.get("member", 0, 0).unwrap().members,
            Bitmap::of(&[3]),
            "the retired member left the membership"
        );
        assert!(
            store.get("withdrawn", 0, 0).unwrap().contents.is_empty(),
            "the content whose generating set lost a source is withdrawn (decision 0135)"
        );
        let described = store.get("described", 0, 0).unwrap();
        assert_eq!(
            described.members,
            Bitmap::of(&[7]),
            "a member outside the generating set leaves the membership"
        );
        assert_eq!(
            described.contents[0].generated_from,
            Bitmap::of(&[7]),
            "and the content whose set did not name it is untouched, its set unchanged"
        );
    }

    /// **Growth is a union, and it touches nothing else about the record.** The identity, the key,
    /// the contents and the edges are what the publication decided; what a join changes is the set.
    #[test]
    fn a_growth_unions_into_the_membership_and_changes_nothing_else() {
        let mut store = ArtifactStore::new();
        assert_eq!(
            store.apply(&publication("clusters/a", 0, 100, &[1, 2, 3]), 0),
            0
        );
        let before = store.level_version("clusters/a", 0);

        assert_eq!(store.apply(&growth("clusters/a", 0, 0, &[3, 4, 5]), 8), 0);

        let record = store
            .get("clusters/a", 0, 0)
            .expect("the artifact is still there");
        assert_eq!(
            record.members,
            Bitmap::of(&[1, 2, 3, 4, 5]),
            "the union, not the delta"
        );
        assert_eq!(
            record.declared_size(),
            5,
            "the criterion's denominator moves with it"
        );
        assert_eq!(record.key.as_deref(), Some("c0"));
        assert_eq!(record.entity, EntityId::new(100));
        assert!(
            store.level_version("clusters/a", 0) > before,
            "every row-space projection built from this membership is now stale; a version \
             that did not move would serve the artifact without the members that just joined"
        );
        assert_eq!(
            store.level_version("clusters/b", 0),
            0,
            "a level nobody wrote to is unmoved — the grain the scale campaign's §8.1 is about"
        );
    }

    /// **A growth cannot create an artifact**, which is what keeps it from resurrecting one a fold
    /// retired — its record can outlive the artifact in the log, and replay would then put the
    /// deleted identity back holding nothing but the join.
    #[test]
    fn a_growth_against_a_hole_adds_nothing() {
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("clusters/a", 0, 100, &[1]), 0), 0);
        store.retire(&Bitmap::of(&[100]));

        assert_eq!(
            store.apply(&growth("clusters/a", 0, 0, &[7, 8]), 8),
            0,
            "an ordinal that is a hole is a legitimate state, not a decode failure to alarm on"
        );
        assert!(
            store.get("clusters/a", 0, 0).is_none(),
            "the hole is still a hole"
        );
        assert_eq!(store.total(), 0);
    }

    /// Damage is a refusal, on the publication's argument: a delta decoded to nothing is an acked
    /// join that did not happen, and the artifact then serves the count it had before — which
    /// nothing distinguishes from a criterion it failed to clear.
    #[test]
    fn a_growth_whose_delta_will_not_decode_is_refused_and_counted() {
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("clusters/a", 0, 100, &[1]), 0), 0);
        let damaged = crate::wal::WalRecord::ArtifactGrow {
            layer: "clusters/a".to_string(),
            level: 0,
            growth: vec![crate::wal::MembershipGrowth {
                ordinal: 0,
                joining: vec![0xff, 0xff, 0xff, 0xff],
                leaving: Vec::new(),
                set: crate::wal::GrownSet::Membership,
            }],
        };
        assert_eq!(store.apply(&damaged, 8), 1);
        assert_eq!(
            store.get("clusters/a", 0, 0).unwrap().members,
            Bitmap::of(&[1]),
            "and nothing was added from bytes that are not a bitmap"
        );
    }

    /// **The failure this bookkeeping exists against, stated as an assertion.**
    ///
    /// The append-only packer starts at a level's published high-water, so a record that grew below
    /// that mark is never packed again. Releasing the log at `mark_published` — which is what marks
    /// the tail durable — would leave the growth reclaimable in the log and absent from every
    /// extent: the artifact comes back from a restart without the point, acked and silent. Only the
    /// fold's whole rewrite reaches it, so only `mark_growth_packed` releases it.
    #[test]
    fn a_growth_pins_the_log_past_every_tail_publication_and_only_a_whole_rewrite_releases_it() {
        let mut store = ArtifactStore::new();
        assert_eq!(
            store.apply(&publication("clusters/a", 0, 100, &[1, 2]), 40),
            0
        );
        store.mark_published("clusters/a", 0, 1);
        assert_eq!(
            store.oldest_wal_pos(),
            None,
            "the publication itself is in an extent, so its record is free"
        );

        assert_eq!(store.apply(&growth("clusters/a", 0, 0, &[3]), 96), 0);
        assert_eq!(
            store.oldest_wal_pos(),
            Some(96),
            "the growth is the only copy of the join"
        );

        // A second publication into the level, packed and marked. The tail is durable and the
        // growth still is not: it sits below the mark this pack started from.
        assert_eq!(
            store.apply(&publication("clusters/a", 1, 101, &[9]), 128),
            0
        );
        store.mark_published("clusters/a", 0, 2);
        assert_eq!(
            store.oldest_wal_pos(),
            Some(96),
            "marking the tail published must not release the growth below it"
        );

        store.mark_growth_packed();
        assert_eq!(
            store.oldest_wal_pos(),
            None,
            "the fold rewrote the level whole"
        );
    }

    /// The growth path is not a removal rule in the other direction: a member that joined is
    /// retired by exactly the routes a declared member is, having no separate provenance once it is
    /// in the set — and a *suppressed* member is retired by neither, growth included.
    #[test]
    fn a_member_that_joined_retires_like_any_other() {
        let mut store = ArtifactStore::new();
        assert_eq!(
            store.apply(&publication("clusters/a", 0, 100, &[1, 2]), 0),
            0
        );
        assert_eq!(store.apply(&growth("clusters/a", 0, 0, &[3, 4]), 8), 0);

        store.retire(&Bitmap::of(&[3]));
        assert_eq!(
            store.get("clusters/a", 0, 0).unwrap().members,
            Bitmap::of(&[1, 2, 4]),
            "the fold's executed deletion takes the joined member exactly as it takes a declared one"
        );
    }

    #[test]
    fn layers_do_not_bleed_into_each_other_in_key_order() {
        // The `layer` iterator walks a range of a BTreeMap keyed by `(name, level)`, so a
        // neighbouring name that sorts adjacently must not be picked up.
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1]), None);
        store.put("clusters/a-suffix", 0, 0, record(101, &[2]), None);
        store.put("clusters/b", 0, 0, record(102, &[3]), None);
        assert_eq!(store.layer("clusters/a").count(), 1);
        assert_eq!(store.layer("clusters/a-suffix").count(), 1);
    }

    // ---- Fills (`ingest.md` §1.5): one store method, replayed from the record ----------------

    fn fill_record(
        layer: &str,
        ordinal: u32,
        part: crate::wal::ArtifactPart,
    ) -> crate::wal::WalRecord {
        crate::wal::WalRecord::ArtifactFill {
            layer: layer.to_string(),
            level: 0,
            ordinal,
            part,
        }
    }

    /// **Every fixed part fills once from its record, an identical record is nothing, a differing
    /// one is damage, and a replay over a fresh store reaches the same state** — the live path
    /// and replay being one method ([`ArtifactStore::fill`]).
    #[test]
    fn each_fixed_part_fills_once_from_its_record_and_replays_to_the_same_state() {
        use crate::wal::{ArtifactPart, ParentRef, PublishedAttachment};
        let values = vec!["a topic".to_string()];
        let records = [
            publication("clusters/a", 0, 100, &[1, 2]),
            publication("clusters/a", 1, 101, &[3]),
            fill_record(
                "clusters/a",
                1,
                ArtifactPart::Parents(vec![ParentRef {
                    level: 0,
                    ordinal: 0,
                }]),
            ),
            fill_record(
                "clusters/a",
                1,
                ArtifactPart::AttachedTo(PublishedAttachment {
                    layer: "clusters/a".into(),
                    level: 0,
                    ordinal: 0,
                    entity: EntityId::new(100),
                }),
            ),
            fill_record(
                "clusters/a",
                1,
                ArtifactPart::Content {
                    rank: 0,
                    values: values.clone(),
                    digest: content_digest(&values),
                },
            ),
            fill_record(
                "clusters/a",
                1,
                ArtifactPart::Shape(
                    ArtifactShapes::new(vec![("default".into(), vec![4, 0, 1])]).unwrap(),
                ),
            ),
        ];
        let expect_state = |store: &ArtifactStore| {
            let record = store.get("clusters/a", 0, 1).unwrap();
            assert_eq!(
                record.parents,
                vec![ParentRef {
                    level: 0,
                    ordinal: 0
                }]
            );
            assert_eq!(
                record.attached_to,
                Some(Attachment {
                    layer: "clusters/a".into(),
                    level: 0,
                    ordinal: 0,
                    entity: EntityId::new(100),
                })
            );
            assert_eq!(record.contents.len(), 1);
            assert_eq!(record.contents[0].values.as_ref(), Some(&values));
            assert_eq!(record.contents[0].digest, content_digest(&values));
            assert!(store.shape_of("clusters/a", 0, 1).is_some());
            assert_eq!(
                store.cascade_from(&[EntityId::new(100)]),
                vec![EntityId::new(101)],
                "a filled attachment enters the dependency index"
            );
        };

        let mut live = ArtifactStore::new();
        for (position, record) in records.iter().enumerate() {
            assert_eq!(live.apply(record, position as u64 * 8), 0);
        }
        expect_state(&live);
        assert_eq!(
            live.oldest_wal_pos(),
            Some(0),
            "the publication pins the log first"
        );

        // Replay over a store seeded from what an extent would restore: the publications
        // through `seed`, then every record of the log.
        let mut replayed = ArtifactStore::new();
        for (position, record) in records.iter().enumerate() {
            assert_eq!(replayed.apply(record, position as u64 * 8), 0);
        }
        expect_state(&replayed);
        assert_eq!(
            replayed.level_version("clusters/a", 0),
            live.level_version("clusters/a", 0)
        );

        // Identical fills change nothing; differing ones are damage, counted and not applied.
        for record in &records[2..] {
            assert_eq!(live.apply(record, 64), 0);
        }
        expect_state(&live);
        let other = vec!["another topic".to_string()];
        assert_eq!(
            live.apply(
                &fill_record(
                    "clusters/a",
                    1,
                    ArtifactPart::Content {
                        rank: 0,
                        values: other.clone(),
                        digest: content_digest(&other),
                    }
                ),
                72
            ),
            1
        );
        assert_eq!(
            live.apply(
                &fill_record(
                    "clusters/a",
                    1,
                    ArtifactPart::Parents(vec![ParentRef {
                        level: 0,
                        ordinal: 1
                    }])
                ),
                80
            ),
            1
        );
        expect_state(&live);
        // A fill against a hole fills nothing and is counted, on the growth's argument.
        assert_eq!(
            live.apply(
                &fill_record("clusters/a", 7, ArtifactPart::Parents(Vec::new())),
                88
            ),
            1
        );
    }

    /// **A shape's digest is recomputed from the bytes beside it before it is trusted**, and a
    /// shape held differently is refused by that digest.
    #[test]
    fn a_shape_fill_whose_digest_disagrees_is_refused_and_a_differing_shape_is_a_conflict() {
        use crate::wal::ArtifactPart;
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("boxes", 0, 100, &[]), 0), 0);
        let shape = ArtifactShapes::new(vec![("default".into(), vec![1, 2, 3])]).unwrap();
        let mut tampered = shape.clone();
        tampered.digest[0] ^= 0xff;
        assert_eq!(
            store.fill("boxes", 0, 0, &ArtifactPart::Shape(tampered)),
            FillOutcome::Undecodable
        );
        assert!(store.shape_of("boxes", 0, 0).is_none());
        assert_eq!(
            store.fill("boxes", 0, 0, &ArtifactPart::Shape(shape.clone())),
            FillOutcome::Filled
        );
        assert_eq!(
            store.fill("boxes", 0, 0, &ArtifactPart::Shape(shape)),
            FillOutcome::Identical
        );
        let other = ArtifactShapes::new(vec![("default".into(), vec![9, 9, 9])]).unwrap();
        assert_eq!(
            store.fill("boxes", 0, 0, &ArtifactPart::Shape(other)),
            FillOutcome::Differs
        );
    }

    /// **A growth moves the level's version and not its lineage version; a parent fill moves
    /// both** (`ingest.md` §4.1) — the second counter that keeps a page of members from
    /// rebuilding the hierarchy.
    #[test]
    fn a_growth_leaves_the_lineage_version_and_a_parent_fill_moves_it() {
        use crate::wal::{ArtifactPart, ParentRef};
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("clusters/t", 0, 100, &[1]), 0), 0);
        assert_eq!(store.apply(&publication("clusters/t", 1, 101, &[2]), 8), 0);
        let (records, lineage) = (
            store.level_version("clusters/t", 0),
            store.lineage_version("clusters/t", 0),
        );
        assert_eq!(lineage, 2, "each publication moved it");

        assert_eq!(store.apply(&growth("clusters/t", 0, 1, &[3, 4]), 16), 0);
        assert_eq!(store.level_version("clusters/t", 0), records + 1);
        assert_eq!(store.lineage_version("clusters/t", 0), lineage);

        let content = vec!["x".to_string()];
        assert_eq!(
            store.apply(
                &fill_record(
                    "clusters/t",
                    1,
                    ArtifactPart::Content {
                        rank: 0,
                        values: content.clone(),
                        digest: content_digest(&content),
                    }
                ),
                24
            ),
            0
        );
        assert_eq!(store.level_version("clusters/t", 0), records + 2);
        assert_eq!(
            store.lineage_version("clusters/t", 0),
            lineage,
            "a content fill moves no edge"
        );

        assert_eq!(
            store.apply(
                &fill_record(
                    "clusters/t",
                    1,
                    ArtifactPart::Parents(vec![ParentRef {
                        level: 0,
                        ordinal: 0
                    }])
                ),
                32
            ),
            0
        );
        assert_eq!(store.level_version("clusters/t", 0), records + 3);
        assert_eq!(store.lineage_version("clusters/t", 0), lineage + 1);

        // A retirement of a member of the level moves both, as a hole has no edges.
        store.retire(&Bitmap::of(&[100]));
        assert_eq!(store.lineage_version("clusters/t", 0), lineage + 2);
    }

    /// **A filled content's values reach the content extent and hold the log until they do**,
    /// wherever the artifact sits against the high-water — and once packed, a further rank is
    /// refused at the registry on [`ArtifactStore::content_row_is_packed`]'s answer.
    #[test]
    fn a_content_fill_is_written_beside_the_tail_and_pins_the_log_until_its_extent_is_durable() {
        use crate::wal::ArtifactPart;
        let mut store = ArtifactStore::new();
        assert_eq!(store.apply(&publication("topics/x", 0, 100, &[1]), 0), 0);
        store.mark_published("topics/x", 0, 1);
        assert_eq!(store.oldest_wal_pos(), None, "packed, nothing pinned");
        assert!(
            !store.content_row_is_packed("topics/x", 0, 0),
            "no content, no row"
        );

        let values = vec!["shipping".to_string()];
        assert_eq!(
            store.apply(
                &fill_record(
                    "topics/x",
                    0,
                    ArtifactPart::Content {
                        rank: 0,
                        values: values.clone(),
                        digest: content_digest(&values),
                    }
                ),
                40
            ),
            0
        );
        assert_eq!(
            store.unpublished_content(),
            vec![(EntityId::new(100), vec![(0u16, "shipping".to_string())])],
            "a filled record below the high-water is written beside the tail"
        );
        assert_eq!(store.oldest_wal_pos(), Some(40), "pinned at the fill");
        assert!(
            !store.content_row_is_packed("topics/x", 0, 0),
            "pending, not yet a row"
        );

        // The fold's rewrite releases the fill's pin but not the content's: the fold carries
        // content extents forward and writes no values.
        store.mark_growth_packed();
        assert_eq!(store.oldest_wal_pos(), Some(40));
        assert_eq!(store.unpublished_content().len(), 1);

        store.mark_content_published();
        assert_eq!(store.oldest_wal_pos(), None);
        assert!(store.unpublished_content().is_empty());
        assert!(store.content_row_is_packed("topics/x", 0, 0));
    }
}
